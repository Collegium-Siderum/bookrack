// SPDX-License-Identifier: Apache-2.0

//! `bookrack runs` — operator-facing surface for the `pipeline_runs`
//! registry and its `pipeline_run_summary` rollup.
//!
//! * `runs list [--last N] [--command NAME]` reads recent runs from
//!   `pipeline_runs`, joins each against its rollup row, and prints a
//!   compact table.
//! * `runs show <run-id>` reads one run and renders its `verdict`,
//!   `flag`, and `coverage` distributions as horizontal histograms
//!   built from `render::distribution::render_histogram_bars`.
//!
//! Both commands open the catalog directly, the same way `distill`
//! does, and never touch the daemon: the runs surface is local-only
//! and read-only.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bookrack_catalog::{Catalog, PipelineRun, PipelineRunSummary};
use bookrack_cli_grammar::RunsAction;
use bookrack_config::Config;
use eyre::{Context as _, Result};
use serde_json::Value as JsonValue;

use crate::render::distribution::render_histogram_bars;

/// Dispatch the requested `bookrack runs` action.
pub fn run(selection: &bookrack_config::LibrarySelection, action: RunsAction) -> Result<()> {
    let catalog_path = resolve_catalog_path(selection)?;
    let catalog =
        Catalog::open(&catalog_path).with_context(|| format!("open {}", catalog_path.display()))?;
    match action {
        RunsAction::List { last, command } => list(&catalog, last, command.as_deref()),
        RunsAction::Show { run_id } => show(&catalog, &run_id),
    }
}

fn resolve_catalog_path(selection: &bookrack_config::LibrarySelection) -> Result<PathBuf> {
    let cfg = Config::resolve(selection).context("resolve configuration")?;
    Ok(cfg.catalog_db())
}

/// Render the recent-runs table. Empty result prints a single `No runs`
/// line so the operator sees an explicit zero rather than blank output.
fn list(catalog: &Catalog, last: Option<usize>, command: Option<&str>) -> Result<()> {
    let runs = catalog
        .list_pipeline_runs(command, last)
        .context("list pipeline_runs")?;
    println!("{}", render_runs_list(catalog, &runs)?);
    Ok(())
}

/// Render `runs show <id>`. Empty rollup (no audit rows under this run)
/// prints the header section but omits the three histograms; that case
/// is normal for runs from commands like `ingest` / `dryrun` that do
/// not write audits today.
fn show(catalog: &Catalog, pipeline_run_id: &str) -> Result<()> {
    let run = catalog
        .pipeline_run(pipeline_run_id)
        .context("read pipeline_runs row")?
        .ok_or_else(|| eyre::eyre!("no pipeline run with id {pipeline_run_id:?}"))?;
    let summary = catalog
        .pipeline_run_summary(pipeline_run_id)
        .context("read pipeline_run_summary row")?;
    println!("{}", render_run_show(&run, summary.as_ref())?);
    Ok(())
}

/// Build the `runs list` text block. Public to the crate so tests can
/// assert on the rendered shape without spawning the binary.
pub(crate) fn render_runs_list(catalog: &Catalog, runs: &[PipelineRun]) -> Result<String> {
    if runs.is_empty() {
        return Ok("No runs.".to_string());
    }
    let mut summaries: BTreeMap<String, PipelineRunSummary> = BTreeMap::new();
    for run in runs {
        if let Some(s) = catalog
            .pipeline_run_summary(&run.pipeline_run_id)
            .context("read pipeline_run_summary row")?
        {
            summaries.insert(run.pipeline_run_id.clone(), s);
        }
    }
    let mut out = String::new();
    out.push_str("run_id                                                  command         started_at            status   n_books  n_papers  needs_work\n");
    for run in runs {
        let summary = summaries.get(&run.pipeline_run_id);
        let n_books = summary.map(|s| s.n_books).unwrap_or(0);
        let n_papers = summary.map(|s| s.n_papers).unwrap_or(0);
        let needs_work = summary
            .map(|s| extract_count(&s.verdict_counts, "needs_work"))
            .unwrap_or(0);
        let status = run.status.as_deref().unwrap_or("");
        let line = format!(
            "{run_id:<55} {command:<15} {started:<21} {status:<8} {n_books:>7}  {n_papers:>8}  {needs_work:>10}\n",
            run_id = run.pipeline_run_id,
            command = run.command,
            started = run.started_at,
            status = status,
            n_books = n_books,
            n_papers = n_papers,
            needs_work = needs_work,
        );
        out.push_str(&line);
    }
    Ok(out.trim_end().to_string())
}

