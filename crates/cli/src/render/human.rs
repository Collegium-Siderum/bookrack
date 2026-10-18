// SPDX-License-Identifier: Apache-2.0

//! Human-readable formatters reused across subcommand renderers.

/// Number of trailing characters of a UUID kept as a job-id surrogate
/// in human output. A UUIDv7 leads with its millisecond timestamp, so
/// jobs queued within the same minute share their leading characters;
/// the trailing ones are random. `queue cancel` and `queue retry --job`
/// match a fragment anywhere in the id, so the printed form is accepted
/// as it stands.
// setting: internal -- how wide an id is printed, not what it identifies
pub const SHORT_ID_LEN: usize = 8;

/// Returns the last [`SHORT_ID_LEN`] characters of a UUID-like
/// identifier. Inputs shorter than the cutoff pass through unchanged.
pub fn short_id(id: &str) -> &str {
    let start = id
        .char_indices()
        .rev()
        .nth(SHORT_ID_LEN - 1)
        .map_or(0, |(i, _)| i);
    &id[start..]
}

/// Trims `s` to at most `max_chars` Unicode scalar values, swapping
/// the last one for `…` when truncation occurred. Inputs short
/// enough pass through unchanged.
pub fn truncate_to(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut buf: String = s.chars().take(keep).collect();
    buf.push('…');
    buf
}

/// Formats a byte count with a binary-unit suffix, one decimal from
/// KiB up (`512 B`, `2.1 GiB`). Defined in `core` so the daemon-side
/// surfaces render a size the same way this one does.
pub use bookrack_core::bytes_human;

/// Formats `Some(path)` as the file's basename or the path itself
/// when no basename can be extracted, and `None` as a dash.
pub fn basename_or_dash(path: Option<&str>) -> &str {
    match path {
        Some(p) => p
            .rsplit_once(std::path::MAIN_SEPARATOR)
            .map_or(p, |(_, b)| b),
        None => "-",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two ids generated within the same minute share their leading
    /// timestamp characters; the short form is the random tail, which
    /// tells them apart.
    #[test]
    fn short_id_keeps_the_random_tail() {
        let first = "019a1b2c-3d4e-7f00-8a11-0123456789ab";
        let second = "019a1b2c-3d4e-7f00-8a11-fedcba987654";
        assert_eq!(short_id(first), "456789ab");
        assert_eq!(short_id(second), "ba987654");
    }

    #[test]
    fn short_id_passes_short_input_through() {
        assert_eq!(short_id("abc"), "abc");
    }

    #[test]
    fn basename_strips_directory() {
        let sep = std::path::MAIN_SEPARATOR;
        let p = format!("foo{sep}bar{sep}baz.epub");
        assert_eq!(basename_or_dash(Some(&p)), "baz.epub");
    }

    #[test]
    fn basename_none_is_dash() {
        assert_eq!(basename_or_dash(None), "-");
    }

    #[test]
    fn truncate_to_keeps_short_strings() {
        assert_eq!(truncate_to("hello", 10), "hello");
        assert_eq!(truncate_to("hello", 5), "hello");
    }

    #[test]
    fn truncate_to_appends_ellipsis() {
        assert_eq!(truncate_to("helloworld", 6), "hello…");
    }

    #[test]
    fn truncate_to_handles_multibyte_chars() {
        let s = "αβγδε";
        assert_eq!(truncate_to(s, 3), "αβ…");
    }
}
