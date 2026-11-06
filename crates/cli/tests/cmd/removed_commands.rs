// SPDX-License-Identifier: Apache-2.0

//! A command outside the surface is a usage error, not a forwarded call.
//!
//! The `papers` namespace has no `ingest` leaf; the paper-side
//! submission verb is `glean`. `papers ingest` exits 2 with clap naming
//! the unknown subcommand, prints nothing on stdout, and carries no
//! deprecation warning. `glean`, given the same arguments, gets past
//! the parser: with no daemon running it also exits 2, but for the
//! missing daemon, so the two refusals are told apart by what stderr
//! says rather than by the exit code.

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
async fn papers_ingest_is_a_usage_error_and_glean_is_not() -> Result<()> {
    let sandbox = Sandbox::new();

    let (code, stdout, stderr) = run(&sandbox, &["papers", "ingest", "/tmp/paper.pdf"]).await?;
    assert_eq!(
        code,
        Some(2),
        "the removed command is a usage error: {stdout} {stderr}"
    );
    assert!(
        stderr.contains("unrecognized subcommand 'ingest'"),
        "clap names the unknown subcommand: {stderr}"
    );
    assert!(
        !stderr.contains("deprecated"),
        "a removed command carries no deprecation warning: {stderr}"
    );
    assert!(stdout.is_empty(), "nothing was submitted: {stdout}");

    let (code, stdout, stderr) = run(&sandbox, &["glean", "/tmp/paper.pdf"]).await?;
    assert_eq!(code, Some(2), "no daemon is running: {stdout} {stderr}");
    assert!(
        !stderr.contains("unrecognized subcommand"),
        "`glean` parses and fails on the missing daemon: {stderr}"
    );
    Ok(())
}
