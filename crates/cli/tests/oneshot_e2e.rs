// SPDX-License-Identifier: Apache-2.0

//! Phase 4 end-to-end coverage for the one-shot CLI subcommands.
//!
//! Asserts the daemon-not-running invariant: every one-shot
//! subcommand that needs a daemon exits with the documented
//! "not running" code (2), while the two documented exceptions —
//! `bookrack quit` and `bookrack doctor` — bail out gracefully with
//! their own contracts (quit reports "no daemon" and exits 0; doctor
//! falls back to the local probe and exits whatever its checks
//! produce).
//!
//! The daemon-running path needs an Ollama-backed library bootstrap
//! and lives behind `#[ignore]` in `control_writes`; Phase 4 adds
//! nothing new there, so this test stays focused on the cheap-to-verify
//! exit-code contract.

#![cfg(unix)]

use std::process::Stdio;

use eyre::Result;

fn bookrack_bin() -> &'static str {
    env!("CARGO_BIN_EXE_bookrack")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oneshot_subcommands_consistent_no_daemon() -> Result<()> {
    let runtime_dir = tempfile::tempdir()?;
    let data_dir = tempfile::tempdir()?;
    let cases: &[(&[&str], CaseExpect)] = &[
        (
            &["ingest", "/tmp/phase4-fixture.txt"],
            CaseExpect::NotRunning,
        ),
        (
            &["metadata", "set", "1", "title", "x"],
            CaseExpect::NotRunning,
        ),
        (&["vectors", "drop"], CaseExpect::NotRunning),
        (&["corpus", "rebuild"], CaseExpect::NotRunning),
        (&["stamps", "reconcile"], CaseExpect::NotRunning),
        (&["remove", "1", "--yes"], CaseExpect::NotRunning),
        (
            &["dryrun", "/tmp/phase4-fixture.txt"],
            CaseExpect::NotRunning,
        ),
        (&["verify"], CaseExpect::NotRunning),
        (&["libraries", "list"], CaseExpect::NotRunning),
        (&["diagnose"], CaseExpect::NotRunning),
        (&["quit"], CaseExpect::Quit),
    ];
    for (argv, expect) in cases {
        let output = tokio::process::Command::new(bookrack_bin())
            .args(argv.iter().copied())
            .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
            .env("BOOKRACK_DATA_DIR", data_dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;
        match expect {
            CaseExpect::NotRunning => {
                assert_eq!(
                    output.status.code(),
                    Some(2),
                    "{:?} expected exit 2 (daemon not running), got stdout={:?} stderr={:?}",
                    argv,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    stderr.contains("bookrack daemon not running"),
                    "{:?} stderr missing daemon-not-running tip: {}",
                    argv,
                    stderr,
                );
            }
            CaseExpect::Quit => {
                assert_eq!(
                    output.status.code(),
                    Some(0),
                    "{:?} expected exit 0 from quit-without-daemon, stderr={:?}",
                    argv,
                    String::from_utf8_lossy(&output.stderr),
                );
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    stderr.contains("no daemon running"),
                    "{:?} stderr missing nothing-to-stop tip: {}",
                    argv,
                    stderr,
                );
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_without_daemon_falls_back_to_local_probe() -> Result<()> {
    let runtime_dir = tempfile::tempdir()?;
    let data_dir = tempfile::tempdir()?;
    let output = tokio::process::Command::new(bookrack_bin())
        .args(["doctor", "--json"])
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
        .env("BOOKRACK_DATA_DIR", data_dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    // Doctor's exit code reflects whether every probe passed; without
    // Ollama installed this run typically fails (Ollama probe), but
    // crucially it does NOT exit with 2 (the daemon-not-running code).
    // The fallback ran — the report landed on stdout as JSON.
    assert_ne!(
        output.status.code(),
        Some(2),
        "doctor should fall back to a local probe, not return the daemon-not-running code 2",
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"rows\""),
        "doctor --json should print a report with a `rows` field, got: {stdout}",
    );
    Ok(())
}

/// `libraries default` resolves locally: with no daemon running it
/// still writes the on-disk registry default and exits 0, rather than
/// the daemon-not-running code 2. A legacy bare-path registry is
/// rewritten into the entry-table form in the process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_default_writes_the_registry_offline() -> Result<()> {
    let runtime_dir = tempfile::tempdir()?;
    let registry_dir = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    std::fs::write(
        &registry_path,
        "default = \"alpha\"\n\
         [libraries]\n\
         alpha = \"/roots/alpha\"\n\
         beta = \"/roots/beta\"\n",
    )?;
    let output = tokio::process::Command::new(bookrack_bin())
        .args(["libraries", "default", "beta"])
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
        .env("BOOKRACK_REGISTRY", &registry_path)
        .env_remove("BOOKRACK_DATA_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "libraries default should write offline and exit 0; stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("default library set to 'beta'"),
        "stdout missing success line: {stdout}",
    );
    let written = std::fs::read_to_string(&registry_path)?;
    assert!(
        written.contains("default = \"beta\""),
        "default pointer not repointed: {written}",
    );
    // The legacy bare-path entries are rewritten into table form, so
    // each now carries an explicit `data_dir` key.
    assert!(
        written.contains("data_dir"),
        "registry not upgraded to entry-table form: {written}",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("registry upgraded to entry-table format"),
        "stderr missing the one-time upgrade notice: {stderr}",
    );
    Ok(())
}

/// A `libraries default` naming a library the registry does not define
/// is operator input, not a system fault: it exits 2 and does not
/// disturb the file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_default_rejects_an_unknown_name_with_exit_2() -> Result<()> {
    let runtime_dir = tempfile::tempdir()?;
    let registry_dir = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    std::fs::write(&registry_path, "[libraries]\nalpha = \"/roots/alpha\"\n")?;
    let output = tokio::process::Command::new(bookrack_bin())
        .args(["libraries", "default", "ghost"])
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
        .env("BOOKRACK_REGISTRY", &registry_path)
        .env_remove("BOOKRACK_DATA_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    assert_eq!(
        output.status.code(),
        Some(2),
        "an unknown library name is a user error (exit 2); stderr={:?}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no library named"),
        "stderr should name the unknown library: {stderr}",
    );
    Ok(())
}

enum CaseExpect {
    NotRunning,
    Quit,
}
