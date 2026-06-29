// SPDX-License-Identifier: Apache-2.0

//! `bookrack exec` — read-side discovery surface for a running
//! daemon, implemented entirely on top of the control plane.
//!
//! Sub-commands:
//!
//! - `info` (default): print the active session — pid, MCP address,
//!   control socket, lock path. Pure file read of the session lock;
//!   never opens the control socket.
//! - `tools`: list the control-plane methods the daemon answers — the
//!   call surface for `bookrack exec <method>` — alongside the MCP
//!   endpoint tools for visibility. Both rows come from the
//!   `daemon.methods` and `daemon.mcp_tools` RPCs.
//! - `logs follow`: stream every tracing event the daemon emits via
//!   the control-plane `log` channel until the daemon shuts down or
//!   the client is interrupted.
//! - `logs tail [<n>]`: subscribe to the same broadcast and emit the
//!   next `n` live log events (defaults to 100). Distinct from the
//!   top-level `bookrack logs --tail`, which snapshots historical
//!   events via the `logs.tail` RPC.
//! - `<method> [<params-json>]`: any control-plane method name
//!   containing a `.` (e.g. `library.show_book`, `library.search`,
//!   `library.show_metadata_audit`). The optional second argument is
//!   the JSON params object; defaults to `null` when omitted. The
//!   `daemon.methods` row set is the source of truth for available
//!   method names; the MCP endpoint tools shown by `tools` are not
//!   callable through this surface, though the `library.*` read
//!   proxies share a name with their MCP counterparts.
//!
//! All human-facing output paths honour the global `--json` and
//! `--quiet` flags installed in `bookrack_cli::render::ctx()`: RPCs,
//! lock peeks, and broadcast subscriptions still run on `--quiet` so
//! a missing daemon surfaces as an error instead of a silent no-op;
//! only the print step is suppressed.

use std::path::Path;

use bookrack_cli::render::ctx;
use bookrack_obs::stream::LogEvent;
use bookrack_session::{LockInfo, peek_lock, resolve_runtime_dir, tty_lock_name};
use eyre::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::cmd::cli_client::{helpers, logs as logs_cmd};

pub async fn run(args: &[String], runtime_dir_override: Option<&Path>) -> Result<()> {
    let runtime_dir = resolve_runtime_dir(runtime_dir_override)
        .context("resolve BOOKRACK_RUNTIME_DIR for `bookrack exec`")?;
    let lock_path = runtime_dir.join(tty_lock_name());

    let subcmd = args.first().map(String::as_str).unwrap_or("info");
    match subcmd {
        "info" => print_info(&lock_path),
        "tools" => print_tools().await,
        "logs" => run_logs(&args[1..]).await,
        method if method.contains('.') => call_method(method, &args[1..]).await,
        other => {
            bail!(
                "bookrack exec: unknown subcommand `{other}`; expected `info`, `tools`, `logs`, \
                 or a control-plane method name (e.g. `library.show_book`). Run \
                 `bookrack exec tools` for the full method list."
            )
        }
    }
}

async fn call_method(method: &str, params: &[String]) -> Result<()> {
    let payload = match params.first() {
        Some(raw) => serde_json::from_str::<Value>(raw)
            .with_context(|| format!("parse params for `{method}` as JSON"))?,
        None => Value::Null,
    };
    let client = helpers::connect(None).await?;
    let value = client
        .call_raw(method, payload)
        .await
        .with_context(|| format!("{method} rpc"))?;
    helpers::print_value(&value);
    Ok(())
}

fn print_info(lock_path: &Path) -> Result<()> {
    let info = peek_lock(lock_path)?;
    let ctx = ctx();
    if ctx.is_quiet() {
        return Ok(());
    }
    if ctx.is_json() {
        helpers::print_value(&info_to_json(lock_path, info.as_ref()));
        return Ok(());
    }
    println!("lock      {}", lock_path.display());
    match info {
        None => {
            println!("pid       (lock file does not exist — no running daemon)");
            println!("mcp       (lock file does not exist)");
            println!("control   (lock file does not exist)");
        }
        Some(info) => {
            println!("pid       {}", info.pid);
            println!("mcp       {}", info.mcp);
            println!(
                "control   {}",
                info.control_sock
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(missing — daemon predates Phase 1)".to_string())
            );
        }
    }
    Ok(())
}

fn info_to_json(lock_path: &Path, info: Option<&LockInfo>) -> Value {
    match info {
        None => json!({
            "lock": lock_path.display().to_string(),
            "pid": Value::Null,
            "mcp": Value::Null,
            "control": Value::Null,
        }),
        Some(info) => json!({
            "lock": lock_path.display().to_string(),
            "pid": info.pid,
            "mcp": info.mcp,
            "control": info
                .control_sock
                .as_deref()
                .map(|p| Value::String(p.display().to_string()))
                .unwrap_or(Value::Null),
        }),
    }
}

async fn print_tools() -> Result<()> {
    let client = helpers::connect(None).await?;
    let methods = client
        .call_raw("daemon.methods", Value::Null)
        .await
        .context("daemon.methods rpc")?;
    let mcp = client
        .call_raw("daemon.mcp_tools", Value::Null)
        .await
        .context("daemon.mcp_tools rpc")?;
    let ctx = ctx();
    if ctx.is_quiet() {
        return Ok(());
    }
    if ctx.is_json() {
        let payload = json!({
            "control_methods": methods.get("methods").cloned().unwrap_or(Value::Array(vec![])),
            "mcp_tools": mcp.get("tools").cloned().unwrap_or(Value::Array(vec![])),
        });
        helpers::print_value(&payload);
        return Ok(());
    }
    println!("Control-plane methods:");
    if let Some(rows) = methods.get("methods").and_then(Value::as_array) {
        for row in rows {
            let name = row.get("name").and_then(Value::as_str).unwrap_or("?");
            let kind = row.get("kind").and_then(Value::as_str).unwrap_or("?");
            println!("  {kind:<6}  {name}");
        }
    }
    println!();
    println!("MCP endpoint tools (visibility only; `bookrack exec` calls control-plane methods):");
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
    let client = helpers::connect(None).await?;
    let mut events = client
        .subscribe()
        .await
        .context("subscribe to control-plane events")?;
    loop {
        match events.recv().await {
            Ok(event) if event.channel == "log" => {
                if let Ok(ev) = serde_json::from_value::<LogEvent>(event.value) {
                    logs_cmd::emit_event(&ev, None);
                }
            }
            Ok(_) => continue,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

async fn tail_logs(limit: u64) -> Result<()> {
    let client = helpers::connect(None).await?;
    let mut events = client
        .subscribe()
        .await
        .context("subscribe to control-plane events")?;
    let mut emitted = 0u64;
    while emitted < limit {
        match events.recv().await {
            Ok(event) if event.channel == "log" => {
                if let Ok(ev) = serde_json::from_value::<LogEvent>(event.value) {
                    logs_cmd::emit_event(&ev, None);
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
