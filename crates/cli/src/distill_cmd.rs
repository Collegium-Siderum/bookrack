// SPDX-License-Identifier: Apache-2.0

//! Local `bookrack distill` subcommand family.
//!
//! Owns the operator-facing surface for the v2 distill rollout:
//!
//! * `bookrack distill build <PATH>...` — resolve each path to a
//!   `(book.toml, source, slug)` triple, run the book's pipeline, and
//!   upsert the resulting drafts into `<data>/reference.db`. The path
//!   shape mirrors `bookrack ingest`: a `book.toml` file, a directory
//!   holding one, a source file with a co-located `book.toml`, or a
//!   list of any of these. `--recursive` walks directories the same
//!   way ingest does; `--dry-run` prints coverage without touching
//!   the database.
//! * `bookrack distill verify <PATH>...` — re-run distill into a
//!   throwaway in-memory map and diff the entry set against the
//!   persistent one. Surfaces added / removed / changed `entry_key`s
//!   without mutating either side.
//! * `bookrack distill list` — list `reference_books` rows with
//!   per-book entry counts and the most recent `built_at`. Renders a
//!   table by default; emits raw JSON under the global `--json` flag.
//!
//! These commands open `Refs` directly rather than going through the
//! daemon's control plane. SQLite's WAL mode makes the local handle
//! safe alongside the daemon's reads; the daemon itself does not
//! write to `reference.db` today.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bookrack_catalog::{
    Catalog, GATE_STATUS_FAIL, GATE_STATUS_OFF, GATE_STATUS_PASS, NewBookDistillAudit,
    NewStageReport,
};
use bookrack_cli_grammar::{
    DistillAction, DistillBuildArgs, DistillLintArgs, DistillListArgs, DistillVerifyArgs,
};
use bookrack_config::Config;
use bookrack_distill::{BookToml, Coverage, EntryDraft, StageReport, load_pipeline};
use bookrack_refs::{IndexKind, IndexSpec, NewBook, NewEntry, Refs};
use eyre::{Context as _, Result, bail, eyre};
use serde_json::{Map as JsonMap, Value as JsonValue, json};

use crate::render::ctx;
use crate::render::table::RowTable;

/// One-shot resolver for the data root paths the distill commands
/// share.
struct DistillPaths {
    refs_path: PathBuf,
    catalog_path: PathBuf,
}

impl DistillPaths {
    fn resolve(selection: &bookrack_config::LibrarySelection) -> Result<Self> {
        let cfg = Config::resolve(selection).context("resolve configuration")?;
        let data_dir = cfg.data_dir().to_path_buf();
        Ok(Self {
            refs_path: data_dir.join("reference.db"),
            catalog_path: cfg.catalog_db(),
        })
    }
}

/// Dispatch the requested distill action.
pub async fn run(
    selection: &bookrack_config::LibrarySelection,
    action: DistillAction,
) -> Result<()> {
    let paths = DistillPaths::resolve(selection)?;
    match action {
        DistillAction::Build(args) => build(&paths, args),
        DistillAction::Verify(args) => verify(&paths, args),
        DistillAction::Lint(args) => lint(args),
        DistillAction::List(args) => list(&paths, args),
    }
}

// ---------------------------------------------------------------------------
// path resolution
// ---------------------------------------------------------------------------

/// One book reachable from a `<PATH>` argument, normalised so the
/// pipeline runner does not have to re-derive any of the three fields.
#[derive(Debug, Clone)]
struct ResolvedBook {
    book_toml: PathBuf,
    source: PathBuf,
    slug: String,
}

/// Resolve every `<PATH>` argument into a flat list of `ResolvedBook`
/// triples. Duplicate slugs across paths are rejected; an unreachable
/// `book.toml` or missing co-located declaration surfaces as an error.
fn resolve_paths(paths: &[PathBuf], recursive: bool) -> Result<Vec<ResolvedBook>> {
    let mut out: Vec<ResolvedBook> = Vec::new();
    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
    for path in paths {
        if !path.exists() {
            bail!("path does not exist: {}", path.display());
        }
        let resolved = if path.is_dir() {
            resolve_directory(path, recursive)?
        } else if is_book_toml(path) {
            vec![load_resolved_from_toml(path, None)?]
        } else {
            vec![resolve_source_file(path)?]
        };
        for book in resolved {
            if let Some(prev) = seen.get(&book.slug) {
                bail!(
                    "duplicate slug {:?} resolved from {} and {}",
                    book.slug,
                    prev.display(),
                    book.book_toml.display()
                );
            }
            seen.insert(book.slug.clone(), book.book_toml.clone());
            out.push(book);
        }
    }
    if out.is_empty() {
        bail!("no distillable books resolved from the given paths");
    }
    Ok(out)
}

fn resolve_directory(dir: &Path, recursive: bool) -> Result<Vec<ResolvedBook>> {
    let tomls = collect_book_tomls(dir, recursive);
    if tomls.is_empty() {
        bail!("no book.toml under {}", dir.display());
    }
    let mut out = Vec::with_capacity(tomls.len());
    for toml in tomls {
        out.push(load_resolved_from_toml(&toml, None)?);
    }
    Ok(out)
}

fn resolve_source_file(source: &Path) -> Result<ResolvedBook> {
    let dir = source
        .parent()
        .ok_or_else(|| eyre!("source path {} has no parent", source.display()))?;
    let stem_toml = source
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|stem| dir.join(format!("{stem}.book.toml")));
    if let Some(candidate) = &stem_toml
        && candidate.is_file()
    {
        return load_resolved_from_toml(candidate, Some(source.to_path_buf()));
    }
    let neighbour = dir.join("book.toml");
    if neighbour.is_file() {
        return load_resolved_from_toml(&neighbour, Some(source.to_path_buf()));
    }
    bail!(
        "no book.toml co-located with {}; distill requires a stage declaration",
        source.display()
    );
}

fn load_resolved_from_toml(
    book_toml: &Path,
    explicit_source: Option<PathBuf>,
) -> Result<ResolvedBook> {
    let parsed =
        BookToml::load(book_toml).with_context(|| format!("load {}", book_toml.display()))?;
    let source = match explicit_source {
        Some(p) => p,
        None => locate_source(book_toml)?,
    };
    Ok(ResolvedBook {
        book_toml: book_toml.to_path_buf(),
        source,
        slug: parsed.book_slug,
    })
}

