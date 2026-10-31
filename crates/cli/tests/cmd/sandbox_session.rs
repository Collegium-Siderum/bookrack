// SPDX-License-Identifier: Apache-2.0

//! The sandbox tool against the real `bookrack` binary, driven in
//! process through `Session`: a real daemon comes up and is judged ready
//! the tool's way, a placed `.env` reaches it, the `rpc` whitelist is
//! all reads, and a stopped daemon's leftover lock still passes.

#![cfg(unix)]

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bookrack_test_support::tool::argv::READ_ONLY_RPC;
use bookrack_test_support::tool::exec::{DaemonStart, ExecOpts};
use bookrack_test_support::tool::{Class, CreateOpts, Preset, Session, SessionOpts, Source};
use bookrack_test_support::{StubSpec, bookrack_bin};
use serde_json::Value;
use tempfile::TempDir;

struct World {
    root: TempDir,
    session: Session,
}

impl World {
    async fn new() -> World {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(root.path().join("ws")).expect("ws");
        let mut session = Session::create(
            &root.path().join("sbx"),
            Source::Prebuilt(bookrack_bin!()),
            Preset::Bare,
            CreateOpts {
                replace: false,
                workspace: Some(root.path().join("ws")),
            },
            SessionOpts::default(),
        )
        .await
        .unwrap_or_else(|o| panic!("create: {o:?}"));
        session.stub_start(StubSpec::default());
        World { root, session }
    }

    fn dir(&self) -> PathBuf {
        self.root.path().join("sbx")
    }

    fn host(&self) -> PathBuf {
        self.dir().join("host")
    }

    /// Run `args` in a cell and return its stdout as JSON.
    async fn exec_json(&mut self, cell: &str, args: &[&str]) -> Value {
        let args: Vec<_> = args.iter().map(|a| a.into()).collect();
        let out = self
            .session
            .exec(&args, ExecOpts::cell(cell))
            .await
            .unwrap_or_else(|o| panic!("exec: {o:?}"));
        assert_eq!(
            (out.outcome.class, out.outcome.code),
            (Class::Exited, Some(0)),
            "{:?}\n{}",
            out.outcome,
            std::fs::read_to_string(self.dir().join("cells").join(cell).join("stderr"))
                .unwrap_or_default()
        );
        let stdout =
            std::fs::read(self.dir().join("cells").join(cell).join("stdout")).expect("stdout");
        serde_json::from_slice(&stdout).expect("stdout is JSON")
    }

    async fn stop(&mut self) {
        let outcome = self
            .session
            .daemon_stop(Duration::from_secs(10))
            .await
            .unwrap_or_else(|o| panic!("stop: {o:?}"));
        assert_eq!(outcome.class, Class::Exited, "{outcome:?}");
    }
}

fn instance_stderr(dir: &Path, n: u32) -> String {
    std::fs::read_to_string(dir.join("run/daemon").join(n.to_string()).join("stderr"))
        .expect("daemon stderr")
}

#[tokio::test]
async fn a_real_daemon_reads_as_ready_through_the_session() {
    let mut world = World::new().await;
    let ready = world
        .session
        .daemon_start(DaemonStart::library("alpha"))
        .await
        .unwrap_or_else(|o| panic!("start: {o:?}"));
    let card = world.exec_json("c1", &["status", "--json"]).await;
    assert_eq!(card["daemon"]["pid"], ready.pid, "{card}");
    world.stop().await;
    let exit: Value =
        serde_json::from_slice(&std::fs::read(world.dir().join("run/daemon/1/exit.json")).unwrap())
            .unwrap();
    assert_eq!(exit["code"], 0, "{exit}");
}

#[tokio::test]
async fn a_real_dotenv_reaches_the_daemon() {
    let mut world = World::new().await;
    let dotenv = world.host().join("inputs/console.env");
    std::fs::write(&dotenv, b"BOOKRACK_LOG_CONSOLE=info\n").expect("write");
    world
        .session
        .daemon_start(DaemonStart::library("alpha").dotenv(&dotenv))
        .await
        .unwrap_or_else(|o| panic!("start: {o:?}"));
    world.stop().await;
    let with = instance_stderr(&world.dir(), 1);
    assert!(
        with.contains("INFO"),
        "the daemon did not log at info: {with}"
    );

    world
        .session
        .daemon_start(DaemonStart::library("alpha"))
        .await
        .unwrap_or_else(|o| panic!("start: {o:?}"));
    world.stop().await;
    let without = instance_stderr(&world.dir(), 2);
    assert!(
        !without.contains("INFO"),
        "the built-in console level let info through: {without}"
    );
}

#[tokio::test]
async fn every_whitelisted_rpc_method_is_a_read() {
    let mut world = World::new().await;
    world
        .session
        .daemon_start(DaemonStart::library("alpha"))
        .await
        .unwrap_or_else(|o| panic!("start: {o:?}"));
    let methods = world
        .exec_json("m", &["rpc", "call", "daemon.methods"])
        .await;
    let rows = methods["methods"].as_array().expect("methods array");
    for name in READ_ONLY_RPC {
        let row = rows
            .iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("{name} is not a method the daemon answers"));
        assert_eq!(row["kind"], "read", "{name} is not a read");
    }
    world.stop().await;
}

#[tokio::test]
async fn a_stopped_daemon_leaves_a_stale_lock_that_still_passes_the_tripwire() {
    let mut world = World::new().await;
    world
        .session
        .daemon_start(DaemonStart::library("alpha"))
        .await
        .unwrap_or_else(|o| panic!("start: {o:?}"));
    world.stop().await;
    assert!(
        world.host().join("runtime/bookrack.tty.lock").exists(),
        "premise: the lock file stays"
    );
    assert!(
        !world.host().join("runtime/control.sock").exists(),
        "premise: the socket is gone"
    );
    let card = world.exec_json("s", &["status", "--json"]).await;
    assert_eq!(card["daemon"]["running"], false, "{card}");
}

#[tokio::test]
async fn a_foreign_control_socket_is_never_dialed() {
    let mut world = World::new().await;
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let foreign = elsewhere.path().join("control.sock");
    let listener = UnixListener::bind(&foreign).expect("bind");
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&accepted);
    std::thread::spawn(move || {
        for _stream in listener.incoming().flatten() {
            count.fetch_add(1, Ordering::SeqCst);
        }
    });
    let lock = world.host().join("runtime/bookrack.tty.lock");
    std::fs::write(
        &lock,
        format!(
            "pid=1\nmcp=127.0.0.1:1\ncontrol_sock={}\n",
            foreign.display()
        ),
    )
    .expect("lock");
    let held = std::fs::File::open(&lock).expect("open");
    held.lock().expect("hold");
    let err = world
        .session
        .exec(&["quit".into()], ExecOpts::default())
        .await
        .expect_err("refused");
    assert_eq!(err.stage, "tripwire", "{err:?}");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        0,
        "the foreign socket was dialed"
    );
}
