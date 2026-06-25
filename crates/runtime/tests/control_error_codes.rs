// SPDX-License-Identifier: Apache-2.0

//! Asserts that write-class JSON-RPC handlers surface caller-side input
//! failures as `INVALID_PARAMS` / `INVALID_LIBRARY` rather than
//! collapsing every downstream error to `INTERNAL_ERROR`.
//!
//! Boots a [`DaemonRuntime`] in the headless profile, then drives a
//! sequence of intentionally-bad write RPCs through a single
//! connection and asserts on `error.code` for each.
//!
//! Ignored by default because the runtime calls
//! [`bookrack_query::Library::open`], which probes the configured
//! Ollama daemon for the embedding model's dimension. Run manually
//! with `cargo test -p bookrack-runtime --test control_error_codes
//! -- --ignored`.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use bookrack_config::LibrarySelection;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use eyre::{Result, eyre};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};
use tokio::net::UnixStream;

const INVALID_PARAMS: i64 = -32602;
const INVALID_LIBRARY: i64 = -32010;

fn build_opts(data_dir: PathBuf, runtime_dir: PathBuf) -> RuntimeOpts {
    let mut opts = RuntimeOpts::headless(Some(data_dir), None);
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_dir);
    opts.selection = LibrarySelection {
        data_dir: opts.selection.data_dir,
        library: opts.selection.library,
    };
    opts
}

async fn send(stream: &mut WriteHalf<UnixStream>, line: &str) -> Result<()> {
    stream.write_all(line.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    Ok(())
}

async fn recv(reader: &mut Lines<BufReader<ReadHalf<UnixStream>>>) -> Result<Value> {
    let line = reader
        .next_line()
        .await?
        .ok_or_else(|| eyre!("eof while expecting response"))?;
    Ok(serde_json::from_str(&line)?)
}

async fn rpc_code(
    writer: &mut WriteHalf<UnixStream>,
    reader: &mut Lines<BufReader<ReadHalf<UnixStream>>>,
    id: u64,
    method: &str,
    params: Value,
) -> Result<(i64, String)> {
    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    send(writer, &frame.to_string()).await?;
    let resp = recv(reader).await?;
    let code = resp["error"]["code"]
        .as_i64()
        .ok_or_else(|| eyre!("expected error payload, got {resp}"))?;
    let message = resp["error"]["message"].as_str().unwrap_or("").to_string();
    Ok((code, message))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a reachable Ollama embedding daemon"]
async fn write_handlers_surface_invalid_params_not_internal() -> Result<()> {
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stream = UnixStream::connect(&sock).await?;
        let (r, mut w) = tokio::io::split(stream);
        let mut reader = BufReader::new(r).lines();

        // `library.set_default` with an unknown name has always returned
        // INVALID_LIBRARY (-32010); this guards the regression.
        let (code, _) = rpc_code(
            &mut w,
            &mut reader,
            1,
            "library.set_default",
            serde_json::json!({ "name": "ghost-library" }),
        )
        .await?;
        assert_eq!(code, INVALID_LIBRARY, "library.set_default unknown name");

        // `metadata.set` with an intake id that does not exist now
        // surfaces `OpsError::IntakeNotFound` as INVALID_PARAMS instead
        // of collapsing to INTERNAL_ERROR.
        let (code, msg) = rpc_code(
            &mut w,
            &mut reader,
            2,
            "metadata.set",
            serde_json::json!({
                "book": 9_999_999_i64,
                "field": "title",
                "value": "anything",
            }),
        )
        .await?;
        assert_eq!(code, INVALID_PARAMS, "metadata.set unknown intake: {msg}");

        // `metadata.set` with a field name outside the editable set
        // surfaces `OpsError::UnknownMetadataField` as INVALID_PARAMS.
        let (code, msg) = rpc_code(
            &mut w,
            &mut reader,
            3,
            "metadata.set",
            serde_json::json!({
                "book": 1,
                "field": "definitely_not_a_real_field",
                "value": "x",
            }),
        )
        .await?;
        assert_eq!(code, INVALID_PARAMS, "metadata.set unknown field: {msg}");

        // `corpus.rebuild` targeting an unknown book surfaces
        // `IngestError::UnknownIntake` (or `OpsError::IntakeNotFound`,
        // depending on where the lookup happens first) as INVALID_PARAMS.
        let (code, msg) = rpc_code(
            &mut w,
            &mut reader,
            4,
            "corpus.rebuild",
            serde_json::json!({
                "book": 9_999_999_i64,
                "yes": true,
            }),
        )
        .await?;
        assert_eq!(code, INVALID_PARAMS, "corpus.rebuild unknown book: {msg}");

        // `vectors.reembed` targeting an unknown book surfaces
        // `IngestError::UnknownIntake` as INVALID_PARAMS.
        let (code, msg) = rpc_code(
            &mut w,
            &mut reader,
            5,
            "vectors.reembed",
            serde_json::json!({
                "book": 9_999_999_i64,
                "yes": true,
            }),
        )
        .await?;
        assert_eq!(code, INVALID_PARAMS, "vectors.reembed unknown book: {msg}");

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    runtime.run_until_shutdown(None, repl_handle).await?;
    driver.await??;
    Ok(())
}
