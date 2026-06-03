// SPDX-License-Identifier: Apache-2.0

//! CLI surface for `bookrack dryrun`.
//!
//! Walks a path, runs the pre-vector pipeline simulation on every
//! supported file, and writes a per-book JSONL plus a sidecar
//! `.summary.json` of the aggregate. By default the artifacts live under
//! `<data_root>/dryruns/`; the directory is pruned to the most recent
//! [`DRYRUN_KEEP`] runs after each successful invocation.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bookrack_config::Config;
use bookrack_ingest::{
    DryrunBookReport, DryrunParams, DryrunSummary, collect_files, dryrun_book, summarize,
};
use sha2::{Digest, Sha256};

/// How many dryrun JSONL artifacts to keep under `<data_root>/dryruns/`
/// before pruning the oldest. Matches the catalog backup retention so
/// the two operational artifact directories age out at the same cadence.
const DRYRUN_KEEP: usize = 5;

pub fn run(
    cfg: &Config,
    path: &Path,
    out: Option<&Path>,
    stdout: bool,
    no_chunk: bool,
    profile_name: Option<&str>,
) -> Result<()> {
    let files = collect_files(path);
    if files.is_empty() {
        anyhow::bail!("no supported files found under {}", path.display());
    }
    eprintln!(
        "bookrack dryrun: {} files under {}",
        files.len(),
        path.display()
    );

    let params = DryrunParams {
        skip_chunks: no_chunk,
        audit_rules: crate::load_audit_rules(cfg),
        audit_profile: crate::load_audit_profile(cfg, profile_name),
        ..Default::default()
    };

    let mut reports = Vec::with_capacity(files.len());
    let report_step = (files.len() / 20).max(1);
    for (i, file) in files.iter().enumerate() {
        let rec = dryrun_book(file, &params);
        log_book(&rec);
        reports.push(rec);
        if (i + 1).is_multiple_of(report_step) && i + 1 < files.len() {
            eprintln!("  {}/{}", i + 1, files.len());
        }
    }
    let summary = summarize(&reports);

    if stdout {
        let stdout_handle = std::io::stdout();
        let mut writer = BufWriter::new(stdout_handle.lock());
        write_jsonl(&mut writer, &reports)?;
        writer.flush()?;
    } else {
        let (jsonl_path, summary_path) = resolve_output_paths(cfg, path, out)?;
        write_artifact(&jsonl_path, &summary_path, &reports, &summary)?;
        eprintln!(
            "wrote per-book report to {} ({} files)",
            jsonl_path.display(),
            reports.len()
        );
        eprintln!("wrote summary to {}", summary_path.display());
        if out.is_none() {
            prune_old_dryruns(jsonl_path.parent().expect("parent exists"))?;
        }
    }
    print_summary_to_stderr(&summary);
    Ok(())
}

/// One book's progress line. Kept tight so even a thousand-file walk does
/// not flood the terminal: format and verdict first, then the outcome.
fn log_book(rec: &DryrunBookReport) {
    let stem = if rec.stem.chars().count() > 60 {
        let mut s: String = rec.stem.chars().take(57).collect();
        s.push('…');
        s
    } else {
        rec.stem.clone()
    };
    let verdict = rec.verdict.as_deref().unwrap_or("-");
    let confidence = rec.confidence.as_deref().unwrap_or("-");
    eprintln!(
        "  [{:6}] {:10} {:10} {} ({} ms)",
        rec.format, verdict, confidence, stem, rec.elapsed_ms
    );
}

fn print_summary_to_stderr(summary: &DryrunSummary) {
    eprintln!();
    eprintln!("--- summary ---");
    eprintln!("  total files: {}", summary.n_files);
    eprintln!("  by format:    {}", format_counts(&summary.formats));
    eprintln!(
        "  extract:      {}",
        format_counts(&summary.extract_outcomes)
    );
    if !summary.verdicts.is_empty() {
        eprintln!("  verdicts:     {}", format_counts(&summary.verdicts));
    }
    if !summary.confidence.is_empty() {
        eprintln!("  confidence:   {}", format_counts(&summary.confidence));
    }
}

fn format_counts(counts: &std::collections::BTreeMap<String, usize>) -> String {
    counts
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("  ")
}

fn resolve_output_paths(
    cfg: &Config,
    input: &Path,
    out: Option<&Path>,
) -> Result<(PathBuf, PathBuf)> {
    if let Some(o) = out {
        let summary = sidecar_summary_path(o);
        if let Some(parent) = o.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("create dryrun output directory {}", parent.display()))?;
        }
        Ok((o.to_path_buf(), summary))
    } else {
        let dir = cfg.data_dir().join("dryruns");
        fs::create_dir_all(&dir)
            .with_context(|| format!("create dryrun output directory {}", dir.display()))?;
        let stamp = utc_timestamp_for_filename();
        let sha = input_hash(input);
        let base = dir.join(format!("dryrun-{stamp}-{sha}"));
        let jsonl = base.with_extension("jsonl");
        let summary = sidecar_summary_path(&jsonl);
        Ok((jsonl, summary))
    }
}

fn sidecar_summary_path(jsonl: &Path) -> PathBuf {
    let mut name = jsonl
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dryrun.jsonl".to_string());
    if let Some(stripped) = name.strip_suffix(".jsonl") {
        name = stripped.to_string();
    }
    let mut summary = jsonl.to_path_buf();
    summary.set_file_name(format!("{name}.summary.json"));
    summary
}

