// SPDX-License-Identifier: Apache-2.0

//! Copy every `crash-*.txt` from `<data_dir>/logs/` into
//! `<bundle>/crashes/`, with the scrubber applied to the body.

use std::path::Path;

use bookrack_config::Config;

use crate::Result;
use crate::scrub::Scrubber;

/// Walk `cfg.logs_dir()` for crash reports and stream each into the
/// bundle. A missing logs directory is silently treated as "no
/// crashes."
pub fn collect(cfg: &Config, bundle_dir: &Path, scrubber: &Scrubber) -> Result<()> {
    let logs_dir = cfg.logs_dir();
    let crashes_dir = bundle_dir.join("crashes");
    std::fs::create_dir_all(&crashes_dir)?;

    let read = match std::fs::read_dir(&logs_dir) {
        Ok(r) => r,
        Err(_) => return Ok(()),
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
        let body = std::fs::read_to_string(entry.path())?;
        let scrubbed = scrubber.scrub_string(&body);
        std::fs::write(crashes_dir.join(name_str), scrubbed)?;
    }
    Ok(())
}
