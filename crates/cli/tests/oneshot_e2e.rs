// SPDX-License-Identifier: Apache-2.0

//! End-to-end coverage for the one-shot CLI subcommands.
//!
//! Asserts the daemon-not-running invariant: a one-shot subcommand
//! that routes through the control plane exits with the documented
//! "not running" code (2) and names `bookrack run` on stderr, whether
//! it reads or writes. The exceptions are the clients that can answer
//! without a daemon — `bookrack doctor` falls back to the local probe,
//! `bookrack status` reports the absence and exits 0 — plus
//! `bookrack quit`, which has nothing to stop.
//!
//! The daemon-running path is covered by the control-plane integration
//! tests in `bookrack-runtime`, which answer the daemon's embedder
//! probe with `bookrack_test_support::EmbedStub` and so need no Ollama
//! daemon. This test stays on the exit-code contract.

#![cfg(unix)]

use std::process::Stdio;

use bookrack_session::RootLock;
use bookrack_test_support::{Sandbox, bookrack_cmd};
use eyre::Result;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oneshot_subcommands_consistent_no_daemon() -> Result<()> {
    let sandbox = Sandbox::new();
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
        (&["diagnose"], CaseExpect::NotRunning),
        // Read-shaped clients route through the same connect helper as
        // the write verbs above, so the rule is about reaching the
        // daemon rather than about mutating anything.
        (&["queue", "list"], CaseExpect::NotRunning),
        (&["logs"], CaseExpect::NotRunning),
        (&["papers", "list"], CaseExpect::NotRunning),
        (&["intake", "list-ocr-pending"], CaseExpect::NotRunning),
        (&["rpc", "list"], CaseExpect::NotRunning),
        (
            &["rpc", "call", "library.info", "{}"],
            CaseExpect::NotRunning,
        ),
        // `index-profile apply` is deliberately absent: against a data
        // root with nothing built its plan is empty, and an empty plan
        // is declared offline without the daemon being reached at all.
        // `index_profile_resolution.rs` pins that path.
        //
        // The exception. `bookrack status` answers offline instead of
        // failing, and is pinned separately by
        // `status_without_daemon_prints_a_short_card_and_exits_zero`.
        (&["quit"], CaseExpect::Quit),
    ];
    for (argv, expect) in cases {
        let output = tokio::process::Command::from(bookrack_cmd!(&sandbox).build())
            .args(argv.iter().copied())
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
    let sandbox = Sandbox::new();
    let output = tokio::process::Command::from(bookrack_cmd!(&sandbox).build())
        .args(["doctor", "--json"])
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

/// `bookrack status` answers "not running" as a legal verdict: a
/// fresh runtime directory with no lock renders the short card and
/// exits 0, not the daemon-not-running code 2. `--json` keeps the
/// short card a single valid JSON object.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_without_daemon_prints_a_short_card_and_exits_zero() -> Result<()> {
    let sandbox = Sandbox::new();
    let output = tokio::process::Command::from(bookrack_cmd!(&sandbox).build())
        .args(["status"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "status with no daemon should exit 0; stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("daemon.running") && stdout.contains("bookrack run"),
        "short card should report not-running and point at `bookrack run`: {stdout}",
    );

    let json_out = tokio::process::Command::from(bookrack_cmd!(&sandbox).build())
        .args(["--json", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    assert_eq!(json_out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&json_out.stdout);
    let value: serde_json::Value = serde_json::from_str(stdout.trim())?;
    assert_eq!(
        value["daemon"]["running"],
        serde_json::json!(false),
        "{stdout}",
    );

    let quiet = tokio::process::Command::from(bookrack_cmd!(&sandbox).build())
        .args(["--quiet", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    assert_eq!(quiet.status.code(), Some(0));
    assert!(
        quiet.stdout.is_empty(),
        "quiet mode must print nothing: {:?}",
        String::from_utf8_lossy(&quiet.stdout),
    );
    Ok(())
}

/// A held flock whose control socket answers nothing within the probe
/// window is a stale session: `bookrack status` exits 3. The fixture
/// holds the flock from the test process itself — the stale verdict
/// exists only while the lock is genuinely held.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_with_a_stale_lock_exits_three() -> Result<()> {
    use std::io::Write;

    use fs2::FileExt;

    let sandbox = Sandbox::new();
    let lock_path = sandbox.tty_lock_path();
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    holder.try_lock_exclusive()?;
    let mut writer = &holder;
    writeln!(writer, "pid=999999")?;
    writeln!(writer, "mcp=disabled")?;
    writeln!(
        writer,
        "control_sock={}",
        sandbox.runtime_dir().join("no-such-control.sock").display()
    )?;
    writer.flush()?;

    let output = tokio::process::Command::from(bookrack_cmd!(&sandbox).build())
        .args(["status"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(3),
        "a held lock with a dead control plane is stale (exit 3); stderr={stderr:?}",
    );
    assert!(
        stderr.contains("stale"),
        "the error should name the stale lock: {stderr}",
    );
    drop(holder);
    Ok(())
}

/// `libraries list` resolves locally: with no daemon running it still
/// renders every registry entry and exits 0, rather than the
/// daemon-not-running code 2. A mixed registry — legacy bare-path and
/// entry-table forms side by side — lists in full, with the legacy
/// entry's kind defaulting to `prod`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_list_renders_the_registry_offline() -> Result<()> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    std::fs::write(
        &registry_path,
        "default = \"alpha\"\n\
         [libraries]\n\
         alpha = \"/roots/alpha\"\n\
         [libraries.beta]\n\
         data_dir = \"/roots/beta\"\n\
         kind = \"test\"\n",
    )?;
    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "list"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "libraries list should render offline and exit 0; stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for needle in ["alpha", "/roots/alpha", "beta", "/roots/beta", "test"] {
        assert!(
            stdout.contains(needle),
            "list output missing {needle:?}: {stdout}",
        );
    }
    // Listing is read-only: the legacy entry must survive unrewritten.
    let written = std::fs::read_to_string(&registry_path)?;
    assert!(
        written.contains("alpha = \"/roots/alpha\""),
        "list must not rewrite the registry: {written}",
    );
    Ok(())
}

/// `libraries default` resolves locally: with no daemon running it
/// still writes the on-disk registry default and exits 0, rather than
/// the daemon-not-running code 2. A legacy bare-path registry is
/// rewritten into the entry-table form in the process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_default_writes_the_registry_offline() -> Result<()> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    std::fs::write(
        &registry_path,
        "default = \"alpha\"\n\
         [libraries]\n\
         alpha = \"/roots/alpha\"\n\
         beta = \"/roots/beta\"\n",
    )?;
    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "default", "beta"])
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

