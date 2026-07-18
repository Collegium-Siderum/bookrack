// SPDX-License-Identifier: Apache-2.0

//! Human-readable formatters reused across subcommand renderers.

/// Number of leading characters of a UUID kept as a memorable
/// job-id surrogate in human output. Matches the prefix that
/// `queue cancel <prefix>` accepts.
pub const SHORT_ID_LEN: usize = 8;

/// Returns the first [`SHORT_ID_LEN`] characters of a UUID-like
/// identifier. Inputs shorter than the cutoff pass through unchanged.
pub fn short_id(id: &str) -> &str {
    let end = id
        .char_indices()
        .map(|(i, _)| i)
        .nth(SHORT_ID_LEN)
        .unwrap_or(id.len());
    &id[..end]
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
/// KiB up (`512 B`, `2.1 GiB`).
pub fn bytes_human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

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

    #[test]
    fn bytes_pick_the_right_binary_unit() {
        assert_eq!(bytes_human(0), "0 B");
        assert_eq!(bytes_human(512), "512 B");
        assert_eq!(bytes_human(2048), "2.0 KiB");
        assert_eq!(bytes_human(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(bytes_human(2_254_857_830), "2.1 GiB");
    }

    #[test]
    fn short_id_truncates() {
        assert_eq!(short_id("0190f6c0-ac42-7e05-7000-123456789abc"), "0190f6c0");
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
