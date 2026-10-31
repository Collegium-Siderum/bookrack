// SPDX-License-Identifier: Apache-2.0

//! Pins the readiness probe of `DaemonProcess`: which control socket it
//! dials, how it holds the daemon to the wire contract, how a leader's
//! exit is seen at every step, and what each failing branch is typed as.
//!
//! The child is `sh`. Where a check needs the control plane to answer,
//! the test holds the listener and answers `daemon.version` itself.
//! Unless a test is about the exit, the script stays alive after its
//! ready line, so the probe's first look at the leader does not end the
//! probe before the branch under test.

#![cfg(unix)]

mod common;

use std::ffi::OsStr;
use std::future::Future;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bookrack_test_support::{DaemonProcess, FailureKind, ReadyFailure, Stage};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::task::JoinHandle;

use common::Probe;

/// Writes a session lock into `$1` naming the shell itself and the
/// control socket `$2`, the way a daemon that bound its sockets does.
const WRITE_OWN_LOCK: &str =
    "printf 'pid=%s\\nmcp=127.0.0.1:1\\ncontrol_sock=%s\\n' $$ \"$2\" > \"$1\"";

/// Prints a complete ready line.
const PRINT_READY_LINE: &str = "echo \"bookrack daemon running: pid=$$ control_sock=$2\"";

/// The success answer to the probe's request.
const ANSWER: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"version\":\"stub\"}}\n";

/// A runtime directory: the lock and the control socket beside it.
struct Runtime {
    dir: TempDir,
    lock: PathBuf,
    sock: PathBuf,
}

fn runtime() -> Runtime {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = dir.path().join("session.lock");
    let sock = dir.path().join("control.sock");
    Runtime { dir, lock, sock }
}

/// `sh -c script` with `args` as `$1`, `$2`, ...
fn sh(script: &str, args: &[&OsStr], stdin: Stdio) -> DaemonProcess {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script).arg("sh").args(args);
    DaemonProcess::spawn_with_stdin(cmd, stdin).expect("spawn sh")
}

/// A script that writes its own lock naming `sock`, prints its ready
/// line, and stays alive.
fn ready_daemon(rt: &Runtime, sock: &Path) -> DaemonProcess {
    sh(
        &format!("{WRITE_OWN_LOCK}; {PRINT_READY_LINE}; exec sleep 30"),
        &[rt.lock.as_os_str(), sock.as_os_str()],
        Stdio::null(),
    )
}

/// A listener the test holds, counting the connections it accepts and
/// handing each to `handler`.
struct Listener {
    accepts: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn listen<F, Fut>(path: &Path, handler: F) -> Listener
where
    F: Fn(UnixStream) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = UnixListener::bind(path).expect("bind the test's listener");
    let accepts = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&accepts);
    let handler = Arc::new(handler);
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(handler(stream));
        }
    });
    Listener { accepts, task }
}

/// Read the probe's request line off `stream`.
async fn read_request(stream: UnixStream) -> BufReader<UnixStream> {
    let mut reader = BufReader::new(stream);
    let mut request = String::new();
    reader
        .read_line(&mut request)
        .await
        .expect("read the request");
    assert!(
        request.contains("\"daemon.version\""),
        "the probe must call daemon.version: {request:?}",
    );
    reader
}

/// Hold a connection until the other side closes it.
async fn hold(mut reader: BufReader<UnixStream>) {
    let mut rest = Vec::new();
    let _ = reader.read_to_end(&mut rest).await;
}

/// Answer `daemon.version`, then hold the connection.
async fn answer(stream: UnixStream) {
    let mut reader = read_request(stream).await;
    reader
        .get_mut()
        .write_all(ANSWER)
        .await
        .expect("write the answer");
    hold(reader).await;
}

fn unwrap_failure<T: std::fmt::Debug>(result: Result<T, ReadyFailure>, what: &str) -> ReadyFailure {
    match result {
        Err(failure) => failure,
        Ok(ready) => panic!("{what} was read as ready: {ready:?}"),
    }
}

