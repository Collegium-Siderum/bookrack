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
//! Runs live next to the audit rows they group, so the registry is
//! split across two databases: book-side commands register in
//! `catalog.db`, the glean pipeline in `papers_catalog.db`. Both
//! commands read the two and merge. The catalogs open directly, the
//! same way `distill` does, and never touch the daemon: the runs
//! surface is local-only and read-only.

use std::collections::BTreeMap;

use bookrack_catalog::{Catalog, PipelineRun, PipelineRunSummary, RunProfileBucket};
use bookrack_cli_grammar::RunsAction;
use bookrack_config::Config;
use eyre::{Context as _, Result};
use serde_json::Value as JsonValue;

use crate::render::distribution::render_histogram_bars;

/// Dispatch the requested `bookrack runs` action.
pub fn run(selection: &bookrack_config::LibrarySelection, action: RunsAction) -> Result<()> {
    let cfg = Config::resolve(selection).context("resolve configuration")?;
    let catalogs = open_run_catalogs(&cfg)?;
    match action {
        RunsAction::List { last, command } => list(&catalogs, last, command.as_deref()),
        RunsAction::Show { run_id } => show(&catalogs, &run_id),
    }
}

/// Open every catalog that carries a `pipeline_runs` registry. The
/// paper catalog joins only when its file already exists, so a
/// read-only `runs` invocation does not materialize an empty papers
/// database as a side effect.
fn open_run_catalogs(cfg: &Config) -> Result<Vec<Catalog>> {
    let book_path = cfg.catalog_db();
    let mut catalogs =
        vec![Catalog::open(&book_path).with_context(|| format!("open {}", book_path.display()))?];
    let papers_path = cfg.papers_catalog_db();
    if papers_path.exists() {
        catalogs.push(
            Catalog::open(&papers_path)
                .with_context(|| format!("open {}", papers_path.display()))?,
        );
    }
    Ok(catalogs)
}

/// Render the recent-runs table. Empty result prints a single `No runs`
/// line so the operator sees an explicit zero rather than blank output.
fn list(catalogs: &[Catalog], last: Option<usize>, command: Option<&str>) -> Result<()> {
    let rows = collect_runs(catalogs, last, command)?;
    println!("{}", render_runs_list(&rows));
    Ok(())
}

/// Pull recent runs from every catalog, join each against its rollup
/// row in the catalog it came from, and merge into one newest-first
/// list. The per-catalog `last` limit keeps each source query bounded;
/// the merged list truncates to the same limit again, so the union
/// still contains the global most-recent N.
fn collect_runs(
    catalogs: &[Catalog],
    last: Option<usize>,
    command: Option<&str>,
) -> Result<Vec<(PipelineRun, Option<PipelineRunSummary>)>> {
    let mut rows = Vec::new();
    for catalog in catalogs {
        let runs = catalog
            .list_pipeline_runs(command, last)
            .context("list pipeline_runs")?;
        for run in runs {
            let summary = catalog
                .pipeline_run_summary(&run.pipeline_run_id)
                .context("read pipeline_run_summary row")?;
            rows.push((run, summary));
        }
    }
    rows.sort_by(|(a, _), (b, _)| {
        (b.started_at.as_str(), b.pipeline_run_id.as_str())
            .cmp(&(a.started_at.as_str(), a.pipeline_run_id.as_str()))
    });
    if let Some(limit) = last {
        rows.truncate(limit);
    }
    Ok(rows)
}

/// Render `runs show <id>`. The id resolves against each catalog in
/// turn. Empty rollup (no audit rows under this run) prints the header
/// section but omits the three histograms; that case is normal for
/// runs from commands like `ingest` / `dryrun` that do not write
/// audits today.
fn show(catalogs: &[Catalog], pipeline_run_id: &str) -> Result<()> {
    for catalog in catalogs {
        let Some(run) = catalog
            .pipeline_run(pipeline_run_id)
            .context("read pipeline_runs row")?
        else {
            continue;
        };
        let summary = catalog
            .pipeline_run_summary(pipeline_run_id)
            .context("read pipeline_run_summary row")?;
        let buckets = catalog
            .run_profile_buckets(pipeline_run_id)
            .context("read profile buckets")?;
        println!("{}", render_run_show(&run, summary.as_ref(), &buckets)?);
        return Ok(());
    }
    Err(eyre::eyre!("no pipeline run with id {pipeline_run_id:?}"))
}