/// `libraries config --unset index_profile` clears the reference from
/// every site that can hold one — the manifest that owns it, plus a
/// legacy `config.toml` declaration and the registry's cached copy — so
/// `index-profile current` afterwards reports no profile instead of
/// resolving a leftover.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_config_unset_index_profile_clears_every_reference_site() -> Result<()> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let data_dir = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    std::fs::write(
        &registry_path,
        format!(
            "[libraries.alpha]\n\
             data_dir = \"{}\"\n\
             kind = \"test\"\n\
             index_profile = \"qwen3-0.6b-default\"\n",
            data_dir.path().display()
        ),
    )?;
    std::fs::write(
        data_dir.path().join("config.toml"),
        "index_profile = \"qwen3-0.6b-default\"\n",
    )?;
    std::fs::write(
        data_dir.path().join("bookrack-library.toml"),
        "format = \"bookrack-library\"\n\
         format_version = 1\n\
         uuid = \"01890a5d-0000-7000-8000-00000000000e\"\n\
         name = \"alpha\"\n\
         kind = \"test\"\n\
         index_profile = \"qwen3-0.6b-default\"\n",
    )?;
    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "config", "alpha", "--unset", "index_profile"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "unset should succeed offline; stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("unset index_profile (library manifest)"),
        "stdout should name the site that owns the reference: {stdout}",
    );
    let manifest_written = std::fs::read_to_string(data_dir.path().join("bookrack-library.toml"))?;
    assert!(
        !manifest_written.contains("index_profile"),
        "the manifest still records the profile: {manifest_written}",
    );
    assert!(
        manifest_written.contains("uuid = \"01890a5d-0000-7000-8000-00000000000e\""),
        "clearing the profile must leave the identity intact: {manifest_written}",
    );
    let registry_written = std::fs::read_to_string(&registry_path)?;
    assert!(
        !registry_written.contains("index_profile"),
        "registry entry still records the profile: {registry_written}",
    );
    let config_written = std::fs::read_to_string(data_dir.path().join("config.toml"))?;
    assert!(
        !config_written.contains("index_profile"),
        "config.toml still records the profile: {config_written}",
    );
    // The reference is gone from both sites, so `current` reports none.
    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["index-profile", "current", "--library", "alpha"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "current should resolve offline; stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("profile: none"),
        "current should report no profile after the unset: {stdout}",
    );
    Ok(())
}

