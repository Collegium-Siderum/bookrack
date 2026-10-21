// SPDX-License-Identifier: Apache-2.0

//! Anchors the behaviour-sensitive deps of `normalize` to a frozen
//! SHA-256. Bumping any of the listed crates' versions in
//! `Cargo.lock` flips this test red until both
//! `NORMALIZE_VERSION` and `FROZEN_DEPS_HASH` are refreshed in
//! lockstep. The list is the normalizer's row of the bump-to-refresh
//! matrix in `docs/UPGRADE.md`.

use std::path::PathBuf;

use bookrack_normalize::{FROZEN_DEPS_HASH, NORMALIZE_VERSION};
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
    let lock_path = workspace_cargo_lock();
    let lock = std::fs::read_to_string(&lock_path)
        .unwrap_or_else(|e| panic!("read {}: {}", lock_path.display(), e));
    let entries = locked_entries(&lock, BEHAVIOR_SENSITIVE_CRATES)
        .unwrap_or_else(|missing| panic!("Cargo.lock has no entry for {missing:?}"));
    let actual = digest(&entries);
    assert_eq!(
        actual, FROZEN_DEPS_HASH,
        "behaviour-sensitive deps changed; Cargo.lock now holds {entries:?}. Either keep \
         the previous versions in Cargo.lock, or bump bookrack_normalize::NORMALIZE_VERSION \
         and refresh FROZEN_DEPS_HASH to {actual} (see docs/UPGRADE.md)"
    );
}

#[test]
fn locked_entries_ignore_unrelated_packages_and_package_order() {
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
        locked_entries(&forward, &["unicode-normalization"]),
        Ok(expected.clone())
    );
    assert_eq!(
        locked_entries(&reversed, &["unicode-normalization"]),
        Ok(expected)
    );
}

#[test]
fn locked_entries_keep_every_version_of_a_guarded_crate() {
    let lock = lock_of(&[
        ("unicode-normalization", "0.1.25"),
        ("tinyvec", "1.10.0"),
        ("unicode-normalization", "0.1.24"),
    ]);
    assert_eq!(
        locked_entries(&lock, &["unicode-normalization"]),
        Ok(vec![
            "unicode-normalization@0.1.24".to_string(),
            "unicode-normalization@0.1.25".to_string(),
        ])
    );
}

#[test]
fn locked_entries_name_every_guarded_crate_missing_from_the_lock() {
    let lock = lock_of(&[("tinyvec", "1.10.0")]);
    assert_eq!(
        locked_entries(&lock, &["tinyvec", "unicode-normalization"]),
        Err(vec!["unicode-normalization".to_string()])
    );
}

/// `name@version` for every `[[package]]` in `lock` whose name is in
/// `crates`, sorted bytewise; a crate locked at two versions yields two
/// entries. `Err` lists the crates in `crates` with no entry at all.
fn locked_entries(lock: &str, crates: &[&str]) -> Result<Vec<String>, Vec<String>> {
    let mut entries = Vec::new();
    let mut name: Option<&str> = None;
    for line in lock.lines().map(str::trim) {
        if line == "[[package]]" {
            name = None;
        } else if let Some(n) = quoted_value(line, "name") {
            name = Some(n);
        } else if let Some(v) = quoted_value(line, "version")
            && let Some(n) = name.take()
            && crates.contains(&n)
        {
            entries.push(format!("{n}@{v}"));
        }
    }
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
    entries.sort();
    Ok(entries)
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

/// A minimal `Cargo.lock` holding one registry package per pair.
fn lock_of(packages: &[(&str, &str)]) -> String {
    let mut lock = String::from("# This file is automatically @generated by Cargo.\nversion = 4\n");
    for (name, version) in packages {
        lock.push_str(&format!(
            "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
             dependencies = [\n \"memchr 2.7.4\",\n]\n"
        ));
    }
    lock
}

fn workspace_cargo_lock() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../../Cargo.lock");
    p
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
