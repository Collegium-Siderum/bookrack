// SPDX-License-Identifier: Apache-2.0

//! Per-source collectors. Each module writes one or more files into
//! the bundle staging directory.
//!
//! Collectors **never** mutate the live data root — they only read and
//! copy. A collector whose source is missing or empty writes an empty
//! file (or skips, when even the directory should be omitted) and
//! logs a `tracing::debug!`; only a hard IO or schema failure bubbles
//! up as a [`crate::DiagnoseError`].

pub mod catalog;
pub mod corpus;
pub mod crashes;
pub mod env;
pub mod logs;
pub mod vectors;

use std::path::PathBuf;

use bookrack_config::Config;

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
