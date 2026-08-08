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
use std::path::{Path, PathBuf};

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

/// Open every catalog that carries a `pipeline_runs` registry,
/// read-only. Each catalog joins only when its file already exists,
/// so a `runs` invocation neither materializes an empty database as a
/// side effect nor competes with a live writer; a root with no
/// catalog at all yields an empty set and renders as zero runs.
fn open_run_catalogs(cfg: &Config) -> Result<Vec<(PathBuf, Catalog)>> {
    let mut catalogs = Vec::new();
    for path in [cfg.catalog_db(), cfg.papers_catalog_db()] {
        if path.exists() {
            let catalog = Catalog::open_read_only(&path)
                .with_context(|| format!("open {}", path.display()))?;
            catalogs.push((path, catalog));
        }
    }
    Ok(catalogs)
}

/// What `runs list` prints in the status column for an open run whose
/// owner is provably gone.
const ABANDONED_MARK: &str = "abandoned?";

/// The status to print for one row. A run reading `running` whose
/// liveness record proves its owner is gone prints `abandoned?` — the
/// question mark keeps the table honest about the column being this
/// command's reading rather than what the database says. Every other
/// row prints its stored status verbatim.
fn displayed_status(run: &PipelineRun, catalog_db: &Path) -> String {
    let stored = run.status.as_deref().unwrap_or("");
    if stored != bookrack_catalog::RUN_STATUS_RUNNING {
        return stored.to_string();
    }
    let abandoned = bookrack_catalog::run_locks_dir(catalog_db).is_some_and(|dir| {
        bookrack_catalog::run_liveness(&dir, &run.pipeline_run_id)
            == bookrack_catalog::RunLiveness::Abandoned
    });
    if abandoned {
        ABANDONED_MARK.to_string()
    } else {
        stored.to_string()
    }
}

/// Render the recent-runs table. Empty result prints a single `No runs`
/// line so the operator sees an explicit zero rather than blank output.
fn list(catalogs: &[(PathBuf, Catalog)], last: Option<usize>, command: Option<&str>) -> Result<()> {
    let rows = collect_runs(catalogs, last, command)?;
    println!("{}", render_runs_list(&rows));
    Ok(())
}

/// One row of the `runs list` table: the registry row, its rollup, and
/// the status to print — which for an open run is a reading of its
/// liveness record, not just the stored column.
pub(crate) struct RunRow {
    run: PipelineRun,
    summary: Option<PipelineRunSummary>,
    status: String,
}