/// A lock naming a socket outside its own directory routes a client
/// somewhere this lock's daemon is not. The probe refuses it without
/// connecting.
#[tokio::test]
async fn a_lock_naming_a_foreign_socket_is_never_dialed() {
    let rt = runtime();
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let foreign = elsewhere.path().join("control.sock");
    let listener = listen(&foreign, answer);
    let child = ready_daemon(&rt, &foreign);

    let failure = match child.wait_ready(&rt.lock, Duration::from_secs(5)).await {
        Err(failure) => failure,
        Ok(_) => panic!("a lock naming a foreign socket was read as ready"),
    };
    assert_eq!(
        (failure.kind, failure.stage),
        (FailureKind::NotReady, Stage::Endpoint),
        "{failure}"
    );
    assert_eq!(
        listener.accepts.load(Ordering::SeqCst),
        0,
        "the foreign socket must never be dialed"
    );
}

/// A `control.sock` beside the lock that is a link to a socket elsewhere
/// is refused without connecting, though its path and directory match.
#[tokio::test]
async fn a_control_socket_that_is_a_symlink_is_never_dialed() {
    let rt = runtime();
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let target = elsewhere.path().join("control.sock");
    let listener = listen(&target, answer);
    std::os::unix::fs::symlink(&target, &rt.sock).expect("link control.sock");
    let child = ready_daemon(&rt, &rt.sock);

    let failure = match child.wait_ready(&rt.lock, Duration::from_secs(5)).await {
        Err(failure) => failure,
        Ok(_) => panic!("a control socket that is a link was read as ready"),
    };
    assert_eq!(
        (failure.kind, failure.stage),
        (FailureKind::NotReady, Stage::Endpoint),
        "{failure}"
    );
    assert_eq!(
        listener.accepts.load(Ordering::SeqCst),
        0,
        "the linked socket must never be dialed"
    );
}

/// A runtime directory reached through an alias is still the lock's own
/// directory: the lock and the socket it names both sit under the alias,
/// and the listener is bound under the real directory. Each side is
/// canonicalized, so the match holds; canonicalizing one side only fails
/// here on every platform, because the alias is not canonical by
/// construction.
#[tokio::test]
async fn a_socket_reached_through_a_directory_alias_is_ready() {
    let base = tempfile::tempdir().expect("tempdir");
    let real = base.path().join("real");
    let alias = base.path().join("alias");
    std::fs::create_dir(&real).expect("create the real directory");
    std::os::unix::fs::symlink(&real, &alias).expect("link the alias");
    let _listener = listen(&real.join("control.sock"), answer);
    let lock = alias.join("session.lock");
    let sock = alias.join("control.sock");
    let mut child = sh(
        &format!("{WRITE_OWN_LOCK}; {PRINT_READY_LINE}; exec sleep 30"),
        &[lock.as_os_str(), sock.as_os_str()],
        Stdio::null(),
    );

    let ready = child
        .probe_ready(&lock, Duration::from_secs(5))
        .await
        .expect("a socket beside the lock, reached through an alias, is ready");
    assert_eq!(ready.control_sock, sock);
}

/// A leader that exits before it is ready has its streams drained before
/// its group is killed, so it keeps its own status and what a descendant
/// still prints after it is gone.
#[tokio::test]
async fn a_child_that_exits_before_ready_keeps_its_status() {
    let rt = runtime();
    let child = sh(
        "( sleep 0.5; echo late >&2; sleep 0.5 ) & exit 3",
        &[],
        Stdio::null(),
    );

    let failure = match child.wait_ready(&rt.lock, Duration::from_secs(5)).await {
        Err(failure) => failure,
        Ok(_) => panic!("a child that exited was read as ready"),
    };
    assert_eq!(
        (failure.kind, failure.stage),
        (FailureKind::NotReady, Stage::Run),
        "{failure}"
    );
    assert_eq!(failure.exit.and_then(|s| s.code()), Some(3), "{failure}");
    assert!(
        failure.why.starts_with("the child exited with"),
        "{failure}"
    );
    assert!(
        failure.stderr.contains("late"),
        "the descendant's late output must be kept: {failure}"
    );
}

