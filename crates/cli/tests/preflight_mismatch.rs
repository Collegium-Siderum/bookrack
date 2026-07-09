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

fn run_routed_command(runtime_dir: &Path) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_bookrack"))
        .args(["--library", "asked", "diagnose"])
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir)
        .env_remove("BOOKRACK_DATA_DIR")
        .output()
        .expect("spawn bookrack")
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
    use std::io::Write;

    use fs2::FileExt;

    let runtime_root = tempfile::tempdir().unwrap();
    let lock_path = runtime_root.path().join("bookrack.tty.lock");
    let mut holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    holder.try_lock_exclusive().unwrap();
    holder
        .write_all(lock_content(runtime_root.path()).as_bytes())
        .unwrap();
    holder.flush().unwrap();

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
