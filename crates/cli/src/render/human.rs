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
}
