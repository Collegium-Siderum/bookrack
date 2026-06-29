// SPDX-License-Identifier: Apache-2.0

//! Horizontal histogram rendering shared by the `runs show` surface
//! and any later subcommand that wants to print a one-dimensional
//! distribution. The bars are built from a Unicode full-block run, so
//! `key | <bar> N (P%)` reads at a glance on any 256-colour terminal;
//! callers that need pure-ASCII output can fall back to
//! [`render_histogram_bars_with`] and pick their own bar glyph.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Default bar glyph: U+2588 FULL BLOCK. Encoded as an escape so the
/// source stays Latin-only per the project's writing discipline.
const DEFAULT_BAR: char = '\u{2588}';

/// Render a histogram block whose rows are sorted by `counts`' natural
/// key order. Each row reads `<key> | <bar> <count> (<pct>%)`. Buckets
/// with `count == 0` are dropped. `width` is the maximum number of
/// bar glyphs the longest bar should reach; shorter bars scale linearly
/// against the largest count.
pub fn render_histogram_bars(counts: &BTreeMap<String, u64>, width: usize) -> String {
    render_histogram_bars_with(counts, width, DEFAULT_BAR)
}

/// Variant of [`render_histogram_bars`] that lets the caller pick the
/// bar glyph (e.g. `#` for a fixed-width ASCII fallback).
pub fn render_histogram_bars_with(
    counts: &BTreeMap<String, u64>,
    width: usize,
    bar: char,
) -> String {
    let kept: Vec<(&String, u64)> = counts
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(k, n)| (k, *n))
        .collect();
    if kept.is_empty() {
        return String::new();
    }
    let total: u64 = kept.iter().map(|(_, n)| *n).sum();
    let max_count = kept.iter().map(|(_, n)| *n).max().unwrap_or(1);
    let key_width = kept
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (key, count) in kept {
        let bar_len = if width == 0 || max_count == 0 {
            0
        } else {
            // Scale this bucket's bar against the largest one, rounding
            // toward at-least-one so a non-zero bucket always shows a
            // visible bar.
            let scaled = (count as u128 * width as u128) / max_count as u128;
            scaled.max(1) as usize
        };
        let bar_str: String = std::iter::repeat_n(bar, bar_len).collect();
        let pct = if total == 0 {
            0.0
        } else {
            (count as f64 / total as f64) * 100.0
        };
        // Pad the key column so bars line up. Format string uses byte
        // width but every key here is ASCII so chars == bytes.
        let _ = writeln!(out, "  {key:<key_width$} | {bar_str} {count} ({pct:.0}%)");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_map_renders_to_an_empty_string() {
        let counts: BTreeMap<String, u64> = BTreeMap::new();
        assert_eq!(render_histogram_bars(&counts, 20), "");
    }

    #[test]
    fn zero_buckets_drop_out() {
        let mut counts = BTreeMap::new();
        counts.insert("clean".to_string(), 0);
        counts.insert("needs_work".to_string(), 0);
        assert_eq!(render_histogram_bars(&counts, 20), "");
    }

    #[test]
    fn the_largest_bucket_uses_the_full_width() {
        let mut counts = BTreeMap::new();
        counts.insert("clean".to_string(), 3);
        counts.insert("needs_work".to_string(), 1);
        let out = render_histogram_bars_with(&counts, 12, '#');
        // "clean" is the largest, gets all 12 bar glyphs.
        assert!(out.contains("clean      | ############ 3 (75%)"));
        // "needs_work" gets a quarter, rounded toward 1 above the floor.
        assert!(out.contains("needs_work | #### 1 (25%)"));
    }

    #[test]
    fn a_nonzero_bucket_below_the_scale_floor_still_renders_one_bar() {
        let mut counts = BTreeMap::new();
        counts.insert("a".to_string(), 100);
        counts.insert("b".to_string(), 1);
        let out = render_histogram_bars_with(&counts, 10, '#');
        // 1 * 10 / 100 == 0 by integer math, but the floor keeps it at 1.
        let b_line = out.lines().find(|l| l.contains("b |")).expect("b row");
        let bars: String = b_line.chars().filter(|c| *c == '#').collect();
        assert_eq!(bars, "#");
    }

    #[test]
    fn keys_are_padded_to_the_widest_key() {
        let mut counts = BTreeMap::new();
        counts.insert("aaa".to_string(), 1);
        counts.insert("bbbbbb".to_string(), 1);
        let out = render_histogram_bars_with(&counts, 4, '#');
        let aaa = out
            .lines()
            .find(|l| l.starts_with("  aaa"))
            .expect("aaa row");
        assert!(
            aaa.starts_with("  aaa    |"),
            "key column must pad to the widest key, got {aaa:?}"
        );
    }
}
