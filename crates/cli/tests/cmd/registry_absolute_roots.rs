// SPDX-License-Identifier: Apache-2.0

//! The registry records absolute data roots. A relative path typed at
//! a registry verb is resolved against the working directory before
//! it is written, so a later invocation from another directory still
//! finds the same root.

#![cfg(unix)]

use std::path::Path;
use std::process::Stdio;

use bookrack_test_support::{Sandbox, bookrack_cmd};
use eyre::Result;

/// `libraries add <name> ./lib` records `<cwd>/lib`, not `./lib`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_add_records_a_relative_root_as_absolute() -> Result<()> {
    let sandbox = Sandbox::new();
    let root = sandbox.cwd().join("lib");
    std::fs::create_dir_all(&root)?;
    let output = tokio::process::Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .args(["libraries", "add", "shelf", "./lib", "--yes"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "libraries add should register the root; stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let written = std::fs::read_to_string(sandbox.registry_path())?;
    let recorded = written
        .lines()
        .find_map(|line| line.strip_prefix("data_dir = "))
        .map(|value| value.trim_matches('"').to_string())
        .unwrap_or_else(|| panic!("registry has no data_dir line: {written}"));
    let recorded = Path::new(&recorded);
    assert!(
        recorded.is_absolute(),
        "registry should record an absolute root, got {}: {written}",
        recorded.display(),
    );
    // The child reports its working directory as the kernel spells it,
    // which on a symlinked temp root differs from the path the test
    // built; both name the same directory.
    assert_eq!(recorded.canonicalize()?, root.canonicalize()?);
    Ok(())
}