/// A leader that exits after the probe dialed and before the answer
/// came back is seen by the look after the answer. The listener makes
/// the order exact: it releases the script only once it has the
/// request, and answers only once the exit is visible.
#[tokio::test]
async fn a_child_that_exits_during_the_probe_is_not_ready() {
    let rt = runtime();
    let go = rt.dir.path().join("go");
    let listener = UnixListener::bind(&rt.sock).expect("bind the test's listener");
    let (mut probe, stdin) = Probe::new();
    let mut child = sh(
        &format!(
            "{WRITE_OWN_LOCK}; {PRINT_READY_LINE}; \
             while [ ! -e \"$3\" ]; do sleep 0.01; done; exit 0"
        ),
        &[rt.lock.as_os_str(), rt.sock.as_os_str(), go.as_os_str()],
        stdin,
    );
    let pid = child.id().expect("pid");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut reader = read_request(stream).await;
        std::fs::write(&go, b"").expect("create the go file");
        probe.wait_exit(pid).await;
        reader
            .get_mut()
            .write_all(ANSWER)
            .await
            .expect("write the answer");
        hold(reader).await;
    });

    let failure = unwrap_failure(
        child.probe_ready(&rt.lock, Duration::from_secs(10)).await,
        "a child that exited before the answer came back",
    );
    assert_eq!(
        (failure.kind, failure.stage),
        (FailureKind::NotReady, Stage::Run),
        "{failure:?}"
    );
    let failure = child.give_up(failure).await;
    assert_eq!(
        (failure.kind, failure.stage),
        (FailureKind::NotReady, Stage::Run),
        "{failure}"
    );
    assert_eq!(failure.exit.and_then(|s| s.code()), Some(0), "{failure}");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the listener still holds the probe's connection 5s after the probe ended")
        .expect("the listener task");
}

/// A daemon that dies during the call shows up as a broken connection;
/// the look after the failed call names the real cause. Asserted on the
/// probe's own result: `give_up` would name the exit either way.
#[tokio::test]
async fn a_daemon_that_dies_mid_call_is_reported_as_exited() {
    let rt = runtime();
    let go = rt.dir.path().join("go");
    let listener = UnixListener::bind(&rt.sock).expect("bind the test's listener");
    let (mut probe, stdin) = Probe::new();
    let mut child = sh(
        &format!(
            "{WRITE_OWN_LOCK}; {PRINT_READY_LINE}; \
             while [ ! -e \"$3\" ]; do sleep 0.01; done; exit 0"
        ),
        &[rt.lock.as_os_str(), rt.sock.as_os_str(), go.as_os_str()],
        stdin,
    );
    let pid = child.id().expect("pid");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let reader = read_request(stream).await;
        std::fs::write(&go, b"").expect("create the go file");
        probe.wait_exit(pid).await;
        drop(reader);
    });

    let failure = unwrap_failure(
        child.probe_ready(&rt.lock, Duration::from_secs(10)).await,
        "a daemon that died mid-call",
    );
    assert_eq!(
        (failure.kind, failure.stage),
        (FailureKind::NotReady, Stage::Run),
        "the exit, not the broken call, is the failure: {failure:?}"
    );
    assert!(
        failure.why.contains("did not answer daemon.version"),
        "the call's own error is kept: {failure:?}"
    );
    let control_plane = failure.why.clone();
    let failure = child.give_up(failure).await;
    assert_eq!(failure.exit.and_then(|s| s.code()), Some(0), "{failure}");
    assert!(
        failure.why.starts_with("the child exited with")
            && failure.why.contains(&format!("; {control_plane}")),
        "the exit leads and the call's error follows: {failure}"
    );
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the listener still holds the probe's connection 5s after the probe ended")
        .expect("the listener task");
}

