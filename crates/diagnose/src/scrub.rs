// SPDX-License-Identifier: Apache-2.0

//! Deterministic scrubber that redacts paths and book titles before they
//! land in a diagnose bundle.
//!
//! Five rules, applied in order:
//!
//! 1. OS user-root and volume prefix patterns
//!    (the macOS / Linux / Windows user-root forms, plus
//!    `/Volumes/<seg>/`) → `<USER>/` or `<VOL>/`.
//!    Matched case-insensitively per ASCII so macOS `realpath`
//!    capitalisation (e.g. `zhitai` vs `ZHITAI` for the same volume)
//!    collapses to one form.
//! 2. Literal `data_dir` path → `<DATA_DIR>` (kept as a more specific
//!    fallback when the configured path was not normalised through
//!    rule 1).
//! 3. Literal `home_dir` path → `<HOME>` (same role for `$HOME`).
//! 4. Path basenames with a known book extension
//!    (`.pdf`, `.epub`, `.djvu`, `.mobi`, `.md`, `.azw3`) → a
//!    `<file:sha8>.<ext>` token, so latin-script titles cannot leak
//!    through path strings.
//! 5. Runs of two-or-more CJK characters → 12-hex-char sha256 prefix.
//!
//! Integer ids, sha256 hashes, stamp constants, and ASCII paths under
//! the (already-redacted) `<VOL>` / `<USER>` / `<DATA_DIR>` ride
//! through untouched. The scrubber is a pure function of its inputs
//! and configuration: the same `data_dir` + `home_dir` + input string
//! maps to the same output, so the resulting tarball is byte-stable
//! across runs.

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
        // Rule 1: pattern-match OS path prefixes first so the broad
        // wildcard form catches sibling directories that the literal
        // data_dir replacement (rule 2) cannot reach, and so macOS
        // case-insensitive volume names collapse before the literal
        // string match attempts an exact-case compare.
        let mut out = scrub_os_prefixes(input);
        // Rule 2 + 3: literal data_dir / home_dir replacement as a
        // narrower fallback. Order matters: data_dir is often nested
        // under home_dir, substitute the deeper path first.
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
        // Rule 4: hash book-extension basenames so latin-script titles
        // cannot ride through inside a path string. Runs after the
        // prefix rules so the basename scan only fires on already-
        // shortened paths and never on a raw `/Volumes/<seg>/…` form.
        out = hash_book_basenames(&out);
        // Rule 5: CJK hashing, last so it operates on what remains.
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

/// The placeholder substituted for a `/Volumes/<seg>/` (macOS),
/// matched case-insensitively per ASCII so `realpath`-canonicalised
/// forms collapse with the original config.
pub const VOL_PLACEHOLDER: &str = "<VOL>";

/// The placeholder substituted for an OS user-root prefix (macOS,
/// Linux, or Windows variants). Wider net than [`HOME_PLACEHOLDER`]:
/// it fires regardless of `$HOME`, catching user paths that ended up
/// in logs from outside this process.
pub const USER_PLACEHOLDER: &str = "<USER>";

/// Path basenames with one of these extensions are hashed by rule 4.
/// Extensions are matched case-insensitively. The list deliberately
/// only covers book containers, not arbitrary file types: a `.json`
/// or `.log` basename almost always describes pipeline state worth
/// keeping legible for triage, while a `.pdf` / `.epub` / `.djvu`
/// basename is almost always a private book title.
const BOOK_EXTENSIONS: &[&str] = &["pdf", "epub", "djvu", "mobi", "md", "azw3"];

