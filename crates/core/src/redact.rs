// SPDX-License-Identifier: Apache-2.0

//! Removing build-machine prefixes from dependency source locations.
//!
//! A dependency compiled from the cargo registry, a git checkout, or
//! the standard library records `file!()` as an absolute path on the
//! machine that built it, and some errors print that location. The
//! prefix up to the dependency's own directory names the build
//! machine; the rest — crate, file, line, column — is what diagnoses
//! the error.
//!
//! [`redact_source_paths`] removes the prefix from complete source
//! locations only, so that a path in a book's text or a data path in
//! an error message is left alone. A location is recognised by three
//! parts, all required:
//!
//! 1. **a marker** — `registry/src/<index>/`, `git/checkouts/<repo>/<rev>/`,
//!    or `rustc/<40 hex digits>/`, with `/` or `\` as the separator and
//!    a separator before it;
//! 2. **a tail** — the crate-relative path that follows, ending in
//!    `.rs:<line>` or `.rs:<line>:<column>`, with no whitespace;
//! 3. **a root** — where the absolute prefix starts: a `/`, a drive
//!    letter followed by `:\` or `:/`, or a UNC `\\`, at the start of
//!    the text or right after whitespace, `(`, `[`, `"`, `'`, `=`, or
//!    `,`.
//!
//! The root taken is the last candidate before the marker, provided no
//! `": "`, newline, or tab lies between the two. Taking the last one
//! keeps an earlier path in the message intact; spaces, brackets,
//! quotes, and commas inside directory names do not end the prefix,
//! because they are not followed by a `/`. The limit of that rule: a
//! directory name holding one of those characters followed by `/`, or
//! holding `": "`, ends the prefix there, and the part before it stays.

use std::borrow::Cow;

/// `text` with the build-machine prefix removed from every complete
/// dependency source location, leaving `<crate>-<version>/src/...rs:L:C`
/// (or `library/...` for the standard library). Borrowed when nothing
/// matched; applying it to its own output changes nothing.
pub fn redact_source_paths(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut out: Option<String> = None;
    // Bytes before `copied` are already in `out` (or need no change).
    let mut copied = 0;
    // No root may start before `floor`: the end of the previous match.
    let mut floor = 0;
    let mut i = 1;
    while i < bytes.len() {
        if !is_sep(bytes[i - 1]) {
            i += 1;
            continue;
        }
        let Some(marker_end) = marker_end(bytes, i) else {
            i += 1;
            continue;
        };
        let Some(tail_end) = tail_end(text, marker_end) else {
            i += 1;
            continue;
        };
        let Some(root) = root_before(bytes, floor, i - 1) else {
            i += 1;
            continue;
        };
        let buf = out.get_or_insert_with(String::new);
        buf.push_str(&text[copied..root]);
        copied = marker_end;
        floor = tail_end;
        i = tail_end.max(i + 1);
    }
    match out {
        None => Cow::Borrowed(text),
        Some(mut buf) => {
            buf.push_str(&text[copied..]);
            Cow::Owned(buf)
        }
    }
}

fn is_sep(b: u8) -> bool {
    b == b'/' || b == b'\\'
}

/// A byte that may precede the start of an absolute prefix.
fn is_root_boundary(b: u8) -> bool {
    b.is_ascii_whitespace() || matches!(b, b'(' | b'[' | b'"' | b'\'' | b'=' | b',')
}

/// The end of the marker starting at `at`, if one does.
fn marker_end(bytes: &[u8], at: usize) -> Option<usize> {
    let rest = &bytes[at..];
    if let Some(after) = rest.strip_prefix(b"registry") {
        let after = sep_then(after, b"src")?;
        let after = sep_then_segment(after)?;
        return Some(bytes.len() - sep_tail(after)?.len());
    }
    if let Some(after) = rest.strip_prefix(b"git") {
        let after = sep_then(after, b"checkouts")?;
        let after = sep_then_segment(after)?;
        let after = sep_then_segment(after)?;
        return Some(bytes.len() - sep_tail(after)?.len());
    }
    if let Some(after) = rest.strip_prefix(b"rustc") {
        let after = sep_tail(after)?;
        let hash = after.get(..40)?;
        if !hash.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        return Some(bytes.len() - sep_tail(&after[40..])?.len());
    }
    None
}

