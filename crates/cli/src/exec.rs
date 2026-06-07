// SPDX-License-Identifier: Apache-2.0

//! `bookrack exec` — talk to the live daemon session over MCP.
//!
//! Reads `${BOOKRACK_RUNTIME_DIR}/bookrack.tty.lock` to learn what
//! session is running (pid, MCP listener address). Sub-commands:
//!
//! - `info` (default): print the session — pid, MCP address, lock
//!   path. Pure file read.
//! - `tools`: open an MCP client connection and run `tools/list`
//!   against the live server, so the operator sees the current tool
//!   surface, not a stale compile-time slice.
//! - `library.<name> [<json>]`: open an MCP client connection and
//!   call the named tool with `arguments`. Argument parsing is plain
//!   JSON forwarding: the second positional token is the input
//!   object the tool's schema expects, so the daemon owns input
//!   validation and `bookrack exec` does not duplicate any tool
//!   schema.
//!
//! `bookrack exec` never opens a catalog, corpus, or vector store —
//! the "no DB handle outside the scheduler" invariant is what gives
//! the daemon-REPL session its single-writer guarantee.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

use crate::run::{resolve_runtime_dir, tty_lock_name};

/// Run a `bookrack exec` invocation.
///
/// Sub-commands (positional, no flags):
///
/// - `info` (default): print the active session — pid, MCP address,
///   lock path. Errors if no session is running.
/// - `tools`: open a live MCP connection and run `tools/list`.
/// - `library.<name> [<json-object>]`: call the named MCP tool with
///   the second positional token as raw JSON arguments. `{}` and
///   omitted-args both encode as `arguments: None` on the wire.
pub async fn run(args: &[String], runtime_dir_override: Option<&Path>) -> Result<()> {
    let runtime_dir = resolve_runtime_dir(runtime_dir_override)
        .context("resolve BOOKRACK_RUNTIME_DIR for `bookrack exec`")?;
    let lock_path = runtime_dir.join(tty_lock_name());

    let subcmd = args.first().map(String::as_str).unwrap_or("info");
    match subcmd {
        "info" => print_info(&lock_path),
        "tools" => print_tools_live(&lock_path).await,
        name if name.starts_with("library.") => {
            // Parse the optional JSON-object argument before connecting
            // so a syntax error stays local and does not waste an MCP
            // round-trip.
            let arguments = match args.get(1).map(String::as_str) {
                None | Some("") => serde_json::Value::Null,
                Some(raw) => serde_json::from_str::<serde_json::Value>(raw)
                    .with_context(|| format!("parse JSON arguments for `{name}`: {raw}"))?,
            };
            let mcp = require_mcp_addr(&lock_path)?;
            let result = call_tool(&mcp, name, arguments).await?;
            print_tool_result(&result)
        }
        other => {
            bail!(
                "bookrack exec: unknown subcommand `{other}`; \
                 expected `info`, `tools`, or `library.<tool> [<json>]`"
            )
        }
    }
}

/// Read the active MCP address from the session lock, or error with
/// a message telling the operator to start the daemon.
fn require_mcp_addr(lock_path: &Path) -> Result<String> {
    match read_lock_info(lock_path)? {
        None => bail!(
            "no bookrack session running (lock {} absent); \
             start one with `bookrack run`",
            lock_path.display()
        ),
        Some(info) => match info.mcp {
            Some(addr) if addr != "disabled" => Ok(addr),
            Some(_) => bail!(
                "the running session has MCP disabled; \
                 restart `bookrack run` without --no-mcp"
            ),
            None => bail!(
                "the session lock at {} carries no MCP address; \
                 restart `bookrack run`",
                lock_path.display()
            ),
        },
    }
}

/// Parsed view of a session lock file. Either field is `None` when
/// the running daemon wrote a lock file in an older format or when
/// the field's value did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LockInfo {
    pub(crate) pid: Option<u32>,
    pub(crate) mcp: Option<String>,
}

/// Parse a session-lock file body. Format is line-oriented
/// `key=value` lines (see `crates/cli/src/run.rs` `TtyLock::acquire`);
/// unknown lines are tolerated so a future field addition does not
/// break older `bookrack exec` binaries.
pub(crate) fn parse_lock(text: &str) -> LockInfo {
    let mut pid = None;
    let mut mcp = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "pid" => pid = value.trim().parse::<u32>().ok(),
                "mcp" => mcp = Some(value.trim().to_string()),
                _ => {}
            }
        }
    }
    LockInfo { pid, mcp }
}

