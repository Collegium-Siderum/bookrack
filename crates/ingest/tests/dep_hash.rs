// SPDX-License-Identifier: Apache-2.0

//! Anchors the crates that decide chunk boundaries to a frozen list.
//! Moving `text-splitter` or any `icu_*` crate in `Cargo.lock` flips
//! this test red until both `CHUNK_VERSION` and `FROZEN_SPLITTER_DEPS`
//! are restated in lockstep. The rule covers the whole `icu_*` family
//! because its crates are released together and move as one.

use std::path::PathBuf;

use bookrack_ingest::{CHUNK_VERSION, FROZEN_SPLITTER_DEPS};

#[test]
fn chunk_version_and_frozen_splitter_deps_are_pinned_as_a_pair() {
    // The two constants move in lockstep: a list refresh without the
    // version bump (or the reverse) must not slip through as a
    // one-sided edit. Restating the pair here makes every bump an
    // explicit, reviewable confirmation of both sides.
    assert_eq!(
        (CHUNK_VERSION, FROZEN_SPLITTER_DEPS),
        (
            1,
            &[
                "icu_collections@2.2.0",
                "icu_locale@2.2.0",
                "icu_locale_core@2.2.0",
                "icu_locale_data@2.2.0",
                "icu_normalizer@2.2.0",
                "icu_normalizer_data@2.2.0",
                "icu_properties@2.2.0",
                "icu_properties_data@2.2.0",
                "icu_provider@2.2.0",
                "icu_segmenter@2.2.0",
                "icu_segmenter_data@2.2.0",
                "text-splitter@0.30.1",
            ][..]
        ),
        "CHUNK_VERSION and FROZEN_SPLITTER_DEPS bump together; restate the pair here"
    );
}

#[test]
fn splitter_deps_match_frozen_list() {
    let lock_path = workspace_cargo_lock();
    let lock = std::fs::read_to_string(&lock_path)
        .unwrap_or_else(|e| panic!("read {}: {}", lock_path.display(), e));
    let locked = locked_entries(&lock);
    let (gone, new) = drift(FROZEN_SPLITTER_DEPS, &locked);
    assert!(
        gone.is_empty() && new.is_empty(),
        "chunking deps moved in Cargo.lock: no longer locked {gone:?}, newly locked {new:?}. \
         Either keep the previous versions in Cargo.lock, or bump \
         bookrack_ingest::CHUNK_VERSION and restate FROZEN_SPLITTER_DEPS as {locked:#?} \
         (see docs/UPGRADE.md)"
    );
}

#[test]
fn locked_entries_take_the_splitter_and_the_icu_family_only() {
    let lock = lock_of(&[
        ("memchr", "2.7.4"),
        ("icu_segmenter", "2.2.0"),
        ("unicode-segmentation", "1.12.0"),
        ("text-splitter", "0.30.1"),
        ("icu_provider", "2.2.0"),
    ]);
    assert_eq!(
        locked_entries(&lock),
        vec![
            "icu_provider@2.2.0",
            "icu_segmenter@2.2.0",
            "text-splitter@0.30.1",
        ]
    );
}

#[test]
fn locked_entries_ignore_package_order() {
    let forward = lock_of(&[("text-splitter", "0.30.1"), ("icu_segmenter", "2.2.0")]);
    let reversed = lock_of(&[("icu_segmenter", "2.2.0"), ("text-splitter", "0.30.1")]);
    assert_eq!(locked_entries(&forward), locked_entries(&reversed));
}

#[test]
fn locked_entries_keep_every_version_of_a_guarded_crate() {
    let lock = lock_of(&[
        ("icu_collections", "2.3.0"),
        ("text-splitter", "0.30.1"),
        ("icu_collections", "2.2.0"),
    ]);
    assert_eq!(
        locked_entries(&lock),
        vec![
            "icu_collections@2.2.0",
            "icu_collections@2.3.0",
            "text-splitter@0.30.1",
        ]
    );
}

#[test]
fn drift_names_what_left_and_what_arrived() {
    let frozen = [
        "icu_locale@2.2.0",
        "icu_segmenter@2.2.0",
        "text-splitter@0.30.1",
    ];
    let locked = vec![
        "icu_locale_fallback@2.3.0".to_string(),
        "icu_segmenter@2.3.0".to_string(),
    ];
    assert_eq!(
        drift(&frozen, &locked),
        (
            vec![
                "icu_locale@2.2.0".to_string(),
                "icu_segmenter@2.2.0".to_string(),
                "text-splitter@0.30.1".to_string(),
            ],
            vec![
                "icu_locale_fallback@2.3.0".to_string(),
                "icu_segmenter@2.3.0".to_string(),
            ]
        )
    );
}

/// Whether a `Cargo.lock` package is one the chunk boundaries depend on.
fn is_guarded(name: &str) -> bool {
    name == "text-splitter" || name.starts_with("icu_")
}

/// `name@version` for every guarded `[[package]]` in `lock`, sorted
/// bytewise; a crate locked at two versions yields two entries.
fn locked_entries(lock: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut name: Option<&str> = None;
    for line in lock.lines().map(str::trim) {
        if line == "[[package]]" {
            name = None;
        } else if let Some(n) = quoted_value(line, "name") {
            name = Some(n);
        } else if let Some(v) = quoted_value(line, "version")
            && let Some(n) = name.take()
            && is_guarded(n)
        {
            entries.push(format!("{n}@{v}"));
        }
    }
    entries.sort();
    entries
}

/// Entries of `frozen` absent from `locked`, then entries of `locked`
/// absent from `frozen`.
fn drift(frozen: &[&str], locked: &[String]) -> (Vec<String>, Vec<String>) {
    let gone = frozen
        .iter()
        .filter(|f| !locked.iter().any(|l| l == *f))
        .map(|f| f.to_string())
        .collect();
    let new = locked
        .iter()
        .filter(|l| !frozen.contains(&l.as_str()))
        .cloned()
        .collect();
    (gone, new)
}

/// The value of a `key = "value"` line, or `None` for any other line.
fn quoted_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.strip_prefix(key)?
        .strip_prefix(" = \"")?
        .strip_suffix('"')
}

/// A minimal `Cargo.lock` holding one registry package per pair.
fn lock_of(packages: &[(&str, &str)]) -> String {
    let mut lock = String::from("# This file is automatically @generated by Cargo.\nversion = 4\n");
    for (name, version) in packages {
        lock.push_str(&format!(
            "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
             dependencies = [\n \"icu_segmenter 2.2.0\",\n]\n"
        ));
    }
    lock
}

fn workspace_cargo_lock() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../../Cargo.lock");
    p
}
