// SPDX-License-Identifier: Apache-2.0

//! Shared helpers for the `bookrack run` end-to-end tests.
//!
//! Spawning the daemon as a child with piped stdout/stderr and then
//! blocking on `wait()` deadlocks the moment either pipe's ~64 KiB
//! kernel buffer fills, because the child blocks on its own write and
//! the test never reads. Tracing-heavy startup paths can saturate the
//! stderr pipe in well under a second of real work.
//!
//! [`DaemonProcess`] fixes this in one place: it spawns the child
//! with `kill_on_drop` set so a panicked test never leaks a daemon,
//! and starts two background tasks that continuously drain stdout
//! and stderr into in-memory buffers. The test calls
//! [`DaemonProcess::wait_with_output`] to retrieve the exit status
//! and captured streams once the daemon exits on its own.

#![allow(dead_code)]

use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use eyre::{Context, ContextCompat, Result};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

pub fn bookrack_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bookrack"))
}

/// Vector width the loopback embed stub reports for every input.
pub const STUB_DIMENSION: usize = 8;

static EMBED_STUB: OnceLock<String> = OnceLock::new();

/// Start a loopback HTTP stub answering the Ollama surface daemon
/// bring-up touches — `POST /api/embed` with fixed-width vectors, an
/// empty model list for everything else — and point
/// `BOOKRACK_OLLAMA_URL` at it, so both in-process runtimes and
/// spawned `bookrack` subprocesses (which inherit the environment)
/// run with no embedding daemon installed. Call as the first
/// statement of every test.
pub fn embed_stub_url() -> &'static str {
    EMBED_STUB.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind embed stub");
        let url = format!("http://{}", listener.local_addr().expect("embed stub addr"));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = serve_embed_requests(stream);
                });
            }
        });
        // SAFETY: env is mutated exactly once, inside
        // `OnceLock::get_or_init`'s single-initialization guarantee,
        // as the first statement of every test in this binary, before
        // any concurrent env reads.
        unsafe { std::env::set_var("BOOKRACK_OLLAMA_URL", &url) };
        url
    })
}

fn serve_embed_requests(stream: TcpStream) -> std::io::Result<()> {
    let mut reader = std::io::BufReader::new(stream.try_clone()?);
    let mut out = stream;
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }
        let mut content_length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header)? == 0 {
                return Ok(());
            }
            let header = header.trim();
            if header.is_empty() {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        let response = if request_line.starts_with("POST /api/embed") {
            let inputs = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["input"].as_array().map(Vec::len))
                .unwrap_or(0);
            serde_json::json!({
                "embeddings": vec![vec![0.5f32; STUB_DIMENSION]; inputs],
            })
        } else {
            serde_json::json!({ "models": [] })
        };
        let payload = response.to_string();
        write!(
            out,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
             content-length: {}\r\n\r\n{payload}",
            payload.len(),
        )?;
        out.flush()?;
    }
}

/// Wait until the daemon's TTY session lock contains the MCP address,
/// the marker that startup has finished and the control socket is
/// listening. Returns `true` on success, `false` on timeout.
pub async fn wait_for_lock(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if path.exists() {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            if text.contains("mcp=") {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Owns a spawned `bookrack run` child plus background drainer tasks
/// for stdout and stderr.
pub struct DaemonProcess {
    child: Child,
    stdout_handle: JoinHandle<String>,
    stderr_handle: JoinHandle<String>,
    /// Keeps the per-spawn daemon state directory alive for the
    /// child's lifetime; the spawned daemon writes its queue snapshot
    /// and logs here instead of the user's real per-user directory.
    _daemon_state_dir: tempfile::TempDir,
}

impl DaemonProcess {
    /// Spawn the configured command with stdin closed and stdout/stderr
    /// piped; immediately start background drainer tasks so the
    /// child's pipes never fill while the test holds `wait()`. The
    /// child's daemon state directory is pinned to a fresh tempdir,
    /// overriding any value the caller or the environment carries.
    pub fn spawn(cmd: &mut Command) -> Result<Self> {
        let daemon_state_dir = tempfile::tempdir().context("daemon state tempdir")?;
        let mut child = cmd
            .env("BOOKRACK_DAEMON_STATE_DIR", daemon_state_dir.path())
            .env("BOOKRACK_OLLAMA_URL", embed_stub_url())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("spawn bookrack run")?;
        let stdout = child.stdout.take().context("piped stdout")?;
        let stderr = child.stderr.take().context("piped stderr")?;
        let stdout_handle = tokio::spawn(drain(stdout));
        let stderr_handle = tokio::spawn(drain(stderr));
        Ok(Self {
            child,
            stdout_handle,
            stderr_handle,
            _daemon_state_dir: daemon_state_dir,
        })
    }

    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Block until the child exits «with the supplied timeout» and
    /// return the exit status plus everything its stdout and stderr
    /// produced over the run.
    pub async fn wait_with_output(
        mut self,
        timeout: Duration,
    ) -> Result<(ExitStatus, String, String)> {
        let status = tokio::time::timeout(timeout, self.child.wait())
            .await
            .context("daemon did not exit within the deadline")?
            .context("daemon wait failed")?;
        let stdout = self.stdout_handle.await.unwrap_or_default();
        let stderr = self.stderr_handle.await.unwrap_or_default();
        Ok((status, stdout, stderr))
    }
}

async fn drain<R: AsyncReadExt + Unpin + Send + 'static>(mut reader: R) -> String {
    let mut buf = String::new();
    let _ = reader.read_to_string(&mut buf).await;
    buf
}
