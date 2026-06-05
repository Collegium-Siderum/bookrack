// SPDX-License-Identifier: Apache-2.0

//! Deterministic scrubber that redacts paths and book titles before they
//! land in a diagnose bundle.
//!
//! Three rules, applied in order:
//!
//! 1. Literal `data_dir` path → `<DATA_DIR>`.
//! 2. Literal `home_dir` path → `<HOME>`.
//! 3. Runs of two-or-more CJK characters → 12-hex-char sha256 prefix.
//!
//! Integer ids, sha256 hashes, stamp constants, and ASCII paths under
//! the (already-redacted) `data_dir` ride through untouched. The
//! scrubber is a pure function of its inputs and configuration: the
//! same `data_dir` + `home_dir` + input string maps to the same
//! output, so the resulting tarball is byte-stable across runs.

use std::path::Path;

use sha2::{Digest, Sha256};

/// Scrubber configuration. Build with [`Scrubber::new`] for the live
/// bundle path, or [`Scrubber::passthrough`] for `--no-scrub`.
pub struct Scrubber {
    enabled: bool,
    data_dir_str: Option<String>,
    home_dir_str: Option<String>,
}

impl Scrubber {
    /// Build a scrubber for the live bundle. `data_dir_path` is the
    /// configured BOOKRACK_DATA_DIR; `home_dir_path` is `$HOME` (or
    /// platform equivalent). Either may be `None` if the host did not
    /// expose one — the scrubber simply skips that substitution.
    pub fn new(data_dir_path: Option<&Path>, home_dir_path: Option<&Path>) -> Scrubber {
        Scrubber {
            enabled: true,
            data_dir_str: data_dir_path.map(|p| p.to_string_lossy().into_owned()),
            home_dir_str: home_dir_path.map(|p| p.to_string_lossy().into_owned()),
        }
    }

    /// Build a no-op scrubber. Used when the operator passes
    /// `--no-scrub` to retain a verbatim bundle for local inspection.
    pub fn passthrough() -> Scrubber {
        Scrubber {
            enabled: false,
            data_dir_str: None,
            home_dir_str: None,
        }
    }

    /// Apply every rule to one string, returning the scrubbed result.
    pub fn scrub_string(&self, input: &str) -> String {
        if !self.enabled {
            return input.to_string();
        }
        let mut out = input.to_string();
        // Order matters: data_dir is often nested under home_dir, so
        // substitute the deeper path first.
        if let Some(d) = &self.data_dir_str
            && !d.is_empty()
        {
            out = out.replace(d, DATA_DIR_PLACEHOLDER);
        }
        if let Some(h) = &self.home_dir_str
            && !h.is_empty()
        {
            out = out.replace(h, HOME_PLACEHOLDER);
        }
        out = hash_cjk_runs(&out);
        out
    }

    /// Recursively scrub every string node in a JSON value, in-place.
    /// Numbers, booleans, and nulls are left untouched.
    pub fn scrub_value(&self, value: &mut serde_json::Value) {
        if !self.enabled {
            return;
        }
        match value {
            serde_json::Value::String(s) => *s = self.scrub_string(s),
            serde_json::Value::Array(arr) => arr.iter_mut().for_each(|v| self.scrub_value(v)),
            serde_json::Value::Object(obj) => obj.values_mut().for_each(|v| self.scrub_value(v)),
            _ => {}
        }
    }
}

/// The placeholder substituted for the configured data_dir path.
pub const DATA_DIR_PLACEHOLDER: &str = "<DATA_DIR>";

/// The placeholder substituted for the user's home directory path.
pub const HOME_PLACEHOLDER: &str = "<HOME>";

/// Replace each run of two or more CJK characters with the 12-hex-char
/// prefix of its sha256, wrapped in `<cjk:…>` so a reader can spot a
/// redaction.
fn hash_cjk_runs(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut run = String::new();
    for c in input.chars() {
        if is_cjk(c) {
            run.push(c);
        } else {
            flush_run(&mut out, &mut run);
            out.push(c);
        }
    }
    flush_run(&mut out, &mut run);
    out
}

