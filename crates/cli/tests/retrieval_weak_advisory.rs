// SPDX-License-Identifier: Apache-2.0

//! End-to-end coverage for the `retrieval show` weak-recall advisory:
//! the threshold it judges against comes from the target library's own
//! `config.toml`, under the environment layer.
//!
//! The whole chain is offline — `retrieval` needs no daemon, no
//! embedder, and no vector store — so the test seeds one sidecar row
//! into a real `catalog.db` and drives the installed binary against it.

#![cfg(unix)]

use std::process::Stdio;

use bookrack_catalog::{Catalog, NewMcpToolCall, NewRetrievalCall};
use bookrack_test_support::{Sandbox, bookrack_cmd};
use eyre::Result;

/// Run `retrieval show <call_id>` against `data_dir`, returning stdout.
///
/// The threshold variable needs no explicit removal: the builder sweeps
/// every `BOOKRACK_*` name it did not set, so a knob invented after
/// this test was written is cut off without anyone remembering to
/// name it.
fn show(sandbox: &Sandbox, data_dir: &std::path::Path, call_id: i64) -> Result<String> {
    let out = bookrack_cmd!(sandbox)
        .without_data_dir()
        .build()
        .args([
            "--data-dir",
            &data_dir.display().to_string(),
            "retrieval",
            "show",
            &call_id.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    assert_eq!(
        out.status.code(),
        Some(0),
        "retrieval show should succeed offline; stderr={:?}",
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn retrieval_show_reads_the_weak_threshold_from_the_target_library() -> Result<()> {
    let sandbox = Sandbox::new();
    let root = tempfile::tempdir()?;
    // An empty registry the resolver can read: `--data-dir` names the
    // root, but resolution still consults the registry, and the host's
    // must stay out of it. The sandbox already carries one.
    let config = root.path().join("config.toml");

    // One recorded call, one hit at 0.62 -- between the two thresholds
    // the test drives, so the verdict can only come from the file.
    let call_id = {
        let catalog = Catalog::open(&root.path().join("catalog.db"))?;
        catalog.record_tool_call_with_retrieval(
            &NewMcpToolCall::new("mcp", "library.search", "ok"),
            Some(&NewRetrievalCall {
                fingerprint: "deadbeefcafef00d".to_string(),
                top_k: 10,
                query_text: Some("what is a monad".to_string()),
                hits: vec![("p-alpha".to_string(), 0.62)],
            }),
        )?
    };

    // A threshold above the hit: the recall set is not weak.
    std::fs::write(&config, "[search]\nweak_threshold = 0.9\n")?;
    let lenient = show(&sandbox, root.path(), call_id)?;
    assert!(
        lenient.contains("0.6200"),
        "the hit should render: {lenient}"
    );
    assert!(
        !lenient.contains("weak recall:"),
        "0.62 is below a 0.9 threshold: {lenient}"
    );

    // The same call, the same hit, a stricter threshold in the same
    // file: now the whole set is weak, and the line names 0.5000.
    std::fs::write(&config, "[search]\nweak_threshold = 0.5\n")?;
    let strict = show(&sandbox, root.path(), call_id)?;
    assert!(
        strict.contains("weak recall:") && strict.contains("0.5000"),
        "0.62 is at or above a 0.5 threshold: {strict}"
    );

    // The environment layer still wins over the file, which is the
    // documented precedence for this knob.
    let overridden = bookrack_cmd!(&sandbox)
        .without_data_dir()
        .extra_env("BOOKRACK_SEARCH_WEAK_THRESHOLD", "0.9")
        .build()
        .args([
            "--data-dir",
            &root.path().display().to_string(),
            "retrieval",
            "show",
            &call_id.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    let text = String::from_utf8_lossy(&overridden.stdout);
    assert!(
        !text.contains("weak recall:"),
        "the env override should lift the threshold back above the hit: {text}"
    );
    Ok(())
}
