// SPDX-License-Identifier: Apache-2.0

//! Pins the detached half of `bookrack-sandbox`: a stub and a daemon
//! held by background processes, the handoff that starts them, the stop
//! that ends them, and the recovery that closes an instance whose
//! processes are gone without ever signaling one it does not own.

#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use bookrack_test_support::tool::exec::finish_spawn;
use bookrack_test_support::tool::{Outcome, STAGE_INSTANCE};
use bookrack_test_support::{DaemonProcess, SpawnOpts, TEARDOWN_BUDGET};
use rustix::process::{Pid, Signal};
use serde_json::{Value, json};

use common::sandbox::{
    DAEMON_BODY, PRELUDE, Probe, QUIT_ARM_OK, QUIT_DAEMON_BODY, World, code, read_json,
    serve_version, stderr,
};

/// What a client adds over a supervisor's own bound, restated.
const CLIENT_MARGIN: Duration = Duration::from_secs(2);

/// Replace the stand-in with `body` after the prelude and copy it in.
fn rebuild(world: &World, body: &str) {
    common::sandbox::write_script(
        &world.stand_in,
        &format!("{}{body}\n", PRELUDE.replace("__HOOK__", "")),
    );
    let out = world.tool(&["build", &world.dir_s()]);
    assert!(out.status.success(), "build failed: {}", stderr(&out));
}

/// A stand-in that behaves like a daemon, with the control socket
/// answered by the test.
fn ready_world() -> (World, Arc<AtomicUsize>) {
    let world = World::new(DAEMON_BODY);
    let accepted = serve_version(&world.host().join("runtime/control.sock"));
    (world, accepted)
}

fn instance(world: &World, kind: &str, n: u32) -> PathBuf {
    world.dir.join("run").join(kind).join(n.to_string())
}

fn spawn(world: &World, args: &[&str]) -> Child {
    world.command(args).spawn().expect("spawn bookrack-sandbox")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Wait until `ready` holds, up to `within`.
fn wait_until(what: &str, within: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "{what} did not happen within {within:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_file(path: &Path) {
    wait_until(&path.display().to_string(), Duration::from_secs(15), || {
        path.exists()
    });
}

fn pid(raw: u32) -> Pid {
    Pid::from_raw(raw as i32).expect("a positive pid")
}

fn alive(raw: u32) -> bool {
    match rustix::process::test_kill_process(pid(raw)) {
        Ok(()) => true,
        Err(err) => err != rustix::io::Errno::SRCH,
    }
}

fn kill(raw: u32) {
    let _ = rustix::process::kill_process(pid(raw), Signal::KILL);
}

fn kill_group(raw: u32) {
    let _ = rustix::process::kill_process_group(pid(raw), Signal::KILL);
}

fn registered_pid(world: &World, kind: &str, file: &str) -> u32 {
    let path = instance(world, kind, 1).join(file);
    wait_for_file(&path);
    read_json(&path)["pid"].as_u64().expect("a registered pid") as u32
}

/// A pid file the stand-in writes through a rename, so a reader never
/// sees it half written.
fn pid_file_line(path: &Path) -> String {
    format!(
        "echo $$ > \"{0}.tmp\" && mv \"{0}.tmp\" \"{0}\"",
        path.display()
    )
}

fn read_pid(path: &Path) -> u32 {
    wait_for_file(path);
    std::fs::read_to_string(path)
        .expect("pid file")
        .trim()
        .parse()
        .expect("a pid")
}

/// A stand-in daemon that takes the host's session lock with `flock`,
/// writes it, prints the ready line, then its pid file, then sleeps.
fn holding_body(pidfile: &Path) -> String {
    format!(
        r#"exec perl -e '
use Fcntl qw(:flock);
my ($lock, $sock, $pidfile) = @ARGV;
open(my $fh, ">", $lock) or die "open: $!";
flock($fh, LOCK_EX) or die "flock: $!";
$fh->autoflush(1);
print $fh "pid=$$\nmcp=127.0.0.1:1\ncontrol_sock=$sock\n";
$| = 1;
print "bookrack daemon running: pid=$$ mcp=127.0.0.1:1 control_sock=$sock\n";
open(my $p, ">", "$pidfile.tmp") or die "pid: $!";
print $p "$$\n";
close $p;
rename("$pidfile.tmp", $pidfile) or die "rename: $!";
sleep 60;
' "$BOOKRACK_RUNTIME_DIR/bookrack.tty.lock" "$BOOKRACK_RUNTIME_DIR/control.sock" "{}""#,
        pidfile.display()
    )
}

/// One HTTP request to a stub, read by its content-length.
fn http(url: &str, method: &str, path: &str, body: &[u8]) -> (u16, Value) {
    let addr = url.strip_prefix("http://").expect("loopback url");
    let mut stream = TcpStream::connect(addr).expect("connect to the stub");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nhost: {addr}\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .expect("write");
    stream.write_all(body).expect("write body");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("status line");
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status");
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).expect("header");
        let header = header.trim().to_ascii_lowercase();
        if header.is_empty() {
            break;
        }
        if let Some(v) = header.strip_prefix("content-length:") {
            length = v.trim().parse().expect("length");
        }
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).expect("body");
    (
        status,
        serde_json::from_slice(&payload).expect("a JSON body"),
    )
}

fn stub_start(world: &World, extra: &[&str]) -> String {
    let dir = world.dir_s();
    let mut args = vec!["stub", "start", dir.as_str()];
    args.extend_from_slice(extra);
    let out = world.tool(&args);
    assert_eq!(code(&out), 0, "stub start: {}", stderr(&out));
    stdout(&out).trim().to_string()
}

fn daemon_start(world: &World) {
    daemon_start_with(world, &[]);
}

fn daemon_start_with(world: &World, extra: &[&str]) {
    let dir = world.dir_s();
    let mut args = vec!["daemon", "start", dir.as_str(), "--library", "alpha"];
    args.extend_from_slice(extra);
    let out = world.tool(&args);
    assert_eq!(code(&out), 0, "daemon start: {}", stderr(&out));
    assert!(
        stdout(&out).starts_with("bookrack daemon running: "),
        "{}",
        stdout(&out)
    );
}

fn status(world: &World, kind: &str) -> Value {
    let out = world.tool(&[kind, "status", &world.dir_s()]);
    assert_eq!(code(&out), 0, "{kind} status: {}", stderr(&out));
    serde_json::from_str(stdout(&out).trim()).expect("status is JSON")
}

