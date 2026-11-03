// SPDX-License-Identifier: Apache-2.0

//! What the suites of `bookrack_test_support`'s sandbox tool share: a
//! shell script standing in for the binary under test, a sandbox built
//! from it, and the probes and control-socket answer the daemon-shaped
//! stand-ins rely on.
//!
//! The stand-in answers `config effective --json` with a compliant
//! report for the sandbox it runs in, so T7 passes and the rest of the
//! script is each test's own.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

pub const TOOL: &str = env!("CARGO_BIN_EXE_bookrack-sandbox");

/// The stand-in's opening: a compliant `config effective --json` report
/// for the sandbox named by the environment, with `__HOOK__` run just
/// before it exits.
pub const PRELUDE: &str = r#"#!/bin/sh
case " $* " in
  *" config effective "*)
    H=$(dirname "$BOOKRACK_REGISTRY")
    if [ "${BOOKRACK_NO_DOTENV:-}" = "1" ]; then ND=1; DP=; else ND=0; DP=", \"dotenv_path\": \"$PWD/.env\""; fi
    printf '{"rows":[{"key":"registry","value":"%s","shadowed":[]},{"key":"data_dir","value":"%s/roots/alpha","shadowed":[]},{"key":"daemon_state_dir","value":"%s","shadowed":[]},{"key":"runtime_dir","value":"%s","shadowed":[]},{"key":"backup_dir","value":"%s/roots/alpha/backup","shadowed":[]},{"key":"no_dotenv","value":"%s","shadowed":[]}],"dotenv_foreign":[],"native_dependencies":[]%s}\n' "$BOOKRACK_REGISTRY" "$H" "$BOOKRACK_DAEMON_STATE_DIR" "$BOOKRACK_RUNTIME_DIR" "$H" "$ND" "$DP"
    __HOOK__
    exit 0 ;;
esac
"#;

/// A stand-in that behaves like a daemon: writes the lock, prints the
/// ready line, exits 0 on TERM. The test answers `daemon.version` on the
/// control socket itself.
pub const DAEMON_BODY: &str = r#"
LOCK="$BOOKRACK_RUNTIME_DIR/bookrack.tty.lock"
SOCK="$BOOKRACK_RUNTIME_DIR/control.sock"
printf 'pid=%s\nmcp=127.0.0.1:1\ncontrol_sock=%s\n' $$ "$SOCK" > "$LOCK"
echo "bookrack daemon running: pid=$$ mcp=127.0.0.1:1 control_sock=$SOCK"
trap 'exit 0' TERM
sleep 30 >/dev/null 2>&1 &
wait
"#;

/// [`DAEMON_BODY`] with a `quit` arm: `bookrack quit` records its
/// environment under `__ROOT__/quit.env` and runs `__QUIT_ARM__`, whose
/// usual form creates `__ROOT__/quit.flag`; the daemon leaves when the
/// flag appears, with the exit code the flag holds (empty is 0), and
/// notes a `SIGTERM` under `__ROOT__/term.seen` before exiting 0 on
/// it. The test substitutes both placeholders when it writes the
/// script, so no path travels through the environment.
pub const QUIT_DAEMON_BODY: &str = r#"
case " $* " in
  *" quit "*)
    env > "__ROOT__/quit.env"
    __QUIT_ARM__
    exit 0 ;;
esac
LOCK="$BOOKRACK_RUNTIME_DIR/bookrack.tty.lock"
SOCK="$BOOKRACK_RUNTIME_DIR/control.sock"
printf 'pid=%s\nmcp=127.0.0.1:1\ncontrol_sock=%s\n' $$ "$SOCK" > "$LOCK"
echo "bookrack daemon running: pid=$$ mcp=127.0.0.1:1 control_sock=$SOCK"
trap 'echo term > "__ROOT__/term.seen"; exit 0' TERM
while [ ! -e "__ROOT__/quit.flag" ]; do sleep 0.1; done
CODE=$(cat "__ROOT__/quit.flag")
exit "${CODE:-0}"
"#;

/// The `__QUIT_ARM__` of a `quit` that takes effect: the flag appears,
/// empty, and the daemon leaves with 0.
pub const QUIT_ARM_OK: &str = r#": > "__ROOT__/quit.flag""#;

pub struct World {
    pub root: TempDir,
    pub dir: PathBuf,
    pub stand_in: PathBuf,
}

