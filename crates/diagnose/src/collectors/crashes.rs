// SPDX-License-Identifier: Apache-2.0

//! Copy every `crash-*.txt` from every log source directory (see
//! [`super::log_source_dirs`]) into `<bundle>/crashes/`, with the
//! scrubber applied to the body.

use std::path::Path;

use bookrack_config::Config;

use crate::Result;
use crate::scrub::Scrubber;

use super::{NOTE_LOSSY_UTF8, NOTE_UNREADABLE, NOTE_UNWRITABLE, ReadNote};

/// Walk every log source directory for crash reports and stream each
/// into the bundle. A missing source directory is silently treated as
/// "no crashes."
///
/// A report is copied for whatever it holds: the process that wrote it
/// was dying, so the tail can be a half-written multi-byte character.
/// Bytes that are not valid UTF-8 read back as U+FFFD and the report
/// is listed in `crashes/read-notes.json`; one the collector cannot
/// read or write at all is listed there too and costs only itself.
pub fn collect(cfg: &Config, bundle_dir: &Path, scrubber: &Scrubber) -> Result<()> {
    let crashes_dir = bundle_dir.join("crashes");
    std::fs::create_dir_all(&crashes_dir)?;
    let mut notes = Vec::new();

    for logs_dir in super::log_source_dirs(cfg) {
        let read = match std::fs::read_dir(&logs_dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let name = entry.file_name();
            let name_str = match name.to_str() {
                Some(s) => s,
                None => continue,
            };
            if !name_str.starts_with("crash-") || !name_str.ends_with(".txt") {
                continue;
            }
            let dst = crashes_dir.join(name_str);
            if dst.exists() {
                continue;
            }
            match copy_report(&entry.path(), &dst, scrubber) {
                Ok(false) => {}
                Ok(true) => notes.push(ReadNote {
                    file: name_str.to_string(),
                    state: NOTE_LOSSY_UTF8,
                    error: None,
                }),
                Err((state, e)) => notes.push(ReadNote {
                    file: name_str.to_string(),
                    state,
                    error: Some(e.to_string()),
                }),
            }
        }
    }
    super::write_read_notes(&crashes_dir, &notes, scrubber)?;
    Ok(())
}

/// Scrub the body of one crash report into `dst`, returning whether
/// the source decoded lossily.
fn copy_report(
    src: &Path,
    dst: &Path,
    scrubber: &Scrubber,
) -> std::result::Result<bool, (&'static str, std::io::Error)> {
    let bytes = std::fs::read(src).map_err(|e| (NOTE_UNREADABLE, e))?;
    let (body, lossy) = super::decode_lossy(&bytes);
    let scrubbed = scrubber.scrub_string(&body);
    std::fs::write(dst, scrubbed).map_err(|e| (NOTE_UNWRITABLE, e))?;
    Ok(lossy)
}