/// The bytes after a separator, if `bytes` starts with one.
fn sep_tail(bytes: &[u8]) -> Option<&[u8]> {
    bytes.first().copied().filter(|b| is_sep(*b))?;
    Some(&bytes[1..])
}

/// The bytes after a separator and `word`.
fn sep_then<'a>(bytes: &'a [u8], word: &[u8]) -> Option<&'a [u8]> {
    sep_tail(bytes)?.strip_prefix(word)
}

/// The bytes after a separator and one non-empty path segment.
fn sep_then_segment(bytes: &[u8]) -> Option<&[u8]> {
    let after = sep_tail(bytes)?;
    let len = after
        .iter()
        .take_while(|b| !is_sep(**b) && !b.is_ascii_whitespace())
        .count();
    (len > 0).then_some(&after[len..])
}

/// The end of the `….rs:<line>[:<column>]` tail starting at `from`,
/// with no whitespace before it.
fn tail_end(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut j = from;
    while j < bytes.len() {
        if text[j..].starts_with(char::is_whitespace) {
            return None;
        }
        if bytes[j..].starts_with(b".rs:") && bytes.get(j + 4).is_some_and(u8::is_ascii_digit) {
            let mut end = digits_end(bytes, j + 4);
            if bytes.get(end) == Some(&b':') && bytes.get(end + 1).is_some_and(u8::is_ascii_digit) {
                end = digits_end(bytes, end + 1);
            }
            return Some(end);
        }
        j += text[j..].chars().next().map_or(1, char::len_utf8);
    }
    None
}