/// `libraries config <name> index_profile=<p>` declares into the
/// manifest, refreshes the registry cache, and sweeps a superseded
/// `config.toml` declaration — the one truth write plus cache
/// maintenance, from the local verb as much as from `index-profile
/// apply`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_config_set_index_profile_declares_into_the_manifest() -> Result<()> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let data_dir = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    std::fs::write(
        &registry_path,
        format!(
            "[libraries.alpha]\n\
             data_dir = \"{}\"\n\
             kind = \"test\"\n",
            data_dir.path().display()
        ),
    )?;
    // A root declared the old way: config.toml only, no manifest. The
    // second key is an unrelated preference the sweep must not touch.
    std::fs::write(
        data_dir.path().join("config.toml"),
        "index_profile = \"qwen3-0.6b-default\"\nollama_url = \"http://127.0.0.1:11434\"\n",
    )?;

    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args([
        "libraries",
        "config",
        "alpha",
        "index_profile=qwen3-0.6b-default",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "set should succeed offline; stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // The truth: a manifest minted for the root, carrying the reference.
    let manifest = std::fs::read_to_string(data_dir.path().join("bookrack-library.toml"))?;
    assert!(
        manifest.contains("index_profile = \"qwen3-0.6b-default\""),
        "the manifest should own the reference: {manifest}",
    );
    assert!(manifest.contains("name = \"alpha\""), "{manifest}");
    // The cache: refreshed to match.
    let registry_written = std::fs::read_to_string(&registry_path)?;
    assert!(
        registry_written.contains("index_profile = \"qwen3-0.6b-default\""),
        "the registry cache should be refreshed: {registry_written}",
    );
    // The superseded declaration: swept, leaving unrelated keys alone.
    let config_written = std::fs::read_to_string(data_dir.path().join("config.toml"))?;
    assert!(
        !config_written.contains("index_profile"),
        "the superseded config.toml declaration should be swept: {config_written}",
    );
    assert!(
        config_written.contains("ollama_url"),
        "sweeping must not disturb other keys: {config_written}",
    );

    // `current` reads it back from its new home, with nothing drifted.
    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["index-profile", "current", "--library", "alpha", "--json"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "current should resolve offline; stderr={:?}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(stdout.trim())?;
    assert_eq!(value["profile"]["origin"], "manifest", "{stdout}");
    assert_eq!(value["drift"], serde_json::json!([]), "{stdout}");
    Ok(())
}

/// A registry entry naming a different profile than the manifest is
/// drift, not a conflict: the manifest wins, `current` exits zero and
/// reports the stale copy, and `libraries scan` refreshes it away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_registry_reference_is_reported_as_drift_and_scan_repairs_it() -> Result<()> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let holder = tempfile::tempdir()?;
    let data_dir = holder.path().join("alpha");
    std::fs::create_dir(&data_dir)?;
    let registry_path = registry_dir.path().join("registry.toml");
    // The cache disagrees with the manifest — an entry left behind by a
    // profile change that never refreshed it.
    std::fs::write(
        &registry_path,
        format!(
            "[libraries.alpha]\n\
             data_dir = \"{}\"\n\
             kind = \"test\"\n\
             index_profile = \"qwen3-4b-quality\"\n",
            data_dir.display()
        ),
    )?;
    std::fs::write(
        data_dir.join("bookrack-library.toml"),
        "format = \"bookrack-library\"\n\
         format_version = 1\n\
         uuid = \"01890a5d-0000-7000-8000-00000000000f\"\n\
         name = \"alpha\"\n\
         kind = \"test\"\n\
         index_profile = \"qwen3-0.6b-default\"\n",
    )?;

    let current = || {
        tokio::process::Command::from(
            bookrack_cmd!(&sandbox)
                .registry(&registry_path)
                .without_data_dir()
                .build(),
        )
        .args(["index-profile", "current", "--library", "alpha", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    };

    let output = current().await?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "drift is a finding, not a failure; stderr={:?}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(stdout.trim())?;
    assert_eq!(value["profile"]["name"], "qwen3-0.6b-default", "{stdout}");
    assert_eq!(value["profile"]["origin"], "manifest", "{stdout}");
    assert_eq!(
        value["drift"],
        serde_json::json!([{"source": "registry", "stale_value": "qwen3-4b-quality"}]),
        "{stdout}",
    );

    // `scan --register` re-reads the manifests and refreshes the cache.
    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "scan"])
    .arg(holder.path())
    .arg("--register")
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "scan should succeed; stderr={:?}",
        String::from_utf8_lossy(&output.stderr),
    );

    let output = current().await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(stdout.trim())?;
    assert_eq!(
        value["drift"],
        serde_json::json!([]),
        "scan should have refreshed the stale cache: {stdout}",
    );
    Ok(())
}

/// A `libraries default` naming a library the registry does not define
/// is operator input, not a system fault: it exits 2 and does not
/// disturb the file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_default_rejects_an_unknown_name_with_exit_2() -> Result<()> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    std::fs::write(&registry_path, "[libraries]\nalpha = \"/roots/alpha\"\n")?;
    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "default", "ghost"])
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

/// How far into the binary an id got.
enum Reached {
    /// Past the grammar and into the control-plane client, which is as
    /// far as anything gets without a daemon.
    Dispatch,
    /// Refused while parsing arguments, before dispatch exists.
    Grammar,
}

/// A prefixed paper id survives the whole way to the control-plane
/// client, and a book id does not get past the grammar.
///
/// The exit code cannot tell those apart: a subcommand that routes to
/// an absent daemon exits 2, and so does a `clap` parse failure. What
/// discriminates is the wording each leaves on stderr, so the negative
/// assertions below matter as much as the positive ones — either
/// message alone would pass a test that only asked "did it fail".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_typed_paper_id_reaches_the_control_plane_client() -> Result<()> {
    let sandbox = Sandbox::new();
    let cases: &[(&str, Reached)] = &[
        // The prefixed form of an id this namespace addresses.
        ("paper:101", Reached::Dispatch),
        // The bare form the namespace has always taken.
        ("101", Reached::Dispatch),
        // Well formed, and names the catalog next door.
        ("book:12", Reached::Grammar),
    ];
    for (id, reached) in cases {
        let output = tokio::process::Command::from(bookrack_cmd!(&sandbox).build())
            .args(["papers", "show", id])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{id:?} should exit 2 either way; stderr={stderr}",
        );
        match reached {
            Reached::Dispatch => {
                assert!(
                    stderr.contains("bookrack daemon not running"),
                    "{id:?} should have reached the daemon client: {stderr}",
                );
                assert!(
                    !stderr.contains("invalid value"),
                    "{id:?} should not have been refused by the grammar: {stderr}",
                );
            }
            Reached::Grammar => {
                assert!(
                    stderr.contains("invalid value"),
                    "{id:?} should have been refused by the grammar: {stderr}",
                );
                assert!(
                    stderr.contains("papers namespace"),
                    "{id:?} should be refused for naming another catalog: {stderr}",
                );
                assert!(
                    !stderr.contains("bookrack daemon not running"),
                    "{id:?} should not have reached the daemon client: {stderr}",
                );
            }
        }
    }
    Ok(())
}

/// Write a minimal valid v1 identity manifest into `dir`.
fn write_manifest(dir: &std::path::Path, name: &str) {
    std::fs::write(
        dir.join("bookrack-library.toml"),
        format!(
            "format = \"bookrack-library\"\n\
             format_version = 1\n\
             uuid = \"01890a5d-0000-7000-8000-000000000000\"\n\
             name = \"{name}\"\n\
             kind = \"prod\"\n"
        ),
    )
    .expect("write manifest");
}