/// Locate the OCR Markdown source given the path to its `book.toml`.
/// Accepts either a single `source.md` next to the toml or a
/// `sources/` directory of `*.md` fragments. The directory form is
/// returned verbatim; `read_source` concatenates fragments on demand.
fn locate_source(book_toml: &Path) -> Result<PathBuf> {
    let dir = book_toml
        .parent()
        .ok_or_else(|| eyre!("book.toml path {} has no parent", book_toml.display()))?;
    let single = dir.join("source.md");
    if single.is_file() {
        return Ok(single);
    }
    let multi = dir.join("sources");
    if multi.is_dir() {
        return Ok(multi);
    }
    Err(eyre!(
        "neither {} nor {} exists",
        single.display(),
        multi.display()
    ))
}

fn is_book_toml(path: &Path) -> bool {
    path.file_name().and_then(|s| s.to_str()) == Some("book.toml")
        || path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|n| n.ends_with(".book.toml"))
            .unwrap_or(false)
}

/// Collect every `book.toml` file reachable under `dir`. When
/// `recursive` is false, only the immediate subdirectories' tomls are
/// considered; with `recursive`, the walk descends fully.
fn collect_book_tomls(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let direct = dir.join("book.toml");
    if direct.is_file() {
        out.push(direct);
        return out;
    }
    visit_book_tomls(dir, recursive, &mut out);
    out.sort();
    out
}

fn visit_book_tomls(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let nested = path.join("book.toml");
            if nested.is_file() {
                out.push(nested);
            } else if recursive {
                subdirs.push(path);
            }
        }
    }
    if recursive {
        for sub in subdirs {
            visit_book_tomls(&sub, recursive, out);
        }
    }
}

// ---------------------------------------------------------------------------
// build
// ---------------------------------------------------------------------------

fn build(paths: &DistillPaths, args: DistillBuildArgs) -> Result<()> {
    let books = resolve_paths(&args.paths, args.recursive)?;
    let distill_run_id = chrono::Utc::now().to_rfc3339();
    let pipeline_run_id = open_distill_pipeline_run(paths, &args)?;
    let mut run_status = "ok";
    let outcome = run_books(
        paths,
        &books,
        &args,
        &distill_run_id,
        pipeline_run_id.as_deref(),
    );
    if outcome.is_err() {
        run_status = "error";
    }
    if let Some(pipeline_run_id) = pipeline_run_id.as_deref() {
        finalize_pipeline_run(paths, pipeline_run_id, run_status);
    }
    outcome
}

/// Open a `pipeline_runs` row for this distill build. Audit-write
/// failure must not block the build, and neither must run lifecycle:
/// catalog-open errors here demote to a warning and the build keeps
/// going under a NULL `pipeline_run_id`.
fn open_distill_pipeline_run(
    paths: &DistillPaths,
    args: &DistillBuildArgs,
) -> Result<Option<String>> {
    if args.no_audit_write {
        return Ok(None);
    }
    let catalog = match Catalog::open(&paths.catalog_path) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(
                error = %err,
                path = %paths.catalog_path.display(),
                "distill: failed to open catalog.db for pipeline_run lifecycle",
            );
            return Ok(None);
        }
    };
    let library_root = paths
        .catalog_path
        .parent()
        .and_then(|p| p.to_str())
        .map(str::to_string);
    let id = catalog
        .open_pipeline_run("distill_build", None, library_root.as_deref())
        .context("open pipeline run")?;
    Ok(Some(id))
}

/// Close the run row and refresh its rollup. Best-effort: any error
/// here logs and the build's exit status stays untouched.
fn finalize_pipeline_run(paths: &DistillPaths, pipeline_run_id: &str, status: &str) {
    let catalog = match Catalog::open(&paths.catalog_path) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(
                error = %err,
                path = %paths.catalog_path.display(),
                "distill: failed to open catalog.db to close pipeline run",
            );
            return;
        }
    };
    if let Err(err) = catalog.close_pipeline_run(pipeline_run_id, status) {
        tracing::warn!(error = %err, pipeline_run_id, "distill: close_pipeline_run failed");
    }
    if let Err(err) = catalog.compute_run_summary(pipeline_run_id) {
        tracing::warn!(error = %err, pipeline_run_id, "distill: compute_run_summary failed");
    }
}

fn run_books(
    paths: &DistillPaths,
    books: &[ResolvedBook],
    args: &DistillBuildArgs,
    distill_run_id: &str,
    pipeline_run_id: Option<&str>,
) -> Result<()> {
    for book in books {
        let parsed = BookToml::load(&book.book_toml)
            .with_context(|| format!("load {}", book.book_toml.display()))?;
        let pipeline = load_pipeline(&book.book_toml)
            .with_context(|| format!("assemble pipeline for {}", book.slug))?;
        let source = read_source(&book.source)
            .with_context(|| format!("read OCR source for {}", book.slug))?;
        let extras = compose_extras(&book.slug, distill_run_id);
        let started_at = chrono::Utc::now();
        let (drafts, coverage) = pipeline
            .run_with_extras(source, extras)
            .with_context(|| format!("run pipeline for {}", book.slug))?;
        let finished_at = chrono::Utc::now();

        print_stage_table(&book.slug, &coverage.stage_reports);

        // The retention guard runs first so its verdict is on the audit
        // row before we either bail or proceed. A `fail` row exists in
        // `book_distill_audit` even though the build bails, which is the
        // whole point of recording the gate verdict.
        let gate_outcome = compute_gate_outcome(&book.slug, &coverage.stage_reports, args);
        if !args.no_audit_write {
            write_distill_audit(
                paths,
                book,
                &parsed,
                &coverage,
                &started_at,
                &finished_at,
                &gate_outcome,
                pipeline_run_id,
            );
        }
        if let Some(err) = gate_outcome.error {
            return Err(err);
        }

        if args.dry_run {
            println!(
                "[dry-run] {}: entries={} coverage_pct={:.1}",
                book.slug,
                drafts.len(),
                coverage.coverage_pct()
            );
            continue;
        }

        let mut refs = Refs::open(&paths.refs_path)
            .with_context(|| format!("open {}", paths.refs_path.display()))?;
        register_book_indexes(&mut refs, &book.slug, &parsed)?;
        upsert_book_row(&refs, &parsed, distill_run_id)?;
        for draft in &drafts {
            let entry = draft_to_new_entry(draft);
            refs.upsert_entry(&entry)?;
        }
        println!(
            "{}: entries={} coverage_pct={:.1} written to {}",
            book.slug,
            drafts.len(),
            coverage.coverage_pct(),
            paths.refs_path.display(),
        );
    }

    Ok(())
}