/// Build the `runs list` text block from pre-joined (run, rollup)
/// pairs. Public to the crate so tests can assert on the rendered
/// shape without spawning the binary.
pub(crate) fn render_runs_list(rows: &[(PipelineRun, Option<PipelineRunSummary>)]) -> String {
    if rows.is_empty() {
        return "No runs.".to_string();
    }
    let mut out = String::new();
    out.push_str("run_id                                                  command         started_at            status   n_books  n_papers  needs_work\n");
    for (run, summary) in rows {
        let summary = summary.as_ref();
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
    out.trim_end().to_string()
}

/// Build the `runs show <id>` text block.
pub(crate) fn render_run_show(
    run: &PipelineRun,
    summary: Option<&PipelineRunSummary>,
    buckets: &[RunProfileBucket],
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
    if !buckets.is_empty() {
        out.push_str("\nprofiles:\n");
        for bucket in buckets {
            let fingerprint = bucket.profile_fingerprint.as_deref().unwrap_or("(legacy)");
            let identity = match bucket.profile_name.as_deref() {
                Some(name) => format!("{name} @ {fingerprint}"),
                None => fingerprint.to_string(),
            };
            out.push_str(&format!(
                "  {kind:<6} {identity:<45} {n:>5}\n",
                kind = bucket.kind,
                identity = identity,
                n = bucket.n,
            ));
        }
    }
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
        let out = render_runs_list(&[]);
        assert_eq!(out, "No runs.");
    }

    #[test]
    fn runs_list_aggregates_per_run_columns() {
        let catalog = open_in_memory();
        seed_run(&catalog, "run-a", "distill_build", "2026-06-28T10:00:00Z");
        seed_summary(&catalog, "run-a", 3, r#"{"clean":2,"needs_work":1}"#);
        let rows = collect_runs(std::slice::from_ref(&catalog), None, None).expect("collect");
        let out = render_runs_list(&rows);
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
    fn runs_list_merges_two_catalogs_newest_first() {
        let books = open_in_memory();
        let papers = open_in_memory();
        seed_run(&books, "run-book", "distill_build", "2026-06-28T10:00:00Z");
        seed_run(&papers, "run-paper", "glean", "2026-06-28T11:00:00Z");
        catalogs_seed_paper_summary(&papers, "run-paper");
        let catalogs = [books, papers];

        let rows = collect_runs(&catalogs, None, None).expect("collect");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0.pipeline_run_id, "run-paper");
        assert_eq!(rows[1].0.pipeline_run_id, "run-book");
        // The rollup joins from the catalog its run came from.
        assert_eq!(rows[0].1.as_ref().map(|s| s.n_papers), Some(1));

        // The merged list re-applies the limit after the union.
        let rows = collect_runs(&catalogs, Some(1), None).expect("collect");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.pipeline_run_id, "run-paper");
    }

    fn catalogs_seed_paper_summary(catalog: &Catalog, id: &str) {
        catalog
            .upsert_pipeline_run_summary(&NewPipelineRunSummary {
                pipeline_run_id: id.to_string(),
                n_books: 0,
                n_papers: 1,
                verdict_counts: r#"{"clean":1}"#.to_string(),
                flag_counts: "{}".to_string(),
                coverage_summary: "{}".to_string(),
                wall_clock_ms: Some(500),
                computed_at: "2026-06-28T11:00:06Z".to_string(),
            })
            .expect("upsert summary");
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
        let out = render_run_show(&run, Some(&summary), &[]).expect("render");
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
        let out = render_run_show(&run, None, &[]).expect("render");
        assert!(out.contains("run_id:       run-c"));
        assert!(out.contains("no rollup recorded for this run."));
        assert!(!out.contains("verdict:"));
    }

    #[test]
    fn runs_show_groups_by_profile_fingerprint() {
        let catalog = open_in_memory();
        seed_run(&catalog, "run-d", "glean_review", "2026-06-28T13:00:00Z");
        let run = catalog
            .pipeline_run("run-d")
            .expect("read")
            .expect("present");
        let buckets = vec![
            RunProfileBucket {
                kind: "paper".to_string(),
                profile_fingerprint: Some("0123456789abcdef".to_string()),
                profile_name: Some("default".to_string()),
                n: 4,
            },
            RunProfileBucket {
                kind: "paper".to_string(),
                profile_fingerprint: None,
                profile_name: Some("default".to_string()),
                n: 1,
            },
            RunProfileBucket {
                kind: "book".to_string(),
                profile_fingerprint: Some("fedcba9876543210".to_string()),
                profile_name: None,
                n: 2,
            },
        ];
        let out = render_run_show(&run, None, &buckets).expect("render");
        assert!(out.contains("\nprofiles:\n"));
        assert!(out.contains("default @ 0123456789abcdef"));
        assert!(out.contains("default @ (legacy)"));
        assert!(out.contains("fedcba9876543210"));
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
