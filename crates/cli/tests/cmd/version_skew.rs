// SPDX-License-Identifier: Apache-2.0

//! What the client says when the daemon it reaches is another version.
//!
//! The daemon here is a stand-in: a held session lock and a control
//! socket in a sandbox's runtime directory, answering each method from a
//! script. That is the only way to put a daemon of another version in
//! front of the binary under test.
//!
//! Three promises: a second `bookrack run` still hands over and exits
//! zero, and says which versions are involved; a routed command the
//! daemon refuses as an unknown method or bad parameters carries the
//! version difference in its hint, the daemon's own hint kept; and the
//! status card of a daemon whose `status` reports no identity takes the
//! version from `daemon.version`.

#![cfg(unix)]

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use bookrack_test_support::{Sandbox, bookrack_cmd};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;

const THIS_VERSION: &str = env!("CARGO_PKG_VERSION");
const OLDER_VERSION: &str = "0.14.0";

/// How the stand-in answers one method.
#[derive(Clone)]
enum Reply {
    Result(Value),
    Error {
        code: i32,
        message: &'static str,
        data: Option<Value>,
    },
}

/// A scripted daemon on a sandbox's runtime directory. Holds the
/// session lock for as long as it lives.
struct StandIn {
    sandbox: Sandbox,
    calls: Arc<Mutex<Vec<String>>>,
    _lock: std::fs::File,
    server: tokio::task::JoinHandle<()>,
}

impl StandIn {
    /// Bind the control socket, write and hold the lock, and answer
    /// `daemon.version` with `version`, each method in `script` as
    /// scripted, and anything else with `fallback`.
    fn start(version: &str, script: &[(&str, Reply)], fallback: Reply) -> StandIn {
        let sandbox = Sandbox::new();
        let runtime_dir = sandbox.runtime_dir();
        std::fs::create_dir_all(&runtime_dir).expect("create the runtime directory");
        let sock = runtime_dir.join("control.sock");
        let listener = UnixListener::bind(&sock).expect("bind the control socket");

        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(sandbox.tty_lock_path())
            .expect("open the session lock");
        lock.try_lock().expect("hold the session lock");
        let mut writer = &lock;
        write!(
            writer,
            "pid={}\nmcp=disabled\ncontrol_sock={}\n",
            std::process::id(),
            sock.display()
        )
        .expect("write the session lock");
        writer.flush().expect("flush the session lock");

        let mut replies: HashMap<String, Reply> = script
            .iter()
            .map(|(method, reply)| (method.to_string(), reply.clone()))
            .collect();
        replies.insert(
            "daemon.version".to_string(),
            Reply::Result(json!({
                "version": version,
                "started_at": "2026-01-01T00:00:00Z",
            })),
        );
        let replies = Arc::new(replies);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&calls);
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let replies = Arc::clone(&replies);
                let seen = Arc::clone(&seen);
                let fallback = fallback.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = BufReader::new(read).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let Ok(request) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        let method = request["method"].as_str().unwrap_or_default().to_string();
                        seen.lock().expect("call log").push(method.clone());
                        let mut frame = json!({ "jsonrpc": "2.0", "id": request["id"] });
                        match replies.get(&method).unwrap_or(&fallback) {
                            Reply::Result(value) => frame["result"] = value.clone(),
                            Reply::Error {
                                code,
                                message,
                                data,
                            } => {
                                frame["error"] = json!({ "code": code, "message": message });
                                if let Some(data) = data {
                                    frame["error"]["data"] = data.clone();
                                }
                            }
                        }
                        let mut bytes = frame.to_string().into_bytes();
                        bytes.push(b'\n');
                        if write.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        StandIn {
            sandbox,
            calls,
            _lock: lock,
            server,
        }
    }

    /// Run the binary against this stand-in.
    async fn run(&self, args: &[&str]) -> std::process::Output {
        Command::from(bookrack_cmd!(&self.sandbox).without_data_dir().build())
            .args(args)
            .output()
            .await
            .expect("run bookrack")
    }

    /// Run a second `bookrack run` against this stand-in, on the
    /// sandbox's own data root.
    async fn second_launch(&self) -> std::process::Output {
        Command::from(bookrack_cmd!(&self.sandbox).build())
            .args(["run", "--no-mcp"])
            .output()
            .await
            .expect("run bookrack")
    }

    /// Every method the stand-in was asked, in arrival order.
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("call log").clone()
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn method_not_found() -> Reply {
    Reply::Error {
        code: -32601,
        message: "method not found",
        data: None,
    }
}

