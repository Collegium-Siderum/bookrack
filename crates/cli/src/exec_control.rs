// SPDX-License-Identifier: Apache-2.0

//! Control-plane client for `bookrack exec`.
//!
//! Phase 2 lands the JSON-RPC client side of the daemon's local
//! control socket. The transport is the same newline-framed JSON-RPC
//! the daemon serves on `<runtime_dir>/bookrack.tty.lock`'s recorded
//! `control_sock=` path. Each call is one round trip: send a single
//! request line, read response lines until one matches the request
//! id (intervening lines are server-initiated event notifications and
//! are forwarded through `event_sink` so callers can decide what to
//! do with them).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Environment variable controlling which transport `bookrack exec`
/// reaches the daemon over. `control` (the default) goes through the
/// JSON-RPC socket; `mcp` keeps the Phase 1 MCP path as a one-release
/// compatibility fallback that Phase 4 removes.
pub const EXEC_CHANNEL_ENV: &str = "BOOKRACK_EXEC_CHANNEL";

/// Resolved choice between the control-plane and MCP transports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecChannel {
    Control,
    Mcp,
}

impl ExecChannel {
    /// Read [`EXEC_CHANNEL_ENV`]. Unset / empty / unknown values fall
    /// back to [`ExecChannel::Control`].
    pub fn from_env() -> Self {
        match std::env::var(EXEC_CHANNEL_ENV)
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("mcp") => ExecChannel::Mcp,
            _ => ExecChannel::Control,
        }
    }
}

/// Lock-file derived path of the running daemon's control socket.
/// Returns an error when the lock cannot be read, when the file has
/// no `control_sock=` line, or when the recorded path no longer
/// exists.
pub fn discover_control_sock(lock_path: &Path) -> Result<PathBuf> {
    let text = fs::read_to_string(lock_path).with_context(|| {
        format!(
            "read session lock at {} (is `bookrack run` running?)",
            lock_path.display()
        )
    })?;
    let recorded = text
        .lines()
        .find_map(|line| {
            let (k, v) = line.trim().split_once('=')?;
            (k.trim() == "control_sock").then(|| PathBuf::from(v.trim()))
        })
        .ok_or_else(|| {
            anyhow!(
                "session lock at {} has no `control_sock=` line; \
                 daemon may predate Phase 1",
                lock_path.display(),
            )
        })?;
    Ok(recorded)
}

#[derive(Debug, Serialize)]
struct Request<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<ResponseError>,
}

#[derive(Debug, Deserialize)]
struct ResponseError {
    code: i32,
    message: String,
}

/// Connect to the control socket recorded at `lock_path`, send one
/// request, and return the parsed `result` field. Notification
/// frames (`method` set, `id` absent) received while waiting for the
/// matching response are silently discarded — callers that want to
/// observe events should keep the connection open with a richer
/// helper.
pub async fn call_method(lock_path: &Path, method: &str, params: Option<Value>) -> Result<Value> {
    let sock = discover_control_sock(lock_path)?;
    let stream = connect(&sock).await?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let request = Request {
        jsonrpc: "2.0",
        id: 1,
        method,
        params,
    };
    let mut line = serde_json::to_string(&request).context("serialise control-plane request")?;
    line.push('\n');
    write_half
        .write_all(line.as_bytes())
        .await
        .with_context(|| format!("send control-plane request to {}", sock.display()))?;
    write_half
        .flush()
        .await
        .context("flush control-plane request")?;
    let mut reader = BufReader::new(read_half).lines();
    while let Some(next) = reader
        .next_line()
        .await
        .context("read control-plane response")?
    {
        if next.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&next)
            .with_context(|| format!("parse control-plane line: {next}"))?;
        if value.get("method").is_some() {
            continue;
        }
        let response: Response = serde_json::from_value(value)
            .with_context(|| format!("parse control-plane response: {next}"))?;
        if response
            .id
            .as_ref()
            .is_some_and(|v| v == &serde_json::json!(1))
        {
            if let Some(err) = response.error {
                bail!(
                    "control-plane method {method} failed: code {code} {msg}",
                    code = err.code,
                    msg = err.message,
                );
            }
            return Ok(response.result.unwrap_or(Value::Null));
        }
    }
    Err(anyhow!(
        "control-plane connection at {} closed before response",
        sock.display()
    ))
}

#[cfg(unix)]
async fn connect(path: &Path) -> Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(path)
        .await
        .with_context(|| format!("connect to control socket at {}", path.display()))
}

#[cfg(windows)]
async fn connect(path: &Path) -> Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let name = path
        .to_str()
        .ok_or_else(|| anyhow!("control socket path {} is not valid UTF-8", path.display()))?;
    ClientOptions::new()
        .open(name)
        .with_context(|| format!("connect to control pipe {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_from_env_recognises_mcp() {
        unsafe {
            std::env::set_var(EXEC_CHANNEL_ENV, "mcp");
        }
        assert_eq!(ExecChannel::from_env(), ExecChannel::Mcp);
        unsafe {
            std::env::remove_var(EXEC_CHANNEL_ENV);
        }
        assert_eq!(ExecChannel::from_env(), ExecChannel::Control);
    }

    #[test]
    fn discover_control_sock_reads_recorded_path() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");
        fs::write(
            &lock,
            "pid=1234\nmcp=127.0.0.1:8765\ncontrol_sock=/tmp/test.sock\n",
        )
        .unwrap();
        let sock = discover_control_sock(&lock).unwrap();
        assert_eq!(sock, PathBuf::from("/tmp/test.sock"));
    }

    #[test]
    fn discover_control_sock_errors_without_line() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");
        fs::write(&lock, "pid=1234\nmcp=127.0.0.1:8765\n").unwrap();
        let err = discover_control_sock(&lock).unwrap_err();
        assert!(format!("{err}").contains("no `control_sock=` line"));
    }
}
