// SPDX-License-Identifier: Apache-2.0

//! Pins the promises of `bookrack-sandbox` and of the in-process
//! `Session`, with a shell script standing in for the binary under test
//! (`common::sandbox`).

#![cfg(unix)]

mod common;

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bookrack_test_support::tool::exec::{DaemonStart, ExecOpts};
use bookrack_test_support::tool::{
    Class, CreateOpts, PAUSE_REPLACE_AFTER_CLEAR, Preset, Session, SessionOpts, Source,
};
use serde_json::Value;
use tempfile::TempDir;

use common::sandbox::{
    DAEMON_BODY, PRELUDE, Probe, TOOL, World, code, read_json, serve_version, signature, stderr,
    write_script,
};

// Timeouts, capture, exit codes

#[test]
fn a_command_past_its_timeout_takes_its_tree_with_it() {
    let world = World::new("");
    let mut probe = Probe::new(world.root.path());
    write_script(
        &world.stand_in,
        &format!(
            "{}(exec 3>\"{}\"; printf . >&3; sleep 30) & sleep 30\n",
            PRELUDE.replace("__HOOK__", ""),
            probe.path.display()
        ),
    );
    let rebuilt = world.tool(&["build", &world.dir_s()]);
    assert!(rebuilt.status.success(), "{}", stderr(&rebuilt));
    let handle = {
        let mut cmd = world.command(&["exec", &world.dir_s(), "--timeout", "1", "--", "run"]);
        std::thread::spawn(move || cmd.output().expect("exec"))
    };
    probe.wait_held();
    let out = handle.join().expect("exec thread");
    assert_eq!(code(&out), 124, "{}", stderr(&out));
    assert_eq!(world.last_record()["class"], "timeout");
    assert!(
        probe.closes_within(Duration::from_secs(2)),
        "a descendant still holds the probe after the timeout"
    );
}

#[test]
fn a_held_stream_is_a_capture_failure() {
    let world = World::new("");
    let mut probe = Probe::new(world.root.path());
    write_script(
        &world.stand_in,
        &format!(
            "{}(exec 3>\"{}\"; printf . >&3; sleep 30) & printf x\n",
            PRELUDE.replace("__HOOK__", ""),
            probe.path.display()
        ),
    );
    assert!(world.tool(&["build", &world.dir_s()]).status.success());
    let handle = {
        let mut cmd = world.command(&["exec", &world.dir_s(), "--cell", "h", "--", "run"]);
        std::thread::spawn(move || cmd.output().expect("exec"))
    };
    probe.wait_held();
    let out = handle.join().expect("exec thread");
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    let record = world.last_record();
    assert_eq!(record["class"], "capture-failed");
    assert_eq!(record["stage"], "drain");
    assert_eq!(
        std::fs::read(world.dir.join("cells/h/stdout")).expect("stdout"),
        b"x"
    );
    assert!(probe.closes_within(Duration::from_secs(2)));
}

