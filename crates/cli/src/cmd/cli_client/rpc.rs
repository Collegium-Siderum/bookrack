// SPDX-License-Identifier: Apache-2.0

//! `bookrack rpc` — the control-plane escape hatch.
//!
//! Two actions, both implemented entirely on top of the control plane:
//!
//! - `list`: the method table the running daemon answers — the call
//!   surface for `rpc call` — alongside the MCP endpoint tools for
//!   visibility. Both rows come from the `daemon.methods` and
//!   `daemon.mcp_tools` RPCs.
//! - `call <method> [<params-json>]`: any control-plane method name
//!   (e.g. `library.show_book`, `library.search`,
//!   `library.show_metadata_audit`). The optional second argument is
//!   the JSON params object; defaults to `null` when omitted. The
//!   `daemon.methods` row set is the source of truth for available
//!   method names; the MCP endpoint tools shown by `list` are not
//!   callable through this surface, though the `library.*` read
//!   proxies share a name with their MCP counterparts.
//!
//! Both output paths honour the global `--json` and `--quiet` flags
//! installed in `bookrack_cli::render::ctx()`: the RPCs still run on
//! `--quiet` so a missing daemon surfaces as an error instead of a
//! silent no-op; only the print step is suppressed.

use std::path::Path;

use bookrack_cli::error::BookrackCliError;
use bookrack_cli::render::ctx;
use bookrack_cli_grammar::RpcAction;
use eyre::Result;
use serde_json::{Value, json};

use super::helpers;

pub async fn run(action: &RpcAction, runtime_dir: Option<&Path>) -> Result<()> {
    match action {
        RpcAction::List => list(runtime_dir).await,
        RpcAction::Call { method, params } => call(method, params.as_deref(), runtime_dir).await,
    }
}

async fn call(method: &str, params: Option<&str>, runtime_dir: Option<&Path>) -> Result<()> {
    let payload = match params {
        Some(raw) => {
            serde_json::from_str::<Value>(raw).map_err(|e| BookrackCliError::RpcParamsInvalid {
                method: method.to_string(),
                detail: e.to_string(),
            })?
        }
        None => Value::Null,
    };
    let client = helpers::connect(runtime_dir).await?;
    let value = helpers::dispatch(&client, method, payload).await?;
    helpers::print_value(&value);
    Ok(())
}

async fn list(runtime_dir: Option<&Path>) -> Result<()> {
    let client = helpers::connect(runtime_dir).await?;
    let methods = helpers::dispatch(&client, "daemon.methods", Value::Null).await?;
    let mcp = helpers::dispatch(&client, "daemon.mcp_tools", Value::Null).await?;
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
    println!(
        "MCP endpoint tools (visibility only; `bookrack rpc call` calls control-plane methods):"
    );
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
