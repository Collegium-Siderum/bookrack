// SPDX-License-Identifier: Apache-2.0

//! Copy every `crash-*.txt` from every log source directory (see
//! [`super::log_source_dirs`]) into `<bundle>/crashes/`, with the
//! scrubber applied to the body.

use std::path::Path;

use bookrack_config::Config;

use crate::Result;
use crate::scrub::Scrubber;

/// Walk every log source directory for crash reports and stream each
/// into the bundle. A missing source directory is silently treated as
/// "no crashes."
pub fn collect(cfg: &Config, bundle_dir: &Path, scrubber: &Scrubber) -> Result<()> {
    let crashes_dir = bundle_dir.join("crashes");
    std::fs::create_dir_all(&crashes_dir)?;

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
            let body = std::fs::read_to_string(entry.path())?;
            let scrubbed = scrubber.scrub_string(&body);
            std::fs::write(dst, scrubbed)?;
        }
    }
    Ok(())
}
