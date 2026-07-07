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
    let runtime_dir = tempfile::tempdir()?;
    let root = tempfile::tempdir()?;
    write_manifest(root.path(), "alpha");
    let output = tokio::process::Command::new(bookrack_bin())
        .args(["libraries", "detect"])
        .arg(root.path())
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
        .env_remove("BOOKRACK_DATA_DIR")
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
    let runtime_dir = tempfile::tempdir()?;
    let root = tempfile::tempdir()?;
    let output = tokio::process::Command::new(bookrack_bin())
        .args(["libraries", "detect"])
        .arg(root.path())
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
        .env_remove("BOOKRACK_DATA_DIR")
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
    let runtime_dir = tempfile::tempdir()?;
    let root = tempfile::tempdir()?;
    let output = tokio::process::Command::new(bookrack_bin())
        .args(["libraries", "detect"])
        .arg(root.path().join("nope"))
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
        .env_remove("BOOKRACK_DATA_DIR")
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
    let runtime_dir = tempfile::tempdir()?;
    let parent = tempfile::tempdir()?;
    let lib = parent.path().join("lib-a");
    std::fs::create_dir(&lib)?;
    write_manifest(&lib, "alpha");
    let output = tokio::process::Command::new(bookrack_bin())
        .args(["--json", "libraries", "scan"])
        .arg(parent.path())
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
        .env_remove("BOOKRACK_DATA_DIR")
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
    let runtime_dir = tempfile::tempdir()?;
    let output = tokio::process::Command::new(bookrack_bin())
        .args(["libraries", "scan"])
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
        .env_remove("BOOKRACK_DATA_DIR")
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
    let runtime_dir = tempfile::tempdir()?;
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
    let output = tokio::process::Command::new(bookrack_bin())
        .args(["libraries", "scan"])
        .arg(parent.path())
        .arg("--register")
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
    Ok(())
}

/// `libraries register` on a read-only root cannot write the identity
/// manifest, but degrades to a uuid-less entry rather than failing, so a
/// snapshot or optical volume is still registrable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_register_degrades_on_a_read_only_root() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let runtime_dir = tempfile::tempdir()?;
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
    let output = tokio::process::Command::new(bookrack_bin())
        .args(["libraries", "register"])
        .arg(root.path())
        .args(["--name", "ro", "--yes"])
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
        .env("BOOKRACK_REGISTRY", &registry_path)
        .env_remove("BOOKRACK_DATA_DIR")
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

/// `libraries remove --purge` refuses to delete a target that no longer
/// detects as a data root, so an entry pointing at the wrong directory
/// cannot destroy it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_remove_purge_refuses_a_non_library_target() -> Result<()> {
    let runtime_dir = tempfile::tempdir()?;
    let registry_dir = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    let target = tempfile::tempdir()?;
    std::fs::write(
        &registry_path,
        format!("[libraries]\nvictim = \"{}\"\n", target.path().display()),
    )?;
    let output = tokio::process::Command::new(bookrack_bin())
        .args(["libraries", "remove", "victim", "--purge", "--yes"])
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
    let runtime_dir = tempfile::tempdir()?;
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
    let output = tokio::process::Command::new(bookrack_bin())
        .args(["libraries", "remove", "gone", "--purge", "--yes"])
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

/// `libraries register` refuses a derived name that already belongs to a
/// different library: the operator must pick an explicit alias (exit 2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_register_rejects_a_derived_name_clash() -> Result<()> {
    let runtime_dir = tempfile::tempdir()?;
    let registry_dir = tempfile::tempdir()?;
    let registry_path = registry_dir.path().join("registry.toml");
    let first = tempfile::tempdir()?;
    let second = tempfile::tempdir()?;
    write_manifest_uuid(first.path(), "dup", "01890a5d-0000-7000-8000-000000000001");
    write_manifest_uuid(second.path(), "dup", "01890a5d-0000-7000-8000-000000000002");
    let register = |root: &std::path::Path| {
        tokio::process::Command::new(bookrack_bin())
            .args(["libraries", "register"])
            .arg(root)
            .arg("--yes")
            .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
            .env("BOOKRACK_REGISTRY", &registry_path)
            .env_remove("BOOKRACK_DATA_DIR")
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

enum CaseExpect {
    NotRunning,
    Quit,
}