/// A leader that exits after the probe failed, and before `give_up`, is
/// seen by `give_up`'s own look: it keeps its status and the probe's
/// reason stays as a lead.
#[tokio::test]
async fn a_late_exit_is_seen_by_give_up() {
    let rt = runtime();
    let go = rt.dir.path().join("go");
    let (mut probe, stdin) = Probe::new();
    let mut child = sh(
        &format!(
            "printf 'pid=1\\nmcp=127.0.0.1:1\\ncontrol_sock=%s\\n' \"$2\" > \"$1\"; \
             {PRINT_READY_LINE}; while [ ! -e \"$3\" ]; do sleep 0.01; done; exit 4"
        ),
        &[rt.lock.as_os_str(), rt.sock.as_os_str(), go.as_os_str()],
        stdin,
    );
    let pid = child.id().expect("pid");

    let failure = unwrap_failure(
        child.probe_ready(&rt.lock, Duration::from_secs(5)).await,
        "a lock naming another pid",
    );
    assert_eq!(
        (failure.kind, failure.stage),
        (FailureKind::NotReady, Stage::DaemonLock),
        "premise: the leader was alive when the probe failed: {failure:?}"
    );
    std::fs::write(&go, b"").expect("create the go file");
    probe.wait_exit(pid).await;

    let failure = child.give_up(failure).await;
    assert_eq!(
        (failure.kind, failure.stage),
        (FailureKind::NotReady, Stage::Run),
        "{failure}"
    );
    assert_eq!(failure.exit.and_then(|s| s.code()), Some(4), "{failure}");
    assert!(failure.why.contains("names pid 1"), "{failure}");
}

/// What the listener sends after reading the request.
enum Reply {
    Bytes(Vec<u8>),
    Pause(Duration),
    Close,
}

/// The probe holds the daemon to the wire contract of the control
/// plane. Two well-formed exchanges are ready; every violation is
/// refused at once rather than waited out, and the listener keeps the
/// connection open after it, so a probe that kept reading would run
/// into its budget instead.
#[tokio::test]
async fn the_probe_holds_the_daemon_to_the_wire_contract() {
    let event = b"{\"jsonrpc\":\"2.0\",\"method\":\"event\",\"params\":{\"channel\":\"jobs\",\"value\":{}}}\n";
    let cases: Vec<(&str, Vec<Reply>, bool)> = vec![
        (
            "an event before the answer",
            vec![Reply::Bytes(event.to_vec()), Reply::Bytes(ANSWER.to_vec())],
            true,
        ),
        (
            "an answer split across two writes",
            vec![
                Reply::Bytes(ANSWER[..12].to_vec()),
                Reply::Pause(Duration::from_millis(100)),
                Reply::Bytes(ANSWER[12..].to_vec()),
            ],
            true,
        ),
        (
            "65 KiB without a newline",
            vec![Reply::Bytes(vec![b'a'; 65 * 1024])],
            false,
        ),
        (
            "an answer to id 2",
            vec![Reply::Bytes(
                b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n".to_vec(),
            )],
            false,
        ),
        (
            "an error answer",
            vec![Reply::Bytes(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32603,\"message\":\"no\"}}\n"
                    .to_vec(),
            )],
            false,
        ),
        (
            "a frame that is not JSON",
            vec![Reply::Bytes(b"not json\n".to_vec())],
            false,
        ),
        ("a close without an answer", vec![Reply::Close], false),
    ];
    for (name, sends, ready) in cases {
        let rt = runtime();
        let sends = Arc::new(sends);
        let _listener = listen(&rt.sock, move |stream| {
            let sends = Arc::clone(&sends);
            async move {
                let mut reader = read_request(stream).await;
                for send in sends.iter() {
                    match send {
                        Reply::Bytes(bytes) => {
                            if reader.get_mut().write_all(bytes).await.is_err() {
                                return;
                            }
                        }
                        Reply::Pause(pause) => tokio::time::sleep(*pause).await,
                        Reply::Close => return,
                    }
                }
                hold(reader).await;
            }
        });
        let mut child = ready_daemon(&rt, &rt.sock);

        let started = Instant::now();
        let result = child.probe_ready(&rt.lock, Duration::from_secs(5)).await;
        let elapsed = started.elapsed();
        if ready {
            result.unwrap_or_else(|failure| panic!("{name}: must be ready: {failure:?}"));
        } else {
            let failure = unwrap_failure(result, name);
            assert_eq!(
                (failure.kind, failure.stage),
                (FailureKind::NotReady, Stage::ControlPlane),
                "{name}: {failure:?}"
            );
            assert!(
                elapsed < Duration::from_secs(1),
                "{name}: a violation must be refused at once, took {elapsed:?}: {failure:?}"
            );
        }
    }
}