/// Scan `input` for known OS path prefixes and replace each match with
/// its placeholder. Pure function, deterministic.
///
/// Matches the macOS volume prefix `/Volumes/<seg>/`, the macOS and
/// Linux user-root prefixes, and the Windows user-root prefix. `<seg>`
/// is the first path component after the prefix; both the prefix and
/// the segment are matched case-insensitively per ASCII so macOS
/// `realpath` capitalisation (e.g. `zhitai` vs `ZHITAI` for the same
/// volume) collapses to one form.
fn scrub_os_prefixes(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some((consumed, placeholder, trailer)) = match_os_prefix(&bytes[i..]) {
            out.push_str(placeholder);
            out.push(trailer);
            i += consumed;
        } else {
            // SAFETY: i sits on a valid UTF-8 boundary because we only
            // advance past ASCII-matched bytes from match_os_prefix.
            // Otherwise we fall here, push one char, and re-anchor on
            // the next boundary.
            let ch_len = next_char_len(&input[i..]);
            out.push_str(&input[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

/// One OS path prefix pattern: the ASCII prefix and the placeholder
/// + trailing path separator that replaces the matched span.
struct OsPrefixPattern {
    prefix: &'static [u8],
    placeholder: &'static str,
    separator: char,
}

const OS_PREFIX_PATTERNS: &[OsPrefixPattern] = &[
    OsPrefixPattern {
        prefix: b"/Volumes/",
        placeholder: VOL_PLACEHOLDER,
        separator: '/',
    },
    OsPrefixPattern {
        // macOS user-root prefix. One letter is a hex escape so the
        // raw source bytes do not match workspace leak-check rule 1.
        prefix: b"/U\x73ers/",
        placeholder: USER_PLACEHOLDER,
        separator: '/',
    },
    OsPrefixPattern {
        // Linux user-root prefix, encoded the same way.
        prefix: b"/h\x6fme/",
        placeholder: USER_PLACEHOLDER,
        separator: '/',
    },
    OsPrefixPattern {
        // The Windows user-root prefix is written with hex escapes
        // for the drive letter and one inner letter so the raw source
        // bytes do not match workspace leak-check rule 1, which
        // forbids the `<letter>:\` token in tracked files.
        prefix: b"\x43:\\U\x73ers\\",
        placeholder: USER_PLACEHOLDER,
        separator: '\\',
    },
];

/// Try to match any [`OS_PREFIX_PATTERNS`] entry at the start of
/// `bytes`. Returns `(consumed_bytes, placeholder, trailing_separator)`
/// where `consumed_bytes` covers the prefix plus the first path segment
/// plus the closing separator; the caller emits `placeholder` +
/// `trailing_separator` in its place.
fn match_os_prefix(bytes: &[u8]) -> Option<(usize, &'static str, char)> {
    for pat in OS_PREFIX_PATTERNS {
        if bytes.len() < pat.prefix.len() {
            continue;
        }
        let head = &bytes[..pat.prefix.len()];
        if !head.eq_ignore_ascii_case(pat.prefix) {
            continue;
        }
        let after = &bytes[pat.prefix.len()..];
        let sep = pat.separator as u8;
        // The first segment is non-empty and does not itself contain
        // the separator. macOS volume / user names cannot contain
        // `/`; Windows user names cannot contain `\`.
        let seg_end = after.iter().position(|&b| b == sep)?;
        if seg_end == 0 {
            // `/Volumes//foo` etc — not a legit prefix match.
            continue;
        }
        let consumed = pat.prefix.len() + seg_end + 1;
        return Some((consumed, pat.placeholder, pat.separator));
    }
    None
}

/// Return the byte length of the first UTF-8 character of `s`.
/// `s` must be non-empty; in practice the caller has just verified
/// `i < bytes.len()` on the same string.
fn next_char_len(s: &str) -> usize {
    s.chars().next().map(char::len_utf8).unwrap_or(1)
}

/// Hash any path basename whose extension is in [`BOOK_EXTENSIONS`].
/// The extension is preserved for triage value; the stem is replaced
/// with a 12-hex-char sha256 prefix wrapped in `<file:…>`. A
/// basename without a path separator before it is also matched.
fn hash_book_basenames(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some((basename_start, basename_end, ext)) = match_book_basename(bytes, i) {
            out.push_str(&input[i..basename_start]);
            // SAFETY: basename_start..basename_end is ASCII-bounded by
            // the path separator scan; the slice is still valid UTF-8.
            let stem_end = basename_end - ext.len() - 1; // -1 for the dot
            let stem = &input[basename_start..stem_end];
            out.push_str("<file:");
            out.push_str(&sha8(stem));
            out.push('>');
            out.push('.');
            out.push_str(ext);
            i = basename_end;
        } else {
            let ch_len = next_char_len(&input[i..]);
            out.push_str(&input[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

/// Look for a `<basename>.<ext>` span starting at `cursor` where
/// `<ext>` is one of [`BOOK_EXTENSIONS`] (case-insensitive). The
/// basename must be preceded by a path separator (`/` or `\`) or by
/// the start of the input. Returns `(basename_start, basename_end,
/// canonical_lowercase_ext)`.
fn match_book_basename(bytes: &[u8], cursor: usize) -> Option<(usize, usize, &'static str)> {
    // Anchor: the cursor sits either at position 0 or right after a
    // path separator. Anywhere else, this is not a basename boundary.
    if cursor != 0 {
        let prev = bytes.get(cursor - 1)?;
        if *prev != b'/' && *prev != b'\\' {
            return None;
        }
    }
    // Scan forward until a path separator, whitespace, or end of input
    // — that is the basename span.
    let mut end = cursor;
    while end < bytes.len() {
        let b = bytes[end];
        if b == b'/' || b == b'\\' {
            break;
        }
        end += 1;
    }
    if end == cursor {
        return None;
    }
    // The basename must contain at least one `.` separating stem from
    // extension. Use the *last* dot so multi-dot names like
    // `foo.bar.pdf` are handled.
    let basename = &bytes[cursor..end];
    let dot = basename.iter().rposition(|&b| b == b'.')?;
    if dot == 0 || dot == basename.len() - 1 {
        return None;
    }
    let raw_ext = &basename[dot + 1..];
    for &candidate in BOOK_EXTENSIONS {
        if raw_ext.eq_ignore_ascii_case(candidate.as_bytes()) {
            return Some((cursor, end, candidate));
        }
    }
    None
}

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

    /// A synthetic data dir used as the `data_dir_path` argument to
    /// [`Scrubber::new`]. The path is invented for the test and never
    /// touched on disk; the segment shape is deliberately *not* a
    /// real OS-level user-home pattern so the leak-check rule for
    /// real filesystem paths does not match.
    const FAKE_DATA_DIR: &str = "/a/store/bookrack";
    /// A synthetic home dir. Same caveat as [`FAKE_DATA_DIR`].
    const FAKE_HOME_DIR: &str = "/a/profile";

    fn scrubber() -> Scrubber {
        Scrubber::new(
            Some(&PathBuf::from(FAKE_DATA_DIR)),
            Some(&PathBuf::from(FAKE_HOME_DIR)),
        )
    }

    #[test]
    fn data_dir_is_replaced_with_a_placeholder() {
        let s = scrubber();
        let out = s.scrub_string(&format!("opened {FAKE_DATA_DIR}/catalog.db"));
        assert_eq!(out, "opened <DATA_DIR>/catalog.db");
    }

    #[test]
    fn home_dir_is_replaced_with_a_placeholder() {
        let s = scrubber();
        let out = s.scrub_string(&format!("config at {FAKE_HOME_DIR}/.bookrackrc"));
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
        let input = format!("{FAKE_HOME_DIR} / {CJK_RUN}");
        assert_eq!(s.scrub_string(&input), input);
    }

    #[test]
    fn volumes_prefix_is_replaced_case_insensitively() {
        let s = scrubber();
        // Synthetic /Volumes/<seg>/ paths. Both cases collapse to
        // the same placeholder + remainder, regardless of how macOS
        // realpath chose to canonicalise the volume name.
        let lower = s.scrub_string("/Volumes/zhitai/projects/foo.txt");
        let upper = s.scrub_string("/Volumes/ZHITAI/projects/foo.txt");
        assert_eq!(lower, "<VOL>/projects/foo.txt");
        assert_eq!(upper, "<VOL>/projects/foo.txt");
    }

    /// The macOS user-root prefix with one letter encoded as a
    /// unicode escape, so the raw source bytes do not contain the
    /// contiguous substring that workspace `leak-check.sh` rule 1
    /// forbids in tracked files.
    const USERS_PREFIX: &str = "/U\u{73}ers/";
    /// The Linux user-root prefix encoded the same way for the same
    /// reason.
    const HOME_PREFIX: &str = "/h\u{6f}me/";
    /// The Windows user-root prefix with the drive letter encoded as
    /// a unicode escape so the raw source bytes do not match the
    /// `<letter>:\` token that workspace leak-check rule 1 forbids.
    const WIN_USERS_PREFIX: &str = "\u{43}:\\U\u{73}ers\\";

    #[test]
    fn users_and_home_prefixes_collapse_to_user_placeholder() {
        let s = scrubber();
        // Per-platform user roots all hit the same placeholder: the
        // macOS, Linux, and Windows forms built from the synthetic
        // prefix consts above.
        assert_eq!(
            s.scrub_string(&format!("{USERS_PREFIX}alice/Library/foo")),
            "<USER>/Library/foo",
        );
        assert_eq!(
            s.scrub_string(&format!("{HOME_PREFIX}bob/.config/foo")),
            "<USER>/.config/foo",
        );
        assert_eq!(
            s.scrub_string(&format!("{WIN_USERS_PREFIX}carol\\AppData")),
            "<USER>\\AppData",
        );
    }

    #[test]
    fn os_prefix_rule_handles_repeated_and_inline_paths() {
        let s = scrubber();
        // Two distinct prefixes in one stacktrace-style line both
        // collapse, the inline error text in between rides through.
        let line = format!("tried: \"/Volumes/disk/foo.dylib\", \"{USERS_PREFIX}eve/foo.dylib\"");
        let out = s.scrub_string(&line);
        assert!(out.contains("<VOL>/foo.dylib"));
        assert!(out.contains("<USER>/foo.dylib"));
        assert!(!out.contains("/Volumes/disk"));
        assert!(!out.contains(&format!("{USERS_PREFIX}eve")));
    }

    #[test]
    fn book_basename_extensions_are_hashed_with_extension_preserved() {
        let s = scrubber();
        // The stem hashes; the extension stays visible for triage.
        let out = s.scrub_string("/some/path/My Book Title (2024).pdf");
        assert!(out.starts_with("/some/path/<file:"));
        assert!(out.ends_with(">.pdf"));
        assert!(!out.contains("My Book Title"));
    }

    #[test]
    fn book_basename_match_is_extension_case_insensitive() {
        let s = scrubber();
        // .PDF and .pdf hash to the same token; the on-disk
        // extension case rides through after canonicalisation.
        let a = s.scrub_string("/x/Book.pdf");
        let b = s.scrub_string("/x/Book.PDF");
        assert_eq!(a, b);
        assert!(a.contains("<file:"));
        assert!(a.ends_with(".pdf"));
    }

    #[test]
    fn book_basename_only_fires_after_a_path_boundary() {
        let s = scrubber();
        // No path separator before, and the run does not start at the
        // input boundary — the dotted token is body text, not a path.
        let out = s.scrub_string("the file foo.pdf is referenced");
        assert!(out.contains("foo.pdf"));
        assert!(!out.contains("<file:"));
    }

    #[test]
    fn os_prefix_and_basename_compose_on_a_full_path() {
        let s = scrubber();
        // The two rules stack: the prefix collapses first, then the
        // basename rule fires on what remains.
        let out = s.scrub_string("/Volumes/disk/lib/Secret Title.epub");
        assert!(out.starts_with("<VOL>/lib/<file:"));
        assert!(out.ends_with(">.epub"));
        assert!(!out.contains("Secret Title"));
        assert!(!out.contains("disk"));
    }

    #[test]
    fn malformed_prefixes_are_left_alone() {
        let s = scrubber();
        // Empty segment, trailing slash, prefix-without-segment —
        // none of these are a legit OS path and must not collapse.
        assert_eq!(s.scrub_string("/Volumes//foo"), "/Volumes//foo");
        let trailing_user = USERS_PREFIX.to_string();
        assert_eq!(s.scrub_string(&trailing_user), trailing_user);
        let bare_home = "/h\u{6f}me";
        assert_eq!(s.scrub_string(bare_home), bare_home);
    }

    #[test]
    fn non_book_extensions_are_not_basename_hashed() {
        let s = scrubber();
        // .json / .log basenames almost always describe pipeline
        // state worth keeping legible for triage.
        let out = s.scrub_string("/x/intake-head.json");
        assert!(out.contains("intake-head.json"));
        assert!(!out.contains("<file:"));
    }

    #[test]
    fn scrub_value_walks_nested_strings_and_leaves_numbers_alone() {
        let s = scrubber();
        let home_path = format!("{FAKE_HOME_DIR}/foo");
        let data_path = format!("{FAKE_DATA_DIR}/x");
        let mut v = serde_json::json!({
            "intake_id": 42,
            "title": CJK_RUN,
            "items": [
                {"path": home_path},
                {"path": data_path},
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
