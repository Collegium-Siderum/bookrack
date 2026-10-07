// SPDX-License-Identifier: Apache-2.0

//! Injection profiles and glossary-hit assembly.
//!
//! A unit carries an injection profile naming how much of the glossary
//! a translation prompt should see; the matrix here maps each profile
//! and term kind to an [`InjectionMode`]. Hit assembly scans a
//! segment's source text for glossary terms, resolves each hit's mode
//! under the effective profile, attaches the candidate renderings for
//! the target language, and drops hits whose mode is [`InjectionMode::None`]
//! so they cost no prompt tokens.

use crate::glossary_terms::{TermMatch, TermRow};
use crate::glossary_translations::TranslationRow;
use crate::{Translate, TranslateError, TranslateResult};

/// The profile names this build knows. New profiles arrive through
/// the CLI in a later milestone; until then the set is closed.
pub const PROFILES: [&str; 3] = ["prose", "default", "academic"];

/// How one glossary term reaches a translation prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionMode {
    /// Not injected at all; the hit is dropped from the package.
    None,
    /// Replaced by a placeholder before translation and restored after.
    Placeholder,
    /// Listed once in a chapter-head table; not injected per segment.
    ChapterHead,
    /// The primary rendering and its explanation, per segment.
    PrimaryOnly,
    /// The primary rendering plus the alternatives, per segment.
    PrimaryPlusAlts,
}

impl InjectionMode {
    /// Whether the hit goes into the per-segment prompt. A chapter-head
    /// hit stays in the package for the chapter table but does not
    /// decide the prompt kind.
    pub fn injects_per_segment(self) -> bool {
        matches!(
            self,
            InjectionMode::Placeholder
                | InjectionMode::PrimaryOnly
                | InjectionMode::PrimaryPlusAlts
        )
    }

    /// The wire spelling, `snake_case`.
    pub fn as_str(self) -> &'static str {
        match self {
            InjectionMode::None => "none",
            InjectionMode::Placeholder => "placeholder",
            InjectionMode::ChapterHead => "chapter_head",
            InjectionMode::PrimaryOnly => "primary_only",
            InjectionMode::PrimaryPlusAlts => "primary_plus_alts",
        }
    }
}

/// Fail on a profile name outside [`PROFILES`].
pub fn ensure_known_profile(profile: &str) -> TranslateResult<()> {
    if PROFILES.contains(&profile) {
        Ok(())
    } else {
        Err(TranslateError::UnknownProfile {
            name: profile.to_owned(),
        })
    }
}

/// Resolve the injection mode for one term kind under one profile.
///
/// Proper nouns are all treated as chapter-head material: the store
/// records no frequency, so the high/low split is folded into one
/// row. A term kind outside the store's closed set resolves to
/// [`InjectionMode::None`].
pub fn mode_for(profile: &str, term_kind: &str) -> TranslateResult<InjectionMode> {
    ensure_known_profile(profile)?;
    Ok(match (term_kind, profile) {
        ("do_not_translate", "prose") => InjectionMode::Placeholder,
        ("do_not_translate", _) => InjectionMode::PrimaryPlusAlts,
        ("proper_noun", _) => InjectionMode::ChapterHead,
        ("term", "academic") => InjectionMode::PrimaryPlusAlts,
        _ => InjectionMode::None,
    })
}

/// One glossary hit in a segment's source text, with its renderings
/// for the target language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlossaryHitRow {
    pub term: TermRow,
    /// Half-open char range of the first occurrence in the source text.
    pub span_in_source: (usize, usize),
    pub mode: InjectionMode,
    /// The rendering `primary_choice_id` names, when it is active or
    /// a candidate for this target language.
    pub primary: Option<TranslationRow>,
    /// The remaining active and candidate renderings, active first,
    /// then by id.
    pub alternatives: Vec<TranslationRow>,
}

