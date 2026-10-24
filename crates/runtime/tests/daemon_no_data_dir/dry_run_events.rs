// SPDX-License-Identifier: Apache-2.0

//! A dry-run leg takes the write session but announces no change.
//!
//! Every dry run goes through the daemon's write mutex, so a subscriber
//! sees `mcp.availability` pause and resume around it, exactly as around
//! a write. What it must not see is a `library.changed`: the dry run
//! leaves every library as it found it, and that event tells every
//! subscriber to refresh. The execute leg minted from the same plan is
//! the write, and it still announces.
//!
//! The embedder probe daemon bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is required.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, Sandbox, process_env};
use eyre::{ContextCompat, Result};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, connect, join_with_deadline, recv, send};

/// The data roots of the two registered libraries.
struct Roots {
    #[allow(dead_code)]
    alpha: PathBuf,
}

/// Isolate the process and seed a two-library registry: `alpha` (the
/// registry default) and `beta`, each with its own data root under the
/// sandbox.
fn world() -> &'static Roots {
    static ROOTS: OnceLock<Roots> = OnceLock::new();
    let sandbox: &Sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    ROOTS.get_or_init(|| {
        let alpha = sandbox.data_root("alpha-root");
        let beta = sandbox.data_root("beta-root");
        sandbox.write_registry_entries(
            Some("alpha"),
            &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
        );
        Roots { alpha }
    })
}

async fn start(runtime_root: &Path) -> Result<DaemonRuntime> {
    let mut opts = RuntimeOpts::headless(None, Some("alpha".to_string()));
    opts.no_mcp = true;
    opts.spawn_queue_worker = true;
    opts.runtime_dir = Some(runtime_root.to_path_buf());
    DaemonRuntime::start(opts).await
}

async fn call(
    reader: &mut Reader,
    writer: &mut Writer,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    send(writer, &req.to_string()).await?;
    let resp = recv(reader).await?;
    eyre::ensure!(resp["error"].is_null(), "{method} failed: {resp}");
    Ok(resp["result"].clone())
}

/// Subscribe and drain the snapshot bundle, whose last channel is
/// `daemon.version`, so only live broadcasts follow.
async fn subscribe_live(sock: &Path) -> Result<Reader> {
    let (mut reader, mut writer) = connect(sock).await?;
    send(
        &mut writer,
        r#"{"jsonrpc":"2.0","id":1,"method":"events.subscribe"}"#,
    )
    .await?;
    let _ = recv(&mut reader).await?;
    loop {
        let frame = recv(&mut reader).await?;
        if frame["params"]["channel"].as_str() == Some("daemon.version") {
            break;
        }
    }
    Ok(reader)
}

/// A `corpus.rebuild` dry run on an empty library, then the execute
/// leg of the plan it minted. On the event stream the dry run must
/// leave exactly its `mcp.availability` pair and no `library.changed`;
/// the first `library.changed` belongs to the execute leg, so it comes
/// after three `mcp.availability` frames: the dry run's `true` and
/// `false`, and the execute leg's `true`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dry_run_pauses_mcp_but_announces_no_change() -> Result<()> {
    let _ = world();
    let runtime_root = tempfile::tempdir()?;
    let runtime = start(runtime_root.path()).await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let mut observer = subscribe_live(&sock).await?;
        let (mut reader, mut writer) = connect(&sock).await?;

        let plan = call(
            &mut reader,
            &mut writer,
            2,
            "corpus.rebuild",
            json!({ "dry_run": true, "library": "beta" }),
        )
        .await?;
        let plan_id = plan["plan_id"]
            .as_str()
            .context("the dry run returns a plan id")?
            .to_string();
        assert_eq!(plan["library"], json!("beta"), "{plan}");

        let executed = call(
            &mut reader,
            &mut writer,
            3,
            "corpus.rebuild",
            json!({ "plan_id": plan_id, "yes": true, "library": "beta" }),
        )
        .await?;
        assert_eq!(executed["library"], json!("beta"), "{executed}");

        let deadline = tokio::time::sleep(Duration::from_secs(20));
        tokio::pin!(deadline);
        let mut availability: Vec<bool> = Vec::new();
        let changed = loop {
            tokio::select! {
                _ = &mut deadline => panic!(
                    "no library.changed after the execute leg; mcp.availability so far: \
                     {availability:?}"
                ),
                frame = recv(&mut observer) => {
                    let frame = frame?;
                    match frame["params"]["channel"].as_str() {
                        Some("mcp.availability") => availability.push(
                            frame["params"]["value"]["paused"]
                                .as_bool()
                                .context("mcp.availability carries paused")?,
                        ),
                        Some("library.changed") => break frame["params"]["value"].clone(),
                        _ => {}
                    }
                }
            }
        };
        assert_eq!(
            availability,
            vec![true, false, true],
            "the dry run must pause and resume MCP, and the first library.changed must \
             belong to the execute leg; the mcp.availability frames before it were \
             {availability:?} and the event carried {changed}",
        );
        assert_eq!(changed["library"], json!("beta"), "{changed}");

        send(
            &mut writer,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}
