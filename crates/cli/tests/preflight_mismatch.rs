// SPDX-License-Identifier: Apache-2.0

//! Library-mismatch pre-flight contract against the session lock: the
//! check trusts the lock file's recorded identity only while some
//! process holds the flock. A dead session's leftover content must
//! fall through to the ordinary daemon-not-running path instead of
//! refusing the command, while a held lock still refuses a routed
//! command whose explicit selection names a different library.
//!
//! Every case runs against a registry that names both `asked` and
//! `served`, so the selection the tests pass is one the resolver can
//! actually satisfy. Without it, a command that died in resolution
//! would satisfy the negative assertions ("did not refuse") for
//! entirely the wrong reason.

#![cfg(unix)]

use std::process::Output;

use bookrack_test_support::{Sandbox, bookrack_cmd};

/// A sandbox whose registry names the library the tests select and the
/// one the lock claims to serve.
fn world() -> Sandbox {
    let sandbox = Sandbox::new();
    let asked = sandbox.data_root("asked");
    let served = sandbox.data_root("served");
    sandbox.write_registry_entries(
        None,
        &[("asked", asked.as_path()), ("served", served.as_path())],
    );
    sandbox
}

fn run_command(sandbox: &Sandbox, args: &[&str]) -> Output {
    let mut full = vec!["--library", "asked"];
    full.extend_from_slice(args);
    bookrack_cmd!(sandbox)
        .without_data_dir()
        .build()
        .args(&full)
        .output()
        .expect("spawn bookrack")
}

fn run_routed_command(sandbox: &Sandbox) -> Output {
    run_command(sandbox, &["diagnose"])
}

/// Take the flock on the lock file, seed it with `library_name=served`
/// content, and hand the held file back so the daemon it stands in for
/// stays "alive" for the duration of the test.
fn hold_mismatched_lock(sandbox: &Sandbox) -> std::fs::File {
    use std::io::Write;

    use fs2::FileExt;

    let mut holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(sandbox.tty_lock_path())
        .unwrap();
    holder.try_lock_exclusive().unwrap();
    holder.write_all(lock_content(sandbox).as_bytes()).unwrap();
    holder.flush().unwrap();
    holder
}

/// Lock contents for a daemon serving `served` — the registry entry of
/// that name, so the identity the lock claims is one the registry
/// agrees exists.
fn lock_content(sandbox: &Sandbox) -> String {
    format!(
        "pid=999999\nmcp=disabled\ncontrol_sock={}\ndata_dir={}\nlibrary_name=served\n",
        sandbox.runtime_dir().join("no-such-control.sock").display(),
        sandbox.data_root("served").display(),
    )
}

/// Nothing in the failure text may suggest the command died before the
/// pre-flight ran: a selection that does not resolve produces the same
/// silence about "refusing to act" as a selection the check waved
/// through.
fn assert_the_selection_resolved(stderr: &str) {
    for wrong_reason in ["no library named", "--library needs a registry"] {
        assert!(
            !stderr.contains(wrong_reason),
            "`asked` must resolve, or the test proves nothing: {stderr}",
        );
    }
}

#[test]
fn leftover_lock_content_without_a_holder_does_not_refuse() {
    let sandbox = world();
    std::fs::write(sandbox.tty_lock_path(), lock_content(&sandbox)).unwrap();

    let out = run_routed_command(&sandbox);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_the_selection_resolved(&stderr);
    assert!(
        !stderr.contains("refusing to act"),
        "stale lock content must not trip the mismatch check: {stderr}"
    );
    assert_eq!(out.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains("daemon not running"),
        "expected the ordinary not-running path: {stderr}"
    );
}

#[test]
fn held_lock_still_refuses_a_differently_named_selection() {
    let sandbox = world();
    let holder = hold_mismatched_lock(&sandbox);

    let out = run_routed_command(&sandbox);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains("refusing to act on library asked"),
        "expected the mismatch refusal: {stderr}"
    );
    assert!(stderr.contains("library served"), "stderr: {stderr}");
    drop(holder);
}

