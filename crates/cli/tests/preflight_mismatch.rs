// SPDX-License-Identifier: Apache-2.0

//! Library-mismatch pre-flight contract against the session lock: the
//! check trusts the lock file's recorded identity only while some
//! process holds the flock. A dead session's leftover content must
//! fall through to the ordinary daemon-not-running path instead of
//! refusing the command, while a held lock still refuses a routed
//! command whose explicit selection names a different library.

#![cfg(unix)]

use std::path::Path;
use std::process::Output;

fn run_command(runtime_dir: &Path, args: &[&str]) -> Output {
    let mut full = vec!["--library", "asked"];
    full.extend_from_slice(args);
    std::process::Command::new(env!("CARGO_BIN_EXE_bookrack"))
        .args(&full)
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir)
        .env_remove("BOOKRACK_DATA_DIR")
        .output()
        .expect("spawn bookrack")
}

fn run_routed_command(runtime_dir: &Path) -> Output {
    run_command(runtime_dir, &["diagnose"])
}

/// Take the flock on the lock file, seed it with `library_name=served`
/// content, and hand the held file back so the daemon it stands in for
/// stays "alive" for the duration of the test.
fn hold_mismatched_lock(runtime_dir: &Path) -> std::fs::File {
    use std::io::Write;

    use fs2::FileExt;

    let lock_path = runtime_dir.join("bookrack.tty.lock");
    let mut holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    holder.try_lock_exclusive().unwrap();
    holder
        .write_all(lock_content(runtime_dir).as_bytes())
        .unwrap();
    holder.flush().unwrap();
    holder
}

fn lock_content(runtime_dir: &Path) -> String {
    format!(
        "pid=999999\nmcp=disabled\ncontrol_sock={}\ndata_dir={}\nlibrary_name=served\n",
        runtime_dir.join("no-such-control.sock").display(),
        runtime_dir.join("served-data").display(),
    )
}

#[test]
fn leftover_lock_content_without_a_holder_does_not_refuse() {
    let runtime_root = tempfile::tempdir().unwrap();
    let lock_path = runtime_root.path().join("bookrack.tty.lock");
    std::fs::write(&lock_path, lock_content(runtime_root.path())).unwrap();

    let out = run_routed_command(runtime_root.path());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("refusing to act"),
        "stale lock content must not trip the mismatch check: {stderr}"
    );
    assert!(
        stderr.contains("daemon not running"),
        "expected the ordinary not-running path: {stderr}"
    );
}

#[test]
fn held_lock_still_refuses_a_differently_named_selection() {
    let runtime_root = tempfile::tempdir().unwrap();
    let holder = hold_mismatched_lock(runtime_root.path());

    let out = run_routed_command(runtime_root.path());
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
#[test]
fn held_lock_refuses_doctor_because_it_is_not_exempt() {
    let runtime_root = tempfile::tempdir().unwrap();
    let holder = hold_mismatched_lock(runtime_root.path());

    let out = run_command(runtime_root.path(), &["doctor"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains("refusing to act on library asked"),
        "doctor must hit the mismatch refusal: {stderr}"
    );
    drop(holder);
}

/// `retrieval` resolves its data root locally and never routes through
/// the daemon, so it is on the exemption list: even a held lock naming
/// a different library must not trip the mismatch refusal. It reaches
/// its own local resolution instead (and reports no catalog for the
/// unconfigured selection).
#[test]
fn held_lock_does_not_refuse_exempt_retrieval() {
    let runtime_root = tempfile::tempdir().unwrap();
    let holder = hold_mismatched_lock(runtime_root.path());

    let out = run_command(runtime_root.path(), &["retrieval", "list"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("refusing to act"),
        "retrieval is exempt and must not trip the mismatch check: {stderr}"
    );
    drop(holder);
}
