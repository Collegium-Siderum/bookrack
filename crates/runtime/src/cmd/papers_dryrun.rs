// SPDX-License-Identifier: Apache-2.0

//! `bookrack papers dryrun` — paper-side pre-vector simulation. Peer
//! of [`crate::cmd::dryrun`] for the paper pipeline; writes a JSONL of
//! [`bookrack_glean::dryrun::DryrunPaperReport`] plus a summary
//! sidecar under `<data_root>/dryruns/`.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use bookrack_config::Config;
use bookrack_glean::dryrun::{
    DryrunPaperParams, DryrunPaperReport, DryrunPaperSummary, collect_files, dryrun_paper,
    summarize,
};
use eyre::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// How many paper dryrun JSONL artifacts to keep under
/// `<data_root>/dryruns/` before pruning the oldest. Independent of
/// the book-side retention so a heavy book sweep does not displace a
/// recent paper dryrun.
const PAPERS_DRYRUN_KEEP: usize = 5;

/// What [`run`] produced. Mirrors [`crate::cmd::dryrun::DryrunRunOutcome`]
/// for the paper pipeline so the caller can render its own summary
/// line and stream the JSONL where it likes; the JSONL itself always
/// lives on disk under `<data_root>/dryruns/...` or `out`.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct PapersDryrunRunOutcome {
    pub jsonl_path: PathBuf,
    pub summary_path: PathBuf,
    pub summary: DryrunPaperSummary,
    pub file_count: usize,
}

pub fn run(
    cfg: &Config,
    path: &Path,
    out: Option<&Path>,
    skip_chunks: bool,
) -> Result<PapersDryrunRunOutcome> {
    let files = collect_files(path);
    if files.is_empty() {
        eyre::bail!("no supported paper files found under {}", path.display());
    }
    eprintln!(
        "bookrack papers dryrun: {} files under {}",
        files.len(),
        path.display()
    );

    let params = DryrunPaperParams {
        skip_chunks,
        ..Default::default()
    };

    let mut reports = Vec::with_capacity(files.len());
    let report_step = (files.len() / 20).max(1);
    for (i, file) in files.iter().enumerate() {
        let rec = dryrun_paper(file, &params);
        log_paper(&rec);
        reports.push(rec);
        if (i + 1).is_multiple_of(report_step) && i + 1 < files.len() {
            eprintln!("  {}/{}", i + 1, files.len());
        }
    }
    let summary = summarize(&reports);

    let (jsonl_path, summary_path) = resolve_output_paths(cfg, path, out)?;
    write_artifact(&jsonl_path, &summary_path, &reports, &summary)?;
    if out.is_none() {
        prune_old_papers_dryruns(jsonl_path.parent().expect("parent exists"))?;
    }
    Ok(PapersDryrunRunOutcome {
        jsonl_path,
        summary_path,
        summary,
        file_count: reports.len(),
    })
}

pub fn print_summary_to_stderr(summary: &DryrunPaperSummary) {
    eprintln!("summary:");
    eprintln!("  n_files:            {}", summary.n_files);
    eprintln!(
        "  extract_outcomes:   {}",
        format_counts(&summary.extract_outcomes)
    );
    eprintln!("  formats:            {}", format_counts(&summary.formats));
    eprintln!(
        "  doi_hits:           {}/{}",
        summary.doi_hits, summary.n_files
    );
    eprintln!(
        "  arxiv_hits:         {}/{}",
        summary.arxiv_hits, summary.n_files
    );
    eprintln!(
        "  venue_hits:         {}/{}",
        summary.venue_hits, summary.n_files
    );
    eprintln!(
        "  issn_hits:          {}/{}",
        summary.issn_hits, summary.n_files
    );
    eprintln!(
        "  abstract_hits:      {}/{}",
        summary.abstract_hits, summary.n_files
    );
    eprintln!(
        "  abstract_sources:   {}",
        format_counts(&summary.abstract_sources)
    );
    eprintln!(
        "  title_hits:         {}/{}",
        summary.title_hits, summary.n_files
    );
    eprintln!(
        "  year_hits:          {}/{}",
        summary.year_hits, summary.n_files
    );
}