/// `doctor` is not on the exemption list: with a daemon holding the
/// lock on a differently named library, the pre-flight must refuse
/// before `doctor` self-resolves, or it would silently report on the
/// served library instead of the one the selection named.
///
/// The unlocked run comes first and establishes what the refusal has
/// to beat: `--library asked` resolves and `doctor` reports on that
/// root. The refusal is therefore shown to arrive ahead of a
/// resolution that would otherwise have succeeded, which is the whole
/// claim.
#[test]
fn held_lock_refuses_doctor_because_it_is_not_exempt() {
    let sandbox = world();

    let unlocked = run_command(&sandbox, &["doctor"]);
    let unlocked_stderr = String::from_utf8_lossy(&unlocked.stderr);
    assert_the_selection_resolved(&unlocked_stderr);
    assert!(
        !unlocked_stderr.contains("refusing to act"),
        "nothing holds the lock, so nothing may refuse: {unlocked_stderr}"
    );
    let unlocked_stdout = String::from_utf8_lossy(&unlocked.stdout);
    assert!(
        unlocked_stdout.contains(&sandbox.data_root("asked").display().to_string()),
        "doctor must report on the selected root when it is free to: {unlocked_stdout}"
    );

    let holder = hold_mismatched_lock(&sandbox);
    let out = run_command(&sandbox, &["doctor"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains("refusing to act on library asked"),
        "doctor must hit the mismatch refusal: {stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout)
            .contains(&sandbox.data_root("served").display().to_string()),
        "the refusal must land before any report on the served library",
    );
    drop(holder);
}

/// `retrieval` resolves its data root locally and never routes through
/// the daemon, so it is on the exemption list: even a held lock naming
/// a different library must not trip the mismatch refusal. It reaches
/// its own local resolution instead, and reports the explicit zero for
/// a root that has no catalog.
#[test]
fn held_lock_does_not_refuse_exempt_retrieval() {
    let sandbox = world();
    let holder = hold_mismatched_lock(&sandbox);

    let out = run_command(&sandbox, &["retrieval", "list"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_the_selection_resolved(&stderr);
    assert!(
        !stderr.contains("refusing to act"),
        "retrieval is exempt and must not trip the mismatch check: {stderr}"
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "the exempt command must run to completion: {stderr}"
    );
    assert!(
        stdout.contains("No retrieval calls."),
        "the exempt command must reach its own local resolution and report \
         the empty `asked` root: stdout={stdout:?} stderr={stderr}",
    );
    drop(holder);
}

/// A lock the check cannot examine at all is a different state from a
/// lock nobody holds, and only one of them is ordinary.
///
/// Nobody holding the lock is what every machine without a daemon looks
/// like, and it stays silent. A lock that cannot be opened means the
/// comparison did not happen — the command runs anyway, because an
/// unreadable lock is no evidence that a daemon is serving something
/// else, but a check that silently did not run is exactly what this
/// module exists to prevent.
#[test]
fn an_unreadable_lock_says_the_check_did_not_run() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = world();
    let lock = sandbox.tty_lock_path();
    std::fs::write(&lock, lock_content(&sandbox)).unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o000)).unwrap();
    // A user who can read it despite the mode bits (running as root)
    // never reaches the branch under test; restore and skip rather than
    // assert a state this process cannot produce.
    if std::fs::read(&lock).is_ok() {
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).ok();
        eprintln!("skipping unreadable-lock test: this user can read a mode-000 file");
        return;
    }

    let out = run_routed_command(&sandbox);
    let stderr = String::from_utf8_lossy(&out.stderr);
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).ok();

    assert!(
        stderr.contains("could not read the session lock"),
        "a check that could not run must say so: {stderr}"
    );
    assert!(
        !stderr.contains("refusing to act"),
        "an unreadable lock is not evidence of a mismatch: {stderr}"
    );
}