/// `libraries detect` on a manifest-bearing root resolves locally,
/// prints a confirmed verdict, and exits 0 with no daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_detect_confirms_a_manifest_root_offline() -> Result<()> {
    let sandbox = Sandbox::new();
    let root = tempfile::tempdir()?;
    write_manifest(root.path(), "alpha");
    let output = tokio::process::Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .args(["libraries", "detect"])
        .arg(root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "detect on a confirmed root should exit 0; stderr={:?}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("confirmed"),
        "stdout missing verdict: {stdout}"
    );
    Ok(())
}

/// `libraries detect` on a directory that is not a data root exits 1
/// (a determination, not the daemon-not-running code 2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_detect_on_a_plain_dir_exits_1() -> Result<()> {
    let sandbox = Sandbox::new();
    let root = tempfile::tempdir()?;
    let output = tokio::process::Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .args(["libraries", "detect"])
        .arg(root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    assert_eq!(
        output.status.code(),
        Some(1),
        "detect on a plain directory should exit 1; stderr={:?}",
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(())
}

/// `libraries detect` on a missing path is a caller-input fault: exit 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_detect_on_a_missing_path_exits_2() -> Result<()> {
    let sandbox = Sandbox::new();
    let root = tempfile::tempdir()?;
    let output = tokio::process::Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .args(["libraries", "detect"])
        .arg(root.path().join("nope"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    assert_eq!(
        output.status.code(),
        Some(2),
        "detect on a missing path should exit 2; stderr={:?}",
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(())
}

/// `libraries scan <parent>` walks a parent directory, lists the data
/// roots below it, and exits 0 offline. `--json` carries the found root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_scan_lists_child_roots_offline() -> Result<()> {
    let sandbox = Sandbox::new();
    let parent = tempfile::tempdir()?;
    let lib = parent.path().join("lib-a");
    std::fs::create_dir(&lib)?;
    write_manifest(&lib, "alpha");
    let output = tokio::process::Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .args(["--json", "libraries", "scan"])
        .arg(parent.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "scan should exit 0 offline; stderr={:?}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"confirmed\"") && stdout.contains("lib-a"),
        "scan --json should list the confirmed child root: {stdout}",
    );
    Ok(())
}

/// `libraries scan` with neither a parent nor `--volumes` is a clap
/// argument error (exit 2): exactly one target is required.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_scan_requires_a_target() -> Result<()> {
    let sandbox = Sandbox::new();
    let output = tokio::process::Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .args(["libraries", "scan"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    assert_eq!(
        output.status.code(),
        Some(2),
        "scan with no target should be a clap usage error (exit 2)",
    );
    Ok(())
}

/// Write a minimal valid v1 identity manifest with an explicit uuid, so
/// two roots can be given distinct identities.
fn write_manifest_uuid(dir: &std::path::Path, name: &str, uuid: &str) {
    std::fs::write(
        dir.join("bookrack-library.toml"),
        format!(
            "format = \"bookrack-library\"\n\
             format_version = 1\n\
             uuid = \"{uuid}\"\n\
             name = \"{name}\"\n\
             kind = \"prod\"\n"
        ),
    )
    .expect("write manifest");
}

/// `libraries scan --register` recovers a lost registry: pointed at a
/// parent of confirmed roots with no registry file present, it registers
/// each one, so a reinstall rebuilds the registry from the manifests on
/// disk in a single command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_scan_register_rebuilds_the_registry() -> Result<()> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    // The registry file does not exist yet — as after a reinstall.
    let registry_path = registry_dir.path().join("registry.toml");
    let parent = tempfile::tempdir()?;
    let a = parent.path().join("lib-a");
    let b = parent.path().join("lib-b");
    std::fs::create_dir(&a)?;
    std::fs::create_dir(&b)?;
    write_manifest_uuid(&a, "alpha", "01890a5d-0000-7000-8000-00000000000a");
    write_manifest_uuid(&b, "beta", "01890a5d-0000-7000-8000-00000000000b");
    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "scan"])
    .arg(parent.path())
    .arg("--register")
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "scan --register should exit 0 offline; stderr={:?}",
        String::from_utf8_lossy(&output.stderr),
    );
    let written = std::fs::read_to_string(&registry_path)?;
    for needle in [
        "alpha",
        "beta",
        "01890a5d-0000-7000-8000-00000000000a",
        "01890a5d-0000-7000-8000-00000000000b",
    ] {
        assert!(
            written.contains(needle),
            "rebuilt registry missing {needle:?}: {written}",
        );
    }
    // The rebuilt registry serves `libraries list` again: both roots
    // show up under their manifest names, closing the recovery loop.
    let list = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "list"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(
        list.status.code(),
        Some(0),
        "list after rebuild should exit 0; stderr={:?}",
        String::from_utf8_lossy(&list.stderr),
    );
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    for needle in ["alpha", "beta"] {
        assert!(
            list_stdout.contains(needle),
            "list after rebuild missing {needle:?}: {list_stdout}",
        );
    }
    Ok(())
}

/// `libraries register` on a read-only root cannot write the identity
/// manifest, but degrades to a uuid-less entry rather than failing, so a
/// snapshot or optical volume is still registrable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_register_degrades_on_a_read_only_root() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    let root = tempfile::tempdir()?;
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o555))?;
    // A user who can write despite the mode bits (running as root) would
    // never hit the degrade path; skip rather than assert a false state.
    if std::fs::File::create(root.path().join(".probe")).is_ok() {
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).ok();
        return Ok(());
    }
    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "register"])
    .arg(root.path())
    .args(["--name", "ro", "--yes"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    // Restore write permission so tempdir teardown can remove the root.
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).ok();
    assert_eq!(
        output.status.code(),
        Some(0),
        "a read-only root should still register (exit 0); stderr={:?}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("read-only"),
        "stderr should warn about the read-only root: {stderr}",
    );
    let written = std::fs::read_to_string(&registry_path)?;
    assert!(written.contains("ro"), "entry not recorded: {written}");
    assert!(
        !written.contains("uuid"),
        "a degraded entry must carry no uuid cache: {written}",
    );
    Ok(())
}