fn read_lock_info(path: &Path) -> Result<Option<LockInfo>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("read session lock {}", path.display()))?;
    Ok(Some(parse_lock(&text)))
}

fn print_info(lock_path: &Path) -> Result<()> {
    match read_lock_info(lock_path)? {
        None => {
            bail!(
                "no bookrack session running (lock {} absent); \
                 start one with `bookrack run`",
                lock_path.display()
            )
        }
        Some(info) => {
            println!("session");
            println!(
                "  pid       {}",
                info.pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            );
            println!("  mcp       {}", info.mcp.as_deref().unwrap_or("unknown"));
            println!("  lock      {}", lock_path.display());
            println!();
            println!("Connect an MCP client to http://<mcp>/mcp.");
            println!("Run `bookrack exec tools` to list available tool names.");
            Ok(())
        }
    }
}

/// Hit the live daemon's `tools/list` and render the server's
/// authoritative tool slice. Replaces the previous compile-time
/// `TOOL_NAMES` const so the operator never sees a drifted list.
async fn print_tools_live(lock_path: &Path) -> Result<()> {
    let mcp = require_mcp_addr(lock_path)?;
    let transport = StreamableHttpClientTransport::with_client(
        reqwest::Client::new(),
        StreamableHttpClientTransportConfig::with_uri(format!("http://{mcp}/mcp")),
    );
    let client = ().serve(transport).await.context("connect MCP streamable-HTTP transport")?;
    let tools = client.list_tools(None).await.context("list MCP tools")?;
    let _ = client.cancel().await;
    println!("MCP endpoint: http://{mcp}/mcp");
    println!();
    println!("Tools:");
    for tool in tools.tools {
        println!("  {}", tool.name);
    }
    Ok(())
}

/// Open a streamable-HTTP MCP session against the daemon listening at
/// `mcp_addr`, call the named tool with `arguments`, and return the
/// server's [`CallToolResult`]. `arguments` is the raw JSON object the
/// tool's input schema expects; `serde_json::Value::Null` and an empty
/// object both encode to `arguments: None` on the wire.
///
/// The transport's `mcp-session-id` header is managed by rmcp's
/// `LocalSessionManager`; this helper has no session bookkeeping of
/// its own. The connection is cancelled before returning so the
/// server's per-session state drops immediately.
async fn call_tool(
    mcp_addr: &str,
    name: &str,
    arguments: serde_json::Value,
) -> Result<CallToolResult> {
    let arguments = match arguments {
        serde_json::Value::Object(map) => Some(map),
        serde_json::Value::Null => None,
        other => bail!("tool arguments must be a JSON object, got {other}"),
    };
    let transport = StreamableHttpClientTransport::with_client(
        reqwest::Client::new(),
        StreamableHttpClientTransportConfig::with_uri(format!("http://{mcp_addr}/mcp")),
    );
    let client = ().serve(transport).await.context("connect MCP streamable-HTTP transport")?;
    let mut req = CallToolRequestParams::new(name.to_string());
    req.arguments = arguments;
    let result = client
        .call_tool(req)
        .await
        .with_context(|| format!("call MCP tool `{name}`"))?;
    let _ = client.cancel().await;
    Ok(result)
}

/// Render a [`CallToolResult`] to stdout. Text content is printed
/// verbatim (the server controls formatting); other content kinds
/// are flagged so the operator notices when the server returns
/// something `bookrack exec` cannot render yet. An `is_error` result
/// surfaces as an `Err` so shell-level `&&` chaining stops on a
/// tool-side failure.
fn print_tool_result(result: &CallToolResult) -> Result<()> {
    for content in &result.content {
        if let Some(text) = content.as_text() {
            println!("{}", text.text);
        } else {
            eprintln!("bookrack exec: tool returned non-text content; ignoring");
        }
    }
    if result.is_error.unwrap_or(false) {
        bail!("the MCP tool returned an error result");
    }
    Ok(())
}

