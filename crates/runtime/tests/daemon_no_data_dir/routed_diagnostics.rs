// SPDX-License-Identifier: Apache-2.0

//! `doctor.gather` and `diagnose.run` act on the library their
//! `library` parameter names, and on the registry's current default
//! when it is absent.
//!
//! Both methods gather data that belongs to one library — store rows,
//! open runs, free disk; the catalog snapshot, the audit overlay, the
//! bundle's own landing place — so a daemon serving several libraries
//! must let the caller say which. The negative case is the registry's
//! own: a name it does not hold is `-32010`. The default case is pinned
//! by moving the pointer: after `library.set_default`, an unnamed call
//! follows the pointer rather than the library the daemon was brought
//! up under.
//!
//! The registry env is pinned to a per-binary tempdir, so the test
//! never touches the user's real registry file. The embedder probe
//! daemon bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is required.

#![cfg(unix)]

use std::path::Path;

use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, process_env};
use eyre::{Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, connect, join_with_deadline, recv, send};

const INVALID_LIBRARY: i64 = -32010;

async fn call(
    writer: &mut Writer,
    reader: &mut Reader,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    send(writer, &frame.to_string()).await?;
    recv(reader).await
}

/// The `data root` row of a `doctor.gather` report.
fn data_root_value(report: &Value) -> Result<String> {
    report["rows"]
        .as_array()
        .ok_or_else(|| eyre!("no rows in the report: {report}"))?
        .iter()
        .find(|row| row["label"] == "data root")
        .and_then(|row| row["value"].as_str())
        .map(str::to_string)
        .ok_or_else(|| eyre!("no data root row in the report: {report}"))
}

fn seed_root(root: &Path) -> Result<()> {
    for db in ["catalog.db", "papers_catalog.db"] {
        bookrack_catalog::Catalog::open(&root.join(db))?;
    }
    for db in ["corpus.db", "papers_corpus.db"] {
        bookrack_corpus::Corpus::open(&root.join(db))?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_two_diagnostics_act_on_the_library_named_or_defaulted() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    seed_root(&alpha)?;
    seed_root(&beta)?;
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    let runtime_root = tempfile::tempdir()?;

    let mut opts = RuntimeOpts::headless(None, Some("alpha".to_string()));
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());
    let runtime = DaemonRuntime::start(opts).await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let alpha_str = alpha.display().to_string();
    let beta_str = beta.display().to_string();
    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        // Unknown names are the registry's refusal, on both methods.
        for (id, method) in [(1, "doctor.gather"), (2, "diagnose.run")] {
            let resp = call(&mut w, &mut reader, id, method, json!({"library": "ghost"})).await?;
            assert_eq!(
                resp["error"]["code"].as_i64(),
                Some(INVALID_LIBRARY),
                "{method} must refuse a name the registry does not hold: {resp}"
            );
        }

        // Named: the report and the bundle are beta's, on a daemon
        // brought up under alpha.
        let resp = call(
            &mut w,
            &mut reader,
            3,
            "doctor.gather",
            json!({"library": "beta"}),
        )
        .await?;
        assert_eq!(
            data_root_value(&resp["result"])?,
            beta_str,
            "doctor.gather must report the named library's root: {resp}"
        );
        let resp = call(
            &mut w,
            &mut reader,
            4,
            "diagnose.run",
            json!({"library": "beta"}),
        )
        .await?;
        let out_path = resp["result"]["out_path"]
            .as_str()
            .ok_or_else(|| eyre!("no out_path: {resp}"))?;
        assert!(
            out_path.starts_with(&beta_str),
            "the bundle must land under the named library's root, got {out_path}: {resp}"
        );
        assert_eq!(
            resp["result"]["library"].as_str(),
            Some("beta"),
            "the result names the library the bundle describes: {resp}"
        );

        // Unnamed: the current default, which starts at alpha...
        let resp = call(&mut w, &mut reader, 5, "doctor.gather", Value::Null).await?;
        assert_eq!(data_root_value(&resp["result"])?, alpha_str, "{resp}");

        // ...and follows the pointer once it moves, rather than staying
        // on the library the daemon was brought up under.
        let resp = call(
            &mut w,
            &mut reader,
            6,
            "library.set_default",
            json!({"name": "beta"}),
        )
        .await?;
        assert_eq!(resp["result"]["ok"], Value::Bool(true), "{resp}");
        let resp = call(&mut w, &mut reader, 7, "doctor.gather", Value::Null).await?;
        assert_eq!(
            data_root_value(&resp["result"])?,
            beta_str,
            "an unnamed doctor.gather follows the moved default: {resp}"
        );
        let resp = call(&mut w, &mut reader, 8, "diagnose.run", Value::Null).await?;
        let out_path = resp["result"]["out_path"]
            .as_str()
            .ok_or_else(|| eyre!("no out_path: {resp}"))?;
        assert!(
            out_path.starts_with(&beta_str),
            "an unnamed diagnose.run follows the moved default, got {out_path}: {resp}"
        );

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await?;
    Ok(())
}
