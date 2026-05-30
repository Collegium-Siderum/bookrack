// SPDX-License-Identifier: Apache-2.0

//! Deterministic sentence counting.
//!
//! [`count_sentences`] estimates how many sentences a passage holds by
//! counting runs of terminal punctuation. It is intentionally crude — a
//! statistic stored on each prose leaf, not a linguistic parser — but it
//! is pure and frozen: its output feeds a stored field, so any change to
//! its behaviour must bump [`SENTENCE_VERSION`] and re-derive that field.
//!
//! The terminator set covers Latin (`.`, `!`, `?`, `;`) and the
//! full-width / ideographic equivalents. A run of consecutive
//! terminators (`...`, `?!`) counts once. Any text with non-whitespace
//! content scores at least one sentence; whitespace-only text scores
//! zero.

/// Version of the sentence-counting algorithm.
///
/// Bump on any change to [`count_sentences`]'s behaviour: the count is
/// stored on each prose leaf, so a change is a re-derivation commitment.
pub const SENTENCE_VERSION: u32 = 1;

/// Count the sentences in `text`.
///
/// Whitespace-only (or empty) input yields 0; any input with content
/// yields at least 1, even when it carries no terminator.
pub fn count_sentences(text: &str) -> i64 {
    let mut runs: i64 = 0;
    let mut in_run = false;
    let mut has_content = false;
    for c in text.chars() {
        if is_terminator(c) {
            if !in_run {
                runs += 1;
                in_run = true;
            }
            has_content = true;
        } else {
            in_run = false;
            if !c.is_whitespace() {
                has_content = true;
            }
        }
    }
    if !has_content {
        return 0;
    }
    runs.max(1)
}

/// Whether `c` ends a sentence: Latin terminators plus the full-width and
/// ideographic forms a CJK text uses.
fn is_terminator(c: char) -> bool {
    matches!(
        c,
        '.' | '!' | '?' | ';'
            | '\u{3002}' // ideographic full stop
            | '\u{FF01}' // fullwidth exclamation mark
            | '\u{FF1F}' // fullwidth question mark
            | '\u{FF1B}' // fullwidth semicolon
            | '\u{2026}' // horizontal ellipsis
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin_terminators_each_count_once() {
        assert_eq!(count_sentences("One. Two! Three?"), 3);
        assert_eq!(count_sentences("A clause; another clause."), 2);
    }

    #[test]
    fn a_run_of_terminators_counts_once() {
        assert_eq!(count_sentences("Wait... what?!"), 2);
        assert_eq!(count_sentences("Done!!!"), 1);
    }

    #[test]
    fn cjk_terminators_are_recognized() {
        // "ni hao. zai jian." with ideographic full stops.
        let text = "\u{4F60}\u{597D}\u{3002}\u{518D}\u{89C1}\u{3002}";
        assert_eq!(count_sentences(text), 2);
        // Fullwidth question mark.
        assert_eq!(count_sentences("\u{4F60}\u{597D}\u{FF1F}"), 1);
    }

    #[test]
    fn content_without_a_terminator_counts_as_one() {
        assert_eq!(count_sentences("no terminator here"), 1);
    }

    #[test]
    fn whitespace_only_counts_as_zero() {
        assert_eq!(count_sentences(""), 0);
        assert_eq!(count_sentences("   \n\t "), 0);
    }

    #[test]
    fn version_is_one() {
        assert_eq!(SENTENCE_VERSION, 1);
    }
}
