// SPDX-License-Identifier: Apache-2.0

//! Anchors the behaviour-sensitive dependencies of the text crate to
//! frozen values, one guard per version stamp.
//!
//! The normalizer's guard pins `unicode-normalization` to a SHA-256:
//! moving it in `Cargo.lock` flips the test red until both
//! `NORMALIZE_VERSION` and `FROZEN_DEPS_HASH` are refreshed in
//! lockstep.
//!
//! The chunker's guard pins `text-splitter` and every `icu_*` crate to
//! a frozen list: moving any of them flips the test red until both
//! `CHUNK_VERSION` and `FROZEN_SPLITTER_DEPS` are restated in lockstep.
//! The rule covers the whole `icu_*` family because its crates are
//! released together and move as one.
//!
//! Both lists are rows of the bump-to-refresh matrix in
//! `docs/UPGRADE.md`.

// Parallel implementation: dep-hash-guards.

use std::path::PathBuf;

use bookrack_text::{CHUNK_VERSION, FROZEN_DEPS_HASH, FROZEN_SPLITTER_DEPS, NORMALIZE_VERSION};
use sha2::{Digest, Sha256};

const BEHAVIOR_SENSITIVE_CRATES: &[&str] = &["unicode-normalization"];

#[test]
fn normalize_version_and_frozen_deps_hash_are_pinned_as_a_pair() {
    // The two constants move in lockstep: a deps-hash refresh without
    // the version bump (or the reverse) must not slip through as a
    // one-sided edit. Restating the pair here makes every bump an
    // explicit, reviewable confirmation of both sides.
    assert_eq!(
        (NORMALIZE_VERSION, FROZEN_DEPS_HASH),
        (
            1,
            "f6f8b19c40dbdbab017aba7eb0f1c4056649cac1ea1645f3cf44c66a9abdfd7d"
        ),
        "NORMALIZE_VERSION and FROZEN_DEPS_HASH bump together; restate the pair here"
    );
}

#[test]
fn behavior_sensitive_deps_hash_matches_frozen_value() {
    let lock = read_workspace_lock();
    let entries = named_entries(&lock, BEHAVIOR_SENSITIVE_CRATES)
        .unwrap_or_else(|missing| panic!("Cargo.lock has no entry for {missing:?}"));
    let actual = digest(&entries);
    assert_eq!(
        actual, FROZEN_DEPS_HASH,
        "behaviour-sensitive deps changed; Cargo.lock now holds {entries:?}. Either keep \
         the previous versions in Cargo.lock, or bump bookrack_text::NORMALIZE_VERSION \
         and refresh FROZEN_DEPS_HASH to {actual} (see docs/UPGRADE.md)"
    );
}

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
    let lock = read_workspace_lock();
    let locked = splitter_entries(&lock);
    let (gone, new) = drift(FROZEN_SPLITTER_DEPS, &locked);
    assert!(
        gone.is_empty() && new.is_empty(),
        "chunking deps moved in Cargo.lock: no longer locked {gone:?}, newly locked {new:?}. \
         Either keep the previous versions in Cargo.lock, or bump \
         bookrack_text::CHUNK_VERSION and restate FROZEN_SPLITTER_DEPS as {locked:#?} \
         (see docs/UPGRADE.md)"
    );
}

#[test]
fn named_entries_ignore_unrelated_packages_and_package_order() {
    let forward = lock_of(&[
        ("memchr", "2.7.4"),
        ("unicode-normalization", "0.1.25"),
        ("tinyvec", "1.10.0"),
    ]);
    let reversed = lock_of(&[
        ("tinyvec", "1.10.0"),
        ("unicode-normalization", "0.1.25"),
        ("memchr", "2.7.4"),
    ]);
    let expected = vec!["unicode-normalization@0.1.25".to_string()];
    assert_eq!(
        named_entries(&forward, &["unicode-normalization"]),
        Ok(expected.clone())
    );
    assert_eq!(
        named_entries(&reversed, &["unicode-normalization"]),
        Ok(expected)
    );
}

#[test]
fn named_entries_keep_every_version_of_a_named_crate() {
    let lock = lock_of(&[
        ("unicode-normalization", "0.1.25"),
        ("tinyvec", "1.10.0"),
        ("unicode-normalization", "0.1.24"),
    ]);
    assert_eq!(
        named_entries(&lock, &["unicode-normalization"]),
        Ok(vec![
            "unicode-normalization@0.1.24".to_string(),
            "unicode-normalization@0.1.25".to_string(),
        ])
    );
}

