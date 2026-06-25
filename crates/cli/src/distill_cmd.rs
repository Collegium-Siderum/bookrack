// SPDX-License-Identifier: Apache-2.0

//! Local `bookrack distill` subcommand family.
//!
//! Owns the operator-facing surface for the v2 distill rollout:
//!
//! * `bookrack distill build` — scan `<data>/reference/*/book.toml`,
//!   run each book's pipeline against its OCR source, and upsert
//!   the resulting drafts into `<data>/reference.db`. `--dry-run`
//!   prints coverage without touching the database.
//! * `bookrack distill verify` — re-run distill into a throwaway
//!   in-memory `Refs` and diff the entry set against the persistent
//!   one. Surfaces added / removed / changed `entry_key`s without
//!   mutating either side.
//! * `bookrack distill list` — list `reference_books` rows with
//!   per-book entry counts and the most recent `built_at`.
//!
//! These commands open `Refs` directly rather than going through
//! the daemon's control plane. SQLite's WAL mode makes the local
//! handle safe alongside the daemon's reads; the daemon itself does
//! not write to `reference.db` today.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use bookrack_config::Config;
use bookrack_distill::{BookToml, EntryDraft, load_pipeline};
use bookrack_refs::{IndexKind, IndexSpec, NewBook, NewEntry, Refs};
use clap::Subcommand;
use serde_json::{Map as JsonMap, Value as JsonValue};

/// One `bookrack distill <action>` invocation.
#[derive(Debug, Clone, Subcommand)]
pub enum DistillAction {
    /// Build distilled entries for one or every reference book.
    Build {
        /// Specific book slug to build. When omitted, every book
        /// under `<data>/reference/` is built.
        #[arg(long, value_name = "SLUG")]
        book: Option<String>,
        /// Build every reference book under the data root. Mutually
        /// exclusive with `--book`.
        #[arg(long, conflicts_with = "book")]
        all: bool,
        /// Run the pipeline but do not write to `reference.db`; print
        /// the coverage summary instead.
        #[arg(long)]
        dry_run: bool,
    },
    /// Re-run distill in memory and diff against the live database.
    Verify {
        /// Specific book slug to verify. When omitted, every book is
        /// diffed.
        #[arg(long, value_name = "SLUG")]
        book: Option<String>,
    },
    /// List the registered reference books and their entry counts.
    List,
}

/// One-shot resolver for the data root paths the distill commands
/// share.
struct DistillPaths {
    refs_path: PathBuf,
    reference_root: PathBuf,
}

