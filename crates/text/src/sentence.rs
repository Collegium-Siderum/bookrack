// SPDX-License-Identifier: Apache-2.0

//! Sentence terminators, sentence counting, and sentence boundaries.
//!
//! One terminator table serves everything here. The set covers Latin
//! (`.`, `!`, `?`, `;`) and the full-width / ideographic equivalents.
//!
//! [`count_sentences`] estimates how many sentences a passage holds by
//! counting runs of terminal punctuation. It is intentionally crude — a
//! statistic stored on each prose leaf, not a linguistic parser — but it
//! is pure and frozen: its output feeds a stored field, so any change to
//! its behaviour must bump [`SENTENCE_VERSION`] and re-derive that field.
//! A run of consecutive terminators (`...`, `?!`) counts once. Any text
//! with non-whitespace content scores at least one sentence;
//! whitespace-only text scores zero.
//!
//! [`sentence_boundaries`] places the char offsets at which a new
//! sentence starts, and [`split_at_sentences`] cuts a text at them once
//! it outgrows a length threshold.

/// Version of the sentence-counting algorithm.
///
/// [`count_sentences`] derives the per-leaf `sentence_count` stored in
/// `corpus.db`, so a change to its behaviour leaves existing counts
/// stale. This constant records the algorithm revision for that reason.
/// It is not yet wired into any stamp chain, so bumping it does not
/// itself trigger a re-derivation — an existing corpus keeps its old
/// counts until it is rebuilt.
// setting: internal -- a version stamp; docs/UPGRADE.md's runbook governs a bump
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
pub fn is_terminator(c: char) -> bool {
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

/// Whether `c` ends a sentence and has no second reading as
/// abbreviation or list punctuation: [`is_terminator`] without the
/// ASCII full stop, the semicolon, and the fullwidth semicolon.
pub fn is_unambiguous_terminator(c: char) -> bool {
    is_terminator(c) && !matches!(c, '.' | ';' | '\u{FF1B}')
}

/// Whether `c` may trail a terminator and still belong to the sentence
/// it closes: closing quotation marks and brackets. Which mark closes a
/// quotation depends on the typographic convention — German and Swiss
/// text close with the marks English and French open with — so a mark
/// that closes under any convention counts; a quotation never opens
/// directly after a terminator.
pub fn is_closer(c: char) -> bool {
    matches!(
        c,
        '"' | '\''
            | ')'
            | ']'
            | '\u{AB}' // left-pointing double angle quotation mark (closes in German)
            | '\u{BB}' // right-pointing double angle quotation mark
            | '\u{2018}' // left single quotation mark (closes in German)
            | '\u{2019}' // right single quotation mark
            | '\u{201C}' // left double quotation mark (closes in German)
            | '\u{201D}' // right double quotation mark
            | '\u{2039}' // single left-pointing angle quotation mark (closes in German)
            | '\u{203A}' // single right-pointing angle quotation mark
            | '\u{3009}' // right angle bracket
            | '\u{300B}' // right double angle bracket
            | '\u{300D}' // right corner bracket
            | '\u{300F}' // right white corner bracket
            | '\u{FF09}' // fullwidth right parenthesis
            | '\u{FF3D}' // fullwidth right square bracket
    )
}

/// Char offsets at which a new sentence starts: after a run of
/// terminators and whatever closers and whitespace trail it, provided
/// the text goes on with something a sentence can start with. A comma
/// or a lowercase letter there means the terminator closed a quotation
/// inside a sentence that continues — `»Hilfe!«, hielt` or `»Komm!«
/// sagte er` — so no boundary is placed. Strictly increasing, never 0
/// and never the text's length.
pub fn sentence_boundaries(text: &str) -> Vec<usize> {
    let mut boundaries = Vec::new();
    let mut after_terminator = false;
    for (i, c) in text.chars().enumerate() {
        if is_terminator(c) {
            after_terminator = true;
        } else if after_terminator {
            if is_closer(c) || c.is_whitespace() {
                continue;
            }
            if c != ',' && !c.is_lowercase() {
                boundaries.push(i);
            }
            after_terminator = false;
        }
    }
    boundaries
}

/// Cut `text` into half-open char ranges that chain from 0 to its
/// length. A text within `max_chars` is one range. Past the threshold,
/// each range ends at the last sentence boundary within the threshold
/// from its start; when no boundary lies within it, at the first one
/// beyond, so a long sentence stays whole; with no boundary left, the
/// remainder is one range. Empty text yields no ranges.
pub fn split_at_sentences(text: &str, max_chars: usize) -> Vec<(usize, usize)> {
    let len = text.chars().count();
    if len == 0 {
        return Vec::new();
    }
    let max_chars = max_chars.max(1);
    let boundaries = sentence_boundaries(text);
    let mut ranges = Vec::new();
    let mut start = 0;
    while len - start > max_chars {
        let window_end = start + max_chars;
        let within = boundaries
            .iter()
            .copied()
            .rfind(|&b| b > start && b <= window_end);
        let cut = within.or_else(|| boundaries.iter().copied().find(|&b| b > window_end));
        match cut {
            Some(end) => {
                ranges.push((start, end));
                start = end;
            }
            None => break,
        }
    }
    ranges.push((start, len));
    ranges
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

    #[test]
    fn the_unambiguous_terminators_are_the_set_without_stop_and_semicolons() {
        let unambiguous: Vec<char> = ('\0'..=char::MAX)
            .filter(|c| is_unambiguous_terminator(*c))
            .collect();
        assert_eq!(
            unambiguous,
            ['!', '?', '\u{2026}', '\u{3002}', '\u{FF01}', '\u{FF1F}']
        );
        let terminators = ('\0'..=char::MAX).filter(|c| is_terminator(*c)).count();
        assert_eq!(terminators, 9);
    }

    #[test]
    fn boundaries_follow_terminator_runs_closers_and_whitespace() {
        assert_eq!(sentence_boundaries("One. Two! Three?"), vec![5, 10]);
        // An ellipsis followed by a lowercase word trails off inside the
        // sentence; only the `?!` run ends it.
        assert_eq!(sentence_boundaries("Wait... what?! Yes."), vec![15]);
        assert_eq!(sentence_boundaries("Wait... What?! Yes."), vec![8, 15]);
        assert_eq!(sentence_boundaries("He said \"go.\" She went."), vec![14]);
        assert_eq!(
            sentence_boundaries("\u{4ed6}\u{8bf4}\u{3002}\u{300d}\u{5979}\u{8d70}\u{4e86}\u{3002}"),
            vec![4]
        );
        assert_eq!(
            sentence_boundaries("no terminator here"),
            Vec::<usize>::new()
        );
        assert_eq!(sentence_boundaries("ends here."), Vec::<usize>::new());
    }

    /// A closing quotation mark of any convention stays with the sentence
    /// it closes: German and Swiss typography close with `\u{ab}`,
    /// `\u{201c}`, `\u{2018}` and `\u{2039}`, which open a quotation in
    /// English or French. A terminator that only closes a quotation
    /// inside a running sentence — followed by a comma or a lowercase
    /// word — is no boundary either.
    #[test]
    fn closing_quotation_marks_of_every_convention_stay_with_their_sentence() {
        assert_eq!(
            sentence_boundaries("\u{bb}Komm!\u{ab} sagte er. Dann ging er."),
            vec![18]
        );
        assert_eq!(
            sentence_boundaries("He asked \"why?\", then left. Later."),
            vec![28]
        );
        assert_eq!(
            sentence_boundaries("\u{bb}Hilfe!\u{ab}, hielt sie"),
            Vec::<usize>::new()
        );
        assert_eq!(
            sentence_boundaries("\u{bb}Ja.\u{ab} Dann ging er."),
            vec![6]
        );
        assert_eq!(
            sentence_boundaries("\u{201e}Nein!\u{201c} sagte er."),
            Vec::<usize>::new()
        );
        assert_eq!(
            sentence_boundaries("\u{201a}Nein!\u{2018} sagte er."),
            Vec::<usize>::new()
        );
        assert_eq!(
            sentence_boundaries("\u{203a}Nein!\u{2039} sagte er."),
            Vec::<usize>::new()
        );
        assert_eq!(
            sentence_boundaries("\u{300a}\u{4e66}\u{3002}\u{300b}\u{5979}\u{8d70}\u{4e86}\u{3002}"),
            vec![4]
        );
    }

    fn chained(ranges: &[(usize, usize)], len: usize) -> bool {
        ranges.first().is_some_and(|r| r.0 == 0)
            && ranges.last().is_some_and(|r| r.1 == len)
            && ranges.windows(2).all(|w| w[0].1 == w[1].0)
            && ranges.iter().all(|r| r.0 < r.1)
    }

    #[test]
    fn a_text_within_the_threshold_is_one_range_and_empty_text_is_none() {
        assert_eq!(split_at_sentences("One. Two.", 100), vec![(0, 9)]);
        assert_eq!(split_at_sentences("", 100), Vec::<(usize, usize)>::new());
    }

    #[test]
    fn a_long_text_is_cut_at_the_last_boundary_within_the_threshold() {
        let text = "One two. Three four. Five six. Seven.";
        assert_eq!(sentence_boundaries(text), vec![9, 21, 31]);
        let ranges = split_at_sentences(text, 20);
        assert_eq!(ranges, vec![(0, 9), (9, 21), (21, 37)]);
        assert!(chained(&ranges, text.chars().count()));
        let ranges = split_at_sentences(text, 25);
        assert_eq!(
            ranges,
            vec![(0, 21), (21, 37)],
            "two boundaries inside the window: the last one is the cut"
        );
    }

    #[test]
    fn a_sentence_longer_than_the_threshold_is_kept_whole() {
        let text = "Short. A very long sentence that runs past the threshold without a stop. End.";
        let ranges = split_at_sentences(text, 10);
        assert_eq!(ranges, vec![(0, 7), (7, 73), (73, 77)]);
        assert!(chained(&ranges, text.chars().count()));

        let single = "One long sentence with no terminator at all";
        assert_eq!(
            split_at_sentences(single, 5),
            vec![(0, single.chars().count())]
        );
    }

    #[test]
    fn cjk_text_is_cut_at_fullwidth_terminators_by_char_not_byte() {
        let text =
            "\u{4e00}\u{4e8c}\u{4e09}\u{3002}\u{56db}\u{4e94}\u{516d}\u{ff01}\u{4e03}\u{516b}";
        let ranges = split_at_sentences(text, 5);
        assert_eq!(ranges, vec![(0, 4), (4, 8), (8, 10)]);
        assert!(chained(&ranges, 10));
    }

    #[test]
    fn every_split_chains_from_zero_to_the_length() {
        let samples = [
            "A. B. C. D. E. F.",
            "Alpha beta gamma. Delta? Epsilon! Zeta... Eta.",
            "\u{4ed6}\u{8bf4}\u{3002}\u{5979}\u{8d70}\u{4e86}\u{ff01}\u{4ed6}\u{4e5f}\u{8d70}\u{4e86}\u{3002}",
            "No stops anywhere in this one at all",
            "x",
        ];
        for text in samples {
            for max in [1, 3, 7, 20, 1000] {
                let ranges = split_at_sentences(text, max);
                assert!(
                    chained(&ranges, text.chars().count()),
                    "{text:?} @ {max}: {ranges:?}"
                );
            }
        }
    }
}