/// Build the `runs show <id>` text block.
pub(crate) fn render_run_show(
    run: &PipelineRun,
    summary: Option<&PipelineRunSummary>,
) -> Result<String> {
    let mut out = String::new();
    out.push_str(&format!("run_id:       {}\n", run.pipeline_run_id));
    out.push_str(&format!("command:      {}\n", run.command));
    out.push_str(&format!("started_at:   {}\n", run.started_at));
    out.push_str(&format!(
        "finished_at:  {}\n",
        run.finished_at.as_deref().unwrap_or("-")
    ));
    out.push_str(&format!(
        "status:       {}\n",
        run.status.as_deref().unwrap_or("-")
    ));
    out.push_str(&format!(
        "library_root: {}\n",
        run.library_root.as_deref().unwrap_or("-")
    ));
    let Some(summary) = summary else {
        out.push_str("\nno rollup recorded for this run.");
        return Ok(out.trim_end().to_string());
    };
    out.push_str(&format!("n_books:      {}\n", summary.n_books));
    out.push_str(&format!("n_papers:     {}\n", summary.n_papers));

    let verdicts = parse_counts(&summary.verdict_counts)?;
    let flags = parse_counts(&summary.flag_counts)?;
    let coverage = parse_counts(&summary.coverage_summary)?;

    if !verdicts.is_empty() {
        out.push_str("\nverdict:\n");
        out.push_str(&render_histogram_bars(&verdicts, 32));
    }
    if !flags.is_empty() {
        out.push_str("\nflags:\n");
        out.push_str(&render_histogram_bars(&flags, 32));
    }
    if !coverage.is_empty() {
        out.push_str("\ncoverage:\n");
        out.push_str(&render_histogram_bars(&coverage, 32));
    }
    Ok(out.trim_end().to_string())
}

