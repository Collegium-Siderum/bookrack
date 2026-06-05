// SPDX-License-Identifier: Apache-2.0

//! The diagnose bundle's `manifest.json` — a small JSON document that
//! identifies the bookrack build, captures the time window, records
//! whether the bundle was scrubbed, and lists every entry inside the
//! tarball.
//!
//! The schema version is bumped whenever the file shape or scrubbing
//! contract changes, so a future reader can tell which decoder applies.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::DiagnoseError;
use crate::Options;

/// Bundle schema version. Bump when any file's shape changes, when a
/// new scrub rule is added, or when the tarball layout rearranges.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// The top-level `manifest.json` document.
#[derive(Debug, Serialize)]
pub struct Manifest {
    /// Bundle schema version.
    pub schema_version: u32,
    /// `CARGO_PKG_VERSION` at build time.
    pub bookrack_version: &'static str,
    /// `std::env::consts::OS`.
    pub os: &'static str,
    /// `std::env::consts::ARCH`.
    pub arch: &'static str,
    /// When the bundle was assembled, ISO-8601 UTC.
    pub generated_at: String,
    /// The `--days` window the bundle covered.
    pub days: u32,
    /// `true` when the scrubber ran over every collected file.
    pub scrubbed: bool,
    /// Every regular file inside the tarball, in the order it was
    /// written.
    pub files: Vec<FileEntry>,
}

/// One row of the manifest's file table.
#[derive(Debug, Serialize)]
pub struct FileEntry {
    /// Bundle-relative path, posix style (`logs/bookrack.log.YYYY-MM-DD`).
    pub path: String,
    /// File size in bytes.
    pub bytes: u64,
}

/// Build a [`Manifest`] for the given options and bundle staging area.
///
/// Walks `bundle_dir` to list every file the collectors wrote, sorted
/// by path so two runs over the same inputs produce identical
/// manifests.
pub fn build(
    opts: &Options,
    bundle_dir: &Path,
    now: SystemTime,
) -> Result<Manifest, DiagnoseError> {
    let mut files = Vec::new();
    walk(bundle_dir, bundle_dir, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Manifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        bookrack_version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        generated_at: iso8601_z(now),
        days: opts.days,
        scrubbed: opts.scrub,
        files,
    })
}

/// Serialize `manifest` to `<bundle_dir>/manifest.json` (pretty-printed,
/// LF line endings, trailing newline) so the result is byte-stable
/// across hosts.
pub fn write(bundle_dir: &Path, manifest: &Manifest) -> Result<(), DiagnoseError> {
    let path = bundle_dir.join("manifest.json");
    let mut json = serde_json::to_string_pretty(manifest)
        .map_err(|e| DiagnoseError::Io(std::io::Error::other(e)))?;
    json.push('\n');
    std::fs::write(&path, json)?;
    Ok(())
}

/// Format a [`SystemTime`] as `YYYY-MM-DDTHH:MM:SSZ`. Done by hand so
/// the crate does not pull in a date library for one timestamp.
pub fn iso8601_z(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = ymd_hms_from_unix(secs as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Compute `(year, month, day, hour, minute, second)` from a Unix
/// timestamp. Uses the standard civil-from-days algorithm; correct for
/// any date in the proleptic Gregorian calendar.
fn ymd_hms_from_unix(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let day_seconds = 86_400i64;
    let days = secs.div_euclid(day_seconds);
    let day_secs = secs.rem_euclid(day_seconds);
    let hour = (day_secs / 3600) as u32;
    let minute = ((day_secs % 3600) / 60) as u32;
    let second = (day_secs % 60) as u32;

    // Civil-from-days, after Howard Hinnant.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = (y + i64::from(m <= 2)) as i32;
    (year, m, d, hour, minute, second)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<FileEntry>) -> Result<(), DiagnoseError> {
    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            walk(root, &path, out)?;
        } else if meta.is_file() {
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.to_string_lossy().into_owned());
            // The manifest file itself is written after this walk, so
            // it is not present yet — no need to special-case it.
            out.push(FileEntry {
                path: rel,
                bytes: meta.len(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_z_renders_a_known_unix_timestamp() {
        // 1717545600 == 2024-06-05T00:00:00Z (midnight UTC).
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_717_545_600);
        assert_eq!(iso8601_z(t), "2024-06-05T00:00:00Z");
    }

    #[test]
    fn iso8601_z_includes_hour_minute_second() {
        // 1717573200 = 2024-06-05T00:00:00Z + 7h40m = 2024-06-05T07:40:00Z.
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_717_573_200);
        assert_eq!(iso8601_z(t), "2024-06-05T07:40:00Z");
    }

    #[test]
    fn build_lists_every_file_in_sorted_order() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::write(root.join("logs/c"), b"third").unwrap();
        std::fs::write(root.join("logs/a"), b"first").unwrap();
        std::fs::write(root.join("env.txt"), b"hi\n").unwrap();
        let opts = Options::default();
        let m = build(&opts, root, UNIX_EPOCH).unwrap();
        let paths: Vec<&str> = m.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["env.txt", "logs/a", "logs/c"]);
        assert_eq!(m.files[0].bytes, 3);
        assert_eq!(m.files[1].bytes, 5);
    }
}