/// A ready line still missing its newline is not a ready line yet. The
/// script prints half of it, writes its lock only later, then finishes
/// the line; a probe that took the half would look for a lock that is
/// not there.
#[tokio::test]
async fn a_half_written_ready_line_is_not_ready_yet() {
    let rt = runtime();
    let _listener = listen(&rt.sock, answer);
    let mut child = sh(
        &format!(
            "printf 'bookrack daemon running: pid=%s' $$; sleep 0.5; {WRITE_OWN_LOCK}; \
             printf ' control_sock=%s\\n' \"$2\"; exec sleep 30"
        ),
        &[rt.lock.as_os_str(), rt.sock.as_os_str()],
        Stdio::null(),
    );
    let pid = child.id().expect("pid");

    let ready = child
        .probe_ready(&rt.lock, Duration::from_secs(5))
        .await
        .expect("a ready line finished after the lock is ready");
    assert_eq!(
        ready.ready_line,
        format!(
            "bookrack daemon running: pid={pid} control_sock={}",
            rt.sock.display()
        ),
    );
}

/// A leader that ignores `SIGTERM` outlives `stop`'s deadline; the
/// failure is a timeout at the stop, and the group still goes.
#[tokio::test]
async fn a_stop_that_times_out_is_staged_as_stop() {
    let (mut probe, stdin) = Probe::new();
    let child = sh("trap '' TERM; sleep 30", &[], stdin);
    probe.assert_open().await;

    let failure = match child.stop(Duration::from_secs(1)).await {
        Err(failure) => failure,
        Ok(exit) => panic!("a leader ignoring SIGTERM stopped: {:?}", exit.status),
    };
    assert_eq!(
        (failure.kind, failure.stage),
        (FailureKind::Timeout, Stage::Stop),
        "{failure}"
    );
    probe.assert_closed("a leader that ignored the stop").await;
}

/// A cancelled probe leaves the child to its owner: still running, and
/// stoppable afterwards with its own status.
#[tokio::test]
async fn a_cancelled_probe_leaves_the_child_to_its_owner() {
    let rt = runtime();
    let (mut probe, stdin) = Probe::new();
    let mut child = sh("exec sleep 30", &[], stdin);

    let cancelled = tokio::time::timeout(
        Duration::from_millis(200),
        child.probe_ready(&rt.lock, Duration::from_secs(10)),
    )
    .await;
    assert!(
        cancelled.is_err(),
        "premise: the probe outlives the cancellation"
    );
    probe.assert_open().await;

    let exit = child
        .stop(Duration::from_secs(5))
        .await
        .expect("a child left running stops");
    assert_eq!(exit.status.signal(), Some(15), "{:?}", exit.status);
    probe.assert_closed("a stopped child").await;
}