/// `libraries add` on a root with no identity manifest asks before it
/// writes one. With stdin closed the question cannot be answered, so
/// the run is a user error (exit 2) and leaves the target and the
/// registry untouched — a caller that reads exit 0 would believe a
/// library was registered when none was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_add_on_a_closed_stdin_is_a_user_error() -> Result<()> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    let root = tempfile::tempdir()?;
    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "add", "fresh"])
    .arg(root.path())
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(2),
        "an unanswerable manifest confirmation is a user error; stderr={stderr:?}",
    );
    assert!(
        stderr.contains("--yes"),
        "the refusal must name the way to opt in: {stderr}",
    );
    assert!(
        !root.path().join("bookrack-library.toml").exists(),
        "no manifest may be written when the confirmation went unanswered",
    );
    assert!(
        !registry_path.exists(),
        "no registry entry may be written when the confirmation went unanswered",
    );
    Ok(())
}

/// `libraries remove --purge` refuses to delete a target that no longer
/// detects as a data root, so an entry pointing at the wrong directory
/// cannot destroy it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_remove_purge_refuses_a_non_library_target() -> Result<()> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    let target = tempfile::tempdir()?;
    std::fs::write(
        &registry_path,
        format!("[libraries]\nvictim = \"{}\"\n", target.path().display()),
    )?;
    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "remove", "victim", "--purge", "--yes"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(
        output.status.code(),
        Some(2),
        "purge of a non-library target is a user error (exit 2); stderr={:?}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        target.path().exists(),
        "the gate must leave a non-library directory on disk",
    );
    Ok(())
}

/// `libraries remove --purge` on a confirmed root deletes the data and
/// forgets the entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_remove_purge_deletes_a_confirmed_root() -> Result<()> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    let holder = tempfile::tempdir()?;
    let root = holder.path().join("data");
    std::fs::create_dir(&root)?;
    write_manifest_uuid(&root, "gone", "01890a5d-0000-7000-8000-00000000000c");
    std::fs::write(
        &registry_path,
        format!("[libraries]\ngone = \"{}\"\n", root.display()),
    )?;
    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "remove", "gone", "--purge", "--yes"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "purge of a confirmed root should exit 0; stderr={:?}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(!root.exists(), "the data root should be deleted");
    let written = std::fs::read_to_string(&registry_path)?;
    assert!(
        !written.contains("gone"),
        "the entry should be forgotten: {written}",
    );
    Ok(())
}

/// What a `libraries remove --purge` run left behind.
struct PurgeRun {
    code: Option<i32>,
    root_survives: bool,
    entry_survives: bool,
    stderr: String,
}

/// A registry holding one purgeable library named `shelf`. The
/// sandbox and tempdirs are returned so the caller keeps them alive.
struct PurgeFixture {
    sandbox: Sandbox,
    _registry_dir: tempfile::TempDir,
    registry_path: std::path::PathBuf,
    _holder: tempfile::TempDir,
    root: std::path::PathBuf,
}

fn purge_fixture() -> Result<PurgeFixture> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    let holder = tempfile::tempdir()?;
    let root = holder.path().join("data");
    std::fs::create_dir(&root)?;
    write_manifest_uuid(&root, "shelf", "01890a5d-0000-7000-8000-00000000000e");
    std::fs::write(
        &registry_path,
        format!("[libraries]\nshelf = \"{}\"\n", root.display()),
    )?;
    Ok(PurgeFixture {
        sandbox,
        _registry_dir: registry_dir,
        registry_path,
        _holder: holder,
        root,
    })
}