fn pause_file(world: &World, point: &str) -> PathBuf {
    world.dir.join(format!(".pause-{point}"))
}

// Readiness and failure, judged by the supervisor

/// The supervisor's own teardown may take past `--timeout`: here a
/// descendant outside the daemon's group holds its stdout, so the drain
/// waits out its settle deadline. The start waits for the supervisor's
/// record rather than giving up at `--timeout` itself.
#[test]
fn a_daemon_without_a_ready_line_times_out() {
    let world = World::new("");
    let mut probe = Probe::new(world.root.path());
    let escaped = world.root.path().join("escaped.pid");
    rebuild(
        &world,
        &format!(
            "exec 3>\"{}\"; printf . >&3\n\
             perl -e 'setpgrp(0, 0); sleep 30' 3>&- &\n\
             echo $! > \"{}\"\n\
             exec sleep 30",
            probe.path.display(),
            escaped.display()
        ),
    );
    let started = Instant::now();
    let launched = spawn(
        &world,
        &[
            "daemon",
            "start",
            &world.dir_s(),
            "--library",
            "alpha",
            "--timeout",
            "1",
        ],
    );
    probe.wait_held();
    let out = launched.wait_with_output().expect("daemon start");
    let elapsed = started.elapsed();
    kill(read_pid(&escaped));
    assert_eq!(code(&out), 124, "{}", stderr(&out));
    let record = world.last_record();
    assert_eq!(
        (record["class"].as_str(), record["stage"].as_str()),
        (Some("timeout"), Some("ready-line")),
        "{record}"
    );
    let exit = read_json(&instance(&world, "daemon", 1).join("exit.json"));
    assert_eq!(exit["stage"], "ready-line", "{exit}");
    assert!(
        exit["detail"]
            .as_str()
            .is_some_and(|d| d.contains("stdout was still open")),
        "premise: the drain waited out the held stream: {exit}"
    );
    let budget = Duration::from_secs(1) + TEARDOWN_BUDGET + CLIENT_MARGIN;
    assert!(elapsed < budget, "took {elapsed:?}, past {budget:?}");
    assert!(
        probe.closes_within(Duration::from_secs(2)),
        "the stand-in outlived the start"
    );
    let supervisor = registered_pid(&world, "daemon", "supervisor.json");
    wait_until("the supervisor leaving", Duration::from_secs(5), || {
        !alive(supervisor)
    });
}

#[test]
fn a_ready_line_without_a_control_plane_answer_times_out() {
    let world = World::new(DAEMON_BODY);
    let listener =
        UnixListener::bind(world.host().join("runtime/control.sock")).expect("bind control");
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });
    let started = Instant::now();
    let out = world.tool(&[
        "daemon",
        "start",
        &world.dir_s(),
        "--library",
        "alpha",
        "--timeout",
        "1",
    ]);
    let elapsed = started.elapsed();
    assert_eq!(code(&out), 124, "{}", stderr(&out));
    assert_eq!(world.last_record()["stage"], "control-plane");
    let budget = Duration::from_secs(1) + TEARDOWN_BUDGET + CLIENT_MARGIN;
    assert!(elapsed < budget, "took {elapsed:?}, past {budget:?}");
}