pub fn render_outcome(
    outcome: &PapersDryrunRunOutcome,
    stream_jsonl_to_stdout: bool,
) -> Result<()> {
    if stream_jsonl_to_stdout {
        let file = File::open(&outcome.jsonl_path)
            .with_context(|| format!("re-open {}", outcome.jsonl_path.display()))?;
        let mut reader = std::io::BufReader::new(file);
        std::io::copy(&mut reader, &mut std::io::stdout())?;
    }
    eprintln!(
        "wrote {} reports to {}",
        outcome.file_count,
        outcome.jsonl_path.display()
    );
    eprintln!("wrote summary to {}", outcome.summary_path.display());
    print_summary_to_stderr(&outcome.summary);
    Ok(())
}

fn log_paper(rec: &DryrunPaperReport) {
    let stem = if rec.stem.chars().count() > 60 {
        let mut s: String = rec.stem.chars().take(57).collect();
        s.push('…');
        s
    } else {
        rec.stem.clone()
    };
    let abstract_src = rec.abstract_source.as_deref().unwrap_or("-");
    let doi = if rec.doi.is_some() { "doi" } else { "-" };
    let arxiv = if rec.arxiv_id.is_some() { "arxiv" } else { "-" };
    eprintln!(
        "  [{:6}] {:10} {:5} {:5} {} ({} ms)",
        rec.format, abstract_src, doi, arxiv, stem, rec.elapsed_ms
    );
}

fn format_counts(map: &std::collections::BTreeMap<String, usize>) -> String {
    if map.is_empty() {
        return "{}".to_string();
    }
    let pairs: Vec<String> = map.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.join(", ")
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
            fs::create_dir_all(parent).with_context(|| {
                format!("create papers dryrun output directory {}", parent.display())
            })?;
        }
        Ok((o.to_path_buf(), summary))
    } else {
        let dir = cfg.data_dir().join("dryruns");
        fs::create_dir_all(&dir)
            .with_context(|| format!("create dryrun output directory {}", dir.display()))?;
        let stamp = utc_timestamp_for_filename();
        let sha = input_hash(input);
        let base = dir.join(format!("dryrun-paper-{stamp}-{sha}"));
        let jsonl = base.with_extension("jsonl");
        let summary = sidecar_summary_path(&jsonl);
        Ok((jsonl, summary))
    }
}

fn sidecar_summary_path(jsonl: &Path) -> PathBuf {
    jsonl.with_extension("summary.json")
}

fn write_artifact(
    jsonl_path: &Path,
    summary_path: &Path,
    reports: &[DryrunPaperReport],
    summary: &DryrunPaperSummary,
) -> Result<()> {
    let file =
        File::create(jsonl_path).with_context(|| format!("create {}", jsonl_path.display()))?;
    let mut writer = BufWriter::new(file);
    for rec in reports {
        serde_json::to_writer(&mut writer, rec).context("write JSONL record")?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;

    let summary_file =
        File::create(summary_path).with_context(|| format!("create {}", summary_path.display()))?;
    let mut summary_writer = BufWriter::new(summary_file);
    serde_json::to_writer_pretty(&mut summary_writer, summary).context("write summary JSON")?;
    summary_writer.write_all(b"\n")?;
    summary_writer.flush()?;
    Ok(())
}

fn utc_timestamp_for_filename() -> String {
    // Same shape as `crate::cmd::dryrun::utc_timestamp_for_filename`:
    // `YYYYMMDDTHHMMSSZ`. Independent of chrono since the runtime
    // already pulls it in everywhere.
    let now = chrono::Utc::now();
    now.format("%Y%m%dT%H%M%SZ").to_string()
}

fn input_hash(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    hex
}

fn prune_old_papers_dryruns(dir: &Path) -> Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))?;
    let mut jsonls: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("dryrun-paper-") && n.ends_with(".jsonl"))
        })
        .collect();
    jsonls.sort();
    let excess = jsonls.len().saturating_sub(PAPERS_DRYRUN_KEEP);
    for jsonl in &jsonls[..excess] {
        let _ = fs::remove_file(jsonl);
        let _ = fs::remove_file(sidecar_summary_path(jsonl));
    }
    Ok(())
}