/// Pull one named counter out of a JSON object encoded into one of the
/// rollup's TEXT columns. Returns 0 when the key is absent or the
/// value is not a positive integer.
fn extract_count(json: &str, key: &str) -> u64 {
    let parsed: JsonValue = serde_json::from_str(json).unwrap_or(JsonValue::Null);
    parsed.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Parse a `{ "key": N, ... }` JSON object into the histogram's input
/// shape. Non-integer or negative values collapse to 0 and drop out
/// of the resulting map.
fn parse_counts(json: &str) -> Result<BTreeMap<String, u64>> {
    if json.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let value: JsonValue = serde_json::from_str(json)
        .with_context(|| format!("parse rollup JSON column (got {} bytes)", json.len()))?;
    let Some(obj) = value.as_object() else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for (k, v) in obj {
        if let Some(n) = v.as_u64()
            && n > 0
        {
            out.insert(k.clone(), n);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_catalog::{NewPipelineRun, NewPipelineRunSummary};

    fn open_in_memory() -> Catalog {
        Catalog::open_in_memory().expect("open in-memory catalog")
    }

    fn seed_run(catalog: &Catalog, id: &str, command: &str, started_at: &str) {
        catalog
            .insert_pipeline_run(&NewPipelineRun {
                pipeline_run_id: id.to_string(),
                command: command.to_string(),
                command_args: None,
                library_root: Some("lib-a".to_string()),
                started_at: started_at.to_string(),
                finished_at: Some("2026-06-28T10:00:05Z".to_string()),
                status: Some("ok".to_string()),
            })
            .expect("insert pipeline_runs row");
    }

    fn seed_summary(catalog: &Catalog, id: &str, n_books: i64, verdict_counts: &str) {
        catalog
            .upsert_pipeline_run_summary(&NewPipelineRunSummary {
                pipeline_run_id: id.to_string(),
                n_books,
                n_papers: 0,
                verdict_counts: verdict_counts.to_string(),
                flag_counts: "{}".to_string(),
                coverage_summary: "{}".to_string(),
                wall_clock_ms: Some(1_000),
                computed_at: "2026-06-28T10:00:06Z".to_string(),
            })
            .expect("upsert summary");
    }

    #[test]
    fn runs_list_renders_with_zero_runs() {
        let catalog = open_in_memory();
        let out = render_runs_list(&catalog, &[]).expect("render");
        assert_eq!(out, "No runs.");
    }

    #[test]
    fn runs_list_aggregates_per_run_columns() {
        let catalog = open_in_memory();
        seed_run(&catalog, "run-a", "distill_build", "2026-06-28T10:00:00Z");
        seed_summary(&catalog, "run-a", 3, r#"{"clean":2,"needs_work":1}"#);
        let runs = catalog.list_pipeline_runs(None, None).expect("list");
        let out = render_runs_list(&catalog, &runs).expect("render");
        // The header line is the first row.
        let header = out.lines().next().expect("header");
        assert!(header.starts_with("run_id"));
        // The data row carries the run id, command, and per-run counters.
        let data = out.lines().nth(1).expect("data row");
        assert!(data.contains("run-a"));
        assert!(data.contains("distill_build"));
        assert!(
            data.contains("       3  "),
            "n_books column present, got {data:?}"
        );
        assert!(
            data.trim_end().ends_with("1"),
            "needs_work column present, got {data:?}"
        );
    }

    #[test]
    fn runs_show_renders_histogram_with_three_books() {
        let catalog = open_in_memory();
        seed_run(&catalog, "run-b", "distill_build", "2026-06-28T11:00:00Z");
        seed_summary(&catalog, "run-b", 3, r#"{"clean":2,"needs_work":1}"#);
        let run = catalog
            .pipeline_run("run-b")
            .expect("read")
            .expect("present");
        let summary = catalog
            .pipeline_run_summary("run-b")
            .expect("read")
            .expect("present");
        let out = render_run_show(&run, Some(&summary)).expect("render");
        assert!(out.contains("run_id:       run-b"));
        assert!(out.contains("n_books:      3"));
        assert!(out.contains("\nverdict:\n"));
        // Two histogram rows, one per non-zero verdict bucket.
        let bar_rows: Vec<&str> = out
            .lines()
            .filter(|l| l.starts_with("  ") && l.contains(" | "))
            .collect();
        assert_eq!(
            bar_rows.len(),
            2,
            "expected two histogram rows, got {bar_rows:?}"
        );
        assert!(bar_rows.iter().any(|l| l.contains("clean")));
        assert!(bar_rows.iter().any(|l| l.contains("needs_work")));
    }

    #[test]
    fn runs_show_with_no_summary_prints_header_only() {
        let catalog = open_in_memory();
        seed_run(&catalog, "run-c", "ingest", "2026-06-28T12:00:00Z");
        let run = catalog
            .pipeline_run("run-c")
            .expect("read")
            .expect("present");
        let out = render_run_show(&run, None).expect("render");
        assert!(out.contains("run_id:       run-c"));
        assert!(out.contains("no rollup recorded for this run."));
        assert!(!out.contains("verdict:"));
    }

    #[test]
    fn parse_counts_drops_non_positive_and_non_integer_values() {
        let counts =
            parse_counts(r#"{"clean":2,"needs_work":0,"weird":"x","negative":-3}"#).expect("parse");
        assert_eq!(counts.get("clean"), Some(&2));
        assert!(!counts.contains_key("needs_work"));
        assert!(!counts.contains_key("weird"));
        assert!(!counts.contains_key("negative"));
    }
}