#[test]
fn a_daemon_that_exits_before_ready_records_its_status() {
    let world = World::new(
        "printf 'pid=%s\\n' $$ > \"$BOOKRACK_RUNTIME_DIR/bookrack.tty.lock\"; \
         echo 'dying on purpose' >&2; exit 3",
    );
    let out = world.tool(&["daemon", "start", &world.dir_s(), "--library", "alpha"]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert_eq!(world.last_record()["class"], "not-ready");
    let instance = instance(&world, "daemon", 1);
    let exit = read_json(&instance.join("exit.json"));
    assert_eq!(exit["code"], 3, "{exit}");
    let daemon_stderr = std::fs::read_to_string(instance.join("stderr")).expect("stderr");
    assert!(
        daemon_stderr.contains("dying on purpose"),
        "{daemon_stderr}"
    );
}

#[test]
fn a_leftover_lock_does_not_make_a_daemon_ready() {
    let world = World::new("exec sleep 30");
    let mut other = Command::new("sleep").arg("30").spawn().expect("sleep");
    std::fs::write(
        world.tty_lock(),
        format!(
            "pid={}\nmcp=127.0.0.1:1\ncontrol_sock={}\n",
            other.id(),
            world.host().join("runtime/control.sock").display()
        ),
    )
    .expect("write lock");
    let out = world.tool(&[
        "daemon",
        "start",
        &world.dir_s(),
        "--library",
        "alpha",
        "--timeout",
        "1",
    ]);
    let _ = other.kill();
    let _ = other.wait();
    assert_eq!(code(&out), 124, "{}", stderr(&out));
}

#[test]
fn a_ready_line_naming_a_foreign_socket_is_refused() {
    let world = World::new("");
    let foreign = world.root.path().join("foreign.sock");
    let listener = UnixListener::bind(&foreign).expect("bind foreign");
    listener.set_nonblocking(true).expect("nonblocking");
    rebuild(
        &world,
        &format!(
            "printf 'pid=%s\\nmcp=127.0.0.1:1\\ncontrol_sock=%s\\n' $$ \"{0}\" > \
             \"$BOOKRACK_RUNTIME_DIR/bookrack.tty.lock\"\n\
             echo \"bookrack daemon running: pid=$$ mcp=127.0.0.1:1 control_sock={0}\"\n\
             exec sleep 30",
            foreign.display()
        ),
    );
    let out = world.tool(&["daemon", "start", &world.dir_s(), "--library", "alpha"]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert_eq!(world.last_record()["stage"], "endpoint");
    assert!(listener.accept().is_err(), "the foreign socket was dialed");
}

// The handoff

#[test]
fn a_start_killed_before_handoff_starts_nothing() {
    let world = World::new("");
    let marker = world.root.path().join("started.pid");
    rebuild(
        &world,
        &format!("{}\n{DAEMON_BODY}", pid_file_line(&marker)),
    );
    let mut launched = spawn(
        &world,
        &[
            "--pause-at",
            "start:before-go",
            "daemon",
            "start",
            &world.dir_s(),
            "--library",
            "alpha",
        ],
    );
    let pause = pause_file(&world, "start:before-go");
    wait_for_file(&pause);
    launched.kill().expect("kill the start");
    launched.wait().expect("reap the start");
    std::fs::remove_file(&pause).expect("remove the pause file");

    let exit_path = instance(&world, "daemon", 1).join("exit.json");
    wait_for_file(&exit_path);
    let exit = read_json(&exit_path);
    assert_eq!(
        (
            exit["class"].as_str(),
            exit["stage"].as_str(),
            exit["phase"].as_str()
        ),
        (Some("refused"), Some("handoff"), Some("handoff")),
        "{exit}"
    );
    assert!(!marker.exists(), "the daemon started without a go");
    let out = world.tool(&["daemon", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

#[test]
fn a_background_dead_before_go_leaves_no_open_instance() {
    for (kind, registration, extra) in [
        ("daemon", "supervisor.json", &["--library", "alpha"][..]),
        ("stub", "serve.json", &[][..]),
    ] {
        let world = World::new("");
        let dir = world.dir_s();
        let mut args = vec!["--pause-at", "start:before-go", kind, "start", dir.as_str()];
        args.extend_from_slice(extra);
        let launched = spawn(&world, &args);
        let pause = pause_file(&world, "start:before-go");
        wait_for_file(&pause);
        let background = registered_pid(&world, kind, registration);
        kill(background);
        std::thread::sleep(Duration::from_millis(300));
        std::fs::remove_file(&pause).expect("remove the pause file");
        let out = launched.wait_with_output().expect("start");
        assert_eq!(code(&out), 125, "{kind}: {}", stderr(&out));
        assert_eq!(world.last_record()["stage"], "handoff", "{kind}");
        let recovered = read_json(&instance(&world, kind, 1).join("recovered.json"));
        assert_eq!(
            recovered["reason"], "ended-without-record",
            "{kind}: {recovered}"
        );
        let out = world.tool(&["restore", &world.dir_s()]);
        assert_eq!(code(&out), 0, "{kind}: restore: {}", stderr(&out));
    }
}

#[test]
fn a_start_killed_after_its_intent_is_recoverable() {
    let (world, _accepted) = ready_world();
    let mut launched = spawn(
        &world,
        &[
            "--pause-at",
            "start:after-intent",
            "daemon",
            "start",
            &world.dir_s(),
            "--library",
            "alpha",
        ],
    );
    let pause = pause_file(&world, "start:after-intent");
    wait_for_file(&pause);
    launched.kill().expect("kill the start");
    launched.wait().expect("reap the start");
    std::fs::remove_file(&pause).expect("remove the pause file");

    let out = world.tool(&["daemon", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let recovered = read_json(&instance(&world, "daemon", 1).join("recovered.json"));
    assert_eq!(recovered["reason"], "never-handed-off", "{recovered}");
    daemon_start(&world);
    let out = world.tool(&["teardown", &world.dir_s()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

#[test]
fn a_supervisor_lost_while_spawning_is_refused() {
    let world = World::new("");
    let pidfile = world.root.path().join("holder.pid");
    rebuild(&world, &holding_body(&pidfile));
    let launched = spawn(
        &world,
        &[
            "--pause-at",
            "supervise:after-spawn",
            "daemon",
            "start",
            &world.dir_s(),
            "--library",
            "alpha",
        ],
    );
    let holder = read_pid(&pidfile);
    let supervisor = registered_pid(&world, "daemon", "supervisor.json");
    kill(supervisor);
    let out = launched.wait_with_output().expect("start");
    assert_eq!(code(&out), 125, "{}", stderr(&out));

    let state = status(&world, "daemon");
    assert_eq!(state["unfinished"][0]["state"], "unknown", "{state}");
    let out = world.tool(&["daemon", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("spawning") && stderr(&out).contains("lock is held"),
        "{}",
        stderr(&out)
    );
    assert!(
        alive(holder),
        "the stop signaled the daemon it could not judge"
    );
    kill(holder);
    wait_until("the holder leaving", Duration::from_secs(5), || {
        !alive(holder)
    });
    let out = world.tool(&["daemon", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 125, "spawning stays unknown: {}", stderr(&out));
    let out = world.tool(&["daemon", "stop", &world.dir_s(), "--forget"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let _ = std::fs::remove_file(pause_file(&world, "supervise:after-spawn"));
}

#[test]
fn a_spawning_mark_is_refused_even_without_the_lock() {
    let world = World::new("");
    let pidfile = world.root.path().join("holder.pid");
    rebuild(
        &world,
        &format!("{}\nexec sleep 60", pid_file_line(&pidfile)),
    );
    let launched = spawn(
        &world,
        &[
            "--pause-at",
            "supervise:after-spawn",
            "daemon",
            "start",
            &world.dir_s(),
            "--library",
            "alpha",
        ],
    );
    let holder = read_pid(&pidfile);
    kill(registered_pid(&world, "daemon", "supervisor.json"));
    let _ = launched.wait_with_output();
    let out = world.tool(&["daemon", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert!(
        alive(holder),
        "the stop signaled the daemon it could not judge"
    );
    kill(holder);
    let out = world.tool(&["daemon", "stop", &world.dir_s(), "--forget"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let _ = std::fs::remove_file(pause_file(&world, "supervise:after-spawn"));
}

#[test]
fn a_supervisor_closing_mid_request_is_read_from_its_exit() {
    let world = World::new("");
    let instance = instance(&world, "daemon", 1);
    std::fs::create_dir_all(&instance).expect("instance");
    let me = std::process::id();
    std::fs::write(
        instance.join("start.json"),
        json!({ "args": ["run"], "dotenv": null, "started": 0.0, "parent_pid": me,
                "hosting": "detached" })
        .to_string(),
    )
    .expect("start.json");
    std::fs::write(
        instance.join("supervisor.json"),
        json!({ "pid": me, "pgid": me, "nonce": "forged-1" }).to_string(),
    )
    .expect("supervisor.json");
    std::fs::write(
        instance.join("spawn.json"),
        json!({ "spawning": false, "pid": me, "pgid": me }).to_string(),
    )
    .expect("spawn.json");
    let sock = instance.join("supervisor.sock");
    eprintln!("supervisor.sock is {} bytes", sock.as_os_str().len());
    let listener = UnixListener::bind(&sock).expect("bind supervisor.sock");
    let exit_path = instance.join("exit.json");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            if line.contains("status") {
                let mut stream = stream;
                let _ = writeln!(
                    stream,
                    "{}",
                    json!({ "n": 1, "nonce": "forged-1", "phase": "running", "daemon_pid": me })
                );
            } else {
                drop(stream);
                let exit_path = exit_path.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(300));
                    std::fs::write(
                        &exit_path,
                        json!({ "class": "exited", "stage": "run", "code": 0, "signal": null,
                                "detail": null, "finished": 0.0 })
                        .to_string(),
                    )
                    .expect("exit.json");
                });
            }
        }
    });
    let started = Instant::now();
    let out = world.tool(&["daemon", "stop", &world.dir_s()]);
    let elapsed = started.elapsed();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("\"exited\""), "{}", stdout(&out));
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
}

// Stopping and recovery

#[test]
fn a_stop_during_bring_up_stops_the_daemon() {
    let world = World::new("");
    let mut probe = Probe::new(world.root.path());
    rebuild(
        &world,
        &format!(
            "exec 3>\"{}\"; printf . >&3; exec sleep 60",
            probe.path.display()
        ),
    );
    let mut launched = spawn(
        &world,
        &[
            "daemon",
            "start",
            &world.dir_s(),
            "--library",
            "alpha",
            "--timeout",
            "60",
        ],
    );
    probe.wait_held();
    let spawn_json = instance(&world, "daemon", 1).join("spawn.json");
    wait_until("a recorded daemon pid", Duration::from_secs(10), || {
        spawn_json.exists() && read_json(&spawn_json)["pid"].is_u64()
    });
    launched.kill().expect("kill the start");
    launched.wait().expect("reap the start");

    let started = Instant::now();
    let out = world.tool(&["daemon", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    let notes = world.last_record()["notes"].to_string();
    assert!(
        notes.contains("stopped in phase starting"),
        "the stop must be answered by the supervisor, not read from its exit: {notes}"
    );
    let exit = read_json(&instance(&world, "daemon", 1).join("exit.json"));
    assert_eq!(
        (exit["class"].as_str(), exit["phase"].as_str()),
        (Some("not-ready"), Some("starting")),
        "{exit}"
    );
    assert!(
        probe.closes_within(Duration::from_secs(2)),
        "the stand-in outlived the stop"
    );
}

#[test]
fn a_lost_supervisor_is_left_for_the_operator() {
    let world = World::new("");
    let mut probe = Probe::new(world.root.path());
    rebuild(
        &world,
        &format!(
            "exec 3>\"{}\"; printf . >&3\n{DAEMON_BODY}",
            probe.path.display()
        ),
    );
    let _accepted = serve_version(&world.host().join("runtime/control.sock"));
    daemon_start(&world);
    let state = status(&world, "daemon");
    assert_eq!(state["unfinished"][0]["state"], "running", "{state}");
    let supervisor = registered_pid(&world, "daemon", "supervisor.json");
    let daemon = read_json(&instance(&world, "daemon", 1).join("spawn.json"))["pid"]
        .as_u64()
        .expect("daemon pid") as u32;
    kill(supervisor);
    wait_until("the supervisor leaving", Duration::from_secs(5), || {
        !alive(supervisor)
    });

    let state = status(&world, "daemon");
    assert_eq!(state["unfinished"][0]["state"], "held", "{state}");
    let out = world.tool(&["daemon", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&daemon.to_string()),
        "{}",
        stderr(&out)
    );
    assert!(alive(daemon), "the fallback signaled the daemon");
    kill_group(daemon);
    assert!(
        probe.closes_within(Duration::from_secs(2)),
        "the daemon did not die"
    );
    wait_until("the daemon leaving", Duration::from_secs(5), || {
        !alive(daemon)
    });
    let out = world.tool(&["daemon", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let recovered = read_json(&instance(&world, "daemon", 1).join("recovered.json"));
    assert_eq!(recovered["reason"], "lost-supervisor", "{recovered}");
    assert_eq!(status(&world, "daemon")["unfinished"], json!([]));
}

#[tokio::test]
async fn a_record_failure_after_spawn_still_reclaims_and_records() {
    let (mut probe, stdin) = common::Probe::new();
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg("exec sleep 30");
    let daemon = DaemonProcess::spawn_with(
        cmd,
        SpawnOpts {
            stdin,
            ..SpawnOpts::default()
        },
    )
    .expect("spawn sh");
    probe.assert_open().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let failed = Err(Outcome::io(
        STAGE_INSTANCE,
        "write daemon.json",
        std::io::Error::other("injected write failure"),
    ));
    let outcome = match finish_spawn(daemon, dir.path(), failed).await {
        Err(outcome) => outcome,
        Ok(_) => panic!("a failed write handed the daemon back"),
    };
    assert_eq!(outcome.stage, "instance", "{outcome:?}");
    probe
        .assert_closed("the daemon after a failed record")
        .await;
    let exit = read_json(&dir.path().join("exit.json"));
    assert_eq!(
        (exit["class"].as_str(), exit["stage"].as_str()),
        (Some("tool-error"), Some("instance")),
        "{exit}"
    );
    assert!(
        exit["detail"]
            .as_str()
            .is_some_and(|d| d.contains("injected write failure")),
        "{exit}"
    );
}

#[test]
fn an_in_process_instance_waits_for_its_holder() {
    let world = World::new("");
    let mut holder = Command::new("sleep").arg("30").spawn().expect("sleep");
    let mut gone = Command::new("true").spawn().expect("true");
    let gone_pid = gone.id();
    gone.wait().expect("reap");
    let instance = instance(&world, "daemon", 1);
    std::fs::create_dir_all(&instance).expect("instance");
    std::fs::write(
        instance.join("start.json"),
        json!({ "args": ["run"], "dotenv": null, "started": 0.0, "parent_pid": holder.id() })
            .to_string(),
    )
    .expect("start.json");
    std::fs::write(
        instance.join("spawn.json"),
        json!({ "spawning": false, "pid": gone_pid, "pgid": gone_pid }).to_string(),
    )
    .expect("spawn.json");

    let out = world.tool(&["daemon", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&holder.id().to_string()),
        "{}",
        stderr(&out)
    );
    holder.kill().expect("kill the holder");
    holder.wait().expect("reap the holder");
    let out = world.tool(&["daemon", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let recovered = read_json(&instance.join("recovered.json"));
    assert_eq!(recovered["reason"], "lost-holder", "{recovered}");
}

#[test]
fn a_stop_never_signals_a_process_it_does_not_own() {
    let world = World::new("");
    let mut other = Command::new("sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .expect("sleep");
    let other_pid = other.id();
    let instance = instance(&world, "daemon", 1);
    std::fs::create_dir_all(&instance).expect("instance");
    std::fs::write(
        instance.join("start.json"),
        json!({ "args": ["run"], "dotenv": null, "started": 0.0, "parent_pid": other_pid,
                "hosting": "detached" })
        .to_string(),
    )
    .expect("start.json");
    std::fs::write(
        instance.join("supervisor.json"),
        json!({ "pid": other_pid, "pgid": other_pid, "nonce": "forged" }).to_string(),
    )
    .expect("supervisor.json");
    std::fs::write(
        instance.join("spawn.json"),
        json!({ "spawning": false, "pid": other_pid, "pgid": other_pid }).to_string(),
    )
    .expect("spawn.json");
    std::fs::write(
        world.tty_lock(),
        format!(
            "pid={other_pid}\nmcp=127.0.0.1:1\ncontrol_sock={}\n",
            world.host().join("runtime/control.sock").display()
        ),
    )
    .expect("lock");

    let out = world.tool(&["daemon", "stop", &world.dir_s()]);
    let still_running = other.try_wait().expect("look at the process").is_none();
    let _ = other.kill();
    let _ = other.wait();
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert!(stderr(&out).contains("--forget"), "{}", stderr(&out));
    assert!(still_running, "the stop signaled a process it does not own");
}

#[test]
fn stop_and_teardown_are_idempotent() {
    let (world, _accepted) = ready_world();
    stub_start(&world, &[]);
    daemon_start(&world);
    for args in [
        ["daemon", "stop"],
        ["daemon", "stop"],
        ["teardown", ""],
        ["teardown", ""],
    ] {
        let dir = world.dir_s();
        let args: Vec<&str> = args
            .iter()
            .copied()
            .filter(|a| !a.is_empty())
            .chain([dir.as_str()])
            .collect();
        let out = world.tool(&args);
        assert_eq!(code(&out), 0, "{args:?}: {}", stderr(&out));
    }
}

#[test]
fn a_stop_that_has_to_kill_reports_a_timeout() {
    let world = World::new(&DAEMON_BODY.replace("trap 'exit 0' TERM", "trap '' TERM"));
    let _accepted = serve_version(&world.host().join("runtime/control.sock"));
    daemon_start(&world);
    let started = Instant::now();
    let out = world.tool(&["daemon", "stop", &world.dir_s(), "--timeout", "1"]);
    let elapsed = started.elapsed();
    assert_eq!(code(&out), 124, "{}", stderr(&out));
    assert_eq!(world.last_record()["class"], "timeout");
    let budget = Duration::from_secs(1) + TEARDOWN_BUDGET + CLIENT_MARGIN;
    assert!(elapsed < budget, "took {elapsed:?}, past {budget:?}");
}

// The stub

#[test]
fn a_stub_stop_checks_its_identity() {
    let world = World::new("");
    let url = stub_start(&world, &[]);
    let serve = instance(&world, "stub", 1).join("serve.json");
    let real = std::fs::read(&serve).expect("serve.json");
    let mut forged = read_json(&serve);
    forged["nonce"] = json!("forged");
    std::fs::write(&serve, forged.to_string()).expect("forge");

    let out = world.tool(&["stub", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert_eq!(
        http(&url, "GET", "/__stub/identity", b"").0,
        200,
        "the stub was stopped"
    );

    std::fs::write(&serve, real).expect("restore serve.json");
    let out = world.tool(&["stub", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

/// A stub started without `--vectors` answers in the topic mode, the
/// fill the acceptance check reads against.
#[test]
fn a_stub_starts_in_the_topic_mode_by_default() {
    let world = World::new("");
    stub_start(&world, &[]);
    let record = read_json(&instance(&world, "stub", 1).join("start.json"));
    assert_eq!(record["vectors"], "topic", "{record}");
    assert_eq!(record["dimension"], 1024, "{record}");
    let out = world.tool(&["stub", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

#[test]
fn a_stub_starts_once_and_stops_twice() {
    let world = World::new("");
    let url = stub_start(&world, &[]);
    let out = world.tool(&["stub", "start", &world.dir_s()]);
    assert_eq!(code(&out), 125, "a second start: {}", stderr(&out));
    let state = status(&world, "stub");
    assert_eq!(state["unfinished"][0]["state"], "running", "{state}");
    assert_eq!(state["unfinished"][0]["url"], url.as_str(), "{state}");
    for _ in 0..2 {
        let out = world.tool(&["stub", "stop", &world.dir_s()]);
        assert_eq!(code(&out), 0, "{}", stderr(&out));
    }
    assert_eq!(status(&world, "stub")["unfinished"], json!([]));
    let again = stub_start(&world, &[]);
    let second = instance(&world, "stub", 2);
    assert_eq!(
        std::fs::read_to_string(second.join("url"))
            .expect("url")
            .trim(),
        again
    );
    assert_ne!(
        read_json(&second.join("serve.json"))["nonce"],
        read_json(&instance(&world, "stub", 1).join("serve.json"))["nonce"]
    );
    let out = world.tool(&["stub", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

#[test]
fn a_stub_width_out_of_range_is_refused_before_it_starts() {
    let world = World::new("");
    for dim in ["0", "8193"] {
        let out = world.tool(&["stub", "start", &world.dir_s(), "--dim", dim]);
        assert_eq!(code(&out), 125, "{dim}: {}", stderr(&out));
        assert_eq!(world.last_record()["stage"], "stub", "{dim}");
        assert!(stderr(&out).contains("1 to 8192"), "{}", stderr(&out));
    }
    assert!(
        std::fs::read_dir(world.dir.join("run/stub"))
            .expect("run/stub")
            .next()
            .is_none(),
        "a refused width left an instance"
    );
}

#[test]
fn commands_reach_the_running_stub() {
    let world = World::new("echo bookrack 0.0.0-sandbox");
    let url = stub_start(&world, &[]);
    let out = world.tool(&["env", &world.dir_s()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains(&format!("export BOOKRACK_OLLAMA_URL='{url}'")),
        "{}",
        stdout(&out)
    );
    let out = world.tool(&["tripwire", &world.dir_s(), "--", "--version"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let serve = registered_pid(&world, "stub", "serve.json");
    kill(serve);
    wait_until("the stub leaving", Duration::from_secs(5), || !alive(serve));
    let out = world.exec(&[], &["--version"]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert_eq!(world.last_record()["stage"], "stub");
}

#[test]
fn a_stub_without_a_url_is_refused_not_skipped() {
    let world = World::new("");
    let ran = world.root.path().join("ran");
    rebuild(&world, &format!("touch \"{}\"; echo ran", ran.display()));
    let mut other = Command::new("sleep").arg("30").spawn().expect("sleep");
    let instance = instance(&world, "stub", 1);
    std::fs::create_dir_all(&instance).expect("instance");
    std::fs::write(
        instance.join("start.json"),
        json!({ "dimension": 8, "vectors": "spread", "identity": "forged", "started": 0.0,
                "parent_pid": other.id() })
        .to_string(),
    )
    .expect("start.json");
    std::fs::write(
        instance.join("serve.json"),
        json!({ "pid": other.id(), "pgid": other.id(), "nonce": "forged" }).to_string(),
    )
    .expect("serve.json");
    let out = world.exec(&[], &["--version"]);
    let _ = other.kill();
    let _ = other.wait();
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert_eq!(world.last_record()["stage"], "stub");
    assert!(
        !ran.exists(),
        "the command ran with the stub unaccounted for"
    );
}

#[test]
fn the_stub_stays_while_a_daemon_runs() {
    let (world, _accepted) = ready_world();
    stub_start(&world, &[]);
    daemon_start(&world);
    for args in [["stub", "stop"], ["stub", "start"]] {
        let out = world.tool(&[args[0], args[1], &world.dir_s()]);
        assert_eq!(code(&out), 125, "{args:?}: {}", stderr(&out));
        assert_eq!(world.last_record()["stage"], "instance", "{args:?}");
    }
    let out = world.tool(&["teardown", &world.dir_s()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(instance(&world, "daemon", 1).join("exit.json").exists());
    assert!(instance(&world, "stub", 1).join("exit.json").exists());
}

#[test]
fn a_started_stub_answers_at_its_width() {
    let world = World::new("");
    let url = stub_start(&world, &["--dim", "16", "--vectors", "spread"]);
    let embed = json!({ "model": "any", "input": ["one"] }).to_string();
    let (status_code, body) = http(&url, "POST", "/api/embed", embed.as_bytes());
    assert_eq!(status_code, 200, "{body}");
    assert_eq!(body["embeddings"][0].as_array().map(Vec::len), Some(16));
    let out = world.tool(&["stub", "fail", &world.dir_s(), "overloaded"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(http(&url, "POST", "/api/embed", embed.as_bytes()).0, 503);
    let out = world.tool(&["stub", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let addr = url.strip_prefix("http://").expect("loopback");
    assert!(
        TcpStream::connect(addr).is_err(),
        "the stopped stub still answers"
    );
}

// Stopping through quit

/// A daemon-shaped stand-in whose `quit` runs `arm`, with the control
/// socket answered by the test. The root is written into the script.
fn quit_world(arm: &str) -> (World, Arc<AtomicUsize>) {
    let world = World::new("");
    let root = world.root.path().canonicalize().expect("canonical");
    let body = QUIT_DAEMON_BODY
        .replace("__QUIT_ARM__", arm)
        .replace("__ROOT__", &root.display().to_string());
    rebuild(&world, &body);
    let accepted = serve_version(&world.host().join("runtime/control.sock"));
    (world, accepted)
}

fn quit_file(world: &World, name: &str) -> PathBuf {
    world
        .root
        .path()
        .canonicalize()
        .expect("canonical")
        .join(name)
}

/// The variable `key` as `quit` saw it, from the environment it wrote.
fn quit_env(world: &World, key: &str) -> Option<String> {
    let env = std::fs::read_to_string(quit_file(world, "quit.env")).expect("quit.env");
    env.lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")).map(str::to_string))
}

/// Q1: a quit stop runs `quit` in the daemon's plan, sends no signal,
/// and waits for the exit the daemon makes on its own.
#[test]
fn a_quit_stop_runs_quit_in_the_plan_and_waits() {
    let (world, _accepted) = quit_world(QUIT_ARM_OK);
    let url = stub_start(&world, &[]);
    daemon_start(&world);
    let out = world.tool(&["daemon", "stop", &world.dir_s(), "--via", "quit"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let exit = read_json(&instance(&world, "daemon", 1).join("exit.json"));
    assert_eq!(
        (
            exit["class"].as_str(),
            exit["code"].as_i64(),
            exit["phase"].as_str()
        ),
        (Some("exited"), Some(0), Some("running")),
        "{exit}"
    );
    assert_eq!(
        quit_env(&world, "HOME").as_deref(),
        Some(world.host().join("home").to_str().unwrap()),
        "quit ran in the sandbox's plan"
    );
    assert_eq!(
        quit_env(&world, "BOOKRACK_OLLAMA_URL").as_deref(),
        Some(url.as_str())
    );
    assert!(
        !quit_file(&world, "term.seen").exists(),
        "no signal was sent after quit"
    );
    let notes = world.last_record()["notes"].to_string();
    assert!(notes.contains("quit: exited 0"), "{notes}");
}

/// Q1': when the daemon leaves before the stop request reaches the
/// supervisor, the supervisor's own record still says `running`, and the
/// stop reports the daemon's exit rather than a bare ok.
#[test]
fn a_daemon_gone_before_the_stop_request_reports_its_own_exit() {
    for (arm, expected) in [(QUIT_ARM_OK, 0), (r#"printf 3 > "__ROOT__/quit.flag""#, 3)] {
        let (world, _accepted) = quit_world(arm);
        stub_start(&world, &[]);
        daemon_start(&world);
        let mut stopping = spawn(
            &world,
            &[
                "--pause-at",
                "stop:before-request",
                "daemon",
                "stop",
                &world.dir_s(),
                "--via",
                "quit",
            ],
        );
        let pause = pause_file(&world, "stop:before-request");
        wait_for_file(&pause);
        wait_for_file(&instance(&world, "daemon", 1).join("exit.json"));
        std::fs::remove_file(&pause).expect("resume the stop");
        let status = stopping.wait().expect("the stop ends");
        assert_eq!(status.code(), Some(expected), "arm {arm:?}");
        let exit = read_json(&instance(&world, "daemon", 1).join("exit.json"));
        assert_eq!(
            (
                exit["class"].as_str(),
                exit["code"].as_i64(),
                exit["phase"].as_str()
            ),
            (Some("exited"), Some(expected as i64), Some("running")),
            "{exit}"
        );
    }
}

/// Q2: a quit the daemon ignores ends in the wait's timeout, not in a
/// signal.
#[test]
fn a_quit_the_daemon_ignores_ends_in_a_timeout() {
    let (world, _accepted) = quit_world(":");
    stub_start(&world, &[]);
    daemon_start(&world);
    let started = Instant::now();
    let out = world.tool(&[
        "daemon",
        "stop",
        &world.dir_s(),
        "--via",
        "quit",
        "--timeout",
        "1",
    ]);
    let elapsed = started.elapsed();
    assert_eq!(code(&out), 124, "{}", stderr(&out));
    assert_eq!(world.last_record()["class"], "timeout");
    let budget = Duration::from_secs(1) + TEARDOWN_BUDGET + CLIENT_MARGIN + Duration::from_secs(5);
    assert!(elapsed < budget, "took {elapsed:?}, past {budget:?}");
    assert!(!quit_file(&world, "term.seen").exists());
}

/// Q3: a quit that fails leaves the daemon running for a later stop.
#[test]
fn a_failed_quit_leaves_the_daemon_running() {
    let (world, _accepted) = quit_world("exit 3");
    stub_start(&world, &[]);
    daemon_start(&world);
    let out = world.tool(&["daemon", "stop", &world.dir_s(), "--via", "quit"]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    let record = world.last_record();
    assert_eq!(record["class"], "tool-error", "{record}");
    assert_eq!(record["stage"], "stop", "{record}");
    assert!(
        record["problem"]["detail"]
            .as_str()
            .is_some_and(|d| d.contains('3')),
        "{record}"
    );
    let state = status(&world, "daemon");
    assert_eq!(state["unfinished"][0]["state"], "running", "{state}");
    let out = world.tool(&["daemon", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

/// Q3': a quit that took effect and then failed is a failed quit to the
/// tool, which does not touch the daemon; the daemon's own exit is read
/// from the record the supervisor wrote.
#[test]
fn a_quit_that_fails_after_taking_effect_is_read_from_the_record() {
    let (world, _accepted) = quit_world(&format!("{QUIT_ARM_OK}; exit 1"));
    stub_start(&world, &[]);
    daemon_start(&world);
    let out = world.tool(&["daemon", "stop", &world.dir_s(), "--via", "quit"]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert_eq!(world.last_record()["stage"], "stop");
    assert!(!quit_file(&world, "term.seen").exists());
    wait_for_file(&instance(&world, "daemon", 1).join("exit.json"));
    let state = status(&world, "daemon");
    assert_eq!(state["unfinished"], json!([]), "{state}");
    let exit = &state["last_ended"]["exit"];
    assert_eq!(
        (
            exit["class"].as_str(),
            exit["code"].as_i64(),
            exit["phase"].as_str()
        ),
        (Some("exited"), Some(0), Some("running")),
        "{state}"
    );
}

/// Q5: a quit stop during bring-up cancels the start the way a signal
/// stop does, and runs no `quit`.
#[test]
fn quit_during_bring_up_cancels_like_term() {
    let world = World::new("");
    let root = world.root.path().canonicalize().expect("canonical");
    let quit_arm = QUIT_DAEMON_BODY
        .split("esac")
        .next()
        .expect("the quit arm")
        .to_string()
        + "esac\nexec sleep 60\n";
    rebuild(
        &world,
        &quit_arm
            .replace("__QUIT_ARM__", QUIT_ARM_OK)
            .replace("__ROOT__", &root.display().to_string()),
    );
    let mut launched = spawn(
        &world,
        &[
            "daemon",
            "start",
            &world.dir_s(),
            "--library",
            "alpha",
            "--timeout",
            "60",
        ],
    );
    let spawn_json = instance(&world, "daemon", 1).join("spawn.json");
    wait_until("a recorded daemon pid", Duration::from_secs(10), || {
        spawn_json.exists() && read_json(&spawn_json)["pid"].is_u64()
    });
    launched.kill().expect("kill the start");
    launched.wait().expect("reap the start");

    let out = world.tool(&["daemon", "stop", &world.dir_s(), "--via", "quit"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let exit = read_json(&instance(&world, "daemon", 1).join("exit.json"));
    assert_eq!(
        (exit["class"].as_str(), exit["phase"].as_str()),
        (Some("not-ready"), Some("starting")),
        "{exit}"
    );
    assert!(!root.join("quit.flag").exists(), "quit ran during bring-up");
}

/// T9': a quit stop runs the tripwires first; a session lock pointing
/// at a foreign endpoint stops it before `quit` runs.
#[test]
fn a_quit_stop_checks_the_endpoint_again() {
    let (world, _accepted) = quit_world(QUIT_ARM_OK);
    stub_start(&world, &[]);
    daemon_start(&world);
    let lock = world.tty_lock();
    let original = std::fs::read_to_string(&lock).expect("lock");
    let foreign_dir = tempfile::tempdir().expect("foreign dir");
    let foreign = foreign_dir.path().join("f.sock");
    let listener = UnixListener::bind(&foreign).expect("bind the foreign socket");
    listener.set_nonblocking(true).expect("nonblocking");
    let forged: String = original
        .lines()
        .map(|line| {
            if line.starts_with("control_sock=") {
                format!("control_sock={}", foreign.display())
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&lock, forged).expect("forge the lock");

    let out = world.tool(&["daemon", "stop", &world.dir_s(), "--via", "quit"]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    let record = world.last_record();
    assert_eq!(record["class"], "refused", "{record}");
    assert_eq!(record["stage"], "tripwire", "{record}");
    assert!(stderr(&out).contains("T9"), "{}", stderr(&out));
    assert!(!quit_file(&world, "quit.flag").exists(), "quit ran");
    assert!(
        matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "something connected to the foreign endpoint"
    );
    assert!(alive(registered_pid(&world, "daemon", "daemon.json")));

    std::fs::write(&lock, original).expect("restore the lock");
    let out = world.tool(&["daemon", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

// The test world's own cleanup

/// A world dropped part way, with a stub and a daemon still running,
/// leaves neither behind: a test that fails before its own teardown
/// must not strand processes whose tempdir is already gone.
#[test]
fn a_dropped_world_takes_its_running_instances_down() {
    let (world, _accepted) = ready_world();
    stub_start(&world, &[]);
    daemon_start(&world);
    let stub = registered_pid(&world, "stub", "serve.json");
    let supervisor = registered_pid(&world, "daemon", "supervisor.json");
    let daemon = registered_pid(&world, "daemon", "daemon.json");
    for pid in [stub, supervisor, daemon] {
        assert!(alive(pid), "premise: {pid} runs");
    }
    drop(world);
    wait_until(
        "the world's processes to leave",
        Duration::from_secs(10),
        || [stub, supervisor, daemon].iter().all(|pid| !alive(*pid)),
    );
}

// Ending without a command

/// The pids of instance 1's supervisor, its daemon, and its stub's
/// background process.
fn background_pids(world: &World) -> [u32; 3] {
    let ready = instance(world, "daemon", 1).join("daemon.json");
    wait_for_file(&ready);
    [
        registered_pid(world, "daemon", "supervisor.json"),
        read_json(&ready)["pid"].as_u64().expect("a daemon pid") as u32,
        registered_pid(world, "stub", "serve.json"),
    ]
}

/// The `exit.json` instance 1 of `kind` writes once it ends on its own.
fn own_exit(world: &World, kind: &str) -> Value {
    let path = instance(world, kind, 1).join("exit.json");
    wait_until(
        &format!("the {kind} ending on its own"),
        Duration::from_secs(20),
        || path.exists(),
    );
    read_json(&path)
}

/// I1: a sandbox with no call for its idle limit ends its daemon, then
/// its stub; each record says why, and no process is left.
#[test]
fn an_idle_sandbox_ends_its_daemon_then_its_stub() {
    let (world, _accepted) = ready_world();
    stub_start(&world, &["--idle", "1"]);
    daemon_start_with(&world, &["--idle", "1"]);
    let pids = background_pids(&world);
    let daemon_status = status(&world, "daemon");
    assert_eq!(
        daemon_status["unfinished"][0]["idle_ms"], 1000,
        "{daemon_status}"
    );
    let stub_status = status(&world, "stub");
    assert_eq!(
        stub_status["unfinished"][0]["idle_ms"], 1000,
        "{stub_status}"
    );

    let daemon = own_exit(&world, "daemon");
    assert_eq!(
        (
            daemon["class"].as_str(),
            daemon["phase"].as_str(),
            daemon["cause"].as_str()
        ),
        (Some("exited"), Some("running"), Some("idle")),
        "{daemon}"
    );
    let stub = own_exit(&world, "stub");
    assert_eq!(stub["cause"], "idle", "{stub}");
    let finished = |record: &Value| record["finished"].as_f64().expect("finished");
    assert!(
        finished(&stub) >= finished(&daemon),
        "the stub left before the daemon: {stub} {daemon}"
    );
    wait_until("every process leaving", Duration::from_secs(10), || {
        pids.iter().all(|&p| !alive(p))
    });
    let state = status(&world, "daemon");
    assert_eq!(state["last_ended"]["exit"]["cause"], "idle", "{state}");
}

/// I2: a call that holds the session lock keeps the sandbox in use past
/// the idle limit.
#[test]
fn a_call_in_flight_keeps_an_idle_sandbox() {
    let (world, _accepted) = ready_world();
    daemon_start_with(&world, &["--idle", "1"]);
    let exit = instance(&world, "daemon", 1).join("exit.json");
    let lock = std::fs::File::open(world.session_lock()).expect("open the session lock");
    lock.lock().expect("hold the session lock");
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !exit.exists(),
        "the daemon ended while a call held the session lock: {}",
        read_json(&exit)
    );
    drop(lock);
    assert_eq!(own_exit(&world, "daemon")["cause"], "idle");
}

/// I3: every call starts the idle limit over.
#[test]
fn each_call_starts_the_idle_limit_over() {
    let (world, _accepted) = ready_world();
    daemon_start_with(&world, &["--idle", "2"]);
    let exit = instance(&world, "daemon", 1).join("exit.json");
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        status(&world, "daemon");
        assert!(
            !exit.exists(),
            "the daemon ended between calls: {}",
            read_json(&exit)
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    assert_eq!(own_exit(&world, "daemon")["cause"], "idle");
}

/// I4: `--idle 0` never ends on its own, and an idle stub stays while a
/// daemon start is unfinished.
#[test]
fn the_stub_outlives_a_daemon_without_an_idle_limit() {
    let (world, _accepted) = ready_world();
    stub_start(&world, &["--idle", "1"]);
    daemon_start_with(&world, &["--idle", "0"]);
    let state = status(&world, "daemon");
    assert_eq!(state["unfinished"][0]["idle_ms"], Value::Null, "{state}");
    std::thread::sleep(Duration::from_secs(3));
    for kind in ["daemon", "stub"] {
        let exit = instance(&world, kind, 1).join("exit.json");
        assert!(
            !exit.exists(),
            "the {kind} ended on its own: {}",
            read_json(&exit)
        );
    }
    let out = world.tool(&["daemon", "stop", &world.dir_s()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(own_exit(&world, "stub")["cause"], "idle");
}

/// I5: background processes whose sandbox is no longer where they were
/// started end, idle limit or not. A move stands in for the removal, so
/// a failure can put the tree back for the world's teardown.
#[test]
fn a_sandbox_gone_from_its_path_ends_its_background_processes() {
    let (world, _accepted) = ready_world();
    stub_start(&world, &["--idle", "0"]);
    daemon_start_with(&world, &["--idle", "0"]);
    let pids = background_pids(&world);
    let moved = world.root.path().join("moved");
    std::fs::rename(&world.dir, &moved).expect("move the sandbox away");
    let deadline = Instant::now() + Duration::from_secs(10);
    let left = loop {
        if pids.iter().all(|&p| !alive(p)) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !left {
        std::fs::rename(&moved, &world.dir).expect("move the sandbox back");
    }
    let alive_now: Vec<u32> = pids.iter().copied().filter(|&p| alive(p)).collect();
    assert!(left, "processes outlived their sandbox: {alive_now:?}");
}