/// Spawn `libraries remove shelf --purge` against `fixture` with stdin
/// on a pipe the caller drives. `timeout_secs` sets
/// `BOOKRACK_CONFIRM_TIMEOUT_SECS` when present.
fn spawn_purge(
    fixture: &PurgeFixture,
    timeout_secs: Option<&str>,
) -> Result<tokio::process::Child> {
    let mut spawn = bookrack_cmd!(&fixture.sandbox)
        .registry(&fixture.registry_path)
        .without_data_dir()
        .stdin_pipe();
    if let Some(secs) = timeout_secs {
        // `CONFIRM_TIMEOUT_ENV` lives in the `bookrack` binary crate,
        // which has no library target, so the name reaches the child
        // as free text rather than as the constant.
        spawn = spawn.extra_env("BOOKRACK_CONFIRM_TIMEOUT_SECS", secs);
    }
    let mut command = tokio::process::Command::from(spawn.build());
    command
        .args(["libraries", "remove", "shelf", "--purge"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command.spawn()?)
}

fn purge_outcome(fixture: &PurgeFixture, output: std::process::Output) -> Result<PurgeRun> {
    let entry_survives = std::fs::read_to_string(&fixture.registry_path)?.contains("shelf");
    Ok(PurgeRun {
        code: output.status.code(),
        root_survives: fixture.root.exists(),
        entry_survives,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Drive `libraries remove --purge` without `--yes`, answering the
/// retype prompt with `typed`. Writing `typed` and dropping the handle
/// closes the pipe, so `""` is a stdin that reaches end of file before
/// any byte arrives.
async fn purge_answering(typed: &str) -> Result<PurgeRun> {
    use tokio::io::AsyncWriteExt;

    let fixture = purge_fixture()?;
    let mut child = spawn_purge(&fixture, None)?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(typed.as_bytes())
        .await?;
    let output = child.wait_with_output().await?;
    purge_outcome(&fixture, output)
}

/// The Hard retype gate, end to end: an answer that is not the library
/// name leaves the data root and the registry entry exactly where they
/// were. The `--yes` cases above skip this path entirely, so this is
/// the one that proves the typed token is compared at all.
///
/// The bare newline is the load-bearing member of the list: an empty
/// *line* is an answer, and answering declines. An empty *stream* is
/// not an answer at all and is covered by the test below.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_remove_purge_keeps_the_data_when_the_retype_misses() -> Result<()> {
    for typed in ["yes\n", "y\n", "SHELF\n", "shel\n", "\n"] {
        let run = purge_answering(typed).await?;
        assert_eq!(
            run.code,
            Some(0),
            "a declined purge is a clean abort, not a failure; answer={typed:?}",
        );
        assert!(
            run.root_survives,
            "a mistyped confirmation must leave the data root on disk; answer={typed:?}",
        );
        assert!(
            run.entry_survives,
            "a mistyped confirmation must leave the registry entry; answer={typed:?}",
        );
    }
    Ok(())
}

/// A stdin that ends before any byte arrives cannot carry a
/// confirmation, so the purge is a user error (exit 2) rather than the
/// clean abort a typed-in decline earns. Anything else lets a cron or
/// systemd caller — whose stdin is `/dev/null` — read success out of a
/// run that deleted nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_remove_purge_on_a_closed_stdin_is_a_user_error() -> Result<()> {
    let run = purge_answering("").await?;
    assert_eq!(
        run.code,
        Some(2),
        "an unanswerable confirmation is a user error, not a clean abort; stderr={:?}",
        run.stderr,
    );
    assert!(
        run.root_survives,
        "an unanswerable confirmation must leave the data root on disk",
    );
    assert!(
        run.entry_survives,
        "an unanswerable confirmation must leave the registry entry",
    );
    assert!(
        run.stderr.contains("shelf") && run.stderr.contains("--yes"),
        "the refusal must name the library and the way to opt in: {:?}",
        run.stderr,
    );
    Ok(())
}

/// A pipe that stays open but never carries an answer used to block
/// `read_line` forever — and `--purge` takes the data root's exclusive
/// lock *before* it prompts, so a stuck run locked the library out of
/// the daemon until someone killed it. On a non-terminal stdin the read
/// is bounded: the window expires, the run is a user error, and the
/// lock is released on the way out.
///
/// The wait is wrapped so a regression fails the test instead of
/// hanging the suite; "seen red" must not degenerate into "seen hung".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_remove_purge_gives_up_on_a_silent_stdin() -> Result<()> {
    let fixture = purge_fixture()?;
    let mut child = spawn_purge(&fixture, Some("1"))?;
    // Hold the write end open for the whole run: the child sees a pipe
    // with a live writer that never sends a byte, which is what a
    // supervisor spawning with an inherited-but-idle stdin produces.
    let held_stdin = child.stdin.take().expect("piped stdin");
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("the CLI must give up on a silent stdin rather than block forever")?;
    drop(held_stdin);

    let run = purge_outcome(&fixture, output)?;
    assert_eq!(
        run.code,
        Some(2),
        "a confirmation nobody answered is a user error; stderr={:?}",
        run.stderr,
    );
    assert!(run.root_survives, "an expired window must not purge");
    assert!(
        run.entry_survives,
        "an expired window must leave the registry entry"
    );
    assert!(
        !bookrack_session::RootLock::acquire(&fixture.root, std::process::id(), "test")
            .is_err_and(|e| bookrack_session::is_root_lock_conflict(&e)),
        "the data root lock must be released when the run gives up",
    );
    Ok(())
}

/// The other side of the bound: an answer that arrives late over a
/// pipe is still honoured. A caller is not required to have a terminal
/// to confirm — `ssh host …` without `-t` and `docker exec` without
/// `-t` both put a human behind a pipe — so the window must expire on
/// silence, never on the mere absence of a TTY.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_remove_purge_honours_an_answer_that_arrives_late_over_a_pipe() -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let fixture = purge_fixture()?;
    let mut child = spawn_purge(&fixture, Some("10"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    stdin.write_all(b"shelf\n").await?;
    drop(stdin);
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("a late answer must still be read")?;

    let run = purge_outcome(&fixture, output)?;
    assert_eq!(
        run.code,
        Some(0),
        "an answer that arrives inside the window purges; stderr={:?}",
        run.stderr,
    );
    assert!(
        !run.root_survives,
        "the retyped name must purge even when it arrives a second late"
    );
    Ok(())
}

/// The same gate from the other side: the library name retyped exactly
/// does purge, so the test above pins a real comparison rather than a
/// path that never deletes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_remove_purge_deletes_when_the_name_is_retyped() -> Result<()> {
    let run = purge_answering("shelf\n").await?;
    assert_eq!(run.code, Some(0), "an accepted purge exits 0");
    assert!(
        !run.root_survives,
        "the retyped name must purge the data root"
    );
    assert!(
        !run.entry_survives,
        "the retyped name must forget the entry"
    );
    Ok(())
}

/// `libraries remove --purge` refuses a data root another writer holds:
/// the data survives and the registry entry stays, so the operator can
/// retry once the holder is stopped (exit 2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_remove_purge_refuses_a_root_in_use() -> Result<()> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    let holder_dir = tempfile::tempdir()?;
    let root = holder_dir.path().join("data");
    std::fs::create_dir(&root)?;
    write_manifest_uuid(&root, "busy", "01890a5d-0000-7000-8000-00000000000d");
    std::fs::write(
        &registry_path,
        format!("[libraries]\nbusy = \"{}\"\n", root.display()),
    )?;
    let held = RootLock::acquire(&root, std::process::id(), "daemon")?;

    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "remove", "busy", "--purge", "--yes"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(2),
        "purge of a root in use is a user error (exit 2); stderr={stderr:?}",
    );
    assert!(
        stderr.contains("already in use"),
        "the refusal should name the conflict: {stderr:?}",
    );
    assert!(root.exists(), "the data root must survive a refused purge");
    let written = std::fs::read_to_string(&registry_path)?;
    assert!(
        written.contains("busy"),
        "the entry must survive a refused purge: {written}",
    );

    drop(held);
    Ok(())
}

