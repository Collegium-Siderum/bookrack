// SPDX-License-Identifier: Apache-2.0

//! `bookrack exec` — read-side discovery surface for a running
//! daemon, implemented entirely on top of the control plane.
//!
//! Sub-commands:
//!
//! - `info` (default): print the active session — pid, MCP address,
//!   control socket, lock path. Pure file read of the session lock;
//!   never opens the control socket.
//! - `tools`: list the control-plane methods the daemon answers plus
//!   the MCP tools the same daemon exposes. Both rows come from the
//!   `daemon.methods` and `daemon.mcp_tools` RPCs.
//! - `logs follow`: stream every tracing event the daemon emits via
//!   the control-plane `log` channel until the daemon shuts down or
//!   the client is interrupted.
//! - `logs tail [<n>]`: print up to `n` recent log events
//!   (defaults to 100) off the same broadcast.

use std::path::Path;

use anyhow::{Context, Result, bail};
use bookrack_session::{resolve_runtime_dir, tty_lock_name};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::cmd::cli_client::helpers;

pub async fn run(args: &[String], runtime_dir_override: Option<&Path>) -> Result<()> {
    let runtime_dir = resolve_runtime_dir(runtime_dir_override)
        .context("resolve BOOKRACK_RUNTIME_DIR for `bookrack exec`")?;
    let lock_path = runtime_dir.join(tty_lock_name());

    let subcmd = args.first().map(String::as_str).unwrap_or("info");
    match subcmd {
        "info" => print_info(&lock_path),
        "tools" => print_tools().await,
        "logs" => run_logs(&args[1..]).await,
        other => {
            bail!(
                "bookrack exec: unknown subcommand `{other}`; expected `info`, `tools`, or `logs`",
            )
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct LockInfo {
    pub(crate) pid: Option<String>,
    pub(crate) mcp: Option<String>,
    pub(crate) control_sock: Option<String>,
}

/// Parse a lock-file body into [`LockInfo`]. Unknown keys are ignored
/// so future schema additions stay backward-compatible.
pub(crate) fn parse_lock(raw: &str) -> LockInfo {
    let mut info = LockInfo::default();
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("pid=") {
            info.pid = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("mcp=") {
            info.mcp = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("control_sock=") {
            info.control_sock = Some(value.to_string());
        }
    }
    info
}

fn read_lock_info(lock_path: &Path) -> Result<LockInfo> {
    let raw = std::fs::read_to_string(lock_path)
        .with_context(|| format!("read session lock at {}", lock_path.display()))?;
    Ok(parse_lock(&raw))
}

fn print_info(lock_path: &Path) -> Result<()> {
    let info = read_lock_info(lock_path)?;
    println!("lock      {}", lock_path.display());
    println!(
        "pid       {}",
        info.pid.as_deref().unwrap_or("(missing in lock)")
    );
    println!(
        "mcp       {}",
        info.mcp
            .as_deref()
            .unwrap_or("(none — daemon ran with --no-mcp)")
    );
    println!(
        "control   {}",
        info.control_sock
            .as_deref()
            .unwrap_or("(missing — daemon predates Phase 1)")
    );
    Ok(())
}

async fn print_tools() -> Result<()> {
    let client = helpers::connect_or_exit(None).await;
    let methods = client
        .call_raw("daemon.methods", Value::Null)
        .await
        .context("daemon.methods rpc")?;
    let mcp = client
        .call_raw("daemon.mcp_tools", Value::Null)
        .await
        .context("daemon.mcp_tools rpc")?;
    println!("Control-plane methods:");
    if let Some(rows) = methods.get("methods").and_then(Value::as_array) {
        for row in rows {
            let name = row.get("name").and_then(Value::as_str).unwrap_or("?");
            let kind = row.get("kind").and_then(Value::as_str).unwrap_or("?");
            println!("  {kind:<6}  {name}");
        }
    }
    println!();
    println!("MCP tools:");
    if let Some(rows) = mcp.get("tools").and_then(Value::as_array) {
        for row in rows {
            let name = row.get("name").and_then(Value::as_str).unwrap_or("?");
            let description = row.get("description").and_then(Value::as_str).unwrap_or("");
            println!("  {name}");
            if !description.is_empty() {
                println!("    {description}");
            }
        }
    }
    Ok(())
}

async fn run_logs(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("follow") | None => follow_logs().await,
        Some("tail") => {
            let limit = args
                .get(1)
                .map(|s| {
                    s.parse::<u64>()
                        .with_context(|| format!("parse logs tail limit `{s}`"))
                })
                .transpose()?
                .unwrap_or(100);
            tail_logs(limit).await
        }
        Some(other) => bail!(
            "bookrack exec logs: unknown sub-command `{other}`; expected `follow` or `tail [<n>]`"
        ),
    }
}

async fn follow_logs() -> Result<()> {
    let client = helpers::connect_or_exit(None).await;
    let mut events = client
        .subscribe()
        .await
        .context("subscribe to control-plane events")?;
    loop {
        match events.recv().await {
            Ok(event) if event.channel == "log" => {
                if let Ok(text) = serde_json::to_string(&event.value) {
                    println!("{text}");
                }
            }
            Ok(_) => continue,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

async fn tail_logs(limit: u64) -> Result<()> {
    let client = helpers::connect_or_exit(None).await;
    let mut events = client
        .subscribe()
        .await
        .context("subscribe to control-plane events")?;
    let mut emitted = 0u64;
    while emitted < limit {
        match events.recv().await {
            Ok(event) if event.channel == "log" => {
                if let Ok(text) = serde_json::to_string(&event.value) {
                    println!("{text}");
                }
                emitted += 1;
            }
            Ok(_) => continue,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_lock_info_parses_lines() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let lock = tmp.path().join("bookrack.tty.lock");
        std::fs::write(
            &lock,
            "pid=4242\nmcp=127.0.0.1:8765\ncontrol_sock=/tmp/x.sock\n",
        )?;
        let info = read_lock_info(&lock)?;
        assert_eq!(info.pid.as_deref(), Some("4242"));
        assert_eq!(info.mcp.as_deref(), Some("127.0.0.1:8765"));
        assert_eq!(info.control_sock.as_deref(), Some("/tmp/x.sock"));
        Ok(())
    }

    #[test]
    fn read_lock_info_tolerates_unknown_lines() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let lock = tmp.path().join("bookrack.tty.lock");
        std::fs::write(&lock, "pid=1\nfuture_key=ignored\nmcp=:0\n")?;
        let info = read_lock_info(&lock)?;
        assert_eq!(info.pid.as_deref(), Some("1"));
        assert_eq!(info.mcp.as_deref(), Some(":0"));
        assert!(info.control_sock.is_none());
        Ok(())
    }
}