#[test]
fn named_entries_name_every_named_crate_missing_from_the_lock() {
    let lock = lock_of(&[("tinyvec", "1.10.0")]);
    assert_eq!(
        named_entries(&lock, &["tinyvec", "unicode-normalization"]),
        Err(vec!["unicode-normalization".to_string()])
    );
}

#[test]
fn splitter_entries_take_the_splitter_and_the_icu_family_only() {
    let lock = lock_of(&[
        ("memchr", "2.7.4"),
        ("icu_segmenter", "2.2.0"),
        ("unicode-segmentation", "1.12.0"),
        ("text-splitter", "0.30.1"),
        ("icu_provider", "2.2.0"),
    ]);
    assert_eq!(
        splitter_entries(&lock),
        vec![
            "icu_provider@2.2.0",
            "icu_segmenter@2.2.0",
            "text-splitter@0.30.1",
        ]
    );
}

#[test]
fn splitter_entries_ignore_package_order() {
    let forward = lock_of(&[("text-splitter", "0.30.1"), ("icu_segmenter", "2.2.0")]);
    let reversed = lock_of(&[("icu_segmenter", "2.2.0"), ("text-splitter", "0.30.1")]);
    assert_eq!(splitter_entries(&forward), splitter_entries(&reversed));
}

#[test]
fn splitter_entries_keep_every_version_of_a_guarded_crate() {
    let lock = lock_of(&[
        ("icu_collections", "2.3.0"),
        ("text-splitter", "0.30.1"),
        ("icu_collections", "2.2.0"),
    ]);
    assert_eq!(
        splitter_entries(&lock),
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

/// `name@version` for every `[[package]]` in `lock` whose name passes
/// `guarded`, sorted bytewise; a crate locked at two versions yields
/// two entries.
fn collect_entries(lock: &str, guarded: impl Fn(&str) -> bool) -> Vec<String> {
    let mut entries = Vec::new();
    let mut name: Option<&str> = None;
    for line in lock.lines().map(str::trim) {
        if line == "[[package]]" {
            name = None;
        } else if let Some(n) = quoted_value(line, "name") {
            name = Some(n);
        } else if let Some(v) = quoted_value(line, "version")
            && let Some(n) = name.take()
            && guarded(n)
        {
            entries.push(format!("{n}@{v}"));
        }
    }
    entries.sort();
    entries
}

/// The entries of the packages named in `crates`. `Err` lists the
/// crates in `crates` with no entry at all.
fn named_entries(lock: &str, crates: &[&str]) -> Result<Vec<String>, Vec<String>> {
    let entries = collect_entries(lock, |name| crates.contains(&name));
    let missing: Vec<String> = crates
        .iter()
        .filter(|c| {
            !entries
                .iter()
                .any(|e| e.split_once('@').map(|(n, _)| n) == Some(**c))
        })
        .map(|c| c.to_string())
        .collect();
    if !missing.is_empty() {
        return Err(missing);
    }
    Ok(entries)
}

/// The entries of the packages the chunk boundaries depend on:
/// `text-splitter` and every crate of the `icu_*` family, whichever of
/// them the lockfile holds.
fn splitter_entries(lock: &str) -> Vec<String> {
    collect_entries(lock, |name| {
        name == "text-splitter" || name.starts_with("icu_")
    })
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

/// SHA-256 of `entries` joined with `\n`, no trailing newline, as hex.
fn digest(entries: &[String]) -> String {
    hex(&Sha256::digest(entries.join("\n").as_bytes()))
}

/// A minimal `Cargo.lock` holding one registry package per pair. Each
/// package lists dependencies whose names are guarded ones, which a
/// reader of package headers must not take for packages.
fn lock_of(packages: &[(&str, &str)]) -> String {
    let mut lock = String::from("# This file is automatically @generated by Cargo.\nversion = 4\n");
    for (name, version) in packages {
        lock.push_str(&format!(
            "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
             dependencies = [\n \"memchr 2.7.4\",\n \"icu_segmenter 2.2.0\",\n]\n"
        ));
    }
    lock
}

fn read_workspace_lock() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("../../Cargo.lock");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
