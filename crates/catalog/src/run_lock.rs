// SPDX-License-Identifier: Apache-2.0

//! Liveness records for open `pipeline_runs` rows.
//!
//! A registry row is written `running` at entry and rewritten to a
//! terminal status at exit. The exit leg is best-effort by design —
//! audit bookkeeping must not fail the work it records — so a process
//! that dies between the two legs leaves a row that reads `running`
//! forever, indistinguishable in the table from a run still in flight.
//!
//! This module supplies the evidence that tells the two apart. An open
//! run holds an exclusive advisory lock on
//! `<data_root>/.run-locks/<id>.lock` for as long as its process lives.
//! The kernel releases that lock when the holder exits, however it
//! exits, so a later probe answers the question without a clock
//! threshold and without asking about a pid:
//!
//! * contended — a live process still owns the run;
//! * free — the owner is gone and nothing will ever close the row;
//! * no file — the run kept no liveness record, and its absence
//!   supports no conclusion either way.
//!
//! The lock lives on the open file description rather than the process,
//! so a probe taken from inside the very process that owns the run
//! still reports it held. A daemon checking its own library does not
//! mistake its in-flight ingest for an abandoned one.
//!
//! The directory is dot-prefixed for the reason the data-root lock is:
//! the files are ephemeral process state, not library content.

use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// Basename of the lock directory under the data root.
const RUN_LOCKS_DIR_NAME: &str = ".run-locks";

/// Directory holding one lock file per open pipeline run, beside the
/// catalog the runs are registered in. `None` when `catalog_db` has no
/// parent, which is the in-memory and bare-filename cases.
pub fn run_locks_dir(catalog_db: &Path) -> Option<PathBuf> {
    catalog_db.parent().map(|dir| dir.join(RUN_LOCKS_DIR_NAME))
}

/// Lock file path for one run id. The id is a composite carrying an
/// ISO-8601 instant, so it holds characters (`:`) that are not portable
/// in a filename; every byte outside `[A-Za-z0-9._-]` maps to `_`.
/// Callers address a file by rebuilding the name from the row's id and
/// never by parsing a name back into one.
pub fn run_lock_path(dir: &Path, pipeline_run_id: &str) -> PathBuf {
    let mut name: String = pipeline_run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    name.push_str(".lock");
    dir.join(name)
}

/// An exclusive advisory lock naming one open pipeline run.
///
/// Held from the moment the `running` row is written until the value
/// drops. An orderly drop removes the file; every other way a process
/// can end leaves it behind, unlocked, which is exactly the signature
/// [`run_liveness`] reads.
#[derive(Debug)]
pub struct RunLock {
    path: PathBuf,
    /// Owns the lock: releasing it is the file handle closing, which
    /// the kernel also does for a process that dies.
    _file: File,
}

impl RunLock {
    /// Create `dir` if needed, then take the run's lock. Fails with
    /// [`io::ErrorKind::WouldBlock`] when another holder has it, and
    /// with the underlying I/O error when the file cannot be opened.
    pub fn acquire(dir: &Path, pipeline_run_id: &str) -> io::Result<RunLock> {
        fs::create_dir_all(dir)?;
        let path = run_lock_path(dir, pipeline_run_id);
        let file = File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(RunLock { path, _file: file }),
            Err(TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("run lock at {} is already held", path.display()),
            )),
            Err(TryLockError::Error(err)) => Err(err),
        }
    }

    /// Where the lock file sits.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// What a run's liveness record says about the process that opened it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunLiveness {
    /// A live process holds the lock; the run is still in flight.
    Held,
    /// The lock file is present and free: its owner is gone and will
    /// never close the row.
    Abandoned,
    /// No lock file. The run either predates liveness records or could
    /// not write one; nothing follows about its owner.
    NoRecord,
    /// The record exists but could not be read, with the reason.
    Unreadable(String),
}

/// Probe one run's liveness record.
///
/// The probe takes the lock for the duration of the check and releases
/// it on return, so a [`RunLock::acquire`] racing into that window can
/// fail spuriously. The loser of that race opens its run without a
/// record rather than not at all, which reads as [`RunLiveness::NoRecord`]
/// and is never treated as evidence of death.
pub fn run_liveness(dir: &Path, pipeline_run_id: &str) -> RunLiveness {
    let path = run_lock_path(dir, pipeline_run_id);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return RunLiveness::NoRecord,
        Err(err) => return RunLiveness::Unreadable(err.to_string()),
    };
    match file.try_lock() {
        Ok(()) => RunLiveness::Abandoned,
        Err(TryLockError::WouldBlock) => RunLiveness::Held,
        Err(TryLockError::Error(err)) => RunLiveness::Unreadable(err.to_string()),
    }
}

