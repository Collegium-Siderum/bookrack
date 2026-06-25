// SPDX-License-Identifier: Apache-2.0

//! Spike-fixture-driven integration tests.
//!
//! Each test in this file is gated by the
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
//! BOOKRACK_DISTILL_FIXTURE_DIR=$PWD/spikes/distill \
//!   cargo test -p bookrack-distill --test it_pipelines -- --ignored
//! ```
//!
//! The tests are marked `#[ignore]` so they do not run on a default
//! `cargo test`; `--ignored` opts them in.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use bookrack_distill::{Coverage, EntryDraft, load_pipeline};

fn fixture_root() -> Option<PathBuf> {
    env::var("BOOKRACK_DISTILL_FIXTURE_DIR")
        .ok()
        .map(PathBuf::from)
}

/// Read the OCR Markdown for `slug`. Resolution tries, in order:
///
/// 1. `ocr_samples/<candidate>.md` — single-file form.
/// 2. `ocr_samples/<candidate>/*.md` — directory of fragments,
///    concatenated in sorted name order.
/// 3. `ocr_samples/<candidate>*.md` — loose glob over the
///    `ocr_samples/` root, concatenated in sorted name order.
///
/// Each `candidate` is the slug plus, for the prefixed slugs, the
/// short form with `name_translation_` or `philosophy_` stripped.
/// This is the shape the existing spike layout already uses
/// (e.g. `xinhua_p015.md`, `dacidian_shang/page_050.md`,
/// `philosophy/page_201.md`).
fn read_source(root: &Path, slug: &str) -> String {
    let ocr_dir = root.join("ocr_samples");
    let candidates = source_candidates(slug);

    for cand in &candidates {
        let single = ocr_dir.join(format!("{cand}.md"));
        if single.is_file() {
            return read_file(&single);
        }
    }
    for cand in &candidates {
        let dir = ocr_dir.join(cand);
        if dir.is_dir() {
            return concat_dir(&dir);
        }
    }
    for cand in &candidates {
        if let Some(text) = glob_prefix(&ocr_dir, cand) {
            return text;
        }
    }

    panic!(
        "no OCR fixture for {slug:?} under {}; tried candidates {:?}",
        ocr_dir.display(),
        candidates,
    );
}

fn source_candidates(slug: &str) -> Vec<String> {
    let mut out = vec![slug.to_string()];
    if let Some(rest) = slug.strip_prefix("name_translation_") {
        out.push(rest.to_string());
    } else if let Some(rest) = slug.strip_prefix("philosophy_") {
        // `philosophy_xifang` falls back to `philosophy` because the
        // existing spike layout groups the philosophy fragments
        // under `ocr_samples/philosophy/` without an edition tag.
        out.push(rest.to_string());
        out.push("philosophy".to_string());
    }
    out
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
    concat_files(&entries)
}

fn glob_prefix(dir: &Path, prefix: &str) -> Option<String> {
    let mut matches: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.extension().and_then(|s| s.to_str()) == Some("md")
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|stem| stem.starts_with(prefix))
        })
        .collect();
    matches.sort();
    if matches.is_empty() {
        None
    } else {
        Some(concat_files(&matches))
    }
}

fn concat_files(paths: &[PathBuf]) -> String {
    let mut acc = String::new();
    for path in paths {
        let chunk = read_file(path);
        acc.push_str(&chunk);
        if !chunk.ends_with('\n') {
            acc.push('\n');
        }
    }
    acc
}

fn run_book(slug: &str) -> Option<(Vec<EntryDraft>, Coverage)> {
    let root = fixture_root()?;
    let book_path = root.join("book_tomls").join(format!("{slug}.toml"));
    let pipeline = load_pipeline(&book_path)
        .unwrap_or_else(|e| panic!("load_pipeline({}): {e}", book_path.display()));
    let source = read_source(&root, slug);
    let result = pipeline
        .run(source)
        .unwrap_or_else(|e| panic!("pipeline.run for {slug}: {e}"));
    Some(result)
}

fn assert_name_translation(slug: &str) {
    let Some((drafts, coverage)) = run_book(slug) else {
        return;
    };
    assert!(!drafts.is_empty(), "{slug}: pipeline produced zero drafts");
    assert_eq!(coverage.entries, drafts.len());
    // The name-translation pipeline writes `country` whenever
    // `partition_body_around_match` finds a bracketed-tag region.
    // A live book is expected to surface dozens; the loose lower
    // bound here is a regression guard for "no extraction at all".
    let with_country = drafts
        .iter()
        .filter(|d| d.payload.contains_key("country"))
        .count();
    assert!(
        with_country > 0,
        "{slug}: no entries carried a country payload key; \
         partition_body_around_match did not fire"
    );
    // Each emitted draft must declare a non-empty entry_key (the
    // normalize_latin_key projection) and a non-empty headword.
    for draft in &drafts {
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

#[test]
#[ignore]
fn it_name_translation_xinhua() {
    assert_name_translation("name_translation_xinhua");
}

#[test]
#[ignore]
fn it_name_translation_dacidian_shang() {
    assert_name_translation("name_translation_dacidian_shang");
}

#[test]
#[ignore]
fn it_name_translation_dacidian_xia() {
    assert_name_translation("name_translation_dacidian_xia");
}

#[test]
#[ignore]
fn it_philosophy_xifang() {
    let Some((drafts, coverage)) = run_book("philosophy_xifang") else {
        return;
    };
    assert!(
        !drafts.is_empty(),
        "philosophy: pipeline produced zero drafts"
    );
    assert_eq!(coverage.entries, drafts.len());

    // `pair_bilingual_entries` is expected to stamp `pair_mismatch`
    // on the philosophy book's known unpaired entries; the count
    // surfaced in `coverage.pair_mismatch` must match the number of
    // drafts whose `quality_flags` carry the flag.
    let mismatch_drafts = drafts
        .iter()
        .filter(|d| d.quality_flags.iter().any(|f| f == "pair_mismatch"))
        .count();
    assert_eq!(
        mismatch_drafts, coverage.pair_mismatch,
        "philosophy: pair_mismatch flag count on drafts must equal \
         the coverage counter"
    );

    // Every draft from the bilingual pipeline must carry at least
    // one of the bilingual keys, otherwise the unpack_paired_body
    // stage silently dropped its work.
    for draft in &drafts {
        let has_bilingual = ["zh_head", "en_text", "zh_text"]
            .iter()
            .any(|k| draft.payload.contains_key(*k));
        assert!(
            has_bilingual,
            "philosophy: draft missing bilingual payload keys: \
             headword={:?}, payload={:?}",
            draft.headword, draft.payload
        );
    }
}