/// `libraries register` refuses a derived name that already belongs to a
/// different library: the operator must pick an explicit alias (exit 2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_register_rejects_a_derived_name_clash() -> Result<()> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    let first = tempfile::tempdir()?;
    let second = tempfile::tempdir()?;
    write_manifest_uuid(first.path(), "dup", "01890a5d-0000-7000-8000-000000000001");
    write_manifest_uuid(second.path(), "dup", "01890a5d-0000-7000-8000-000000000002");
    let register = |root: &std::path::Path| {
        tokio::process::Command::from(
            bookrack_cmd!(&sandbox)
                .registry(&registry_path)
                .without_data_dir()
                .build(),
        )
        .args(["libraries", "register"])
        .arg(root)
        .arg("--yes")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    };
    let first_out = register(first.path()).await?;
    assert_eq!(
        first_out.status.code(),
        Some(0),
        "the first register should succeed; stderr={:?}",
        String::from_utf8_lossy(&first_out.stderr),
    );
    let second_out = register(second.path()).await?;
    assert_eq!(
        second_out.status.code(),
        Some(2),
        "a derived-name clash is a user error (exit 2); stderr={:?}",
        String::from_utf8_lossy(&second_out.stderr),
    );
    let stderr = String::from_utf8_lossy(&second_out.stderr);
    assert!(
        stderr.contains("already"),
        "stderr should explain the name clash: {stderr}",
    );
    Ok(())
}

/// `libraries config <name> KEY=VALUE` resolves the root from the
/// registry offline, edits its `config.toml` in place preserving a
/// hand-written comment, and notes that the change reaches a running
/// daemon only on restart. A subsequent no-pair invocation dumps the
/// whole file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_config_edits_root_config_offline() -> Result<()> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let root = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    std::fs::write(
        &registry_path,
        format!(
            "[libraries.prod]\ndata_dir = {}\n",
            toml_escape(root.path()),
        ),
    )?;
    // A hand-written comment the edit must not clobber.
    std::fs::write(
        root.path().join("config.toml"),
        "# operator note: leave this here\nollama_url = \"http://old:11434\"\n",
    )?;

    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "config", "prod", "ollama_url=http://new:11434"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "config edit should resolve offline and exit 0; stderr={:?}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("restart the daemon"),
        "a write should note the daemon restart: {stderr}",
    );
    let written = std::fs::read_to_string(root.path().join("config.toml"))?;
    assert!(
        written.contains("# operator note: leave this here"),
        "the hand-written comment was clobbered: {written}",
    );
    assert!(
        written.contains("http://new:11434") && !written.contains("http://old:11434"),
        "the key was not updated: {written}",
    );

    // No pairs: dump the file verbatim, comment included.
    let dump = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "config", "prod"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(dump.status.code(), Some(0));
    let dump_out = String::from_utf8_lossy(&dump.stdout);
    assert!(
        dump_out.contains("# operator note: leave this here")
            && dump_out.contains("http://new:11434"),
        "the dump should print the whole file: {dump_out}",
    );
    Ok(())
}

/// `libraries config` rejects a key outside the whitelist with exit 2
/// (operator input) and leaves the file untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_config_rejects_an_unknown_key_with_exit_2() -> Result<()> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let root = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    std::fs::write(
        &registry_path,
        format!(
            "[libraries.prod]\ndata_dir = {}\n",
            toml_escape(root.path()),
        ),
    )?;
    let output = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "config", "prod", "not_a_key=1"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(
        output.status.code(),
        Some(2),
        "an unknown key is operator input (exit 2); stderr={:?}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        !root.path().join("config.toml").exists(),
        "a rejected batch must not create the file",
    );
    Ok(())
}