/// Verdict of the retention guard for one pipeline run, in the shape the
/// audit row consumes. `error` carries the bail-worthy failure when the
/// guard rejected the run; the caller writes the audit row first and
/// then propagates the error.
struct GateOutcome {
    status: &'static str,
    threshold: Option<f64>,
    error: Option<eyre::Report>,
}

fn compute_gate_outcome(
    slug: &str,
    reports: &[StageReport],
    args: &DistillBuildArgs,
) -> GateOutcome {
    if args.no_retention_check {
        return GateOutcome {
            status: GATE_STATUS_OFF,
            threshold: None,
            error: None,
        };
    }
    match enforce_retention(slug, reports, args.retention_threshold) {
        Ok(()) => GateOutcome {
            status: GATE_STATUS_PASS,
            threshold: Some(args.retention_threshold),
            error: None,
        },
        Err(err) => GateOutcome {
            status: GATE_STATUS_FAIL,
            threshold: Some(args.retention_threshold),
            error: Some(err),
        },
    }
}

/// Write one distill build's audit pair into `catalog.db`. Failure to
/// open or write `catalog.db` is logged and otherwise swallowed: the
/// pipeline's primary output is `reference.db`, and an audit miss must
/// never block a successful build.
#[allow(clippy::too_many_arguments)]
fn write_distill_audit(
    paths: &DistillPaths,
    book: &ResolvedBook,
    parsed: &BookToml,
    coverage: &Coverage,
    started_at: &chrono::DateTime<chrono::Utc>,
    finished_at: &chrono::DateTime<chrono::Utc>,
    gate: &GateOutcome,
    pipeline_run_id: Option<&str>,
) {
    let header = NewBookDistillAudit {
        book_slug: book.slug.clone(),
        source_path: book.source.display().to_string(),
        started_at: format_iso8601(started_at),
        finished_at: format_iso8601(finished_at),
        pages: coverage.pages as i64,
        blocks: coverage.blocks as i64,
        raws: coverage.raws as i64,
        splits: coverage.splits as i64,
        entries: coverage.entries as i64,
        unmatched_lines: coverage.unmatched_lines as i64,
        pair_mismatch: coverage.pair_mismatch as i64,
        gate_status: gate.status.to_string(),
        gate_threshold: gate.threshold,
        profile_ref: bookrack_distill::Catalogs::embedded_fingerprint(),
        extractor_version: parsed.parser_version.clone(),
        pipeline_run_id: pipeline_run_id.map(str::to_string),
        profile_toggle_summary: Some(bookrack_distill::Catalogs::embedded_flag_summary()),
    };
    let stages: Vec<NewStageReport> = coverage
        .stage_reports
        .iter()
        .enumerate()
        .map(|(ord, r)| NewStageReport {
            ord: ord as i64,
            stage_name: r.stage_name.clone(),
            in_kind: r.in_kind.to_string(),
            out_kind: r.out_kind.to_string(),
            in_len: r.in_len as i64,
            out_len: r.out_len as i64,
        })
        .collect();

    let mut catalog = match Catalog::open(&paths.catalog_path) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(
                error = %err,
                path = %paths.catalog_path.display(),
                "distill: failed to open catalog.db for audit write",
            );
            return;
        }
    };
    match catalog.insert_distill_audit(&header, &stages) {
        Ok(run_id) => tracing::debug!(run_id, slug = %book.slug, "distill audit written"),
        Err(err) => tracing::warn!(
            error = %err,
            slug = %book.slug,
            "distill: failed to write book_distill_audit row",
        ),
    }
}

