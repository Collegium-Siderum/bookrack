// SPDX-License-Identifier: Apache-2.0

//! A listing names the library its rows came from, end to end.
//!
//! The name and the rows are produced by two different calls: the rows
//! come from `library.list_books` / `library.list_papers`, and the name
//! from the served set `status` reports. An unnamed invocation reaches
//! the registry's default pointer, while `status`'s own `library` field
//! reports the primary — the library the daemon came up under. On a
//! daemon started under a library that is not the default, an
//! implementation reading that field would print one library's name
//! above another library's rows.
//!
//! What makes this discriminate is that the two roots hold different
//! items: `alpha` carries `alpha-book.epub` and `alpha-paper.pdf`,
//! `beta` carries `beta-*`. Seed both roots with the same filenames and
//! the assertions pass against the defect.
//!
//! The rows are seeded through `Catalog::register_intake`, which is
//! what the listing reads: no extraction, no embedding, no queue. The
//! embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::Duration;

use bookrack_catalog::{Catalog, NewIntake};
use bookrack_core::ItemKind;
use bookrack_test_support::{EmbedStub, Sandbox, bookrack_cmd};
use serde_json::Value;
use tokio::process::Command;

use crate::common::{DaemonProcess, wait_for_lock};

/// Register one book and one paper in a root, named after it.
///
/// The hash is derived from the label too: `register_intake` is
/// idempotent on the hash, so two roots seeded with the same one would
/// silently share a row identity.
fn seed(root: &Path, label: &str) {
    for (file, kind, basename) in [
        ("catalog.db", ItemKind::Book, format!("{label}-book.epub")),
        (
            "papers_catalog.db",
            ItemKind::Paper,
            format!("{label}-paper.pdf"),
        ),
    ] {
        let mut catalog =
            Catalog::open(&root.join(file)).unwrap_or_else(|e| panic!("open {label}/{file}: {e}"));
        let sha = format!("{:0>64}", format!("{label}{}", kind.as_scope_str()));
        let format = basename.rsplit('.').next().expect("an extension");
        catalog
            .register_intake(
                kind,
                &NewIntake::new(sha)
                    .original_path(format!("/inbox/{basename}"))
                    .format(format),
            )
            .unwrap_or_else(|e| panic!("seed {label}/{file}: {e}"));
    }
}

#[tokio::test]
async fn a_listing_names_the_library_its_rows_came_from() {
    let sandbox = Sandbox::new();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    seed(&alpha, "alpha");
    seed(&beta, "beta");
    // `alpha` is the registry default; the daemon comes up under `beta`.
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );

    let lock_path = sandbox.tty_lock_path();
    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .without_data_dir()
            .ollama_url(EmbedStub::url())
            .build(),
    );
    daemon_cmd.args(["--library", "beta", "run"]);
    let daemon = DaemonProcess::spawn(daemon_cmd).expect("spawn bookrack run");
    assert!(
        wait_for_lock(&lock_path, Duration::from_secs(20)).await,
        "session lock did not appear; bookrack run may have failed to start",
    );

    // An unnamed invocation reaches the registry default, and says so.
    let unnamed = run_list(&sandbox, &[]).await;
    assert!(
        unnamed.contains("alpha-book.epub") && unnamed.contains("alpha-paper.pdf"),
        "an unnamed listing reads the registry default:\n{unnamed}",
    );
    assert!(
        unnamed.ends_with("library: alpha"),
        "the name under the rows must be the library they came from, not the \
         primary the daemon came up under:\n{unnamed}",
    );

    // A named one reaches the library it named, and says that.
    let named = run_list(&sandbox, &["--library", "beta"]).await;
    assert!(
        named.contains("beta-book.epub") && named.contains("beta-paper.pdf"),
        "a named listing reads the library it named:\n{named}",
    );
    assert!(
        named.ends_with("library: beta"),
        "the name under the rows must follow the selection:\n{named}",
    );

    // The machine-readable form answers the same question the same way.
    let payload = run_list(&sandbox, &["--json"]).await;
    let payload: Value = serde_json::from_str(&payload)
        .unwrap_or_else(|e| panic!("the payload is not JSON ({e}): {payload}"));
    assert_eq!(
        payload["library"], "alpha",
        "the payload names a different library than the card: {payload}",
    );
    let sources: Vec<&str> = payload["items"]
        .as_array()
        .expect("an items array")
        .iter()
        .filter_map(|item| item["source_filename"].as_str())
        .collect();
    assert!(
        sources.contains(&"alpha-book.epub") && sources.contains(&"alpha-paper.pdf"),
        "the rows must come from the library the payload names: {payload}",
    );
    assert!(
        !sources.iter().any(|s| s.starts_with("beta-")),
        "rows from two libraries reached one payload: {payload}",
    );

    if let Some(id) = daemon.id() {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(id.to_string())
            .status()
            .await;
    }
    let _ = daemon.wait_with_output(Duration::from_secs(5)).await;
}

/// Run `bookrack list` with extra leading arguments and return stdout.
async fn run_list(sandbox: &Sandbox, prefix: &[&str]) -> String {
    let mut cmd = Command::from(bookrack_cmd!(sandbox).without_data_dir().build());
    cmd.args(prefix).arg("list");
    let output = cmd.output().await.expect("run bookrack list");
    let stdout = String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned();
    assert!(
        output.status.success(),
        "listing failed: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    stdout
}
