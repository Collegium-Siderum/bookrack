// SPDX-License-Identifier: Apache-2.0

//! `bookrack status` names one library, and the rows under that name
//! are about the library it named.
//!
//! The full card is built from two calls: `status` reports the primary
//! — the library the daemon came up under — while `library.info`
//! projects the counts. An unnamed `library.info` answers for the
//! registry's default pointer, so a daemon started under a library that
//! is not the default makes the two calls land on different libraries,
//! and the card carries one library's name above another's counts.
//!
//! What makes this test discriminate is the count difference: `alpha`
//! holds one ready book and `beta` none. Give both roots the same
//! counts and the assertion passes against the defect.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required.

#![cfg(unix)]

mod common;

use std::time::Duration;

use bookrack_catalog::{Catalog, NewItemState};
use bookrack_test_support::{EmbedStub, Sandbox, bookrack_cmd};
use serde_json::Value;
use tokio::process::Command;

use crate::common::{DaemonProcess, wait_for_lock};

#[tokio::test]
async fn the_card_counts_the_library_it_names() {
    let sandbox = Sandbox::new();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    // Both roots carry a catalog, so the two libraries differ in their
    // counts alone rather than in whether a store is there at all.
    let catalog = Catalog::open(&alpha.join("catalog.db")).expect("open the alpha catalog");
    catalog
        .upsert_book_state(&NewItemState::new(1, 1, "ready"))
        .expect("record a ready book in alpha");
    drop(catalog);
    drop(Catalog::open(&beta.join("catalog.db")).expect("open the beta catalog"));
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

    let output = Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .args(["--json", "status"])
        .output()
        .await
        .expect("run bookrack status");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "a healthy daemon must produce a card: status={:?} stderr={stderr}",
        output.status,
    );
    let card: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("card is not JSON ({e}): {stdout}"));

    // The identity rows: these have always been the primary's, and they
    // are the control the count assertion is read against.
    assert_eq!(
        card["library"]["name"], "beta",
        "the card names the library the daemon came up under: {card}",
    );
    assert_eq!(
        card["library"]["data_dir"],
        beta.display().to_string(),
        "and its root: {card}",
    );
    // The count row, which the defect took from `alpha` — the registry
    // default — while the rows above said `beta`.
    assert_eq!(
        card["library"]["books_ready"], 0,
        "counts must come from the library the card names, not from the \
         registry default: {card}",
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