fn format_iso8601(dt: &chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Render the per-stage cardinality and retention block for one
/// pipeline run. Each row reports the kind and item count on both
/// sides of `Stage::run`; same-kind stages also get a `retention`
/// column, while cross-kind stages report `--`. The block is keyed
/// to the book slug so multi-book runs stay readable.
fn print_stage_table(slug: &str, reports: &[StageReport]) {
    if reports.is_empty() {
        return;
    }
    println!("{slug}: per-stage cardinality");
    for report in reports {
        let retention = match report.retention() {
            Some(r) => format!("{:>6.1}%", r * 100.0),
            None => "    --".to_string(),
        };
        println!(
            "  {:<28} {:>7}:{:<7} -> {:>7}:{:<7} {}",
            report.stage_name,
            report.in_kind,
            report.in_len,
            report.out_kind,
            report.out_len,
            retention,
        );
        for line in &report.dropped_sample {
            println!("      - dropped: {line}");
        }
    }
}

/// Fail the run when any same-kind stage carried less than `threshold`
/// of its input across. Cross-kind stages have no meaningful ratio
/// and are ignored. The threshold is a fraction in `[0.0, 1.0]`.
fn enforce_retention(slug: &str, reports: &[StageReport], threshold: f64) -> Result<()> {
    if !(0.0..=1.0).contains(&threshold) {
        bail!("retention-threshold must be in [0.0, 1.0], got {threshold}");
    }
    for report in reports {
        if let Some(ratio) = report.retention()
            && ratio < threshold
        {
            bail!(
                "{slug}: stage {:?} retained {:.1}% of its input ({} -> {}), below the {:.1}% threshold; \
                 inspect the stage configuration or pass `--no-retention-check` to skip the guard",
                report.stage_name,
                ratio * 100.0,
                report.in_len,
                report.out_len,
                threshold * 100.0,
            );
        }
    }
    Ok(())
}

/// `bookrack distill lint`: parse + validate each book.toml against
/// the catalogs, then run the pipeline against a truncated source
/// sample and print the per-stage retention table. Keeps going
/// across books so a multi-book invocation surfaces every failure
/// at once; the command exits non-zero when any book failed the
/// static check.
fn lint(args: DistillLintArgs) -> Result<()> {
    let books = resolve_paths(&args.paths, args.recursive)?;
    let distill_run_id = chrono::Utc::now().to_rfc3339();
    let mut failures = 0usize;

    for book in &books {
        match lint_one(book, &distill_run_id, args.sample_lines) {
            Ok(()) => println!("{}: lint OK", book.slug),
            Err(err) => {
                failures += 1;
                println!("{}: lint FAIL", book.slug);
                println!("  {err:#}");
            }
        }
    }

    if failures > 0 {
        bail!("{failures} of {} book(s) failed lint", books.len());
    }
    Ok(())
}

/// One book's lint pass. Returns the first error along the
/// load + sample run chain; `lint` formats the verdict.
fn lint_one(book: &ResolvedBook, distill_run_id: &str, sample_lines: usize) -> Result<()> {
    BookToml::load(&book.book_toml)
        .with_context(|| format!("load {}", book.book_toml.display()))?;
    let pipeline = load_pipeline(&book.book_toml)
        .with_context(|| format!("assemble pipeline for {}", book.slug))?;

    if sample_lines == 0 {
        return Ok(());
    }

    let source =
        read_source(&book.source).with_context(|| format!("read OCR source for {}", book.slug))?;
    let sample = take_first_lines(&source, sample_lines);
    let extras = compose_extras(&book.slug, distill_run_id);
    let (drafts, coverage) = pipeline
        .run_with_extras(sample, extras)
        .with_context(|| format!("sample run for {}", book.slug))?;

    print_stage_table(&book.slug, &coverage.stage_reports);
    println!(
        "  sample: {} line(s) -> {} entry(ies); coverage_pct {:.1}",
        sample_lines,
        drafts.len(),
        coverage.coverage_pct()
    );
    Ok(())
}

/// Truncate `source` to at most `max_lines` `\n`-delimited lines and
/// return the result. A trailing newline is appended so downstream
/// stages that look for it (page markers, anchor regexes) match the
/// same way they would on a complete source.
fn take_first_lines(source: &str, max_lines: usize) -> String {
    let mut out = String::new();
    for (i, line) in source.lines().enumerate() {
        if i >= max_lines {
            break;
        }
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }
    out.push('\n');
    out
}

/// Read the OCR Markdown payload behind a resolved source path.
/// Files are loaded verbatim; directories are treated as a fragment
/// set and concatenated in sorted filename order.
fn read_source(source: &Path) -> Result<String> {
    if source.is_file() {
        return std::fs::read_to_string(source)
            .with_context(|| format!("read {}", source.display()));
    }
    if source.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(source)
            .with_context(|| format!("read_dir {}", source.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
            .collect();
        entries.sort();
        let mut acc = String::new();
        for path in entries {
            let chunk = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            acc.push_str(&chunk);
            if !chunk.ends_with('\n') {
                acc.push('\n');
            }
        }
        return Ok(acc);
    }
    Err(eyre!("source path does not exist: {}", source.display()))
}

fn compose_extras(slug: &str, distill_run_id: &str) -> JsonMap<String, JsonValue> {
    let mut extras = JsonMap::new();
    extras.insert("book_slug".to_string(), JsonValue::String(slug.to_string()));
    extras.insert(
        "distill_run_id".to_string(),
        JsonValue::String(distill_run_id.to_string()),
    );
    extras
}

fn register_book_indexes(refs: &mut Refs, slug: &str, book_toml: &BookToml) -> Result<()> {
    let specs: Vec<IndexSpec> = book_toml
        .indexes
        .iter()
        .map(|i| {
            let kind = parse_index_kind(&i.kind).with_context(|| {
                format!(
                    "book.toml for {slug}: unknown index kind {:?} on field {:?}",
                    i.kind, i.field
                )
            })?;
            Ok(IndexSpec {
                field: i.field.clone(),
                kind,
            })
        })
        .collect::<Result<_>>()?;
    refs.register_book(slug, &specs)?;
    Ok(())
}

fn parse_index_kind(raw: &str) -> Result<IndexKind> {
    match raw {
        "btree" => Ok(IndexKind::Btree),
        other => Err(eyre!("unsupported index kind {other:?}")),
    }
}

fn upsert_book_row(refs: &Refs, book_toml: &BookToml, built_at: &str) -> Result<()> {
    let new_book = NewBook {
        book_slug: book_toml.book_slug.clone(),
        schema_name: book_toml.schema_name.clone(),
        schema_version: book_toml.schema_version,
        parser_version: book_toml.parser_version.clone(),
        // book.toml carries no `[book]` metadata in phase 10; the slug
        // doubles as the human-readable title until that section
        // lands.
        title_zh: book_toml.book_slug.clone(),
        title_en: None,
        edition: None,
        publisher: None,
        year: None,
        isbn: None,
        authority_rank: book_toml.authority_rank,
        built_at: built_at.to_string(),
        intake_id: None,
    };
    refs.upsert_book(&new_book)?;
    Ok(())
}

fn draft_to_new_entry(draft: &EntryDraft) -> NewEntry {
    NewEntry {
        book_slug: draft.book_slug.clone(),
        entry_key: draft.entry_key.clone(),
        headword: draft.headword.clone(),
        aliases: draft.aliases.clone(),
        payload: JsonValue::Object(draft.payload.clone()),
        fts_text: draft.fts_text.clone(),
        source: draft.source.clone(),
        quality_flags: draft.quality_flags.clone(),
    }
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

fn verify(paths: &DistillPaths, args: DistillVerifyArgs) -> Result<()> {
    let books = resolve_paths(&args.paths, args.recursive)?;

    // Guard the `reference.db` location at the entry point rather than
    // letting `Refs::open` create an empty SQLite file at any path it
    // is handed: a typo or wrong directory would otherwise produce a
    // brand-new empty database, the diff would compare every drafted
    // entry against an empty `reference_entries` table, and the report
    // would mislabel the whole book as `added`. Mirrors the no-DB
    // branch in `list`.
    if !paths.refs_path.exists() {
        bail!(
            "verify: reference.db not found at {}",
            paths.refs_path.display()
        );
    }
    if !paths.refs_path.is_file() {
        bail!(
            "verify: expected a file at {}, found a directory or other non-file entry",
            paths.refs_path.display()
        );
    }

    let prod_refs = Refs::open(&paths.refs_path)
        .with_context(|| format!("open {}", paths.refs_path.display()))?;

    let distill_run_id = chrono::Utc::now().to_rfc3339();
    for book in &books {
        let pipeline = load_pipeline(&book.book_toml)?;
        let source = read_source(&book.source)?;
        let extras = compose_extras(&book.slug, &distill_run_id);
        let (drafts, _coverage) = pipeline.run_with_extras(source, extras)?;

        let proposed: BTreeMap<String, EntryDraft> = drafts
            .into_iter()
            .map(|d| (d.entry_key.clone(), d))
            .collect();
        let live = read_live_entries(&prod_refs, &book.slug)?;

        diff_and_report(&book.slug, &proposed, &live);
    }

    Ok(())
}

/// One row of `reference_entries` flattened for diff purposes.
#[derive(Debug, PartialEq, Eq)]
struct LiveEntry {
    headword: String,
    payload_json: String,
}

fn read_live_entries(refs: &Refs, slug: &str) -> Result<BTreeMap<String, LiveEntry>> {
    let conn = refs.connection();
    let mut stmt = conn.prepare(
        "SELECT entry_key, headword, payload_json \
           FROM reference_entries \
          WHERE book_slug = ?1",
    )?;
    let rows = stmt.query_map([slug], |row| {
        Ok((
            row.get::<_, String>(0)?,
            LiveEntry {
                headword: row.get::<_, String>(1)?,
                payload_json: row.get::<_, String>(2)?,
            },
        ))
    })?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (key, entry) = row?;
        out.insert(key, entry);
    }
    Ok(out)
}

fn diff_and_report(
    slug: &str,
    proposed: &BTreeMap<String, EntryDraft>,
    live: &BTreeMap<String, LiveEntry>,
) {
    let proposed_keys: BTreeSet<&str> = proposed.keys().map(String::as_str).collect();
    let live_keys: BTreeSet<&str> = live.keys().map(String::as_str).collect();

    let added: Vec<&str> = proposed_keys.difference(&live_keys).copied().collect();
    let removed: Vec<&str> = live_keys.difference(&proposed_keys).copied().collect();
    let mut changed: Vec<&str> = Vec::new();
    for key in proposed_keys.intersection(&live_keys) {
        let new = &proposed[*key];
        let old = &live[*key];
        let new_payload = serde_json::to_string(&new.payload).unwrap_or_default();
        if new.headword != old.headword || new_payload != old.payload_json {
            changed.push(key);
        }
    }

    println!(
        "{slug}: {} added, {} removed, {} changed",
        added.len(),
        removed.len(),
        changed.len(),
    );
    print_list("added", &added);
    print_list("removed", &removed);
    print_list("changed", &changed);
}

fn print_list(label: &str, keys: &[&str]) {
    for k in keys {
        println!("  {label}: {k}");
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn list(paths: &DistillPaths, _args: DistillListArgs) -> Result<()> {
    if !paths.refs_path.is_file() {
        if ctx().is_quiet() {
            return Ok(());
        }
        if ctx().is_json() {
            println!("{}", json!({"books": []}));
        } else {
            println!("no reference.db at {}", paths.refs_path.display());
        }
        return Ok(());
    }
    let refs = Refs::open(&paths.refs_path)
        .with_context(|| format!("open {}", paths.refs_path.display()))?;
    let conn = refs.connection();

    let mut stmt = conn.prepare(
        "SELECT b.book_slug, b.title_zh, b.authority_rank, b.built_at, \
                COUNT(e.entry_id) AS entry_count \
           FROM reference_books b \
      LEFT JOIN reference_entries e ON e.book_slug = b.book_slug \
       GROUP BY b.book_slug \
       ORDER BY b.authority_rank DESC, b.built_at ASC",
    )?;
    let row_iter = stmt.query_map([], |row| {
        Ok(ListRow {
            slug: row.get::<_, String>(0)?,
            title: row.get::<_, String>(1)?,
            authority_rank: row.get::<_, i64>(2)?,
            built_at: row.get::<_, String>(3)?,
            entry_count: row.get::<_, i64>(4)?,
        })
    })?;
    let mut rows: Vec<ListRow> = Vec::new();
    for row in row_iter {
        rows.push(row?);
    }

    if ctx().is_quiet() {
        return Ok(());
    }
    if ctx().is_json() {
        let payload = json!({
            "books": rows.iter().map(ListRow::to_json).collect::<Vec<_>>(),
        });
        match serde_json::to_string_pretty(&payload) {
            Ok(text) => println!("{text}"),
            Err(_) => println!("{payload}"),
        }
        return Ok(());
    }
    let mut table = RowTable::new(["slug", "title", "authority_rank", "entry_count", "built_at"]);
    for r in &rows {
        table.push_row([
            r.slug.clone(),
            r.title.clone(),
            r.authority_rank.to_string(),
            r.entry_count.to_string(),
            r.built_at.clone(),
        ]);
    }
    println!("{}", table.render());
    Ok(())
}

#[derive(Debug)]
struct ListRow {
    slug: String,
    title: String,
    authority_rank: i64,
    built_at: String,
    entry_count: i64,
}

impl ListRow {
    fn to_json(&self) -> JsonValue {
        json!({
            "slug": self.slug,
            "title": self.title,
            "authority_rank": self.authority_rank,
            "entry_count": self.entry_count,
            "built_at": self.built_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    const TINY_BOOK_TOML: &str = r#"
book_slug      = "tiny"
schema_name    = "name_translation"
schema_version = 1
parser_version = "0.1.0"
authority_rank = 10

[parser]
writes_properties = []
stages = [
  "split_pages",
  { stage = "one_block_per_page", lang = "latin" },
  { stage = "walk_anchors",
    anchor = "latin_headword",
    splice_orphans_to_prev_block = false },
  "split_headline_only",
  { stage = "to_entry_draft",
    key_normalizer = "normalize_latin_key" },
]
"#;

    const TINY_SOURCE: &str = "<!-- page 1 (sheet 1) -->\nSmith\nJones\n";

    fn seed_book_dir(root: &Path, slug: &str) -> PathBuf {
        let book_dir = root.join("reference").join(slug);
        fs::create_dir_all(&book_dir).expect("mkdir");
        let toml = TINY_BOOK_TOML.replace("\"tiny\"", &format!("\"{slug}\""));
        fs::write(book_dir.join("book.toml"), toml).expect("write book.toml");
        fs::write(book_dir.join("source.md"), TINY_SOURCE).expect("write source.md");
        book_dir
    }

    fn make_paths(root: &Path) -> DistillPaths {
        DistillPaths {
            refs_path: root.join("reference.db"),
            catalog_path: root.join("catalog.db"),
        }
    }

    fn build_args(paths: Vec<PathBuf>, dry_run: bool, recursive: bool) -> DistillBuildArgs {
        DistillBuildArgs {
            paths,
            recursive,
            dry_run,
            retention_threshold: 0.10,
            no_retention_check: false,
            no_audit_write: false,
        }
    }

    fn count_audit_rows(paths: &DistillPaths, slug: &str) -> (i64, i64) {
        let catalog = Catalog::open(&paths.catalog_path).expect("open catalog");
        let rows = catalog
            .distill_audits_for_book(slug)
            .expect("read audit rows");
        let header_count = rows.len() as i64;
        let stage_count: i64 = rows
            .iter()
            .map(|r| {
                catalog
                    .distill_stage_reports(r.run_id)
                    .expect("read stage rows")
                    .len() as i64
            })
            .sum();
        (header_count, stage_count)
    }

    fn verify_args(paths: Vec<PathBuf>) -> DistillVerifyArgs {
        DistillVerifyArgs {
            paths,
            recursive: false,
        }
    }

    fn lint_args(paths: Vec<PathBuf>, sample_lines: usize) -> DistillLintArgs {
        DistillLintArgs {
            paths,
            recursive: false,
            sample_lines,
        }
    }

    #[test]
    fn build_writes_book_and_entries_into_reference_db() {
        let tmp = TempDir::new().expect("tmp");
        let book_dir = seed_book_dir(tmp.path(), "tiny");
        let paths = make_paths(tmp.path());

        build(&paths, build_args(vec![book_dir], false, false)).expect("build");

        let refs = Refs::open(&paths.refs_path).expect("open refs");
        let conn = refs.connection();
        let book_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM reference_books WHERE book_slug = 'tiny'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(book_rows, 1);
        let entry_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM reference_entries WHERE book_slug = 'tiny'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(entry_rows, 2, "Smith + Jones");
    }

    #[test]
    fn build_dry_run_does_not_create_reference_db() {
        let tmp = TempDir::new().expect("tmp");
        let book_dir = seed_book_dir(tmp.path(), "tiny");
        let paths = make_paths(tmp.path());

        build(&paths, build_args(vec![book_dir], true, false)).expect("dry-run build");

        assert!(
            !paths.refs_path.exists(),
            "dry-run must not write to reference.db"
        );
    }

    #[test]
    fn build_writes_book_distill_audit_row() {
        let tmp = TempDir::new().expect("tmp");
        let book_dir = seed_book_dir(tmp.path(), "tiny");
        let paths = make_paths(tmp.path());

        build(&paths, build_args(vec![book_dir], false, false)).expect("build");

        let catalog = Catalog::open(&paths.catalog_path).expect("open catalog");
        let rows = catalog.distill_audits_for_book("tiny").expect("read");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.gate_status, GATE_STATUS_PASS);
        assert_eq!(row.gate_threshold, Some(0.10));
        assert_eq!(row.entries, 2, "Smith + Jones");
        assert_eq!(
            row.profile_ref,
            bookrack_distill::Catalogs::embedded_fingerprint(),
            "profile_ref carries the distill catalog fingerprint",
        );
        assert_eq!(row.profile_ref.len(), 16);
        assert!(row.profile_ref.chars().all(|c| c.is_ascii_hexdigit()));
        let summary = row
            .profile_toggle_summary
            .as_deref()
            .expect("flag summary present");
        assert!(summary.starts_with('['), "summary is a JSON array");
        assert!(summary.contains(r#""severity""#));
        assert_eq!(row.extractor_version, "0.1.0");
        let stages = catalog
            .distill_stage_reports(row.run_id)
            .expect("read stages");
        assert!(!stages.is_empty());
        // The first stage in the fixture pipeline is split_pages.
        assert_eq!(stages[0].ord, 0);
        assert_eq!(stages[0].stage_name, "split_pages");
    }

    #[test]
    fn build_with_no_audit_write_skips_the_audit_table() {
        let tmp = TempDir::new().expect("tmp");
        let book_dir = seed_book_dir(tmp.path(), "tiny");
        let paths = make_paths(tmp.path());

        let mut args = build_args(vec![book_dir], false, false);
        args.no_audit_write = true;
        build(&paths, args).expect("build");

        // reference.db is written as usual; catalog.db is never opened.
        assert!(paths.refs_path.is_file(), "reference.db must still exist");
        assert!(
            !paths.catalog_path.exists(),
            "--no-audit-write must not create catalog.db"
        );
    }

    #[test]
    fn build_dry_run_still_writes_audit_row() {
        // A dry-run does not touch reference.db, but the audit row is
        // exactly the kind of observation a dry-run is meant to leave
        // behind: it records what the pipeline would have produced.
        let tmp = TempDir::new().expect("tmp");
        let book_dir = seed_book_dir(tmp.path(), "tiny");
        let paths = make_paths(tmp.path());

        build(&paths, build_args(vec![book_dir], true, false)).expect("dry-run build");

        assert!(
            !paths.refs_path.exists(),
            "dry-run must not write to reference.db"
        );
        let (headers, _stages) = count_audit_rows(&paths, "tiny");
        assert_eq!(headers, 1);
    }

    #[test]
    fn build_fails_retention_still_writes_audit_with_gate_status_fail() {
        let tmp = TempDir::new().expect("tmp");
        let book_dir = seed_book_dir(tmp.path(), "tiny");
        let paths = make_paths(tmp.path());

        // An out-of-range threshold trips enforce_retention's validity
        // check; the fixture pipeline has no same-kind stages so a
        // tightened threshold alone would not. Either way the gate
        // rejects the run, the audit row records `fail`, and the build
        // bails after the row lands.
        let mut args = build_args(vec![book_dir], false, false);
        args.retention_threshold = 1.5;
        let err = build(&paths, args).expect_err("retention must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("retention-threshold") || msg.contains("threshold"),
            "got: {msg}"
        );

        let catalog = Catalog::open(&paths.catalog_path).expect("open catalog");
        let rows = catalog.distill_audits_for_book("tiny").expect("read");
        assert_eq!(rows.len(), 1, "fail run must still leave one audit row");
        assert_eq!(rows[0].gate_status, GATE_STATUS_FAIL);
        assert_eq!(rows[0].gate_threshold, Some(1.5));
    }

    #[test]
    fn build_with_no_retention_check_records_gate_status_off() {
        let tmp = TempDir::new().expect("tmp");
        let book_dir = seed_book_dir(tmp.path(), "tiny");
        let paths = make_paths(tmp.path());

        let mut args = build_args(vec![book_dir], true, false);
        args.no_retention_check = true;
        build(&paths, args).expect("build");

        let catalog = Catalog::open(&paths.catalog_path).expect("open catalog");
        let rows = catalog.distill_audits_for_book("tiny").expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].gate_status, GATE_STATUS_OFF);
        assert_eq!(rows[0].gate_threshold, None);
    }

    #[test]
    fn build_one_dir_and_recursive_root_are_equivalent_for_a_single_book() {
        let tmp_a = TempDir::new().expect("tmp a");
        let book_dir = seed_book_dir(tmp_a.path(), "tiny");
        let paths_a = make_paths(tmp_a.path());
        build(&paths_a, build_args(vec![book_dir], false, false)).expect("build single dir");

        let tmp_b = TempDir::new().expect("tmp b");
        let _ = seed_book_dir(tmp_b.path(), "tiny");
        let paths_b = make_paths(tmp_b.path());
        let reference_root = tmp_b.path().join("reference");
        build(&paths_b, build_args(vec![reference_root], false, true))
            .expect("build recursive root");

        let count = |paths: &DistillPaths| -> i64 {
            let refs = Refs::open(&paths.refs_path).expect("open");
            let conn = refs.connection();
            conn.query_row(
                "SELECT COUNT(*) FROM reference_entries WHERE book_slug = 'tiny'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(count(&paths_a), count(&paths_b));
    }

    #[test]
    fn verify_reports_no_diff_when_db_matches_book_toml() {
        let tmp = TempDir::new().expect("tmp");
        let book_dir = seed_book_dir(tmp.path(), "tiny");
        let paths = make_paths(tmp.path());
        build(&paths, build_args(vec![book_dir.clone()], false, false)).expect("build");

        verify(&paths, verify_args(vec![book_dir])).expect("verify");
        let refs = Refs::open(&paths.refs_path).expect("open after verify");
        let _ = refs
            .lookup_resolved(None, "smith")
            .expect("lookup post-verify");
    }

    #[test]
    fn verify_detects_a_manual_payload_change() {
        let tmp = TempDir::new().expect("tmp");
        let book_dir = seed_book_dir(tmp.path(), "tiny");
        let paths = make_paths(tmp.path());
        build(&paths, build_args(vec![book_dir.clone()], false, false)).expect("build");

        let refs = Refs::open(&paths.refs_path).expect("open");
        refs.connection()
            .execute(
                "UPDATE reference_entries SET payload_json = '{\"manual\":true}' \
                 WHERE entry_key = 'smith'",
                [],
            )
            .expect("manual update");
        drop(refs);

        let live = {
            let refs = Refs::open(&paths.refs_path).expect("re-open");
            read_live_entries(&refs, "tiny").expect("live")
        };
        let book_toml_path = book_dir.join("book.toml");
        let pipeline = load_pipeline(&book_toml_path).expect("pipeline");
        let source = read_source(&book_dir.join("source.md")).expect("source");
        let extras = compose_extras("tiny", "2026-06-25T00:00:00Z");
        let (drafts, _) = pipeline.run_with_extras(source, extras).expect("run");
        let proposed: BTreeMap<String, EntryDraft> = drafts
            .into_iter()
            .map(|d| (d.entry_key.clone(), d))
            .collect();

        let mut found_change = false;
        for key in proposed.keys() {
            if let Some(live_row) = live.get(key) {
                let new_payload = serde_json::to_string(&proposed[key].payload).unwrap_or_default();
                if new_payload != live_row.payload_json {
                    found_change = true;
                    break;
                }
            }
        }
        assert!(
            found_change,
            "verify must catch the manual payload mutation"
        );
    }

    /// `verify` against a path that does not exist must surface a
    /// `not found` error and leave the path untouched. Previously
    /// `Refs::open` would silently create an empty SQLite file there
    /// and the diff would tag every drafted entry as `added`.
    #[test]
    fn verify_errors_when_refs_db_does_not_exist() {
        let tmp = TempDir::new().expect("tmp");
        let book_dir = seed_book_dir(tmp.path(), "tiny");
        let paths = DistillPaths {
            refs_path: tmp.path().join("missing").join("reference.db"),
            catalog_path: tmp.path().join("missing").join("catalog.db"),
        };

        let err = verify(&paths, verify_args(vec![book_dir])).expect_err("must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("not found"), "error text was: {msg}");
        assert!(
            msg.contains(&paths.refs_path.display().to_string()),
            "error text was: {msg}"
        );
        assert!(
            !paths.refs_path.exists(),
            "verify must not create the missing reference.db"
        );
        assert!(
            !paths.refs_path.parent().unwrap().exists(),
            "verify must not create any directories on the missing path"
        );
    }

    /// `verify` against a path that points at a directory must report
    /// the type mismatch instead of letting `Refs::open` fall over the
    /// SQLite error mid-pipeline.
    #[test]
    fn verify_errors_when_refs_path_is_a_directory() {
        let tmp = TempDir::new().expect("tmp");
        let book_dir = seed_book_dir(tmp.path(), "tiny");
        let dir_path = tmp.path().join("reference.db");
        fs::create_dir_all(&dir_path).expect("create dir at refs path");
        let paths = DistillPaths {
            refs_path: dir_path.clone(),
            catalog_path: tmp.path().join("catalog.db"),
        };

        let err = verify(&paths, verify_args(vec![book_dir])).expect_err("must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("expected a file"), "error text was: {msg}");
        assert!(
            msg.contains(&dir_path.display().to_string()),
            "error text was: {msg}"
        );
        assert!(dir_path.is_dir(), "verify must not touch the directory");
    }

    #[test]
    fn list_prints_each_registered_book_with_its_entry_count() {
        let tmp = TempDir::new().expect("tmp");
        let book_dir = seed_book_dir(tmp.path(), "tiny");
        let paths = make_paths(tmp.path());
        build(&paths, build_args(vec![book_dir], false, false)).expect("build");

        list(&paths, DistillListArgs::default()).expect("list");
    }

    #[test]
    fn resolve_accepts_book_toml_directly() {
        let tmp = TempDir::new().expect("tmp");
        let book_dir = seed_book_dir(tmp.path(), "tiny");
        let book_toml = book_dir.join("book.toml");

        let resolved = resolve_paths(std::slice::from_ref(&book_toml), false).expect("resolve");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].slug, "tiny");
        assert_eq!(resolved[0].book_toml, book_toml);
        assert_eq!(resolved[0].source, book_dir.join("source.md"));
    }

    #[test]
    fn resolve_accepts_directory_with_book_toml() {
        let tmp = TempDir::new().expect("tmp");
        let book_dir = seed_book_dir(tmp.path(), "tiny");

        let resolved = resolve_paths(std::slice::from_ref(&book_dir), false).expect("resolve");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].slug, "tiny");
        assert_eq!(resolved[0].book_toml, book_dir.join("book.toml"));
    }

    #[test]
    fn resolve_walks_recursive_root() {
        let tmp = TempDir::new().expect("tmp");
        let _ = seed_book_dir(tmp.path(), "alpha");
        let _ = seed_book_dir(tmp.path(), "beta");
        let reference_root = tmp.path().join("reference");

        let resolved = resolve_paths(&[reference_root], true).expect("resolve");
        let slugs: BTreeSet<&str> = resolved.iter().map(|r| r.slug.as_str()).collect();
        assert!(slugs.contains("alpha"));
        assert!(slugs.contains("beta"));
        assert_eq!(slugs.len(), 2);
    }

    #[test]
    fn resolve_errors_when_md_has_no_book_toml() {
        let tmp = TempDir::new().expect("tmp");
        let lonely = tmp.path().join("lonely.md");
        fs::write(&lonely, TINY_SOURCE).expect("write");

        let err = resolve_paths(std::slice::from_ref(&lonely), false).expect_err("must error");
        let msg = format!("{err}");
        assert!(msg.contains("no book.toml co-located"), "got error: {msg}");
    }

    #[test]
    fn resolve_accepts_md_with_stem_book_toml() {
        let tmp = TempDir::new().expect("tmp");
        let dir = tmp.path().join("custom");
        fs::create_dir_all(&dir).expect("mkdir");
        let source = dir.join("entries.md");
        fs::write(&source, TINY_SOURCE).expect("source");
        let toml = TINY_BOOK_TOML.replace("\"tiny\"", "\"custom\"");
        fs::write(dir.join("entries.book.toml"), toml).expect("toml");

        let resolved = resolve_paths(std::slice::from_ref(&source), false).expect("resolve");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].slug, "custom");
        assert_eq!(resolved[0].source, source);
        assert_eq!(resolved[0].book_toml, dir.join("entries.book.toml"));
    }

    #[test]
    fn resolve_errors_on_duplicate_slug() {
        let tmp = TempDir::new().expect("tmp");
        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        fs::create_dir_all(&dir_a).expect("mkdir a");
        fs::create_dir_all(&dir_b).expect("mkdir b");
        fs::write(dir_a.join("book.toml"), TINY_BOOK_TOML).expect("toml a");
        fs::write(dir_a.join("source.md"), TINY_SOURCE).expect("source a");
        fs::write(dir_b.join("book.toml"), TINY_BOOK_TOML).expect("toml b");
        fs::write(dir_b.join("source.md"), TINY_SOURCE).expect("source b");

        let err = resolve_paths(&[dir_a, dir_b], false).expect_err("must error");
        let msg = format!("{err}");
        assert!(msg.contains("duplicate slug"), "got error: {msg}");
    }

    /// A well-formed book passes lint with a small sample and does
    /// not write anything to disk.
    #[test]
    fn lint_passes_a_well_formed_book_without_touching_the_database() {
        let tmp = TempDir::new().expect("tmp");
        let book_dir = seed_book_dir(tmp.path(), "tiny");
        let paths = make_paths(tmp.path());

        lint(lint_args(vec![book_dir], 16)).expect("lint OK");

        assert!(
            !paths.refs_path.exists(),
            "lint must not create {}",
            paths.refs_path.display()
        );
    }

    /// A book.toml that references a stage the catalog does not know
    /// fails lint, and the process exits non-zero with a per-book
    /// FAIL line.
    #[test]
    fn lint_fails_a_book_whose_stage_is_not_in_the_catalog() {
        let tmp = TempDir::new().expect("tmp");
        let book_dir = tmp.path().join("reference").join("bad");
        fs::create_dir_all(&book_dir).expect("mkdir");
        let toml = TINY_BOOK_TOML
            .replace("\"tiny\"", "\"bad\"")
            .replace("split_pages", "no_such_stage");
        fs::write(book_dir.join("book.toml"), toml).expect("write");
        fs::write(book_dir.join("source.md"), TINY_SOURCE).expect("write");

        let err = lint(lint_args(vec![book_dir], 16)).expect_err("must fail");
        let msg = format!("{err}");
        assert!(msg.contains("1 of 1"), "got: {msg}");
    }
}
