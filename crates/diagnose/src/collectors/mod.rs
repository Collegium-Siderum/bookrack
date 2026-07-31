// SPDX-License-Identifier: Apache-2.0

//! Per-source collectors. Each module writes one or more files into
//! the bundle staging directory.
//!
//! Collectors **never** mutate the live data root — they only read and
//! copy, through read-only doors where the store distinguishes them. A
//! database collector whose store is missing or unopenable records the
//! state in `open-error.json` (see [`write_open_error`]); other
//! collectors with an empty source write an empty file or skip. Only a
//! hard IO failure in the bundle directory itself bubbles up as a
//! [`crate::DiagnoseError`].

pub mod catalog;
pub mod corpus;
pub mod crashes;
pub mod env;
pub mod logs;
pub mod vectors;

use std::path::{Path, PathBuf};

use bookrack_config::Config;

/// Write `<section>/open-error.json` recording why the section has no
/// payload: the store is missing, or it exists but failed to open.
/// The distinction is the point — a maintainer reading the bundle can
/// then tell "never ingested" from "corrupt, newer schema, or locked"
/// instead of guessing at an absent file. Only the store's file name
/// is recorded, never its full path, so the bundle stays free of
/// local filesystem layout.
pub(crate) fn write_open_error(dst: &Path, store: &Path, error: Option<&str>) -> crate::Result<()> {
    let payload = serde_json::json!({
        "store": store.file_name().map(|n| n.to_string_lossy().into_owned()),
        "state": if error.is_some() { "unreadable" } else { "missing" },
        "error": error,
    });
    let mut text = serde_json::to_string_pretty(&payload)?;
    text.push('\n');
    std::fs::write(dst.join("open-error.json"), text)?;
    Ok(())
}

/// The directories log files and crash reports may live in, in
/// collection-priority order: the daemon state directory's `logs/`
/// (where the daemon writes) first, then the per-root `logs/` under
/// the data root (written by earlier binaries; still collected so a
/// bundle assembled right after an upgrade keeps its history). A file
/// name present in both sources is taken from the first.
pub(crate) fn log_source_dirs(cfg: &Config) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(state) = bookrack_config::daemon_state_dir() {
        dirs.push(state.join("logs"));
    }
    let per_root = cfg.logs_dir();
    if !dirs.contains(&per_root) {
        dirs.push(per_root);
    }
    dirs
}