/// Helper used in tests to keep the lock-file path construction next
/// to its readers. Production code never calls this directly.
#[cfg(test)]
fn lock_path_for(runtime_dir: &Path) -> std::path::PathBuf {
    runtime_dir.join(tty_lock_name())
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    #[test]
    fn parse_lock_extracts_pid_and_mcp() {
        let info = parse_lock("pid=4242\nmcp=127.0.0.1:8765\n");
        assert_eq!(info.pid, Some(4242));
        assert_eq!(info.mcp.as_deref(), Some("127.0.0.1:8765"));
    }

    #[test]
    fn parse_lock_tolerates_unknown_lines() {
        let info = parse_lock("pid=12\nmcp=disabled\nfuture-field=hello\n");
        assert_eq!(info.pid, Some(12));
        assert_eq!(info.mcp.as_deref(), Some("disabled"));
    }

    #[test]
    fn parse_lock_recovers_each_field_independently() {
        // pid line is malformed — mcp must still parse out.
        let info = parse_lock("pid=not-a-number\nmcp=127.0.0.1:9090\n");
        assert!(info.pid.is_none());
        assert_eq!(info.mcp.as_deref(), Some("127.0.0.1:9090"));
    }

    #[tokio::test]
    async fn run_info_errors_when_no_session_lock_exists() {
        let dir = tempdir().unwrap();
        let err = run(&[], Some(dir.path()))
            .await
            .expect_err("expected error");
        let msg = err.to_string();
        assert!(msg.contains("no bookrack session running"), "got: {msg}");
        assert!(msg.contains(dir.path().to_string_lossy().as_ref()));
    }

    #[tokio::test]
    async fn run_info_succeeds_when_lock_is_present() {
        let dir = tempdir().unwrap();
        let lock_path = lock_path_for(dir.path());
        fs::write(&lock_path, "pid=1234\nmcp=127.0.0.1:8765\n").unwrap();
        run(&["info".to_string()], Some(dir.path())).await.unwrap();
    }

    #[tokio::test]
    async fn run_tools_without_session_lock_points_at_bookrack_run() {
        // The previous static-list behaviour is gone; without a daemon
        // the live `tools/list` cannot run and the operator gets the
        // same "start `bookrack run`" hint as every other live path.
        let dir = tempdir().unwrap();
        let err = run(&["tools".to_string()], Some(dir.path()))
            .await
            .expect_err("expected error");
        assert!(
            err.to_string().contains("bookrack run"),
            "expected bookrack-run hint, got: {err}"
        );
    }

    #[tokio::test]
    async fn run_rejects_unknown_subcommand() {
        let dir = tempdir().unwrap();
        let err = run(&["ghost".to_string()], Some(dir.path()))
            .await
            .expect_err("expected error");
        assert!(err.to_string().contains("unknown subcommand"));
    }

    #[tokio::test]
    async fn library_tool_call_without_session_points_at_bookrack_run() {
        let dir = tempdir().unwrap();
        let err = run(
            &["library.info".to_string(), "{}".to_string()],
            Some(dir.path()),
        )
        .await
        .expect_err("expected error");
        assert!(
            err.to_string().contains("bookrack run"),
            "expected bookrack-run hint, got: {err}"
        );
    }

    #[tokio::test]
    async fn library_tool_call_rejects_invalid_json_arguments_locally() {
        // JSON syntax validation must happen before a transport is
        // opened: even with no daemon running, a malformed argument
        // surfaces a parse error, not a connection failure.
        let dir = tempdir().unwrap();
        let err = run(
            &["library.search".to_string(), "{not-json".to_string()],
            Some(dir.path()),
        )
        .await
        .expect_err("expected error");
        let msg = err.to_string();
        assert!(
            msg.contains("parse JSON arguments"),
            "expected JSON parse error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn library_tool_call_rejects_non_object_json_locally() {
        let dir = tempdir().unwrap();
        // Even with a fake lock pointing at an unreachable address,
        // the array-shaped argument is rejected before the transport
        // opens.
        let lock_path = lock_path_for(dir.path());
        fs::write(&lock_path, "pid=1\nmcp=127.0.0.1:1\n").unwrap();
        let err = run(
            &["library.search".to_string(), "[1,2,3]".to_string()],
            Some(dir.path()),
        )
        .await
        .expect_err("expected error");
        let msg = err.to_string();
        assert!(
            msg.contains("must be a JSON object"),
            "expected non-object rejection, got: {msg}"
        );
    }

    #[tokio::test]
    async fn library_tool_call_rejects_disabled_mcp() {
        let dir = tempdir().unwrap();
        let lock_path = lock_path_for(dir.path());
        fs::write(&lock_path, "pid=1\nmcp=disabled\n").unwrap();
        let err = run(
            &["library.info".to_string(), "{}".to_string()],
            Some(dir.path()),
        )
        .await
        .expect_err("expected error");
        let msg = err.to_string();
        assert!(
            msg.contains("MCP disabled"),
            "expected disabled-MCP error, got: {msg}"
        );
    }
}