fn write_artifact(
    jsonl_path: &Path,
    summary_path: &Path,
    reports: &[DryrunBookReport],
    summary: &DryrunSummary,
) -> Result<()> {
    let file =
        File::create(jsonl_path).with_context(|| format!("create {}", jsonl_path.display()))?;
    let mut writer = BufWriter::new(file);
    write_jsonl(&mut writer, reports)?;
    writer.flush()?;

    let summary_file =
        File::create(summary_path).with_context(|| format!("create {}", summary_path.display()))?;
    let mut summary_writer = BufWriter::new(summary_file);
    serde_json::to_writer_pretty(&mut summary_writer, summary).context("write summary JSON")?;
    summary_writer.write_all(b"\n")?;
    summary_writer.flush()?;
    Ok(())
}

fn write_jsonl<W: Write>(writer: &mut W, reports: &[DryrunBookReport]) -> Result<()> {
    for rec in reports {
        serde_json::to_writer(&mut *writer, rec).context("serialize dryrun record")?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

/// A timestamp safe to embed in a filename: `YYYY-MM-DDTHH-MM-SSZ`.
fn utc_timestamp_for_filename() -> String {
    // The CLI shells out elsewhere through SQLite for ISO timestamps so
    // there is one clock source, but at this layer there is no database
    // open yet — fall back to the system clock with the `:`s replaced
    // for filesystem portability.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = unix_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}-{mi:02}-{s:02}Z")
}

/// Convert a Unix timestamp (seconds, UTC) into a broken-down
/// year/month/day/hour/minute/second tuple. Implemented inline so the
/// CLI does not pick up a date dependency for one filename component.
fn unix_to_ymdhms(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let mut days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let h = (rem / 3600) as u32;
    let mi = ((rem % 3600) / 60) as u32;
    let s = (rem % 60) as u32;
    // 1970-01-01 is the epoch; walk forward year by year.
    let mut year = 1970i64;
    loop {
        let dy = if is_leap_year(year) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        year += 1;
    }
    let days_in_month: [i64; 12] = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1u32;
    for &dim in days_in_month.iter() {
        if days < dim {
            break;
        }
        days -= dim;
        month += 1;
    }
    let day = days as u32 + 1;
    (year, month, day, h, mi, s)
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

fn input_hash(path: &Path) -> String {
    let mut h = Sha256::new();
    h.update(path.to_string_lossy().as_bytes());
    let digest = format!("{:x}", h.finalize());
    digest[..8].to_string()
}

/// Keep the [`DRYRUN_KEEP`] newest `dryrun-*.jsonl` files (plus their
/// summary sidecars); delete the rest. Filenames lead with a sortable
/// timestamp, so lexical order is chronological.
fn prune_old_dryruns(dir: &Path) -> Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))?;
    let mut jsonls: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("dryrun-") && n.ends_with(".jsonl"))
        })
        .collect();
    jsonls.sort();
    let excess = jsonls.len().saturating_sub(DRYRUN_KEEP);
    for jsonl in &jsonls[..excess] {
        let _ = fs::remove_file(jsonl);
        let _ = fs::remove_file(sidecar_summary_path(jsonl));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn unix_epoch_renders_as_1970_01_01() {
        assert_eq!(unix_to_ymdhms(0), (1970, 1, 1, 0, 0, 0));
        // 1234567890 = 2009-02-13T23:31:30Z.
        assert_eq!(unix_to_ymdhms(1_234_567_890), (2009, 2, 13, 23, 31, 30));
        // 2024-02-29T00:00:00Z = 1709164800 (a leap-day boundary).
        assert_eq!(unix_to_ymdhms(1_709_164_800), (2024, 2, 29, 0, 0, 0));
        // 2024-03-01T00:00:00Z = 1709251200 (the day after the leap day).
        assert_eq!(unix_to_ymdhms(1_709_251_200), (2024, 3, 1, 0, 0, 0));
    }

    #[test]
    fn sidecar_path_swaps_extension() {
        let p = Path::new("/tmp/dryrun-2026-06-02-deadbeef.jsonl");
        assert_eq!(
            sidecar_summary_path(p),
            PathBuf::from("/tmp/dryrun-2026-06-02-deadbeef.summary.json")
        );
    }

    #[test]
    fn prune_keeps_the_newest_runs() {
        let dir = tempdir().expect("tempdir");
        // Lay down DRYRUN_KEEP + 3 stamped artifacts.
        for i in 0..(DRYRUN_KEEP + 3) {
            let stamp = format!("2026-06-02T00-00-{i:02}Z");
            let jsonl = dir.path().join(format!("dryrun-{stamp}-abcdef01.jsonl"));
            fs::write(&jsonl, b"{}\n").expect("write jsonl");
            fs::write(sidecar_summary_path(&jsonl), b"{}").expect("write summary");
        }
        prune_old_dryruns(dir.path()).expect("prune");
        let mut remaining: Vec<String> = fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        remaining.sort();
        // Each kept run has two files (jsonl + summary).
        assert_eq!(remaining.len(), DRYRUN_KEEP * 2);
        // The two oldest seconds (00, 01, 02) are gone, the newest stays.
        assert!(remaining.iter().any(|n| n.contains("00-07Z")));
        assert!(!remaining.iter().any(|n| n.contains("00-00Z")));
    }
}