fn flush_run(out: &mut String, run: &mut String) {
    if run.chars().count() >= 2 {
        out.push_str("<cjk:");
        out.push_str(&sha8(run));
        out.push('>');
    } else {
        out.push_str(run);
    }
    run.clear();
}

/// Twelve hex characters of sha256: 48 bits of identity per redaction,
/// enough to disambiguate the handful of titles in one bundle without
/// turning the placeholder into an opaque wall.
fn sha8(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(12);
    for byte in &digest[..6] {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn is_cjk(c: char) -> bool {
    let n = c as u32;
    // CJK Unified Ideographs and Extension A, plus Hiragana, Katakana,
    // and the most common Hangul syllable block. These cover the
    // languages a Chinese-leaning library is likely to surface; books
    // in Latin alphabets pass through unredacted.
    (0x4E00..=0x9FFF).contains(&n)
        || (0x3400..=0x4DBF).contains(&n)
        || (0x3040..=0x309F).contains(&n)
        || (0x30A0..=0x30FF).contains(&n)
        || (0xAC00..=0xD7A3).contains(&n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scrubber() -> Scrubber {
        Scrubber::new(
            Some(&PathBuf::from("/Volumes/Disk/bookrack")),
            Some(&PathBuf::from("/Users/jane")),
        )
    }

    #[test]
    fn data_dir_is_replaced_with_a_placeholder() {
        let s = scrubber();
        let out = s.scrub_string("opened /Volumes/Disk/bookrack/catalog.db");
        assert_eq!(out, "opened <DATA_DIR>/catalog.db");
    }

    #[test]
    fn home_dir_is_replaced_with_a_placeholder() {
        let s = scrubber();
        let out = s.scrub_string("config at /Users/jane/.bookrackrc");
        assert_eq!(out, "config at <HOME>/.bookrackrc");
    }

    /// A four-character CJK run (U+5510 U+5F8B U+758F U+8B70), kept as
    /// an escape so this source file complies with the workspace's
    /// "no CJK bytes outside fixtures" leak-check rule.
    const CJK_RUN: &str = "\u{7532}\u{4E59}\u{4E19}\u{4E01}";
    /// A single CJK character (U+5E74) — the "won't be hashed because
    /// it is a lone code point" case.
    const SINGLE_CJK: &str = "\u{5E74}";

    #[test]
    fn cjk_runs_are_hashed_deterministically() {
        let s = scrubber();
        let input = format!("title: {CJK_RUN}");
        let a = s.scrub_string(&input);
        let b = s.scrub_string(&input);
        assert_eq!(a, b, "scrubber must be deterministic");
        assert!(a.contains("<cjk:"), "got: {a}");
        assert!(!a.contains(CJK_RUN));
    }

    #[test]
    fn single_cjk_characters_are_left_untouched() {
        let s = scrubber();
        let input = format!("{SINGLE_CJK} next to a non-CJK char");
        let out = s.scrub_string(&input);
        assert!(out.contains(SINGLE_CJK));
    }

    #[test]
    fn passthrough_does_not_alter_the_input() {
        let s = Scrubber::passthrough();
        let input = format!("/Users/jane / {CJK_RUN}");
        assert_eq!(s.scrub_string(&input), input);
    }

    #[test]
    fn scrub_value_walks_nested_strings_and_leaves_numbers_alone() {
        let s = scrubber();
        let mut v = serde_json::json!({
            "intake_id": 42,
            "title": CJK_RUN,
            "items": [
                {"path": "/Users/jane/foo"},
                {"path": "/Volumes/Disk/bookrack/x"},
                {"sha256": "ab1234"},
                {"limit": 50}
            ],
        });
        s.scrub_value(&mut v);
        assert_eq!(v["intake_id"], 42);
        assert!(v["title"].as_str().unwrap().contains("<cjk:"));
        assert_eq!(v["items"][0]["path"], "<HOME>/foo");
        assert_eq!(v["items"][1]["path"], "<DATA_DIR>/x");
        // Hex digits and ASCII paths under the (already-redacted)
        // data dir ride through unaltered.
        assert_eq!(v["items"][2]["sha256"], "ab1234");
        assert_eq!(v["items"][3]["limit"], 50);
    }
}
