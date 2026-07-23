// SPDX-License-Identifier: Apache-2.0

//! Fixture-driven integration tests.
//!
//! The single test in this file is gated by the
//! `BOOKRACK_DISTILL_FIXTURE_DIR` environment variable: when it is
//! unset the test early-returns as a no-op, so a clean CI checkout
//! passes without any local data. Maintainers point the variable at
//! a directory with the layout
//!
//! ```text
//! <root>/book_tomls/<slug>.toml
//! <root>/ocr_samples/<slug>.md           # single-file form
//! # or
//! <root>/ocr_samples/<slug>/             # directory of *.md fragments
//! ```
//!
//! and run
//!
//! ```sh
//! BOOKRACK_DISTILL_FIXTURE_DIR=/path/to/fixtures \
//!   cargo test -p bookrack-distill --test it_pipelines -- --ignored
//! ```
//!
//! Every `book_tomls/*.toml` is loaded and run against the OCR
//! sample that shares its file stem; assertions dispatch on the
//! book's `schema_name`, so adding a fixture book requires no test
//! change. The test is marked `#[ignore]` so it does not run on a
//! default `cargo test`; `--ignored` opts it in.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use bookrack_distill::{BookToml, Coverage, EntryDraft, load_pipeline};

fn fixture_root() -> Option<PathBuf> {
    env::var("BOOKRACK_DISTILL_FIXTURE_DIR")
        .ok()
        .map(PathBuf::from)
}

/// Read the OCR Markdown for `slug`. Resolution tries, in order:
///
/// 1. `ocr_samples/<slug>.md` — single-file form.
/// 2. `ocr_samples/<slug>/*.md` — directory of fragments,
///    concatenated in sorted name order.
fn read_source(root: &Path, slug: &str) -> String {
    let ocr_dir = root.join("ocr_samples");
    let single = ocr_dir.join(format!("{slug}.md"));
    if single.is_file() {
        return read_file(&single);
    }
    let dir = ocr_dir.join(slug);
    if dir.is_dir() {
        return concat_dir(&dir);
    }
    panic!(
        "no OCR fixture for {slug:?} under {}; expected {slug}.md or {slug}/",
        ocr_dir.display(),
    );
}

fn read_file(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn concat_dir(dir: &Path) -> String {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    entries.sort();
    let mut acc = String::new();
    for path in &entries {
        let chunk = read_file(path);
        acc.push_str(&chunk);
        if !chunk.ends_with('\n') {
            acc.push('\n');
        }
    }
    acc
}

fn assert_name_translation(
    slug: &str,
    book: &BookToml,
    drafts: &[EntryDraft],
    coverage: &Coverage,
) {
    assert!(!drafts.is_empty(), "{slug}: pipeline produced zero drafts");
    assert_eq!(coverage.entries, drafts.len());
    assert!(
        coverage.splits > 0,
        "{slug}: the splitter stage recorded zero splits"
    );
    // Recipes that declare `country` in `writes_properties` populate
    // it whenever `partition_body_around_match` finds a bracketed-tag
    // region. A live book is expected to surface dozens; the loose
    // lower bound here is a regression guard for "no extraction at
    // all". Minimal recipes that skip the enrichment stages declare
    // an empty `writes_properties` and are exempt.
    if book.parser.writes_properties.iter().any(|p| p == "country") {
        let with_country = drafts
            .iter()
            .filter(|d| d.payload.contains_key("country"))
            .count();
        assert!(
            with_country > 0,
            "{slug}: no entries carried a country payload key; \
             partition_body_around_match did not fire"
        );
    }
    // Each emitted draft must declare a non-empty entry_key (the
    // normalize_latin_key projection) and a non-empty headword.
    for draft in drafts {
        assert!(
            !draft.entry_key.is_empty(),
            "{slug}: entry with empty entry_key: headword={:?}",
            draft.headword
        );
        assert!(
            !draft.headword.is_empty(),
            "{slug}: entry with empty headword"
        );
    }
}

fn assert_philosophy(slug: &str, drafts: &[EntryDraft], coverage: &Coverage) {
    assert!(!drafts.is_empty(), "{slug}: pipeline produced zero drafts");
    assert_eq!(coverage.entries, drafts.len());
    assert!(
        coverage.splits > 0,
        "{slug}: the splitter stage recorded zero splits"
    );

    // `pair_bilingual_entries` stamps `pair_mismatch` on entries the
    // pairing stage leaves unpaired; the count surfaced in
    // `coverage.pair_mismatch` must match the number of drafts whose
    // `quality_flags` carry the flag.
    let mismatch_drafts = drafts
        .iter()
        .filter(|d| d.quality_flags.iter().any(|f| f == "pair_mismatch"))
        .count();
    assert_eq!(
        mismatch_drafts, coverage.pair_mismatch,
        "{slug}: pair_mismatch flag count on drafts must equal \
         the coverage counter"
    );

    // Every draft from the bilingual pipeline must carry at least
    // one of the bilingual keys, otherwise the unpack_paired_body
    // stage silently dropped its work.
    for draft in drafts {
        let has_bilingual = ["zh_head", "en_text", "zh_text"]
            .iter()
            .any(|k| draft.payload.contains_key(*k));
        assert!(
            has_bilingual,
            "{slug}: draft missing bilingual payload keys: \
             headword={:?}, payload={:?}",
            draft.headword, draft.payload
        );
    }
}

#[test]
#[ignore]
fn it_fixture_books() {
    let Some(root) = fixture_root() else {
        return;
    };
    let toml_dir = root.join("book_tomls");
    let mut books: Vec<PathBuf> = fs::read_dir(&toml_dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", toml_dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("toml"))
        .collect();
    books.sort();
    assert!(
        !books.is_empty(),
        "no book_tomls/*.toml under {}",
        toml_dir.display()
    );

    for book_path in &books {
        let slug = book_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| panic!("non-UTF-8 fixture name: {}", book_path.display()))
            .to_string();
        let book = BookToml::load(book_path)
            .unwrap_or_else(|e| panic!("BookToml::load({}): {e}", book_path.display()));
        let pipeline = load_pipeline(book_path)
            .unwrap_or_else(|e| panic!("load_pipeline({}): {e}", book_path.display()));
        let source = read_source(&root, &slug);
        let (drafts, coverage) = pipeline
            .run(source)
            .unwrap_or_else(|e| panic!("pipeline.run for {slug}: {e}"));

        match book.schema_name.as_str() {
            "name_translation" => assert_name_translation(&slug, &book, &drafts, &coverage),
            "philosophy" => assert_philosophy(&slug, &drafts, &coverage),
            other => panic!("{slug}: no assertions defined for schema {other:?}"),
        }
    }
}
