// SPDX-License-Identifier: Apache-2.0

//! The cornerstone of the host-isolation discipline: proof that a child
//! built by `bookrack_test_support::bookrack_cmd!` reads the sandbox and
//! nothing else.
//!
//! It lives in `bookrack-cli` because cargo defines the path to a
//! binary only while compiling a test target of the crate that declares
//! it, so this is the one place the macro can expand.

#![cfg(unix)]

use std::process::Stdio;

use bookrack_test_support::{Sandbox, bookrack_cmd};
use eyre::Result;

/// A child that names no data root and no registry must report that
/// none is configured, and must name a registry path inside the
/// sandbox.
///
/// The single `data root | (none configured) | FAIL` row is the whole
/// booklet compressed. That row is produced by exactly one arm — the
/// `Err(ConfigError::MissingDataDir)` arm of `push_data_root_row` — so
/// asserting it proves three leaks are absent at once: no host
/// registry, no host data-root variable, and no `.env` reaching the
/// child. Any one of them would have handed the child a *resolved*
/// root and a different row.
///
/// The path inside the `FAIL` note is the one the child computed for
/// itself from `default_registry_path()`. Asserting that it lands
/// inside the sandbox proves the `HOME` / `XDG_CONFIG_HOME`
/// redirection survived the process boundary — an assertion that a
/// check on an empty `libraries list` could not make, because an empty
/// list is what a runner with no registry produces anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sandboxed_child_sees_no_library_and_names_the_sandbox_registry() -> Result<()> {
    let sandbox = Sandbox::new();
    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .without_data_dir()
            .without_registry()
            .build(),
    )
    .args(["doctor", "--json"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        eyre::eyre!(
            "doctor --json did not print a report ({e}); stdout={stdout} stderr={}",
            String::from_utf8_lossy(&output.stderr),
        )
    })?;
    let rows = report["rows"]
        .as_array()
        .ok_or_else(|| eyre::eyre!("no rows in the report: {report}"))?;
    let row = rows
        .iter()
        .find(|row| row["label"] == "data root")
        .ok_or_else(|| eyre::eyre!("no data-root row in the report: {report}"))?;

    assert_eq!(
        row["value"], "(none configured)",
        "the child resolved a data root, so something outside the sandbox \
         reached it: {row}",
    );
    let note = row["note"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("the failing data-root row carries no note: {row}"))?;
    let root = sandbox.path().display().to_string();
    assert!(
        note.contains(&root),
        "the child computed its registry path as {note:?}, outside the \
         sandbox at {root}: the home redirection did not cross the \
         process boundary",
    );
    Ok(())
}