impl Translate {
    /// Glossary hits for `source_text` as a prompt under `profile`
    /// would see them: terms visible to `intake_id` matched by
    /// [`Translate::match_terms`], each resolved to its mode and joined
    /// to the active and candidate renderings into `target_lang`. Hits
    /// whose mode is [`InjectionMode::None`] are dropped.
    pub fn glossary_hits(
        &self,
        intake_id: i64,
        target_lang: &str,
        profile: &str,
        source_text: &str,
    ) -> TranslateResult<Vec<GlossaryHitRow>> {
        ensure_known_profile(profile)?;
        let mut hits = Vec::new();
        for TermMatch {
            term,
            span_in_source,
        } in self.match_terms(intake_id, source_text)?
        {
            let mode = mode_for(profile, &term.term_kind)?;
            if mode == InjectionMode::None {
                continue;
            }
            let mut renderings = self.renderings_for_term(term.term_id, target_lang)?;
            let primary = term
                .primary_choice_id
                .and_then(|id| renderings.iter().position(|r| r.translation_id == id))
                .map(|at| renderings.remove(at));
            hits.push(GlossaryHitRow {
                term,
                span_in_source,
                mode,
                primary,
                alternatives: renderings,
            });
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seed;

    #[test]
    fn the_matrix_covers_every_profile_and_term_kind() {
        let expect = |profile: &str, kind: &str, mode: InjectionMode| {
            assert_eq!(
                mode_for(profile, kind).expect("known"),
                mode,
                "{profile}/{kind}"
            );
        };
        expect("prose", "do_not_translate", InjectionMode::Placeholder);
        expect("prose", "proper_noun", InjectionMode::ChapterHead);
        expect("prose", "term", InjectionMode::None);
        expect("prose", "common_knowledge", InjectionMode::None);
        expect(
            "default",
            "do_not_translate",
            InjectionMode::PrimaryPlusAlts,
        );
        expect("default", "proper_noun", InjectionMode::ChapterHead);
        expect("default", "term", InjectionMode::None);
        expect("default", "common_knowledge", InjectionMode::None);
        expect(
            "academic",
            "do_not_translate",
            InjectionMode::PrimaryPlusAlts,
        );
        expect("academic", "proper_noun", InjectionMode::ChapterHead);
        expect("academic", "term", InjectionMode::PrimaryPlusAlts);
        expect("academic", "common_knowledge", InjectionMode::None);
    }

    #[test]
    fn an_unknown_profile_is_an_error_even_with_nothing_to_match() {
        let err = mode_for("verbose", "term").expect_err("unknown profile");
        assert!(
            matches!(&err, TranslateError::UnknownProfile { name } if name == "verbose"),
            "{err:?}"
        );
        let t = seed::fresh();
        let err = t
            .glossary_hits(1, "zh", "verbose", "nothing here")
            .expect_err("unknown profile");
        assert!(
            matches!(err, TranslateError::UnknownProfile { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn only_per_segment_modes_decide_the_prompt_kind() {
        assert!(InjectionMode::Placeholder.injects_per_segment());
        assert!(InjectionMode::PrimaryOnly.injects_per_segment());
        assert!(InjectionMode::PrimaryPlusAlts.injects_per_segment());
        assert!(!InjectionMode::ChapterHead.injects_per_segment());
        assert!(!InjectionMode::None.injects_per_segment());
    }

    #[test]
    fn a_hit_carries_the_primary_apart_from_the_alternatives_in_status_then_id_order() {
        let t = seed::fresh();
        let term = seed::term(&t, "library", None, "de", "Dasein", "dasein", "term");
        let candidate_first = seed::translation(&t, term, "zh", Some("ci zai"), "candidate");
        let active_later = seed::translation(&t, term, "zh", Some("yuan zai"), "active");
        let active_last = seed::translation(&t, term, "zh", Some("qin zai"), "active");
        seed::set_primary(&t, term, active_later);

        let hits = t
            .glossary_hits(1, "zh", "academic", "Das Dasein ist.")
            .expect("hits");
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.term.term_id, term);
        assert_eq!(hit.span_in_source, (4, 10));
        assert_eq!(hit.mode, InjectionMode::PrimaryPlusAlts);
        assert_eq!(
            hit.primary.as_ref().map(|r| r.translation_id),
            Some(active_later)
        );
        let alternatives: Vec<i64> = hit.alternatives.iter().map(|r| r.translation_id).collect();
        assert_eq!(alternatives, vec![active_last, candidate_first]);
    }

    #[test]
    fn retired_rejected_and_other_language_renderings_stay_out_of_the_package() {
        let t = seed::fresh();
        let term = seed::term(&t, "library", None, "de", "Dasein", "dasein", "term");
        seed::translation(&t, term, "zh", Some("old"), "retired");
        seed::translation(&t, term, "zh", Some("no"), "rejected");
        seed::translation(&t, term, "ja", Some("other"), "active");
        let kept = seed::translation(&t, term, "zh", Some("ci zai"), "active");

        let hits = t
            .glossary_hits(1, "zh", "academic", "Dasein")
            .expect("hits");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].primary, None);
        let ids: Vec<i64> = hits[0]
            .alternatives
            .iter()
            .map(|r| r.translation_id)
            .collect();
        assert_eq!(ids, vec![kept]);
    }

    #[test]
    fn a_primary_pointing_at_a_candidate_rendering_is_still_the_primary() {
        let t = seed::fresh();
        let term = seed::term(&t, "library", None, "de", "Dasein", "dasein", "term");
        let candidate = seed::translation(&t, term, "zh", Some("ci zai"), "candidate");
        seed::set_primary(&t, term, candidate);

        let hits = t
            .glossary_hits(1, "zh", "academic", "Dasein")
            .expect("hits");
        assert_eq!(
            hits[0].primary.as_ref().map(|r| r.translation_id),
            Some(candidate)
        );
        assert!(hits[0].alternatives.is_empty());
    }

    #[test]
    fn a_primary_that_is_retired_is_not_the_primary() {
        let t = seed::fresh();
        let term = seed::term(&t, "library", None, "de", "Dasein", "dasein", "term");
        let retired = seed::translation(&t, term, "zh", Some("old"), "retired");
        seed::set_primary(&t, term, retired);
        let hits = t
            .glossary_hits(1, "zh", "academic", "Dasein")
            .expect("hits");
        assert_eq!(hits[0].primary, None);
    }

    #[test]
    fn a_hit_whose_mode_is_none_is_dropped_and_a_chapter_head_hit_is_kept() {
        let t = seed::fresh();
        seed::term(
            &t,
            "library",
            None,
            "en",
            "Paris",
            "paris",
            "common_knowledge",
        );
        let name = seed::term(&t, "library", None, "en", "Lacan", "lacan", "proper_noun");
        let concept = seed::term(
            &t,
            "library",
            None,
            "en",
            "jouissance",
            "jouissance",
            "term",
        );

        let hits = t
            .glossary_hits(1, "zh", "prose", "Lacan in Paris on jouissance")
            .expect("hits");
        let ids: Vec<(i64, InjectionMode)> =
            hits.iter().map(|h| (h.term.term_id, h.mode)).collect();
        assert_eq!(ids, vec![(name, InjectionMode::ChapterHead)]);

        let hits = t
            .glossary_hits(1, "zh", "academic", "Lacan in Paris on jouissance")
            .expect("hits");
        let ids: Vec<(i64, InjectionMode)> =
            hits.iter().map(|h| (h.term.term_id, h.mode)).collect();
        assert_eq!(
            ids,
            vec![
                (name, InjectionMode::ChapterHead),
                (concept, InjectionMode::PrimaryPlusAlts)
            ]
        );
    }
}