/// A `config.toml` left over from a release that still honoured
/// `embed_model` is refused by name, not ignored: silently dropping it
/// would change which model a write path resolves without the operator
/// seeing it. The refusal reaches the operator the same way any other
/// unusable root config does — `doctor` renders it as a failing row
/// carrying the way out — and following that way out makes the root
/// usable again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_root_config_with_a_retired_key_is_refused_until_the_line_goes() -> Result<()> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let root = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    std::fs::write(
        &registry_path,
        format!(
            "[libraries.prod]\ndata_dir = {}\n",
            toml_escape(root.path()),
        ),
    )?;
    std::fs::write(
        root.path().join("config.toml"),
        "ollama_url = \"http://127.0.0.1:11434\"\nembed_model = \"qwen3-embedding:0.6b\"\n",
    )?;

    let doctor = || async {
        tokio::process::Command::from(
            bookrack_cmd!(&sandbox)
                .registry(&registry_path)
                .without_data_dir()
                .build(),
        )
        .args(["--library", "prod", "doctor"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
    };

    // The root does not resolve, so `doctor` reports it unhealthy and
    // names both the key and the way out.
    let refused = doctor().await?;
    assert_eq!(
        refused.status.code(),
        Some(1),
        "an unusable root config is a self-reported unhealthy doctor (exit 1)",
    );
    let report = String::from_utf8_lossy(&refused.stdout);
    assert!(
        report.contains("data root") && report.contains("FAIL"),
        "the data-root row must fail: {report}",
    );
    assert!(
        report.contains("embed_model"),
        "the refusal must name the key: {report}",
    );
    assert!(
        report.contains("--unset embed_model"),
        "the refusal must carry the way out: {report}",
    );

    // The way out the refusal prescribes works while the stale line is
    // still there -- `libraries config` resolves the root from the
    // registry and edits the file as text, so the cure cannot be
    // blocked by the disease.
    let unset = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "config", "prod", "--unset", "embed_model"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(
        unset.status.code(),
        Some(0),
        "unsetting a retired key must succeed; stderr={:?}",
        String::from_utf8_lossy(&unset.stderr),
    );
    let written = std::fs::read_to_string(root.path().join("config.toml"))?;
    assert!(!written.contains("embed_model"), "{written}");
    assert!(
        written.contains("ollama_url"),
        "the rest of the file survives: {written}"
    );

    // The root resolves again: the data-root row no longer fails.
    let cured = doctor().await?;
    let report = String::from_utf8_lossy(&cured.stdout);
    assert!(
        !report.contains("retired key"),
        "the refusal must be gone once the line is: {report}",
    );

    // And setting it back is refused: the key no longer exists.
    let reset = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "config", "prod", "embed_model=whatever"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(
        reset.status.code(),
        Some(2),
        "a retired key is not a settable key (exit 2); stderr={:?}",
        String::from_utf8_lossy(&reset.stderr),
    );
    Ok(())
}

/// The two process-level keys are refused the same way: a `config.toml`
/// carrying `mcp_addr` or `log_directive` fails every command that
/// resolves the root, and the verbatim dump — the one read surface a
/// retired line survives, because it never parses the document — says so
/// instead of handing back a line that resolves to nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_root_config_with_a_process_level_key_is_refused_and_annotated() -> Result<()> {
    let sandbox = Sandbox::new();
    let registry_dir = tempfile::tempdir()?;
    let root = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    std::fs::write(
        &registry_path,
        format!(
            "[libraries.prod]\ndata_dir = {}\n",
            toml_escape(root.path()),
        ),
    )?;
    std::fs::write(
        root.path().join("config.toml"),
        "ollama_url = \"http://127.0.0.1:11434\"\nmcp_addr = \"127.0.0.1:9999\"\n\
         log_directive = \"debug\"\n",
    )?;

    let doctor = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["--library", "prod", "doctor"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(
        doctor.status.code(),
        Some(1),
        "an unusable root config is a self-reported unhealthy doctor (exit 1)",
    );
    let report = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        report.contains("mcp_addr") && report.contains("BOOKRACK_MCP_ADDR"),
        "the refusal must name the key and its real home: {report}",
    );

    // The verbatim dump still prints the file -- an operator has to see
    // what to delete -- and annotates each retired line on stderr.
    let dump = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["libraries", "config", "prod"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(dump.status.code(), Some(0));
    let dumped = String::from_utf8_lossy(&dump.stdout);
    assert!(
        dumped.contains("mcp_addr") && dumped.contains("log_directive"),
        "the dump prints the file verbatim: {dumped}",
    );
    let notes = String::from_utf8_lossy(&dump.stderr);
    assert!(
        notes.contains("`mcp_addr` is retired") && notes.contains("BOOKRACK_MCP_ADDR"),
        "the dump must annotate a retired line: {notes}",
    );
    assert!(
        notes.contains("`log_directive` is retired") && notes.contains("BOOKRACK_LOG"),
        "both retired lines are annotated: {notes}",
    );

    // Both lines go in one invocation, and the root resolves again.
    let unset = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args([
        "libraries",
        "config",
        "prod",
        "--unset",
        "mcp_addr",
        "--unset",
        "log_directive",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert_eq!(
        unset.status.code(),
        Some(0),
        "unsetting retired keys must succeed; stderr={:?}",
        String::from_utf8_lossy(&unset.stderr),
    );
    let written = std::fs::read_to_string(root.path().join("config.toml"))?;
    assert!(!written.contains("mcp_addr"), "{written}");
    assert!(!written.contains("log_directive"), "{written}");
    assert!(
        written.contains("ollama_url"),
        "the rest of the file survives: {written}"
    );

    let cured = tokio::process::Command::from(
        bookrack_cmd!(&sandbox)
            .registry(&registry_path)
            .without_data_dir()
            .build(),
    )
    .args(["--library", "prod", "doctor"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .await?;
    assert!(
        !String::from_utf8_lossy(&cured.stdout).contains("retired key"),
        "the refusal must be gone once the lines are",
    );
    Ok(())
}

/// Render a path as a TOML basic string for a registry `data_dir` value.
/// Test paths from `tempfile` carry no quotes or backslashes on unix, so
/// wrapping in quotes is sufficient here.
fn toml_escape(path: &std::path::Path) -> String {
    format!("\"{}\"", path.display())
}

enum CaseExpect {
    /// Routes through the control plane: exit 2 and the tip naming
    /// `bookrack run`.
    NotRunning,
    /// `bookrack quit` has nothing to stop: exit 0, said on stderr.
    Quit,
}
