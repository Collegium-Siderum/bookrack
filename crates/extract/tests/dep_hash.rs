// SPDX-License-Identifier: Apache-2.0

//! Anchors the behaviour-sensitive deps of the extractor to a frozen
//! SHA-256. Bumping any of the listed crates' versions in
//! `Cargo.lock` flips this test red until both
//! `EXTRACTOR_VERSION` and `FROZEN_DEPS_HASH` are refreshed in
//! lockstep. The list mirrors the "behaviour-sensitive crates"
//! discipline noted in `CLAUDE.md`.

use std::path::PathBuf;

use bookrack_extract::FROZEN_DEPS_HASH;
use sha2::{Digest, Sha256};

const BEHAVIOR_SENSITIVE_CRATES: &[&str] = &[
    "encoding_rs",
    "pdfium-render",
    "rbook",
    "scraper",
    "unicode-normalization",
];

#[test]
fn behavior_sensitive_deps_hash_matches_frozen_value() {
    let actual = current_deps_hash();
    assert_eq!(
        actual, FROZEN_DEPS_HASH,
        "behaviour-sensitive deps changed; bump bookrack_extract::EXTRACTOR_VERSION \
         and refresh FROZEN_DEPS_HASH to {actual}"
    );
}

fn current_deps_hash() -> String {
    let lock_path = workspace_cargo_lock();
    let lock = std::fs::read_to_string(&lock_path)
        .unwrap_or_else(|e| panic!("read {}: {}", lock_path.display(), e));
    let mut entries = collect_entries(&lock);
    assert_eq!(
        entries.len(),
        BEHAVIOR_SENSITIVE_CRATES.len(),
        "expected to find every behaviour-sensitive crate in Cargo.lock; got {entries:?}"
    );
    entries.sort();
    let body = entries.join("\n");
    hex(&Sha256::digest(body.as_bytes()))
}

fn collect_entries(lock: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    for line in lock.lines() {
        let trimmed = line.trim();
        if trimmed == "[[package]]" {
            name = None;
            version = None;
            continue;
        }
        if let Some(v) = trimmed
            .strip_prefix("name = \"")
            .and_then(|s| s.strip_suffix('\"'))
        {
            name = Some(v.to_string());
        } else if let Some(v) = trimmed
            .strip_prefix("version = \"")
            .and_then(|s| s.strip_suffix('\"'))
        {
            version = Some(v.to_string());
        }
        if let (Some(n), Some(ver)) = (name.as_ref(), version.as_ref()) {
            if BEHAVIOR_SENSITIVE_CRATES.contains(&n.as_str()) {
                entries.push(format!("{n}@{ver}"));
            }
            name = None;
            version = None;
        }
    }
    entries
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
