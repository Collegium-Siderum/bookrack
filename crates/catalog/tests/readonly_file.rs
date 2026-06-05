// SPDX-License-Identifier: Apache-2.0

//! Regression: opening `catalog.db` for read-write must surface a
//! readable error when the file is on disk but mode-444, rather than
//! letting the migration step half-write the schema or panic. Ingest
//! depends on this property so that an operator who has fat-fingered
//! the permissions sees the failure before any vector store directory
//! is created.

#![cfg(unix)]

use std::error::Error;
use std::os::unix::fs::PermissionsExt;

use bookrack_catalog::{Catalog, CatalogError};

/// Walk an error's `source` chain and collect each level's Display.
fn error_chain(err: &dyn Error) -> String {
    let mut out = err.to_string();
    let mut next = err.source();
    while let Some(src) = next {
        out.push_str(": ");
        out.push_str(&src.to_string());
        next = src.source();
    }
    out
}

#[test]
fn opening_a_chmod_444_catalog_for_read_write_fails_readably() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("catalog.db");

    // Seed the file at the current schema, then drop the handle so the
    // file is closed before its permissions are tightened.
    drop(Catalog::open(&path).expect("seed catalog"));

    let mut perms = std::fs::metadata(&path).expect("stat").permissions();
    perms.set_mode(0o444);
    std::fs::set_permissions(&path, perms).expect("chmod 444");

    let err = match Catalog::open(&path) {
        Ok(_) => panic!("a read-only file must refuse rw open"),
        Err(err) => err,
    };
    assert!(
        matches!(err, CatalogError::Sqlite(_)),
        "expected the sqlite layer to surface the readonly failure, got: {err}",
    );
    // Walk the source chain explicitly: thiserror's `{:#}` does not
    // expand the chain, and the top-level Display is a static label.
    let chain = error_chain(&err);
    assert!(
        chain.contains("readonly")
            || chain.contains("read-only")
            || chain.contains("unable to open"),
        "error chain must name the readonly / open failure: {chain}",
    );

    // Nothing was created as a side effect of the failed open: only the
    // seed file remains, and no sibling lance / corpus directories.
    let siblings: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .collect();
    assert_eq!(
        siblings.len(),
        1,
        "unexpected sibling entries: {siblings:?}"
    );
}
