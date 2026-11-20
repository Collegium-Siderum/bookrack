// SPDX-License-Identifier: Apache-2.0

//! `daemon.config` reports the configuration the running daemon uses:
//! the process rows as resolved at start, and each mounted library's
//! rows as resolved when it was mounted.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use bookrack_config::{DaemonConfig, DaemonFlags, EnvSnapshot};
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, Sandbox, process_env};
use eyre::{Result, eyre};
use serde_json::Value;

use crate::common::{Reader, Writer, connect, join_with_deadline, recv, send};

/// Two initialized roots, both registered, `alpha` the default.
fn world() -> (&'static Sandbox, PathBuf, PathBuf) {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    for root in [&alpha, &beta] {
        for db in ["catalog.db", "papers_catalog.db"] {
            bookrack_catalog::Catalog::open_for_write_unbacked(&root.join(db))
                .expect("seed catalog");
        }
        for db in ["corpus.db", "papers_corpus.db"] {
            bookrack_corpus::Corpus::open_for_write(&root.join(db)).expect("seed corpus");
        }
    }
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    (sandbox, alpha, beta)
}

fn top_k(root: &Path, value: usize) {
    std::fs::write(
        root.join("config.toml"),
        format!("[search]\ntop_k = {value}\n"),
    )
    .expect("write config.toml");
}

/// A daemon over the registry, resolved from this process's environment
/// with `extra` on top.
async fn start(runtime_root: &Path, extra: &[(&str, &str)]) -> Result<DaemonRuntime> {
    let mut env = EnvSnapshot::capture();
    for (name, value) in extra {
        env = env.with_var(*name, *value);
    }
    let mut opts = RuntimeOpts::headless(None, Some("alpha".to_string()));
    opts.daemon_config = DaemonConfig::resolve(
        &env,
        &DaemonFlags {
            no_mcp: true,
            ..DaemonFlags::default()
        },
    );
    opts.spawn_queue_worker = true;
    opts.runtime_dir = Some(runtime_root.to_path_buf());
    DaemonRuntime::start(opts).await
}

async fn call(
    reader: &mut Reader,
    w: &mut Writer,
    id: u32,
    method: &str,
    params: Value,
) -> Result<Value> {
    send(
        w,
        &serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
            .to_string(),
    )
    .await?;
    let resp = recv(reader).await?;
    if !resp["error"].is_null() {
        return Err(eyre!("{method} failed: {resp}"));
    }
    Ok(resp["result"].clone())
}

fn row<'a>(rows: &'a Value, key: &str) -> &'a Value {
    rows.as_array()
        .and_then(|rows| rows.iter().find(|row| row["key"] == key))
        .unwrap_or_else(|| panic!("no {key} row in {rows}"))
}

fn library<'a>(config: &'a Value, name: &str) -> Option<&'a Value> {
    config["libraries"]
        .as_array()
        .and_then(|libraries| libraries.iter().find(|library| library["name"] == name))
}

