// SPDX-License-Identifier: Apache-2.0

//! `bookrack exec` — discover and probe the live daemon session.
//!
//! Reads `${BOOKRACK_RUNTIME_DIR}/bookrack.tty.lock` to learn what
//! session is running (pid, MCP listener address) and surfaces it to
//! the operator. The full MCP HTTP client wiring lands in a follow-up
//! commit; this command's current scope is *discovery*: it never
//! opens a database, never opens a process, and never makes an HTTP
//! call — so it stays useful while the daemon-REPL grows its protocol
//! surface, and so the "no DB handle outside the scheduler" invariant
//! holds before the broader CLI face is reshaped.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

use crate::run::{resolve_runtime_dir, tty_lock_name};

/// MCP tool names this binary's `bookrack-mcp` exposes today. Listed
/// statically so `bookrack exec tools` works without opening a session
/// or making an HTTP call; the canonical list is mirrored in
/// [`crates/mcp/src/lib.rs`](../../mcp/src/lib.rs) and changes there
/// must keep this slice in sync.
const TOOL_NAMES: &[&str] = &[
    "library.search",
    "library.search_in_book",
    "library.stats",
    "library.list_books",
    "library.find_books",
    "library.show_book",
    "library.show_toc",
    "library.show_metadata_audit",
    "library.list_pending_reviews",
    "library.show_audit_trail",
    "library.show_pipeline_trail",
    "library.info",
    "library.metadata.set",
    "library.metadata.clear",
    "library.metadata.ack",
    "library.metadata.approve",
    "library.metadata.reject",
];

/// Run a `bookrack exec` invocation.
///
/// Sub-commands (positional, no flags):
///
/// - `info` (default): print the active session — pid, MCP address,
///   lock path. Errors if no session is running.
/// - `tools`: list MCP tools the bookrack server exposes. Does not
///   require a running session.
pub fn run(args: &[String], runtime_dir_override: Option<&Path>) -> Result<()> {
    let runtime_dir = resolve_runtime_dir(runtime_dir_override)
        .context("resolve BOOKRACK_RUNTIME_DIR for `bookrack exec`")?;
    let lock_path = runtime_dir.join(tty_lock_name());

    let subcmd = args.first().map(String::as_str).unwrap_or("info");
    match subcmd {
        "info" => print_info(&lock_path),
        "tools" => {
            print_tools(&lock_path);
            Ok(())
        }
        other => {
            bail!(
                "bookrack exec: unknown subcommand `{other}`; \
                 known subcommands: `info`, `tools`"
            )
        }
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

fn print_tools(lock_path: &Path) {
    if let Ok(Some(info)) = read_lock_info(lock_path)
        && let Some(mcp) = info.mcp.as_deref()
    {
        println!("MCP endpoint: http://{mcp}/mcp");
        println!();
    }
    println!("Tools:");
    for name in TOOL_NAMES {
        println!("  {name}");
    }
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
#[allow(dead_code)] // wired by B2 (argv -> JSON dispatch)
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

    #[test]
    fn run_info_errors_when_no_session_lock_exists() {
        let dir = tempdir().unwrap();
        let err = run(&[], Some(dir.path())).expect_err("expected error");
        let msg = err.to_string();
        assert!(msg.contains("no bookrack session running"), "got: {msg}");
        assert!(msg.contains(dir.path().to_string_lossy().as_ref()));
    }

    #[test]
    fn run_info_succeeds_when_lock_is_present() {
        let dir = tempdir().unwrap();
        let lock_path = lock_path_for(dir.path());
        fs::write(&lock_path, "pid=1234\nmcp=127.0.0.1:8765\n").unwrap();
        run(&["info".to_string()], Some(dir.path())).unwrap();
    }

    #[test]
    fn run_tools_succeeds_without_session() {
        let dir = tempdir().unwrap();
        run(&["tools".to_string()], Some(dir.path())).unwrap();
    }

    #[test]
    fn run_rejects_unknown_subcommand() {
        let dir = tempdir().unwrap();
        let err = run(&["ghost".to_string()], Some(dir.path())).expect_err("expected error");
        assert!(err.to_string().contains("unknown subcommand"));
    }
}