/// Delete an abandoned run's lock file. A record already gone counts as
/// success, so a repair that runs twice does not report a failure on
/// the second pass.
pub fn discard_run_lock(dir: &Path, pipeline_run_id: &str) -> io::Result<()> {
    match fs::remove_file(run_lock_path(dir, pipeline_run_id)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN_ID: &str = "distill_build-2026-06-28T10:00:00Z-deadbeef";

    /// The probe reports a held lock even when the holder is the very
    /// process asking, which is the daemon-checks-its-own-library case.
    #[test]
    fn a_held_lock_reads_as_held_from_the_holding_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = RunLock::acquire(dir.path(), RUN_ID).expect("acquire");
        assert!(lock.path().exists());
        assert_eq!(run_liveness(dir.path(), RUN_ID), RunLiveness::Held);
    }

    /// A second acquire of the same run is refused rather than handing
    /// out a second claim on one run's lifecycle.
    #[test]
    fn the_same_run_cannot_be_locked_twice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _first = RunLock::acquire(dir.path(), RUN_ID).expect("acquire");
        let second = RunLock::acquire(dir.path(), RUN_ID).expect_err("second acquire refused");
        assert_eq!(second.kind(), io::ErrorKind::WouldBlock);
    }

    /// An orderly drop takes the record with it, so a closed run leaves
    /// nothing for the probe to find.
    #[test]
    fn dropping_the_lock_removes_the_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = {
            let lock = RunLock::acquire(dir.path(), RUN_ID).expect("acquire");
            lock.path().to_path_buf()
        };
        assert!(!path.exists());
        assert_eq!(run_liveness(dir.path(), RUN_ID), RunLiveness::NoRecord);
    }

    /// A file left behind unlocked is what a killed process leaves, and
    /// it is the one state that licenses closing the row.
    #[test]
    fn a_leftover_unlocked_record_reads_as_abandoned() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(run_lock_path(dir.path(), RUN_ID), b"").expect("seed record");
        assert_eq!(run_liveness(dir.path(), RUN_ID), RunLiveness::Abandoned);
    }

    /// A run with no record at all is not evidence of death.
    #[test]
    fn a_missing_record_is_not_abandonment() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(run_liveness(dir.path(), RUN_ID), RunLiveness::NoRecord);
    }

    /// The composite id's colons do not reach the filesystem, and two
    /// different ids still land on two different files.
    #[test]
    fn the_lock_file_name_is_portable_and_still_distinguishes_runs() {
        let dir = Path::new("/tmp/x");
        let path = run_lock_path(dir, RUN_ID);
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .to_string();
        assert!(!name.contains(':'), "colon reached the filename: {name}");
        assert!(name.ends_with(".lock"));
        assert!(name.starts_with("distill_build-2026-06-28T10_00_00Z"));
        assert_ne!(
            run_lock_path(dir, "glean-2026-06-28T10:00:00Z-aaaaaaaa"),
            run_lock_path(dir, "glean-2026-06-28T10:00:00Z-bbbbbbbb"),
        );
    }

    /// Discarding is idempotent: the second pass of a repair that
    /// already cleaned up is not a failure.
    #[test]
    fn discarding_an_absent_record_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(run_lock_path(dir.path(), RUN_ID), b"").expect("seed record");
        discard_run_lock(dir.path(), RUN_ID).expect("first discard");
        discard_run_lock(dir.path(), RUN_ID).expect("second discard");
        assert_eq!(run_liveness(dir.path(), RUN_ID), RunLiveness::NoRecord);
    }

    /// The lock directory is derived from the catalog it registers runs
    /// for, so both catalogs under one data root share it.
    #[test]
    fn both_catalogs_under_one_root_share_a_lock_directory() {
        let root = Path::new("/data/library");
        assert_eq!(
            run_locks_dir(&root.join("catalog.db")),
            run_locks_dir(&root.join("papers_catalog.db")),
        );
        assert_eq!(
            run_locks_dir(&root.join("catalog.db")),
            Some(root.join(".run-locks")),
        );
    }
}
