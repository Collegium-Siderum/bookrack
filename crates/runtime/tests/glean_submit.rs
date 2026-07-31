// SPDX-License-Identifier: Apache-2.0

//! Control-plane integration test for the paper-side queue surface:
//! `glean.submit` enqueues paper jobs (`kind = paper`) that
//! `ingest.cancel` and the rest of the `queue.*` lifecycle methods
//! treat the same way as book jobs.
//!
//! The embedder probe daemon bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is
//! required.

#![cfg(unix)]

mod common;

use eyre::{ContextCompat, Result};
use serde_json::Value;

use crate::common::{build_opts, connect, join_with_deadline, recv, send};
use bookrack_test_support::{ProcessEnv, process_env};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn glean_submit_enqueues_paper_jobs_and_ingest_cancel_covers_them() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        // Pause the queue so the worker does not pull the paper job
        // before this client can read it back and cancel it.
        send(&mut w, r#"{"jsonrpc":"2.0","id":1,"method":"queue.pause"}"#).await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(resp["result"]["paused"], Value::Bool(true), "{resp}");

        // Submit a paper job through `glean.submit`.
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"glean.submit","params":{"paths":["/tmp/glean-fixture.pdf"]}}"#,
        )
        .await?;
        let submit_resp = recv(&mut reader).await?;
        let job_ids = submit_resp["result"]["job_ids"]
            .as_array()
            .cloned()
            .context("missing job_ids array")?;
        assert_eq!(job_ids.len(), 1, "{submit_resp}");
        let job_id = job_ids[0]
            .as_str()
            .context("job_id is not a string")?
            .to_string();

        // The job round-trips through `queue.list` with `kind = paper`.
        send(&mut w, r#"{"jsonrpc":"2.0","id":3,"method":"queue.list"}"#).await?;
        let list_resp = recv(&mut reader).await?;
        let row = list_resp["result"]["jobs"]
            .as_array()
            .and_then(|jobs| jobs.iter().find(|j| j["id"].as_str() == Some(&job_id)))
            .cloned()
            .context("paper job missing from queue.list")?;
        assert_eq!(row["kind"].as_str(), Some("paper"), "{row}");
        assert_eq!(row["state"].as_str(), Some("pending"), "{row}");

        // The shared lifecycle method `ingest.cancel` covers both
        // kinds — no `glean.cancel` is needed.
        let cancel_req = format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"ingest.cancel","params":{{"job_id":"{job_id}"}}}}"#
        );
        send(&mut w, &cancel_req).await?;
        let cancel_resp = recv(&mut reader).await?;
        assert_eq!(
            cancel_resp["result"]["ok"],
            Value::Bool(true),
            "{cancel_resp}"
        );

        // Post-cancel state is observable through the same shared
        // `queue.list` surface.
        send(&mut w, r#"{"jsonrpc":"2.0","id":5,"method":"queue.list"}"#).await?;
        let final_resp = recv(&mut reader).await?;
        let final_row = final_resp["result"]["jobs"]
            .as_array()
            .and_then(|jobs| jobs.iter().find(|j| j["id"].as_str() == Some(&job_id)))
            .cloned()
            .context("paper job missing from final queue.list")?;
        assert_eq!(
            final_row["state"].as_str(),
            Some("cancelled"),
            "{final_row}"
        );
        assert_eq!(final_row["kind"].as_str(), Some("paper"), "{final_row}");

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}