/// The probe's connection belongs to the probe: cancelling it mid-call
/// closes the connection with it, every time, so repeated probes leave
/// nothing open on the daemon.
#[tokio::test]
async fn a_probe_cancelled_mid_call_closes_its_connection() {
    let rt = runtime();
    let closed = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&closed);
    let listener = listen(&rt.sock, move |stream| {
        let counted = Arc::clone(&counted);
        async move {
            let reader = read_request(stream).await;
            hold(reader).await;
            counted.fetch_add(1, Ordering::SeqCst);
        }
    });
    let mut child = ready_daemon(&rt, &rt.sock);
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.ready_line().is_none() {
        assert!(
            Instant::now() < deadline,
            "the script printed no ready line"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    for round in 1..=3 {
        let cancelled = tokio::time::timeout(
            Duration::from_millis(300),
            child.probe_ready(&rt.lock, Duration::from_secs(10)),
        )
        .await;
        assert!(
            cancelled.is_err(),
            "premise: round {round} is cancelled mid-call"
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        while closed.load(Ordering::SeqCst) < round {
            assert!(
                Instant::now() < deadline,
                "round {round}: the listener still holds an open connection 1s after the cancel",
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert_eq!(listener.accepts.load(Ordering::SeqCst), 3);
    assert_eq!(closed.load(Ordering::SeqCst), 3, "no connection left open");
}

/// Every readiness branch has exactly one kind and stage, and a leader
/// still running keeps them through `give_up`.
#[tokio::test]
async fn every_ready_branch_has_one_kind_and_stage() {
    use FailureKind::{NotReady, Timeout};

    let elsewhere = tempfile::tempdir().expect("tempdir");
    let foreign = elsewhere.path().join("control.sock");
    let stay = "exec sleep 30";
    let cases: Vec<(&str, String, Duration, (FailureKind, Stage))> = vec![
        (
            "no ready line",
            stay.to_string(),
            Duration::from_millis(300),
            (Timeout, Stage::ReadyLine),
        ),
        (
            "an exit before ready",
            "exit 0".to_string(),
            Duration::from_secs(5),
            (NotReady, Stage::Run),
        ),
        (
            "no lock",
            format!("{PRINT_READY_LINE}; {stay}"),
            Duration::from_secs(5),
            (NotReady, Stage::DaemonLock),
        ),
        (
            "a lock of another pid",
            format!(
                "printf 'pid=1\\nmcp=127.0.0.1:1\\ncontrol_sock=%s\\n' \"$2\" > \"$1\"; \
                 {PRINT_READY_LINE}; {stay}"
            ),
            Duration::from_secs(5),
            (NotReady, Stage::DaemonLock),
        ),
        (
            "a lock without a control socket",
            format!("printf 'pid=%s\\nmcp=127.0.0.1:1\\n' $$ > \"$1\"; {PRINT_READY_LINE}; {stay}"),
            Duration::from_secs(5),
            (NotReady, Stage::DaemonLock),
        ),
        (
            "an unreadable lock",
            format!("mkdir \"$1\"; {PRINT_READY_LINE}; {stay}"),
            Duration::from_secs(5),
            (NotReady, Stage::DaemonLock),
        ),
        (
            "a foreign endpoint",
            format!(
                "printf 'pid=%s\\nmcp=127.0.0.1:1\\ncontrol_sock=%s\\n' $$ \"$3\" > \"$1\"; \
                 {PRINT_READY_LINE}; {stay}"
            ),
            Duration::from_secs(5),
            (NotReady, Stage::Endpoint),
        ),
        (
            "nothing listening",
            format!("{WRITE_OWN_LOCK}; {PRINT_READY_LINE}; {stay}"),
            Duration::from_secs(5),
            (NotReady, Stage::ControlPlane),
        ),
        (
            "a silent control plane",
            format!("{WRITE_OWN_LOCK}; {PRINT_READY_LINE}; {stay}"),
            Duration::from_secs(1),
            (Timeout, Stage::ControlPlane),
        ),
    ];
    for (name, script, budget, expected) in cases {
        let rt = runtime();
        let _listener = match name {
            "nothing listening" => {
                drop(std::os::unix::net::UnixListener::bind(&rt.sock).expect("bind"));
                None
            }
            "a silent control plane" => Some(listen(&rt.sock, |stream| async move {
                hold(read_request(stream).await).await;
            })),
            _ => None,
        };
        let mut child = sh(
            &script,
            &[
                rt.lock.as_os_str(),
                rt.sock.as_os_str(),
                foreign.as_os_str(),
            ],
            Stdio::null(),
        );

        let failure = unwrap_failure(child.probe_ready(&rt.lock, budget).await, name);
        assert_eq!(
            (failure.kind, failure.stage),
            expected,
            "{name}: {failure:?}"
        );
        let failure = child.give_up(failure).await;
        assert_eq!(
            (failure.kind, failure.stage),
            expected,
            "{name}, after give_up: {failure}"
        );
    }
}