/// A world runs the tool's own `teardown` when it goes, so a test that
/// fails before its own teardown leaves no stub or supervisor behind
/// once its tempdir is gone. The teardown signals only what the
/// sandbox's records prove the tool started; its outcome is not judged,
/// and a session lock still held gives up after a short wait.
impl Drop for World {
    fn drop(&mut self) {
        if !self.dir.join(".sandbox.json").exists() {
            return;
        }
        let _ = Command::new(TOOL)
            .args(["--lock-wait", "2", "teardown", &self.dir_s()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl World {
    /// A stand-in with `body` after the prelude, and a `bare` sandbox
    /// built from it.
    pub fn new(body: &str) -> World {
        World::with_hook(body, "")
    }

    pub fn with_hook(body: &str, hook: &str) -> World {
        let root = tempfile::tempdir().expect("tempdir");
        let stand_in = root.path().join("stand-in.sh");
        write_script(
            &stand_in,
            &format!("{}{body}\n", PRELUDE.replace("__HOOK__", hook)),
        );
        std::fs::create_dir(root.path().join("ws")).expect("ws");
        let dir = root.path().canonicalize().expect("canonical").join("sbx");
        let world = World {
            root,
            dir,
            stand_in,
        };
        let out = world.tool(&["new", &world.dir_s(), "--preset", "bare"]);
        assert!(
            out.status.success(),
            "new failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        world
    }

    pub fn dir_s(&self) -> String {
        self.dir.display().to_string()
    }

    pub fn ws(&self) -> String {
        self.root.path().join("ws").display().to_string()
    }

    pub fn host(&self) -> PathBuf {
        self.dir.join("host")
    }

    /// Run the tool; `new` gets `--stand-in` and `--workspace` added.
    pub fn tool(&self, args: &[&str]) -> Output {
        self.tool_with(args, |_| {})
    }

    pub fn tool_with(&self, args: &[&str], configure: impl FnOnce(&mut Command)) -> Output {
        let mut cmd = self.command(args);
        configure(&mut cmd);
        cmd.output().expect("run bookrack-sandbox")
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(TOOL);
        cmd.args(args);
        if args.contains(&"new") {
            cmd.arg("--stand-in")
                .arg(&self.stand_in)
                .arg("--workspace")
                .arg(self.ws());
        }
        cmd.stdin(Stdio::null());
        cmd
    }

    pub fn exec(&self, extra: &[&str], bookrack_args: &[&str]) -> Output {
        let dir = self.dir_s();
        let mut args = vec!["exec", dir.as_str()];
        args.extend_from_slice(extra);
        args.push("--");
        args.extend_from_slice(bookrack_args);
        let args: Vec<String> = args.into_iter().map(str::to_string).collect();
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.tool(&refs)
    }

    pub fn records(&self) -> Vec<Value> {
        let calls = self.dir.join(".calls");
        let mut ids: Vec<String> = std::fs::read_dir(&calls)
            .expect("calls")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n != "last")
            .collect();
        ids.sort();
        ids.iter()
            .map(|id| read_json(&calls.join(id).join("result.json")))
            .collect()
    }

    pub fn last_record(&self) -> Value {
        let id = std::fs::read_to_string(self.dir.join(".calls/last")).expect("last");
        read_json(&self.dir.join(".calls").join(id.trim()).join("result.json"))
    }

    pub fn tty_lock(&self) -> PathBuf {
        self.host().join("runtime/bookrack.tty.lock")
    }

    pub fn session_lock(&self) -> PathBuf {
        self.dir.join(".session.lock")
    }
}

pub fn write_script(path: &Path, text: &str) {
    std::fs::write(path, text).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

pub fn read_json(path: &Path) -> Value {
    serde_json::from_slice(
        &std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

pub fn code(out: &Output) -> i32 {
    out.status.code().expect("an exit code")
}

pub fn signature(path: &Path) -> Vec<String> {
    let out = Command::new(TOOL)
        .arg("sig")
        .arg(path)
        .output()
        .expect("sig");
    assert!(out.status.success(), "sig failed: {}", stderr(&out));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// A FIFO outside the sandbox a stand-in's descendant holds open; EOF
/// on the read end means every holder is dead.
pub struct Probe {
    pub path: PathBuf,
    rx: File,
}

impl Probe {
    pub fn new(dir: &Path) -> Probe {
        let path = dir.join("probe.fifo");
        let status = Command::new("mkfifo").arg(&path).status().expect("mkfifo");
        assert!(status.success());
        let rx = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc_o_nonblock())
            .open(&path)
            .expect("open fifo");
        Probe { path, rx }
    }

    /// Wait until the descendant has written its byte.
    pub fn wait_held(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut buf = [0u8; 1];
        loop {
            match self.rx.read(&mut buf) {
                Ok(1) if buf[0] == b'.' => return,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("probe read: {e}"),
            }
            assert!(
                Instant::now() < deadline,
                "the descendant never took the probe"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Whether every writer is gone within `within`.
    pub fn closes_within(&mut self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        let mut buf = [0u8; 16];
        loop {
            match self.rx.read(&mut buf) {
                Ok(0) => return true,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("probe read: {e}"),
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

pub fn libc_o_nonblock() -> i32 {
    rustix::fs::OFlags::NONBLOCK.bits() as i32
}

/// Answer `daemon.version` on the sandbox's control socket from a
/// thread, counting the connections.
pub fn serve_version(sock: &Path) -> Arc<AtomicUsize> {
    let listener = UnixListener::bind(sock).expect("bind control socket");
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&accepted);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            count.fetch_add(1, Ordering::SeqCst);
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let mut stream = stream;
            let _ = stream.write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"version\":\"stand-in\"}}\n",
            );
        }
    });
    accepted
}