fn keys(rows: &Value) -> BTreeSet<String> {
    rows.as_array()
        .expect("rows")
        .iter()
        .map(|row| row["key"].as_str().expect("key").to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_config_reports_the_environment_the_daemon_started_under() -> Result<()> {
    let (sandbox, _alpha, _beta) = world();
    let registry = sandbox.registry_path().display().to_string();
    let runtime_root = tempfile::tempdir()?;
    let runtime = start(
        runtime_root.path(),
        &[
            ("BOOKRACK_EMBED_BATCH_MAX_CHUNKS", "7"),
            ("BOOKRACK_VECTORS_NPROBES", "8"),
        ],
    )
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let config = call(&mut reader, &mut w, 1, "daemon.config", Value::Null).await?;

        let chunks = row(&config["rows"], "embed.batch_max_chunks");
        assert_eq!(chunks["value"], "7", "{chunks}");
        assert_eq!(chunks["layer"], "environment", "{chunks}");

        // The registry the daemon was started under is reported as the
        // variable that named it.
        let reg = row(&config["rows"], "registry");
        assert_eq!(reg["value"], registry.as_str(), "{reg}");
        assert_eq!(reg["layer"], "environment", "{reg}");

        // A daemon serving no MCP listener reports no address.
        let mcp = row(&config["rows"], "mcp.addr");
        assert!(mcp["value"].is_null(), "{mcp}");
        assert_eq!(mcp["site"], "run --no-mcp", "{mcp}");

        assert_eq!(
            config["retired_variables"][0]["key"], "BOOKRACK_VECTORS_NPROBES",
            "{config}"
        );
        assert!(
            config["unclaimed_variables"].is_null(),
            "a host that supplied no inventory reports the section uncomputed: {config}"
        );
        assert_eq!(
            config["mounted_at_start"],
            serde_json::json!(["alpha", "beta"])
        );
        assert_eq!(config["queue_paused"], false);
        assert!(config["identity"]["instance_id"].is_string(), "{config}");

        // Every knob of the inventory this crate can see is in the
        // snapshot: the process rows and one library's rows together.
        let mut catalog: BTreeSet<String> = bookrack_config::knob_catalog()
            .into_iter()
            .map(|row| row.key)
            .collect();
        for contributor in bookrack_ops::settings::KNOB_CONTRIBUTORS {
            catalog.extend((contributor.catalog)().into_iter().map(|row| row.key));
        }
        let mut reported = keys(&config["rows"]);
        reported.extend(keys(&library(&config, "alpha").expect("alpha")["rows"]));
        assert_eq!(reported, catalog, "the snapshot and the inventory differ");

        call(&mut reader, &mut w, 99, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_library_edited_and_remounted_shows_the_new_value_and_an_unmounted_one_disappears()
-> Result<()> {
    let (_sandbox, alpha, beta) = world();
    top_k(&alpha, 2);
    top_k(&beta, 4);
    let runtime_root = tempfile::tempdir()?;
    let runtime = start(runtime_root.path(), &[]).await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let top_k_of = |config: &Value, name: &str| -> Option<(String, String)> {
            library(config, name).map(|library| {
                let row = row(&library["rows"], "search.top_k");
                (
                    row["value"].as_str().unwrap_or_default().to_string(),
                    row["layer"].as_str().unwrap_or_default().to_string(),
                )
            })
        };
        let file = |value: &str| Some((value.to_string(), "file".to_string()));

        // Each mounted library reports its own value.
        let config = call(&mut reader, &mut w, 1, "daemon.config", Value::Null).await?;
        assert_eq!(top_k_of(&config, "alpha"), file("2"), "{config}");
        assert_eq!(top_k_of(&config, "beta"), file("4"), "{config}");
        assert!(
            library(&config, "beta").expect("beta")["data_dir"]
                .as_str()
                .is_some_and(|dir| dir.ends_with("beta-root")),
            "{config}"
        );

        // An edit on disk does not reach the mounted library.
        top_k(&beta, 6);
        let config = call(&mut reader, &mut w, 2, "daemon.config", Value::Null).await?;
        assert_eq!(top_k_of(&config, "beta"), file("4"), "{config}");

        // Unmounted, it is gone from the report.
        call(
            &mut reader,
            &mut w,
            3,
            "library.unmount",
            serde_json::json!({"name": "beta"}),
        )
        .await?;
        let config = call(&mut reader, &mut w, 4, "daemon.config", Value::Null).await?;
        assert_eq!(top_k_of(&config, "beta"), None, "{config}");
        assert_eq!(
            config["mounted_at_start"],
            serde_json::json!(["alpha", "beta"])
        );

        // Mounted again, it reports what the file says now.
        call(
            &mut reader,
            &mut w,
            5,
            "library.mount",
            serde_json::json!({"name": "beta"}),
        )
        .await?;
        let config = call(&mut reader, &mut w, 6, "daemon.config", Value::Null).await?;
        assert_eq!(top_k_of(&config, "beta"), file("6"), "{config}");
        assert_eq!(top_k_of(&config, "alpha"), file("2"), "{config}");

        call(&mut reader, &mut w, 99, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_config_reports_whether_the_queue_is_paused() -> Result<()> {
    world();
    let runtime_root = tempfile::tempdir()?;
    let runtime = start(runtime_root.path(), &[]).await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let before = call(&mut reader, &mut w, 1, "daemon.config", Value::Null).await?;
        assert_eq!(before["queue_paused"], false);
        call(&mut reader, &mut w, 2, "queue.pause", Value::Null).await?;
        let paused = call(&mut reader, &mut w, 3, "daemon.config", Value::Null).await?;
        assert_eq!(paused["queue_paused"], true);
        call(&mut reader, &mut w, 4, "queue.resume", Value::Null).await?;
        let resumed = call(&mut reader, &mut w, 5, "daemon.config", Value::Null).await?;
        assert_eq!(resumed["queue_paused"], false);

        call(&mut reader, &mut w, 99, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}