fn invalid_params_with_a_hint() -> Reply {
    Reply::Error {
        code: -32602,
        message: "cannot read the request",
        data: Some(json!({
            "detail": null,
            "hint": "Check the parameter names.",
            "retryable": false,
        })),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_handoff_notes_version_skew_and_still_exits_zero() {
    let older = StandIn::start(OLDER_VERSION, &[], method_not_found());
    let out = older.second_launch().await;
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", text(&out.stderr));
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines.first().is_some_and(|line| line.starts_with(&format!(
            "bookrack daemon already running: pid={} control_sock=",
            std::process::id()
        ))),
        "the handoff line is unchanged: {stdout}"
    );
    assert_eq!(
        lines.len(),
        2,
        "one note follows the handoff line: {stdout}"
    );
    assert!(
        lines[1].starts_with("note: ")
            && lines[1].contains(OLDER_VERSION)
            && lines[1].contains(THIS_VERSION)
            && lines[1].contains("bookrack quit"),
        "the note names both versions and the way to switch: {stdout}"
    );

    let same = StandIn::start(THIS_VERSION, &[], method_not_found());
    let out = same.second_launch().await;
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        stdout.lines().count(),
        1,
        "a daemon of this version gets the handoff line alone: {stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_appends_version_skew_hint_on_method_not_found_and_invalid_params() {
    for (reply, kept_hint) in [
        (method_not_found(), None),
        (
            invalid_params_with_a_hint(),
            Some("Check the parameter names."),
        ),
    ] {
        let older = StandIn::start(OLDER_VERSION, &[], reply);

        let out = older.run(&["queue", "list"]).await;
        let stderr = text(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{stderr}");
        let hint = stderr
            .lines()
            .find(|line| line.trim_start().starts_with("hint:"))
            .unwrap_or_else(|| panic!("no hint line in: {stderr}"));
        assert!(
            hint.contains(OLDER_VERSION) && hint.contains(THIS_VERSION),
            "the hint names both versions: {stderr}"
        );
        if let Some(kept) = kept_hint {
            assert!(hint.contains(kept), "the daemon's hint is kept: {stderr}");
        }

        let out = older.run(&["--json", "queue", "list"]).await;
        let stderr = text(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{stderr}");
        let parsed: Value = serde_json::from_str(stderr.trim())
            .unwrap_or_else(|e| panic!("not one JSON object ({e}): {stderr}"));
        let hint = parsed
            .pointer("/hint")
            .or_else(|| parsed.pointer("/error/hint"))
            .or_else(|| parsed.pointer("/error/data/hint"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("no hint field in: {stderr}"));
        assert!(
            hint.contains(OLDER_VERSION) && hint.contains(THIS_VERSION),
            "the JSON hint names both versions: {stderr}"
        );
        if let Some(kept) = kept_hint {
            assert!(hint.contains(kept), "the daemon's hint is kept: {stderr}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_version_is_asked_only_after_a_refusal_that_a_skew_explains() {
    let internal = StandIn::start(
        OLDER_VERSION,
        &[],
        Reply::Error {
            code: -32603,
            message: "internal error",
            data: None,
        },
    );
    let out = internal.run(&["queue", "list"]).await;
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        !stderr.contains(OLDER_VERSION),
        "an internal error carries no version note: {stderr}"
    );
    assert!(
        !internal.calls().iter().any(|m| m == "daemon.version"),
        "the version was asked after an internal error: {:?}",
        internal.calls()
    );

    let same = StandIn::start(THIS_VERSION, &[], method_not_found());
    let out = same.run(&["queue", "list"]).await;
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        !stderr.contains("this CLI is version"),
        "a daemon of this version gets no version note: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_status_card_takes_the_version_from_a_daemon_whose_status_carries_none() {
    let older = StandIn::start(
        OLDER_VERSION,
        &[
            (
                "status",
                Reply::Result(json!({
                    "state": "idle",
                    "queue_pending": 0,
                    "queue_running": 0,
                    "queue_worker_enabled": true,
                    "library": null,
                    "data_dir": "/data/older",
                })),
            ),
            ("library.info", Reply::Result(json!({}))),
        ],
        method_not_found(),
    );
    let out = older.run(&["--json", "status"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let card: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", text(&out.stdout)));
    assert_eq!(card["daemon"]["version"], OLDER_VERSION, "{card}");
    assert!(card["daemon"]["uptime"].is_string(), "{card}");
    assert_eq!(card["daemon"]["instance_id"], Value::Null, "{card}");
    assert_eq!(card["daemon"]["runtime_dir"], Value::Null, "{card}");
}
