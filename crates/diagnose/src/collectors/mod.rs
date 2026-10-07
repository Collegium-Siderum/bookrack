// SPDX-License-Identifier: Apache-2.0

//! Per-source collectors. Each module writes one or more files into
//! the bundle staging directory.
//!
//! Collectors **never** mutate the live data root — they only read and
//! copy, through read-only doors where the store distinguishes them. A
//! database collector whose store is missing or unopenable records the
//! state in `open-error.json` (see [`write_open_error`]); a collector
//! copying a directory of files records the ones it could not copy
//! verbatim in `read-notes.json` (see [`write_read_notes`]); other
//! collectors with an empty source write an empty file or skip. Only a
//! hard IO failure in the bundle directory itself bubbles up as a
//! [`crate::DiagnoseError`].

pub mod catalog;
pub mod corpus;
pub mod crashes;
pub mod env;
pub mod logs;
pub mod papers;
pub mod queue;
pub mod refs;
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

/// Decode `bytes` as UTF-8, substituting U+FFFD for any invalid
/// sequence, and report whether a substitution happened.
///
/// Log files and crash reports are written by a process that can be
/// killed mid-record, so a truncated multi-byte character at the tail
/// is a normal end state for exactly the files a bundle is assembled
/// to explain. The flag is what the caller turns into a
/// [`NOTE_LOSSY_UTF8`] note; a file that already contained U+FFFD is
/// valid UTF-8 and does not raise it.
pub(crate) fn decode_lossy(bytes: &[u8]) -> (std::borrow::Cow<'_, str>, bool) {
    let text = String::from_utf8_lossy(bytes);
    let lossy = matches!(text, std::borrow::Cow::Owned(_));
    (text, lossy)
}

/// One file a collector could not copy verbatim, named by the state
/// it ended in.
pub(crate) struct ReadNote {
    /// File name relative to the section directory the notes file
    /// sits in, so it reads as the tail of the manifest entry when the
    /// file made it into the bundle.
    pub(crate) file: String,
    /// One of the `NOTE_*` states.
    pub(crate) state: &'static str,
    /// The underlying failure, absent when the file was collected.
    pub(crate) error: Option<String>,
}

/// The file is in the bundle, but its bytes were not valid UTF-8 and
/// the damaged sequences read back as U+FFFD.
pub(crate) const NOTE_LOSSY_UTF8: &str = "lossy-utf8";

/// The file is not in the bundle: the source could not be read.
pub(crate) const NOTE_UNREADABLE: &str = "unreadable";

/// The file is not in the bundle: the copy could not be written.
pub(crate) const NOTE_UNWRITABLE: &str = "unwritable";

/// Write `<section>/read-notes.json` listing every file the section
/// could not copy verbatim, or nothing when there is nothing to note.
///
/// A section that drops or degrades a file silently reads as complete,
/// which is the one thing a diagnose bundle must not do. File names
/// are recorded verbatim, so a note lines up with the manifest entry;
/// the error text is scrubbed, because an [`std::io::Error`] message
/// quotes the path it failed on.
pub(crate) fn write_read_notes(
    dst: &Path,
    notes: &[ReadNote],
    scrubber: &crate::scrub::Scrubber,
) -> crate::Result<()> {
    if notes.is_empty() {
        return Ok(());
    }
    let files: Vec<serde_json::Value> = notes
        .iter()
        .map(|n| {
            serde_json::json!({
                "file": n.file,
                "state": n.state,
                "error": n.error.as_deref().map(|e| scrubber.scrub_string(e)),
            })
        })
        .collect();
    let mut text = serde_json::to_string_pretty(&serde_json::json!({ "files": files }))?;
    text.push('\n');
    std::fs::write(dst.join("read-notes.json"), text)?;
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
