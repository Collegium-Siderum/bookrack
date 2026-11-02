// SPDX-License-Identifier: Apache-2.0

//! A deprecated command keeps working and says so on stderr.
//!
//! `papers ingest` prints one warning line naming `glean` before
//! anything else, the failure path included, and is otherwise `glean`:
//! the same exit code, the same stdout, the same stderr after that line.
//! Under `--json` the warning stays on stderr, so stdout carries only
//! the machine-readable output. `glean` prints no warning.

#![cfg(unix)]

use std::process::Stdio;

use bookrack_test_support::{Sandbox, bookrack_cmd};
use eyre::Result;

async fn run(sandbox: &Sandbox, args: &[&str]) -> Result<(Option<i32>, String, String)> {
    let output = tokio::process::Command::from(bookrack_cmd!(sandbox).build())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    Ok((
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn papers_ingest_warns_and_otherwise_behaves_as_glean() -> Result<()> {
    let sandbox = Sandbox::new();
    for global in [None, Some("--json")] {
        let argv = |command: &[&'static str]| -> Vec<&'static str> {
            global
                .into_iter()
                .chain(command.iter().copied())
                .chain(["/tmp/paper.pdf"])
                .collect()
        };
        let (old_code, old_out, old_err) = run(&sandbox, &argv(&["papers", "ingest"])).await?;
        let (new_code, new_out, new_err) = run(&sandbox, &argv(&["glean"])).await?;

        assert_eq!(
            new_code,
            Some(2),
            "{global:?}: no daemon is running: {new_out} {new_err}"
        );
        assert_eq!(old_code, new_code, "{global:?}: {old_err}");
        assert_eq!(old_out, new_out, "{global:?}: stdout differs");

        let (first, rest) = old_err.split_once('\n').unwrap_or((&old_err, ""));
        assert!(
            first.starts_with("warning: `bookrack papers ingest` is deprecated")
                && first.contains("`bookrack glean`"),
            "{global:?}: the first stderr line is the warning naming glean: {old_err}"
        );
        assert_eq!(
            rest, new_err,
            "{global:?}: past the warning, stderr is glean's"
        );
        assert!(
            !new_err.contains("deprecated"),
            "{global:?}: glean does not warn: {new_err}"
        );
    }
    Ok(())
}
