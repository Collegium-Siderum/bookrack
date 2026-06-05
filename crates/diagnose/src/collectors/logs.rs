// SPDX-License-Identifier: Apache-2.0

//! Copy the most recent `bookrack.log.YYYY-MM-DD` files from
//! `<data_dir>/logs/` into `<bundle>/logs/`, scrubbing strings inside
//! each JSON line.

use std::path::Path;
use std::time::SystemTime;

use bookrack_config::Config;

use crate::Result;
use crate::scrub::Scrubber;
use crate::{Options, manifest::iso8601_z};

const LOG_PREFIX: &str = "bookrack.log.";

/// Copy logs whose date matches one of the last `opts.days` days. The
/// date is read from the filename suffix, not from the file
/// timestamp, so a tarball assembled at noon and one assembled at
/// midnight pick the same window if `now` is unchanged.
pub fn collect(
    cfg: &Config,
    opts: &Options,
    now: SystemTime,
    bundle_dir: &Path,
    scrubber: &Scrubber,
) -> Result<()> {
    let logs_src = cfg.logs_dir();
    let logs_dst = bundle_dir.join("logs");
    std::fs::create_dir_all(&logs_dst)?;

    let read = match std::fs::read_dir(&logs_src) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    let mut candidates: Vec<String> = read
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n.starts_with(LOG_PREFIX))
        .collect();
    candidates.sort(); // YYYY-MM-DD sort is lexicographic.

    let cutoff_date = cutoff_date_string(now, opts.days);
    for name in candidates.into_iter().rev().take_while(|n| {
        n.strip_prefix(LOG_PREFIX)
            .is_some_and(|date| date.as_bytes() >= cutoff_date.as_bytes())
    }) {
        let src = logs_src.join(&name);
        let dst = logs_dst.join(&name);
        scrub_file(&src, &dst, scrubber)?;
    }
    Ok(())
}

/// Read every line of `src`, scrub each JSON record, and write the
/// result to `dst`. A line that fails to parse as JSON rides through
/// as a plain string (still scrubbed) so non-JSON tail bytes (e.g.
/// stack traces appended by a crash) do not break the collector.
fn scrub_file(src: &Path, dst: &Path, scrubber: &Scrubber) -> Result<()> {
    let body = std::fs::read_to_string(src)?;
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(line) {
            scrubber.scrub_value(&mut v);
            out.push_str(&v.to_string());
        } else {
            out.push_str(&scrubber.scrub_string(line));
        }
        out.push('\n');
    }
    std::fs::write(dst, out)?;
    Ok(())
}

fn cutoff_date_string(now: SystemTime, days: u32) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cutoff_secs = secs.saturating_sub(u64::from(days).saturating_mul(86_400));
    let iso = iso8601_z(UNIX_EPOCH + Duration::from_secs(cutoff_secs));
    iso[..10].to_string() // "YYYY-MM-DD"
}