fn digits_end(bytes: &[u8], from: usize) -> usize {
    from + bytes[from..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count()
}

/// The last root in `floor..before` with no `": "`, newline, or tab
/// between it and `before`.
fn root_before(bytes: &[u8], floor: usize, before: usize) -> Option<usize> {
    let window = &bytes[floor..before];
    let barrier = window
        .windows(2)
        .rposition(|w| w == b": ")
        .map(|p| p + 2)
        .into_iter()
        .chain(
            window
                .iter()
                .rposition(|b| *b == b'\n' || *b == b'\t')
                .map(|p| p + 1),
        )
        .max()
        .unwrap_or(0);
    (floor + barrier..before).rev().find(|&c| is_root(bytes, c))
}

/// Whether an absolute prefix can start at `c`.
fn is_root(bytes: &[u8], c: usize) -> bool {
    let bounded = c == 0 || is_root_boundary(bytes[c - 1]);
    if !bounded {
        return false;
    }
    match bytes[c] {
        b'/' => true,
        b'\\' => bytes.get(c + 1) == Some(&b'\\'),
        b if b.is_ascii_alphabetic() => {
            bytes.get(c + 1) == Some(&b':') && bytes.get(c + 2).is_some_and(|b| is_sep(*b))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crate-relative location every positive case must keep.
    const KEPT: &str = "lance-index-7.0.0/src/vector/kmeans.rs:1365:20";

    /// A registry location under `prefix`, separated by `sep`.
    fn registry(prefix: &str, sep: char) -> String {
        let tail = KEPT.replace('/', &sep.to_string());
        format!("{prefix}{sep}registry{sep}src{sep}index.crates.io-0123456789abcdef{sep}{tail}")
    }

    /// `text` redacts to `expected`, and redacting again changes
    /// nothing.
    fn assert_redacts(text: &str, expected: &str) {
        let once = redact_source_paths(text);
        assert_eq!(once, expected, "{text}");
        assert!(matches!(once, Cow::Owned(_)), "{text}");
        assert_eq!(redact_source_paths(&once), once, "idempotent: {text}");
    }

    fn assert_untouched(text: &str) {
        assert!(
            matches!(redact_source_paths(text), Cow::Borrowed(t) if t == text),
            "{text}"
        );
    }

    #[test]
    fn a_unix_registry_location_keeps_the_cause_and_the_relative_path() {
        for prefix in ["/disk/cache/cargo", "/opt/rust/cargo-home/.cargo"] {
            let text = format!(
                "lance error: Unprocessable: KMeans cannot train 4 centroids, {}: Unprocessable",
                registry(prefix, '/')
            );
            assert_redacts(
                &text,
                &format!(
                    "lance error: Unprocessable: KMeans cannot train 4 centroids, {KEPT}: Unprocessable"
                ),
            );
        }
    }

    #[test]
    fn git_checkout_and_toolchain_locations_lose_their_prefix() {
        assert_redacts(
            "panicked at /disk/cargo/git/checkouts/lance-0a1b2c/4d5e6f7/rust/lance/src/io.rs:12:5",
            "panicked at rust/lance/src/io.rs:12:5",
        );
        let hash = "0123456789abcdef0123456789abcdef01234567";
        assert_redacts(
            &format!("at /disk/rustc/{hash}/library/core/src/option.rs:2040:5"),
            "at library/core/src/option.rs:2040:5",
        );
    }

    #[test]
    fn spaces_brackets_quotes_commas_and_unicode_in_the_prefix_go_with_it() {
        let drive = 'D';
        let cases = [
            registry("/disk/build cache/cargo", '/'),
            registry(&format!("{drive}:\\Program Files (x86)\\cargo"), '\\'),
            registry("/disk/O'Neil/cargo", '/'),
            registry("/disk/x, y/cargo", '/'),
            registry("/disk/\u{6587}\u{5e93}/cargo", '/'),
        ];
        for location in cases {
            let kept = if location.contains('\\') {
                KEPT.replace('/', "\\")
            } else {
                KEPT.to_string()
            };
            assert_redacts(&format!("cause, {location}"), &format!("cause, {kept}"));
        }
    }

    #[test]
    fn drive_and_unc_roots_are_recognised() {
        let drive = 'E';
        let kept = KEPT.replace('/', "\\");
        assert_redacts(
            &format!("cause: {}", registry(&format!("{drive}:\\cargo"), '\\')),
            &format!("cause: {kept}"),
        );
        assert_redacts(
            &format!("cause: {}", registry("\\\\buildhost\\share\\cargo", '\\')),
            &format!("cause: {kept}"),
        );
    }

    #[test]
    fn an_earlier_path_in_the_message_stays() {
        let text = format!(
            "cannot open '/a/b' at {}",
            registry("/disk/cache/cargo", '/')
        );
        assert_redacts(&text, &format!("cannot open '/a/b' at {KEPT}"));
    }

    #[test]
    fn two_locations_are_both_redacted() {
        let one = registry("/disk/cache/cargo", '/');
        assert_redacts(
            &format!("first, {one}: second: {one}"),
            &format!("first, {KEPT}: second: {KEPT}"),
        );
    }

    #[test]
    fn text_without_a_complete_location_is_untouched() {
        for text in [
            "cannot open /srv/registry/src/archive/catalog.db",
            "cargo/registry/src/index.crates.io-0123/lance-7.0.0/src/io.rs:3:1",
            "a plain sentence with no path",
            "cannot open /srv/library/catalog.db",
            "",
        ] {
            assert_untouched(text);
        }
    }

    #[test]
    fn a_path_before_a_colon_separated_marker_is_not_a_root() {
        assert_untouched(
            "cannot open /srv/data: registry/src/index.crates.io-0123/lance-7.0.0/src/io.rs:3:1",
        );
    }
}