#[test]
fn bytes_are_kept_as_they_were_written() {
    let world = World::new("printf '\\377\\376abc'; head -c 67108864 /dev/zero");
    let out = world.exec(&["--cell", "b"], &["run"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let bytes = std::fs::read(world.dir.join("cells/b/stdout")).expect("stdout");
    assert_eq!(bytes.len(), 5 + 64 * 1024 * 1024);
    assert_eq!(&bytes[..5], b"\xff\xfeabc");
    assert!(bytes[5..].iter().all(|&b| b == 0));
}

#[test]
fn a_command_without_a_cell_streams_as_it_runs() {
    let world = World::new("");
    let mark = world.root.path().join("go");
    write_script(
        &world.stand_in,
        &format!(
            "{}echo first-line\nwhile [ ! -e \"{}\" ]; do sleep 0.05; done\n",
            PRELUDE.replace("__HOOK__", ""),
            mark.display()
        ),
    );
    assert!(world.tool(&["build", &world.dir_s()]).status.success());
    let mut child = world
        .command(&["exec", &world.dir_s(), "--", "run"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let stdout = child.stdout.take().expect("piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).expect("read");
        tx.send(line).expect("send");
    });
    let line = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the first line must arrive while the command still runs");
    assert_eq!(line.trim_end(), "first-line");
    std::fs::write(&mark, b"").expect("mark");
    let status = child.wait().expect("wait");
    assert_eq!(status.code(), Some(0));
}

#[test]
fn a_command_that_exits_124_is_still_exited() {
    let world = World::new("exit 124");
    let out = world.exec(&[], &["run"]);
    assert_eq!(code(&out), 124);
    let record = world.last_record();
    assert_eq!(
        (&record["class"], &record["code"]),
        (&Value::from("exited"), &Value::from(124))
    );

    let killed = World::new("kill -TERM $$");
    let out = killed.exec(&[], &["run"]);
    assert_eq!(code(&out), 143);
    let record = killed.last_record();
    assert_eq!(record["class"], "signaled");
    assert_eq!(record["signal"], 15);
}

// Refusals

#[test]
fn a_tripped_wire_runs_nothing() {
    let world = World::new("");
    let mark = world.root.path().join("ran");
    write_script(
        &world.stand_in,
        &format!(
            "{}touch \"{}\"\n",
            PRELUDE.replace("__HOOK__", ""),
            mark.display()
        ),
    );
    assert!(world.tool(&["build", &world.dir_s()]).status.success());
    let out = world.exec(&[], &["ingest", "/elsewhere/x.txt"]);
    assert_eq!(code(&out), 125);
    assert_eq!(world.last_record()["class"], "refused");
    assert_eq!(world.last_record()["stage"], "tripwire");
    assert!(!mark.exists(), "the command ran despite the refusal");
}

#[test]
fn a_foreign_control_socket_is_refused() {
    let world = World::new("echo quit");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let foreign = elsewhere.path().join("control.sock");
    let _listener = UnixListener::bind(&foreign).expect("bind");
    std::fs::write(
        world.tty_lock(),
        format!(
            "pid=1\nmcp=127.0.0.1:1\ncontrol_sock={}\n",
            foreign.display()
        ),
    )
    .expect("lock");
    let held = File::open(world.tty_lock()).expect("open lock");
    held.lock().expect("hold");
    let out = world.exec(&[], &["quit"]);
    assert_eq!(code(&out), 125);
    let record = world.last_record();
    assert_eq!(record["stage"], "tripwire");
    assert!(
        record["problem"]["detail"]
            .as_str()
            .unwrap_or("")
            .contains("T9"),
        "{record}"
    );
}

#[test]
fn a_replaced_binary_is_refused() {
    let world = World::new("echo ok");
    let bin = world.dir.join("run/bin/bookrack");
    let recorded = std::fs::metadata(&bin)
        .expect("meta")
        .modified()
        .expect("mtime");
    let original = std::fs::read(&bin).expect("read");

    std::fs::write(&bin, b"#!/bin/sh\necho a much longer replacement script\n").expect("write");
    let out = world.exec(&[], &["run"]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert_eq!(world.last_record()["stage"], "binary");

    // Same length, same mtime, same inode: only the digest differs.
    let mut altered = original.clone();
    let last = altered.len() - 1;
    altered[last] = if altered[last] == b'\n' { b'#' } else { b'\n' };
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&bin)
            .expect("open in place");
        file.write_all(&altered).expect("write in place");
        file.set_modified(recorded).expect("set mtime");
    }
    let out = world.exec(&[], &["run"]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert_eq!(world.last_record()["stage"], "binary");
}

#[test]
fn a_build_with_an_unfinished_daemon_instance_is_refused() {
    let world = World::new("echo ok");
    let bin = world.dir.join("run/bin/bookrack");
    let before = signature(&world.dir.join("run/bin"));
    std::fs::create_dir_all(world.dir.join("run/daemon/1")).expect("instance");
    std::fs::write(world.dir.join("run/daemon/1/start.json"), b"{}").expect("start");
    let out = world.tool(&["build", &world.dir_s()]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert_eq!(world.last_record()["stage"], "instance");
    assert_eq!(signature(&world.dir.join("run/bin")), before);
    std::fs::remove_dir_all(world.dir.join("run/daemon/1")).expect("remove");

    std::fs::write(world.tty_lock(), b"pid=1\nmcp=127.0.0.1:1\n").expect("lock");
    let held = File::open(world.tty_lock()).expect("open");
    held.lock().expect("hold");
    let out = world.tool(&["build", &world.dir_s()]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert_eq!(world.last_record()["stage"], "instance");
    assert_eq!(signature(&world.dir.join("run/bin")), before);
    assert!(bin.exists());
}

// Records and the three parts

#[test]
fn every_call_leaves_a_record() {
    let world = World::new("echo ok");
    let before = world.records().len();
    let out = world.exec(&[], &["ingest", "/elsewhere/x.txt"]);
    assert_eq!(code(&out), 125);
    {
        let held = File::open(world.session_lock()).expect("open");
        held.lock().expect("hold");
        let out = world.tool(&["--lock-wait", "1", "exec", &world.dir_s(), "--", "run"]);
        assert_eq!(code(&out), 125, "{}", stderr(&out));
    }
    let out = world.exec(&["--cell", "c"], &["run"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    let records = world.records();
    assert_eq!(records.len(), before + 3, "{records:#?}");
    let classes: Vec<(&str, &str)> = records[before..]
        .iter()
        .map(|r| (r["class"].as_str().unwrap(), r["stage"].as_str().unwrap()))
        .collect();
    assert_eq!(
        classes,
        [
            ("refused", "tripwire"),
            ("refused", "session-lock"),
            ("exited", "run")
        ]
    );
    let in_cell = read_json(&world.dir.join("cells/c/result.json"));
    assert_eq!(in_cell, world.last_record());
    assert_eq!(world.last_record()["command"], "exec");
}

#[test]
fn a_refusal_is_three_part() {
    let world = World::new("echo ok");
    let out = world.exec(&[], &["ingest", "/elsewhere/x.txt"]);
    let text = stderr(&out);
    let lines: Vec<&str> = text.lines().collect();
    let error = lines
        .iter()
        .find(|l| l.starts_with("error: "))
        .expect("error line");
    let detail = lines
        .iter()
        .find(|l| l.starts_with("detail: "))
        .expect("detail line");
    let hint = lines
        .iter()
        .find(|l| l.starts_with("hint: "))
        .expect("hint line");
    let summary = &error["error: ".len()..];
    assert!(summary.chars().next().unwrap().is_lowercase() && !summary.ends_with('.'));
    let problem = &world.last_record()["problem"];
    assert_eq!(problem["summary"], summary);
    assert_eq!(problem["detail"], &detail["detail: ".len()..]);
    assert_eq!(problem["hint"], &hint["hint: ".len()..]);
    assert_eq!(problem["retryable"], false);

    let held = File::open(world.session_lock()).expect("open");
    held.lock().expect("hold");
    let out = world.tool(&["--lock-wait", "1", "exec", &world.dir_s(), "--", "run"]);
    let text = stderr(&out);
    assert!(
        text.contains("error: could not take the session lock"),
        "{text}"
    );
    assert!(text.contains("hint: "), "{text}");
}

// Path boundaries

#[test]
fn a_cell_id_that_climbs_out_is_refused() {
    let world = World::new("echo ok");
    let before = signature(world.root.path());
    for cell in ["../x", "/tmp/x", ".hidden"] {
        let out = world.exec(&["--cell", cell], &["run"]);
        assert_eq!(code(&out), 125, "{cell}: {}", stderr(&out));
    }
    let outside = world.root.path().join("real");
    std::fs::write(&outside, b"in").expect("write");
    let link = world.host().join("inputs/in");
    std::os::unix::fs::symlink(&outside, &link).expect("symlink");
    let out = world.exec(&["--stdin", &link.display().to_string()], &["run"]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    std::fs::remove_file(&link).expect("unlink");
    std::fs::remove_file(&outside).expect("remove");
    let after: Vec<String> = signature(world.root.path())
        .into_iter()
        .filter(|l| !l.contains("sbx/.calls"))
        .collect();
    let before: Vec<String> = before
        .into_iter()
        .filter(|l| !l.contains("sbx/.calls"))
        .collect();
    assert_eq!(after, before, "something was written outside the records");
}

#[test]
fn a_link_inside_the_tool_paths_is_refused() {
    let outside = tempfile::tempdir().expect("tempdir");
    std::fs::write(outside.path().join("keep"), b"keep").expect("write");
    let outside_before = signature(outside.path());

    // host/cwd replaced by a link to the outside, then a dotenv exec.
    let world = World::new("echo ok");
    let dotenv = world.host().join("inputs/f.env");
    std::fs::write(&dotenv, b"X=1\n").expect("write");
    std::fs::remove_dir_all(world.host().join("cwd")).expect("remove cwd");
    std::os::unix::fs::symlink(outside.path(), world.host().join("cwd")).expect("symlink");
    let out = world.exec(&["--dotenv", &dotenv.display().to_string()], &["run"]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert_eq!(
        world.last_record()["stage"],
        "paths",
        "{}",
        world.last_record()
    );

    // cells replaced by a link.
    let world = World::new("echo ok");
    std::fs::remove_dir_all(world.dir.join("cells")).expect("remove cells");
    std::os::unix::fs::symlink(outside.path(), world.dir.join("cells")).expect("symlink");
    let out = world.exec(&["--cell", "c"], &["run"]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert_eq!(
        world.last_record()["stage"],
        "paths",
        "{}",
        world.last_record()
    );

    // summary.tsv hard-linked to an outside file.
    let world = World::new("echo ok");
    std::fs::hard_link(
        outside.path().join("keep"),
        world.dir.join("cells/summary.tsv"),
    )
    .expect("hard link");
    let out = world.exec(&["--cell", "c"], &["run"]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert_eq!(
        world.last_record()["stage"],
        "paths",
        "{}",
        world.last_record()
    );

    // golden replaced by a link, then a snapshot.
    let world = World::new("echo ok");
    std::fs::remove_dir_all(world.dir.join("golden")).expect("remove golden");
    std::os::unix::fs::symlink(outside.path(), world.dir.join("golden")).expect("symlink");
    let out = world.tool(&["snapshot", &world.dir_s()]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));

    assert_eq!(
        signature(outside.path()),
        outside_before,
        "the outside tree was touched"
    );
}

#[test]
fn a_replace_does_not_touch_a_foreign_directory() {
    let world = World::new("echo ok");
    let copy = world.root.path().join("copy");
    let status = Command::new("cp")
        .arg("-R")
        .arg(&world.dir)
        .arg(&copy)
        .status()
        .expect("cp");
    assert!(status.success());
    let before = signature(&copy);
    let out = world.tool(&["new", &copy.display().to_string(), "--replace"]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert_eq!(signature(&copy), before);
}

#[test]
fn a_sandbox_inside_the_workspace_is_refused() {
    let root = tempfile::tempdir().expect("tempdir");
    let stand_in = root.path().join("stand-in.sh");
    write_script(&stand_in, &PRELUDE.replace("__HOOK__", ""));
    let inside = root.path().join("ws/sbx");
    std::fs::create_dir(root.path().join("ws")).expect("ws");
    let out = Command::new(TOOL)
        .args(["new", &inside.display().to_string(), "--stand-in"])
        .arg(&stand_in)
        .arg("--workspace")
        .arg(root.path().join("ws"))
        .output()
        .expect("new");
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("inside the workspace"),
        "{}",
        stderr(&out)
    );
    assert!(!inside.exists());
}

// Replacement, the session lock, and the switch

#[test]
fn a_replace_keeps_the_lock_file_and_the_records() {
    use std::os::unix::fs::MetadataExt;
    let world = World::new("echo ok");
    assert_eq!(code(&world.exec(&[], &["run"])), 0);
    let lock_inode = std::fs::metadata(world.session_lock()).expect("meta").ino();
    let records_before = world.records().len();

    let pause = world
        .dir
        .join(format!(".pause-{PAUSE_REPLACE_AFTER_CLEAR}"));
    let mut replace = world
        .command(&[
            "--pause-at",
            PAUSE_REPLACE_AFTER_CLEAR,
            "new",
            &world.dir_s(),
            "--replace",
        ])
        .spawn()
        .expect("spawn replace");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pause.exists() {
        assert!(Instant::now() < deadline, "the replace never paused");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !world.dir.join("run").exists(),
        "run/ is gone at the pause point"
    );

    let mut third = world.command(&["exec", &world.dir_s(), "--", "run"]);
    let third = std::thread::spawn(move || third.output().expect("third exec"));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        world.records().len(),
        records_before,
        "the third exec must be waiting"
    );
    std::fs::remove_file(&pause).expect("release");
    assert!(replace.wait().expect("replace").success());
    let third = third.join().expect("third thread");
    assert_eq!(code(&third), 0, "{}", stderr(&third));

    assert_eq!(
        std::fs::metadata(world.session_lock()).expect("meta").ino(),
        lock_inode
    );
    let records = world.records();
    assert_eq!(records.len(), records_before + 2, "{records:#?}");
    let replace_rec = records
        .iter()
        .find(|r| r["command"] == "new")
        .expect("new record");
    let third_rec = records.last().expect("third record");
    assert_eq!(third_rec["class"], "exited");
    assert!(
        third_rec["started"].as_f64().unwrap() >= replace_rec["finished"].as_f64().unwrap(),
        "the third exec ran after the replace finished"
    );
}

#[test]
fn a_half_replaced_sandbox_is_refused() {
    let world = World::new("echo ok");
    let pause = world
        .dir
        .join(format!(".pause-{PAUSE_REPLACE_AFTER_CLEAR}"));
    let mut replace = world
        .command(&[
            "--pause-at",
            PAUSE_REPLACE_AFTER_CLEAR,
            "new",
            &world.dir_s(),
            "--replace",
        ])
        .spawn()
        .expect("spawn replace");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pause.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    replace.kill().expect("kill");
    replace.wait().expect("wait");

    let out = world.exec(&[], &["--version"]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    let record = world.last_record();
    assert_eq!(record["stage"], "paths", "{record}");
    assert!(
        stderr(&out).contains("new <dir> --replace"),
        "{}",
        stderr(&out)
    );

    let out = world.tool(&["new", &world.dir_s(), "--replace"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!pause.exists(), "the stale pause file must be cleared");
    assert_eq!(code(&world.exec(&[], &["--version"])), 0);
}

#[test]
fn a_replace_with_an_unfinished_instance_is_refused() {
    let world = World::new("echo ok");
    std::fs::create_dir_all(world.dir.join("run/daemon/1")).expect("instance");
    std::fs::write(world.dir.join("run/daemon/1/start.json"), b"{}").expect("start");
    let before = signature(&world.host());
    let out = world.tool(&["new", &world.dir_s(), "--replace"]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert!(stderr(&out).contains("daemon stop"), "{}", stderr(&out));
    assert_eq!(signature(&world.host()), before);
    std::fs::remove_dir_all(world.dir.join("run/daemon/1")).expect("remove");

    std::fs::write(world.tty_lock(), b"pid=1\nmcp=127.0.0.1:1\n").expect("lock");
    let held = File::open(world.tty_lock()).expect("open");
    held.lock().expect("hold");
    let out = world.tool(&["new", &world.dir_s(), "--replace"]);
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert!(world.dir.join("run/bin/bookrack").exists());
}

#[test]
fn two_execs_never_overlap() {
    let world = World::new("sleep 1; echo done");
    let dotenv = world.host().join("inputs/f.env");
    std::fs::write(&dotenv, b"BOOKRACK_LOG=info\n").expect("write");
    let a = {
        let mut cmd = world.command(&["exec", &world.dir_s(), "--", "run"]);
        std::thread::spawn(move || cmd.output().expect("a"))
    };
    let b = {
        let mut cmd = world.command(&[
            "exec",
            &world.dir_s(),
            "--dotenv",
            &dotenv.display().to_string(),
            "--",
            "run",
        ]);
        std::thread::spawn(move || cmd.output().expect("b"))
    };
    let (a, b) = (a.join().unwrap(), b.join().unwrap());
    assert_eq!(code(&a), 0, "{}", stderr(&a));
    assert_eq!(code(&b), 0, "{}", stderr(&b));
    let records = world.records();
    let execs: Vec<&Value> = records.iter().filter(|r| r["command"] == "exec").collect();
    assert_eq!(execs.len(), 2);
    let (s0, f0) = (
        execs[0]["started"].as_f64().unwrap(),
        execs[0]["finished"].as_f64().unwrap(),
    );
    let (s1, f1) = (
        execs[1]["started"].as_f64().unwrap(),
        execs[1]["finished"].as_f64().unwrap(),
    );
    assert!(f0 <= s1 || f1 <= s0, "the two execs overlapped: {execs:#?}");

    let held = File::open(world.session_lock()).expect("open");
    held.lock().expect("hold");
    let out = world.tool(&["--lock-wait", "1", "exec", &world.dir_s(), "--", "run"]);
    assert_eq!(code(&out), 125);
    assert_eq!(world.last_record()["stage"], "session-lock");
}

#[test]
fn a_restore_waits_for_a_running_exec() {
    let world = World::new("sleep 1; touch \"$BOOKRACK_REGISTRY.touched\"");
    let exec = {
        let mut cmd = world.command(&["exec", &world.dir_s(), "--", "run"]);
        std::thread::spawn(move || cmd.output().expect("exec"))
    };
    std::thread::sleep(Duration::from_millis(200));
    let restore = world.tool(&["restore", &world.dir_s()]);
    let exec = exec.join().unwrap();
    assert_eq!(code(&exec), 0, "{}", stderr(&exec));
    assert!(restore.status.success(), "{}", stderr(&restore));
    let records = world.records();
    let exec_rec = records.iter().find(|r| r["command"] == "exec").unwrap();
    let restore_rec = records.iter().find(|r| r["command"] == "restore").unwrap();
    assert!(restore_rec["started"].as_f64().unwrap() >= exec_rec["finished"].as_f64().unwrap());
    assert!(
        !world.host().join("registry.toml.touched").exists(),
        "the restore ran after the exec and brought the golden tree back"
    );
}

#[test]
fn a_held_session_lock_blocks_restore_and_snapshot() {
    let world = World::new("echo ok");
    std::fs::write(world.tty_lock(), b"pid=1\nmcp=127.0.0.1:1\n").expect("lock");
    let host_before = signature(&world.host());
    let golden_before = signature(&world.dir.join("golden"));
    let held = File::open(world.tty_lock()).expect("open");
    held.lock().expect("hold");
    for command in ["restore", "snapshot"] {
        let out = world.tool(&[command, &world.dir_s()]);
        assert_eq!(code(&out), 125, "{command}: {}", stderr(&out));
        assert_eq!(world.last_record()["stage"], "instance");
    }
    assert_eq!(signature(&world.host()), host_before);
    assert_eq!(signature(&world.dir.join("golden")), golden_before);
}

#[test]
fn a_restore_after_a_crash_between_renames_recovers() {
    let world = World::new("echo ok");
    let host_before = signature(&world.host());
    std::fs::rename(world.host(), world.dir.join("host.old-1")).expect("move aside");
    let out = world.exec(&[], &["run"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(world.host().exists() && !world.dir.join("host.old-1").exists());
    let notes = world.last_record()["notes"].to_string();
    assert!(notes.contains("renamed"), "{notes}");
    assert_eq!(
        signature(&world.host())
            .into_iter()
            .filter(|l| !l.contains("roots/alpha"))
            .collect::<Vec<_>>(),
        host_before
            .into_iter()
            .filter(|l| !l.contains("roots/alpha"))
            .collect::<Vec<_>>()
    );

    let status = Command::new("cp")
        .arg("-R")
        .arg(world.dir.join("golden"))
        .arg(world.dir.join("golden.old-1"))
        .status()
        .expect("cp");
    assert!(status.success());
    let out = world.exec(&[], &["run"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(!world.dir.join("golden.old-1").exists());
    assert!(
        world.last_record()["notes"]
            .to_string()
            .contains("removed the old tree")
    );
}

#[test]
fn a_dotenv_is_placed_for_one_command_and_removed() {
    let world =
        World::new("printf 'ND=%s\\n' \"${BOOKRACK_NO_DOTENV:-}\"; cat .env 2>/dev/null || true");
    let dotenv = world.host().join("inputs/f.env");
    std::fs::write(&dotenv, b"BOOKRACK_LOG=info\n").expect("write");
    let out = world.exec(
        &["--cell", "d", "--dotenv", &dotenv.display().to_string()],
        &["run"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let stdout = std::fs::read_to_string(world.dir.join("cells/d/stdout")).expect("stdout");
    assert_eq!(stdout, "ND=\nBOOKRACK_LOG=info\n");
    assert!(
        !world.host().join("cwd/.env").exists(),
        "the .env must be removed"
    );
    let out = world.exec(&["--cell", "e"], &["run"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        std::fs::read_to_string(world.dir.join("cells/e/stdout")).expect("stdout"),
        "ND=1\n"
    );
}

#[test]
fn a_bare_sandbox_runs_a_command() {
    let world = World::new("echo bookrack 0.0.0-sandbox");
    let out = world.exec(&["--cell", "c"], &["--version"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    for artifact in [
        "cmd",
        "stdout",
        "stderr",
        "sig.before",
        "sig.after",
        "registry.before",
        "registry.after",
        "platform-registry.before",
        "platform-registry.after",
        "result.json",
    ] {
        assert!(
            world.dir.join("cells/c").join(artifact).exists(),
            "{artifact}"
        );
    }
    let summary = std::fs::read_to_string(world.dir.join("cells/summary.tsv")).expect("tsv");
    assert_eq!(summary.lines().count(), 1);
    assert!(summary.starts_with("c\t"), "{summary}");
    let entries =
        bookrack_config::list_libraries_at(&world.host().join("registry.toml")).expect("registry");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "alpha");
    assert!(entries[0].is_default);
    assert_eq!(entries[0].data_dir, world.host().join("roots/alpha"));
}

// The in-process Session

struct SessionWorld {
    root: TempDir,
    stand_in: PathBuf,
}

impl SessionWorld {
    fn new(body: &str, hook: &str) -> SessionWorld {
        let root = tempfile::tempdir().expect("tempdir");
        let stand_in = root.path().join("stand-in.sh");
        write_script(
            &stand_in,
            &format!("{}{body}\n", PRELUDE.replace("__HOOK__", hook)),
        );
        std::fs::create_dir(root.path().join("ws")).expect("ws");
        SessionWorld { root, stand_in }
    }

    async fn session(&self) -> Session {
        Session::create(
            &self.root.path().join("sbx"),
            Source::StandIn(self.stand_in.clone()),
            Preset::Bare,
            CreateOpts {
                replace: false,
                workspace: Some(self.root.path().join("ws")),
            },
            SessionOpts::default(),
        )
        .await
        .unwrap_or_else(|o| panic!("create: {o:?}"))
    }

    fn dir(&self) -> PathBuf {
        self.root
            .path()
            .canonicalize()
            .expect("canonical")
            .join("sbx")
    }
}

#[tokio::test]
async fn a_tripped_daemon_start_leaves_no_instance() {
    let world = SessionWorld::new(DAEMON_BODY, "");
    let mut session = world.session().await;
    let err = session
        .daemon_start(DaemonStart::library("../../outside"))
        .await
        .expect_err("refused");
    assert_eq!(err.stage, "tripwire", "{err:?}");
    assert!(
        std::fs::read_dir(world.dir().join("run/daemon"))
            .unwrap()
            .next()
            .is_none(),
        "no instance may be created for a refused start"
    );

    let outside = tempfile::tempdir().expect("tempdir");
    let dotenv = world.dir().join("host/inputs/f.env");
    std::fs::write(&dotenv, b"X=1\n").expect("write");
    std::fs::remove_dir_all(world.dir().join("host/cwd")).expect("remove");
    std::os::unix::fs::symlink(outside.path(), world.dir().join("host/cwd")).expect("symlink");
    let err = session
        .daemon_start(DaemonStart::library("alpha").dotenv(&dotenv))
        .await
        .expect_err("refused");
    assert_eq!(err.stage, "paths", "{err:?}");
    assert!(
        !outside.path().join(".env").exists(),
        "the .env was written outside"
    );
    assert!(
        std::fs::read_dir(world.dir().join("run/daemon"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[tokio::test]
async fn a_spawn_failure_leaves_an_exit_record() {
    let world = SessionWorld::new("exit 0", "chmod a-x \"$0\"");
    let mut session = world.session().await;
    let err = session
        .daemon_start(DaemonStart::library("alpha"))
        .await
        .expect_err("spawn fails");
    assert_eq!(err.stage, "spawn", "{err:?}");
    let instance = world.dir().join("run/daemon/1");
    assert!(instance.join("start.json").exists());
    let exit = read_json(&instance.join("exit.json"));
    assert_eq!(exit["class"], "tool-error");
    // The instance counts as finished, so a snapshot is not held up.
    std::fs::set_permissions(
        world.dir().join("run/bin/bookrack"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod back");
    session
        .snapshot()
        .await
        .unwrap_or_else(|o| panic!("snapshot: {o:?}"));
}

#[tokio::test]
async fn a_failed_daemon_start_leaves_an_exit_record() {
    let world = SessionWorld::new("echo starting; exit 3", "");
    let mut session = world.session().await;
    let err = session
        .daemon_start(DaemonStart::library("alpha"))
        .await
        .expect_err("not ready");
    assert_eq!(err.class, Class::NotReady, "{err:?}");
    let exit = read_json(&world.dir().join("run/daemon/1/exit.json"));
    assert_eq!(exit["class"], "not-ready");
    assert_eq!(exit["code"], 3);
    session
        .snapshot()
        .await
        .unwrap_or_else(|o| panic!("snapshot: {o:?}"));
}

#[tokio::test]
async fn a_dotenv_for_the_daemon_is_removed_after_it_starts() {
    let body = format!(
        "printf 'NO_DOTENV=%s\\n' \"${{BOOKRACK_NO_DOTENV:-}}\"\nif [ -f .env ]; then printf 'ENV=%s\\n' \"$(cat .env)\"; fi\n{DAEMON_BODY}"
    );
    let world = SessionWorld::new(&body, "");
    let mut session = world.session().await;
    let _accepted = serve_version(&world.dir().join("host/runtime/control.sock"));
    let dotenv = world.dir().join("host/inputs/f.env");
    std::fs::write(&dotenv, b"BOOKRACK_LOG=info").expect("write");
    session
        .daemon_start(DaemonStart::library("alpha").dotenv(&dotenv))
        .await
        .unwrap_or_else(|o| panic!("start: {o:?}"));
    assert!(
        !world.dir().join("host/cwd/.env").exists(),
        "the .env must be gone once started"
    );
    let stdout = std::fs::read_to_string(world.dir().join("run/daemon/1/stdout")).expect("stdout");
    assert!(stdout.contains("NO_DOTENV=\n"), "{stdout}");
    assert!(stdout.contains("ENV=BOOKRACK_LOG=info"), "{stdout}");
    let out = session
        .exec(&["--version".into()], ExecOpts::default())
        .await
        .unwrap_or_else(|o| panic!("exec: {o:?}"));
    assert_eq!(out.outcome.class, Class::Exited, "{:?}", out.outcome);
    session
        .daemon_stop(Duration::from_secs(10))
        .await
        .unwrap_or_else(|o| panic!("stop: {o:?}"));
}

#[tokio::test]
async fn a_stopped_daemon_leaves_its_exit_status() {
    let world = SessionWorld::new(DAEMON_BODY, "");
    let mut session = world.session().await;
    let sock = world.dir().join("host/runtime/control.sock");
    let accepted = serve_version(&sock);
    let started = Instant::now();
    let ready = session
        .daemon_start(DaemonStart::library("alpha"))
        .await
        .unwrap_or_else(|o| panic!("start: {o:?}"));
    assert_eq!(ready.control_sock, sock);
    assert!(accepted.load(Ordering::SeqCst) >= 1);
    let daemon = read_json(&world.dir().join("run/daemon/1/daemon.json"));
    assert_eq!(daemon["pid"], ready.pid);
    assert_eq!(daemon["control_sock"], sock.display().to_string());

    let outcome = session
        .daemon_stop(Duration::from_secs(10))
        .await
        .unwrap_or_else(|o| panic!("stop: {o:?}"));
    assert_eq!(
        (outcome.class, outcome.code),
        (Class::Exited, Some(0)),
        "{outcome:?}"
    );
    let exit = read_json(&world.dir().join("run/daemon/1/exit.json"));
    assert_eq!(exit["class"], "exited");
    assert_eq!(exit["code"], 0);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the stop must not wait out a drain"
    );
}

/// A `new` that fails part way leaves a sandbox `new --replace` can
/// rebuild: the marker is written, not ready, before anything else.
#[test]
fn a_failed_new_can_be_replaced() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(root.path().join("ws")).expect("ws");
    let stand_in = root.path().join("stand-in.sh");
    write_script(
        &stand_in,
        &format!("{}echo ok\n", PRELUDE.replace("__HOOK__", "")),
    );
    let dir = root.path().join("sbx");
    let new = |stand_in: &std::path::Path, replace: bool| {
        let mut cmd = Command::new(TOOL);
        cmd.arg("new")
            .arg(&dir)
            .arg("--stand-in")
            .arg(stand_in)
            .arg("--workspace")
            .arg(root.path().join("ws"))
            .stdin(Stdio::null());
        if replace {
            cmd.arg("--replace");
        }
        cmd.output().expect("new")
    };
    let failed = new(&root.path().join("missing.sh"), false);
    assert_eq!(code(&failed), 125, "{}", stderr(&failed));
    let replaced = new(&stand_in, true);
    assert_eq!(code(&replaced), 0, "{}", stderr(&replaced));
}