/// Pull recent runs from every catalog, join each against its rollup
/// row in the catalog it came from, and merge into one newest-first
/// list. The per-catalog `last` limit keeps each source query bounded;
/// the merged list truncates to the same limit again, so the union
/// still contains the global most-recent N.
fn collect_runs(
    catalogs: &[(PathBuf, Catalog)],
    last: Option<usize>,
    command: Option<&str>,
) -> Result<Vec<RunRow>> {
    let mut rows = Vec::new();
    for (path, catalog) in catalogs {
        let runs = catalog
            .list_pipeline_runs(command, last)
            .context("list pipeline_runs")?;
        for run in runs {
            let summary = catalog
                .pipeline_run_summary(&run.pipeline_run_id)
                .context("read pipeline_run_summary row")?;
            let status = displayed_status(&run, path);
            rows.push(RunRow {
                run,
                summary,
                status,
            });
        }
    }
    rows.sort_by(|a, b| {
        (b.run.started_at.as_str(), b.run.pipeline_run_id.as_str())
            .cmp(&(a.run.started_at.as_str(), a.run.pipeline_run_id.as_str()))
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
fn show(catalogs: &[(PathBuf, Catalog)], pipeline_run_id: &str) -> Result<()> {
    for (path, catalog) in catalogs {
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
        let status = displayed_status(&run, path);
        println!(
            "{}",
            render_run_show(&run, &status, summary.as_ref(), &buckets)?
        );
        return Ok(());
    }
    Err(eyre::eyre!("no pipeline run with id {pipeline_run_id:?}"))
}

/// Build the `runs list` text block from pre-joined (run, rollup)
/// pairs. Public to the crate so tests can assert on the rendered
/// shape without spawning the binary.
pub(crate) fn render_runs_list(rows: &[RunRow]) -> String {
    if rows.is_empty() {
        return "No runs.".to_string();
    }
    let mut out = String::new();
    out.push_str("run_id                                                  command         started_at            status      n_books  n_papers  needs_work\n");
    let mut any_abandoned = false;
    for row in rows {
        let summary = row.summary.as_ref();
        let n_books = summary.map(|s| s.n_books).unwrap_or(0);
        let n_papers = summary.map(|s| s.n_papers).unwrap_or(0);
        let needs_work = summary
            .map(|s| extract_count(&s.verdict_counts, "needs_work"))
            .unwrap_or(0);
        any_abandoned |= row.status == ABANDONED_MARK;
        let line = format!(
            "{run_id:<55} {command:<15} {started:<21} {status:<11} {n_books:>7}  {n_papers:>8}  {needs_work:>10}\n",
            run_id = row.run.pipeline_run_id,
            command = row.run.command,
            started = row.run.started_at,
            status = row.status,
            n_books = n_books,
            n_papers = n_papers,
            needs_work = needs_work,
        );
        out.push_str(&line);
    }
    if any_abandoned {
        out.push_str(&format!(
            "\nRows marked `{ABANDONED_MARK}` are still recorded as running, but nothing owns them \
             any more.\nClose them with `bookrack doctor --close-abandoned-runs`.\n"
        ));
    }
    out.trim_end().to_string()
}

/// Build the `runs show <id>` text block.
pub(crate) fn render_run_show(
    run: &PipelineRun,
    status: &str,
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
        if status.is_empty() { "-" } else { status }
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

    /// A catalog on disk, since an open run's liveness record lives
    /// beside the database file and the marking reads it from there.
    fn open_on_disk(dir: &tempfile::TempDir, name: &str) -> (PathBuf, Catalog) {
        let path = dir.path().join(name);
        let catalog = Catalog::open(&path).expect("open catalog");
        (path, catalog)
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
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, catalog) = open_on_disk(&dir, "catalog.db");
        seed_run(&catalog, "run-a", "distill_build", "2026-06-28T10:00:00Z");
        seed_summary(&catalog, "run-a", 3, r#"{"clean":2,"needs_work":1}"#);
        let catalogs = [(path, catalog)];
        let rows = collect_runs(&catalogs, None, None).expect("collect");
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

    /// An open run whose owner is gone is called out in the table, and
    /// one still owned is not — the two are indistinguishable in the
    /// stored column, which is the defect this marking exists for.
    #[test]
    fn runs_list_marks_an_open_run_nobody_owns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, catalog) = open_on_disk(&dir, "catalog.db");
        let live = catalog
            .open_pipeline_run("ingest", None, None)
            .expect("open live run");
        let dead = catalog
            .open_pipeline_run("distill_build", None, None)
            .expect("open dead run");
        let locks = bookrack_catalog::run_locks_dir(&path).expect("lock dir");
        let _held = bookrack_catalog::RunLock::acquire(&locks, &live).expect("hold live record");
        // What a killed process leaves behind: the record, unheld.
        std::fs::write(bookrack_catalog::run_lock_path(&locks, &dead), b"").expect("seed record");

        let catalogs = [(path, catalog)];
        let rows = collect_runs(&catalogs, None, None).expect("collect");
        let status_of = |id: &str| {
            rows.iter()
                .find(|row| row.run.pipeline_run_id == id)
                .expect("row present")
                .status
                .clone()
        };
        assert_eq!(status_of(&dead), "abandoned?");
        assert_eq!(status_of(&live), "running");

        let out = render_runs_list(&rows);
        assert!(out.contains("abandoned?"), "{out}");
        assert!(
            out.contains("bookrack doctor --close-abandoned-runs"),
            "the legend must name the repair, got {out}"
        );
    }

    /// With nothing abandoned, the table carries no legend to explain.
    #[test]
    fn runs_list_says_nothing_about_abandonment_when_there_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, catalog) = open_on_disk(&dir, "catalog.db");
        seed_run(&catalog, "run-ok", "ingest", "2026-06-28T10:00:00Z");
        let catalogs = [(path, catalog)];
        let out = render_runs_list(&collect_runs(&catalogs, None, None).expect("collect"));
        assert!(!out.contains("abandoned"), "{out}");
    }

    #[test]
    fn runs_list_merges_two_catalogs_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (books_path, books) = open_on_disk(&dir, "catalog.db");
        let (papers_path, papers) = open_on_disk(&dir, "papers_catalog.db");
        seed_run(&books, "run-book", "distill_build", "2026-06-28T10:00:00Z");
        seed_run(&papers, "run-paper", "glean", "2026-06-28T11:00:00Z");
        catalogs_seed_paper_summary(&papers, "run-paper");
        let catalogs = [(books_path, books), (papers_path, papers)];

        let rows = collect_runs(&catalogs, None, None).expect("collect");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].run.pipeline_run_id, "run-paper");
        assert_eq!(rows[1].run.pipeline_run_id, "run-book");
        // The rollup joins from the catalog its run came from.
        assert_eq!(rows[0].summary.as_ref().map(|s| s.n_papers), Some(1));

        // The merged list re-applies the limit after the union.
        let rows = collect_runs(&catalogs, Some(1), None).expect("collect");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].run.pipeline_run_id, "run-paper");
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
        let catalog = Catalog::open_in_memory().expect("open in-memory catalog");
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
        let out = render_run_show(&run, "ok", Some(&summary), &[]).expect("render");
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
        let catalog = Catalog::open_in_memory().expect("open in-memory catalog");
        seed_run(&catalog, "run-c", "ingest", "2026-06-28T12:00:00Z");
        let run = catalog
            .pipeline_run("run-c")
            .expect("read")
            .expect("present");
        let out = render_run_show(&run, "ok", None, &[]).expect("render");
        assert!(out.contains("run_id:       run-c"));
        assert!(out.contains("no rollup recorded for this run."));
        assert!(!out.contains("verdict:"));
    }

    #[test]
    fn runs_show_groups_by_profile_fingerprint() {
        let catalog = Catalog::open_in_memory().expect("open in-memory catalog");
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
        let out = render_run_show(&run, "ok", None, &buckets).expect("render");
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
