// SPDX-License-Identifier: Apache-2.0

//! Shared bring-up helpers for the daemon integration suite.
//!
//! Daemon bring-up opens one [`bookrack_query::Library`] per mounted
//! library, and each open probes the configured embedder over HTTP
//! for its vector dimension. [`embed_stub_url`] starts a loopback
//! HTTP server that answers the Ollama surface the daemon touches —
//! `POST /api/embed` with fixed-width vectors, and a `/api/tags`
//! model list holding the default embed model so the bring-up
//! pre-flight passes — and points `BOOKRACK_OLLAMA_URL` at it, so the
//! whole suite runs with no embedding daemon installed.
//!
//! [`isolate_daemon_state_dir`] pins the daemon state directory to a
//! per-binary tempdir so bring-up never touches the user's real
//! per-user data directory.

#![allow(dead_code)]

use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use bookrack_config::DEFAULT_EMBED_MODEL;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use eyre::{Result, eyre};

/// Vector width the loopback embed stub reports for every input.
pub const STUB_DIMENSION: usize = 8;

/// Upper bound on one test's daemon lifetime. A driver that dies
/// before `daemon.shutdown` reaches the daemon would otherwise leave
/// `run_until_shutdown` blocked on the broadcast forever; the
/// deadline turns that hang into a failure.
pub const TEST_DEADLINE: Duration = Duration::from_secs(60);

static EMBED_STUB: OnceLock<String> = OnceLock::new();
static DAEMON_STATE_DIR: OnceLock<tempfile::TempDir> = OnceLock::new();

/// Pin the embed endpoint and the daemon state directory for this
/// test binary. Call as the first statement of every test.
pub fn init_test_env() {
    embed_stub_url();
    isolate_daemon_state_dir();
}

/// Start the loopback embed stub once per test binary and point
/// `BOOKRACK_OLLAMA_URL` at it. Returns the stub's base URL.
pub fn embed_stub_url() -> &'static str {
    EMBED_STUB.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind embed stub");
        let url = format!("http://{}", listener.local_addr().expect("embed stub addr"));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || serve_embed_connection(stream));
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

/// Redirect the daemon state directory into a per-binary tempdir so
/// bring-up never touches the user's real per-user data directory.
pub fn isolate_daemon_state_dir() {
    DAEMON_STATE_DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("daemon state tempdir");
        // SAFETY: env is mutated exactly once, inside
        // `OnceLock::get_or_init`'s single-initialization guarantee,
        // as the first statement of every test in this binary, before
        // any concurrent env reads.
        unsafe { std::env::set_var("BOOKRACK_DAEMON_STATE_DIR", dir.path()) };
        dir
    });
}

/// Serve one stub connection until the client hangs up. Connections
/// are kept open across requests so a pooled HTTP client can reuse
/// them.
fn serve_embed_connection(stream: TcpStream) {
    let _ = serve_embed_requests(stream);
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
            // `/api/tags`. The pre-flight in `DaemonRuntime::start`
            // refuses bring-up unless the configured model is listed
            // here, so a stub that reported an empty list would fail
            // every daemon-backed test before it began.
            serde_json::json!({ "models": [{ "name": DEFAULT_EMBED_MODEL }] })
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

/// Headless bring-up options against explicit data and runtime roots.
pub fn build_opts(
    data_dir: PathBuf,
    runtime_dir: PathBuf,
    spawn_queue_worker: bool,
) -> RuntimeOpts {
    let mut opts = RuntimeOpts::headless(Some(data_dir), None);
    opts.no_mcp = true;
    opts.spawn_queue_worker = spawn_queue_worker;
    opts.runtime_dir = Some(runtime_dir);
    opts
}

/// Drive `run_until_shutdown` and the test's driver task to completion
/// under [`TEST_DEADLINE`]. If the driver fails before its
/// `daemon.shutdown` reaches the daemon, the broadcast is fired here
/// so the foreground loop still drains and the driver's own error is
/// what surfaces.
pub async fn join_with_deadline(
    runtime: DaemonRuntime,
    repl_handle: tokio::task::JoinHandle<Result<()>>,
    driver: tokio::task::JoinHandle<Result<()>>,
) -> Result<()> {
    let shutdown_tx = runtime.shutdown_tx.clone();
    tokio::time::timeout(TEST_DEADLINE, async move {
        let run = tokio::spawn(runtime.run_until_shutdown(None, repl_handle));
        let driver_result = match driver.await {
            Ok(result) => result,
            Err(join_err) => Err(eyre!("driver task failed: {join_err}")),
        };
        // Idempotent when the driver already sent `daemon.shutdown`;
        // unwedges the foreground loop when it died before doing so.
        let _ = shutdown_tx.send(());
        let run_result = run
            .await
            .map_err(|join_err| eyre!("runtime task failed: {join_err}"))?;
        driver_result.and(run_result)
    })
    .await
    .map_err(|_| eyre!("test deadline elapsed before daemon shutdown"))?
}

// Each test binary consumes its own subset of these helpers; the
// unused remainder must not fail that binary's `-D warnings` build.
#[allow(unused_imports)]
#[cfg(unix)]
pub use socket::{Reader, Writer, await_channel, connect, recv, send};

#[cfg(unix)]
mod socket {
    use std::path::Path;
    use std::time::Duration;

    use eyre::{Result, eyre};
    use serde_json::Value;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};
    use tokio::net::UnixStream;

    pub type Reader = Lines<BufReader<ReadHalf<UnixStream>>>;
    pub type Writer = WriteHalf<UnixStream>;

    /// Connect to the control socket, retrying inside a fixed window
    /// so the driver never races the accept loop's bring-up.
    pub async fn connect(sock: &Path) -> Result<(Reader, Writer)> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match UnixStream::connect(sock).await {
                Ok(stream) => break stream,
                Err(_) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(err) => return Err(eyre!("control socket never accepted: {err}")),
            }
        };
        let (read_half, write_half) = tokio::io::split(stream);
        Ok((BufReader::new(read_half).lines(), write_half))
    }

    pub async fn send(writer: &mut Writer, line: &str) -> Result<()> {
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }

    pub async fn recv(reader: &mut Reader) -> Result<Value> {
        let line = reader
            .next_line()
            .await?
            .ok_or_else(|| eyre!("connection closed before response"))?;
        Ok(serde_json::from_str(&line)?)
    }

    /// Skip frames until a notification on `channel` arrives, bounded
    /// by `timeout`.
    pub async fn await_channel(
        reader: &mut Reader,
        channel: &str,
        timeout: Duration,
    ) -> Result<Value> {
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return Err(eyre!("timed out waiting for channel {channel}")),
                frame = reader.next_line() => {
                    let line = frame?.ok_or_else(|| eyre!("eof while awaiting {channel}"))?;
                    let v: Value = serde_json::from_str(&line)?;
                    if v.get("method").is_some()
                        && v["params"]["channel"].as_str() == Some(channel)
                    {
                        return Ok(v);
                    }
                }
            }
        }
    }
}