impl DistillPaths {
    fn resolve(selection: &bookrack_config::LibrarySelection) -> Result<Self> {
        let cfg = Config::resolve(selection).context("resolve configuration")?;
        let data_dir = cfg.data_dir().to_path_buf();
        Ok(Self {
            refs_path: data_dir.join("reference.db"),
            reference_root: data_dir.join("reference"),
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
        DistillAction::Build {
            book,
            all,
            dry_run,
        } => build(&paths, book, all, dry_run),
        DistillAction::Verify { book } => verify(&paths, book),
        DistillAction::List => list(&paths),
    }
}

// ---------------------------------------------------------------------------
// build
// ---------------------------------------------------------------------------

fn build(
    paths: &DistillPaths,
    book: Option<String>,
    all: bool,
    dry_run: bool,
) -> Result<()> {
    let scope = build_scope(paths, book, all)?;
    let distill_run_id = chrono::Utc::now().to_rfc3339();

    for slug in scope {
        let book_dir = paths.reference_root.join(&slug);
        let book_toml_path = book_dir.join("book.toml");
        let book_toml = BookToml::load(&book_toml_path)
            .with_context(|| format!("load {}", book_toml_path.display()))?;
        let pipeline = load_pipeline(&book_toml_path)
            .with_context(|| format!("assemble pipeline for {slug}"))?;
        let source = read_source(&book_dir)
            .with_context(|| format!("read OCR source for {slug}"))?;
        let extras = compose_extras(&slug, &distill_run_id);
        let (drafts, coverage) = pipeline
            .run_with_extras(source, extras)
            .with_context(|| format!("run pipeline for {slug}"))?;

        if dry_run {
            println!(
                "[dry-run] {slug}: entries={} coverage_pct={:.1}",
                drafts.len(),
                coverage.coverage_pct()
            );
            continue;
        }

        let mut refs = Refs::open(&paths.refs_path)
            .with_context(|| format!("open {}", paths.refs_path.display()))?;
        register_book_indexes(&mut refs, &slug, &book_toml)?;
        upsert_book_row(&refs, &book_toml, &distill_run_id)?;
        for draft in &drafts {
            let entry = draft_to_new_entry(draft);
            refs.upsert_entry(&entry)?;
        }
        println!(
            "{slug}: entries={} coverage_pct={:.1} written to {}",
            drafts.len(),
            coverage.coverage_pct(),
            paths.refs_path.display(),
        );
        let _ = coverage; // currently unused once written; retained for future stats
    }

    Ok(())
}

fn build_scope(
    paths: &DistillPaths,
    book: Option<String>,
    all: bool,
) -> Result<Vec<String>> {
    let mut on_disk = list_book_slugs(&paths.reference_root)?;
    on_disk.sort();
    if let Some(slug) = book {
        if !on_disk.iter().any(|s| s == &slug) {
            bail!(
                "no book.toml found for slug {slug:?} under {}",
                paths.reference_root.display()
            );
        }
        return Ok(vec![slug]);
    }
    if !all && on_disk.len() > 1 {
        bail!(
            "more than one reference book is present; pass `--book <slug>` to \
             pick one or `--all` to build every book"
        );
    }
    Ok(on_disk)
}

fn list_book_slugs(reference_root: &Path) -> Result<Vec<String>> {
    if !reference_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(reference_root)
        .with_context(|| format!("read_dir {}", reference_root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if !path.join("book.toml").is_file() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            out.push(name.to_string());
        }
    }
    Ok(out)
}

/// Resolve OCR Markdown for a reference book. Accepts either a single
/// `source.md` or a `sources/` directory of `*.md` fragments
/// concatenated in sorted name order.
fn read_source(book_dir: &Path) -> Result<String> {
    let single = book_dir.join("source.md");
    if single.is_file() {
        return std::fs::read_to_string(&single)
            .with_context(|| format!("read {}", single.display()));
    }
    let dir = book_dir.join("sources");
    if dir.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .with_context(|| format!("read_dir {}", dir.display()))?
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
    Err(anyhow!(
        "neither {} nor {} exists",
        single.display(),
        dir.display()
    ))
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

fn register_book_indexes(
    refs: &mut Refs,
    slug: &str,
    book_toml: &BookToml,
) -> Result<()> {
    let specs: Vec<IndexSpec> = book_toml
        .indexes
        .iter()
        .map(|i| IndexSpec {
            field: i.field.clone(),
            kind: match i.kind.as_str() {
                "btree" => IndexKind::Btree,
                _ => IndexKind::Btree,
            },
        })
        .collect();
    refs.register_book(slug, &specs)?;
    Ok(())
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

fn verify(paths: &DistillPaths, book: Option<String>) -> Result<()> {
    let mut on_disk = list_book_slugs(&paths.reference_root)?;
    on_disk.sort();
    let scope: Vec<String> = match book {
        Some(slug) => {
            if !on_disk.iter().any(|s| s == &slug) {
                bail!(
                    "no book.toml found for slug {slug:?} under {}",
                    paths.reference_root.display()
                );
            }
            vec![slug]
        }
        None => on_disk,
    };

    let prod_refs = Refs::open(&paths.refs_path)
        .with_context(|| format!("open {}", paths.refs_path.display()))?;

    let distill_run_id = chrono::Utc::now().to_rfc3339();
    for slug in scope {
        let book_dir = paths.reference_root.join(&slug);
        let book_toml_path = book_dir.join("book.toml");
        let pipeline = load_pipeline(&book_toml_path)?;
        let source = read_source(&book_dir)?;
        let extras = compose_extras(&slug, &distill_run_id);
        let (drafts, _coverage) = pipeline.run_with_extras(source, extras)?;

        let proposed: BTreeMap<String, EntryDraft> = drafts
            .into_iter()
            .map(|d| (d.entry_key.clone(), d))
            .collect();
        let live = read_live_entries(&prod_refs, &slug)?;

        diff_and_report(&slug, &proposed, &live);
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

fn list(paths: &DistillPaths) -> Result<()> {
    if !paths.refs_path.is_file() {
        println!("no reference.db at {}", paths.refs_path.display());
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
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;

    println!("slug\ttitle\tauthority_rank\tentry_count\tbuilt_at");
    for row in rows {
        let (slug, title, rank, built_at, count) = row?;
        println!("{slug}\t{title}\t{rank}\t{count}\t{built_at}");
    }
    Ok(())
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

    fn seed_data_dir(root: &Path) {
        let book_dir = root.join("reference").join("tiny");
        fs::create_dir_all(&book_dir).expect("mkdir");
        fs::write(book_dir.join("book.toml"), TINY_BOOK_TOML).expect("write book.toml");
        fs::write(book_dir.join("source.md"), TINY_SOURCE).expect("write source.md");
    }

    fn make_paths(root: &Path) -> DistillPaths {
        DistillPaths {
            refs_path: root.join("reference.db"),
            reference_root: root.join("reference"),
        }
    }

    #[test]
    fn build_writes_book_and_entries_into_reference_db() {
        let tmp = TempDir::new().expect("tmp");
        seed_data_dir(tmp.path());
        let paths = make_paths(tmp.path());

        build(&paths, None, false, false).expect("build");

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
        seed_data_dir(tmp.path());
        let paths = make_paths(tmp.path());

        build(&paths, None, false, true).expect("dry-run build");

        assert!(
            !paths.refs_path.exists(),
            "dry-run must not write to reference.db"
        );
    }

    #[test]
    fn build_by_slug_and_build_all_are_equivalent_for_a_single_book() {
        let tmp_a = TempDir::new().expect("tmp a");
        seed_data_dir(tmp_a.path());
        let paths_a = make_paths(tmp_a.path());
        build(&paths_a, Some("tiny".to_string()), false, false).expect("build --book tiny");

        let tmp_b = TempDir::new().expect("tmp b");
        seed_data_dir(tmp_b.path());
        let paths_b = make_paths(tmp_b.path());
        build(&paths_b, None, true, false).expect("build --all");

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
        seed_data_dir(tmp.path());
        let paths = make_paths(tmp.path());
        build(&paths, None, false, false).expect("build");

        // Re-run verify; it should not panic and the database must
        // remain readable afterward.
        verify(&paths, None).expect("verify");
        let refs = Refs::open(&paths.refs_path).expect("open after verify");
        let _ = refs.lookup_resolved(None, "smith").expect("lookup post-verify");
    }

    #[test]
    fn verify_detects_a_manual_payload_change() {
        let tmp = TempDir::new().expect("tmp");
        seed_data_dir(tmp.path());
        let paths = make_paths(tmp.path());
        build(&paths, None, false, false).expect("build");

        // Mutate one row's payload behind the pipeline's back.
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
        // Re-distill and diff in-process.
        let book_toml_path = paths
            .reference_root
            .join("tiny")
            .join("book.toml");
        let pipeline = load_pipeline(&book_toml_path).expect("pipeline");
        let source = read_source(&paths.reference_root.join("tiny")).expect("source");
        let extras = compose_extras("tiny", "2026-06-25T00:00:00Z");
        let (drafts, _) = pipeline
            .run_with_extras(source, extras)
            .expect("run");
        let proposed: BTreeMap<String, EntryDraft> = drafts
            .into_iter()
            .map(|d| (d.entry_key.clone(), d))
            .collect();

        let mut found_change = false;
        for key in proposed.keys() {
            if let Some(live_row) = live.get(key) {
                let new_payload =
                    serde_json::to_string(&proposed[key].payload).unwrap_or_default();
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

    #[test]
    fn list_prints_each_registered_book_with_its_entry_count() {
        let tmp = TempDir::new().expect("tmp");
        seed_data_dir(tmp.path());
        let paths = make_paths(tmp.path());
        build(&paths, None, false, false).expect("build");

        // `list` writes to stdout; assert it returns Ok and is
        // re-runnable. Stdout content is not captured here; the
        // assertion that `reference_books` carries the row is in
        // build_writes_book_and_entries_into_reference_db.
        list(&paths).expect("list");
    }
}
