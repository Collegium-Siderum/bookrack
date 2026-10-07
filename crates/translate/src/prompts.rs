// SPDX-License-Identifier: Apache-2.0

//! The prompt skeletons shipped with the translation store.
//!
//! Four translator skeletons, one per task mode and glossary
//! scaffolding, and the dispatcher brief that drives the book loop.
//! They are English on purpose: the repository is English-only, and a
//! translation prompt is written in its target language, so the
//! dispatcher's first task for a language is to render the skeletons
//! into it under the library's data root. Each translator skeleton
//! carries the ten rules of the prompt contract as fixed headings, and
//! the tests here pin them.

/// Translator skeleton: a segment with no translation yet, with a
/// glossary block.
pub const TRANSLATOR_DRAFT_WITH_GLOSSARY: &str =
    include_str!("../data/prompts/translator-draft-with_glossary.md");
/// Translator skeleton: a segment with no translation yet, no glossary
/// hits to inject.
pub const TRANSLATOR_DRAFT_CLEAN: &str = include_str!("../data/prompts/translator-draft-clean.md");
/// Translator skeleton: a segment carrying an imported translation to
/// review, with a glossary block.
pub const TRANSLATOR_REVIEW_WITH_GLOSSARY: &str =
    include_str!("../data/prompts/translator-review-with_glossary.md");
/// Translator skeleton: a segment carrying an imported translation to
/// review, no glossary hits to inject.
pub const TRANSLATOR_REVIEW_CLEAN: &str =
    include_str!("../data/prompts/translator-review-clean.md");
/// The dispatcher brief: the book loop, the tiers, the budget brake.
pub const DISPATCHER: &str = include_str!("../data/prompts/dispatcher.md");

/// The headings every translator skeleton carries, in order. The text
/// after each is the rule's body; the heading is the contract.
pub const RULE_HEADINGS: [&str; 10] = [
    "## Rule 1: write in the target language",
    "## Rule 2: the glossary is advisory",
    "## Rule 3: build the chapter term table yourself",
    "## Rule 4: placeholders stand in for protected terms",
    "## Rule 5: three stages, always",
    "## Rule 6: annotate every term and name on every occurrence",
    "## Rule 7: the chapter ends with a review",
    "## Rule 8: long segments take several passes, dialogue may merge",
    "## Rule 9: attribution protocol and review-note shape",
    "## Rule 10: check redirect_loop before writing authority_ref",
];

/// Every placeholder a skeleton may carry, as `{{name}}`.
pub const PLACEHOLDERS: [&str; 8] = [
    "{{target_lang}}",
    "{{source_lang}}",
    "{{segment_package}}",
    "{{glossary_block}}",
    "{{chapter_terms}}",
    "{{library}}",
    "{{intake_id}}",
    "{{budget_per_chapter}}",
];

/// Placeholder the dispatcher fills once the first chapter has been
/// measured.
pub const BUDGET_PER_BOOK: &str = "{{budget_per_book}}";

/// Every shipped skeleton with its file name, the four translator
/// skeletons first.
pub fn skeletons() -> [(&'static str, &'static str); 5] {
    [
        (
            "translator-draft-with_glossary.md",
            TRANSLATOR_DRAFT_WITH_GLOSSARY,
        ),
        ("translator-draft-clean.md", TRANSLATOR_DRAFT_CLEAN),
        (
            "translator-review-with_glossary.md",
            TRANSLATOR_REVIEW_WITH_GLOSSARY,
        ),
        ("translator-review-clean.md", TRANSLATOR_REVIEW_CLEAN),
        ("dispatcher.md", DISPATCHER),
    ]
}

/// The translator skeleton for a task mode and prompt kind, as
/// `translate.fetch_segment` reports them, or `None` for an unknown
/// pair.
pub fn translator_skeleton(task_mode: &str, prompt_kind: &str) -> Option<&'static str> {
    match (task_mode, prompt_kind) {
        ("draft", "with_glossary") => Some(TRANSLATOR_DRAFT_WITH_GLOSSARY),
        ("draft", "clean") => Some(TRANSLATOR_DRAFT_CLEAN),
        ("review", "with_glossary") => Some(TRANSLATOR_REVIEW_WITH_GLOSSARY),
        ("review", "clean") => Some(TRANSLATOR_REVIEW_CLEAN),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placeholders_in(text: &str) -> Vec<&str> {
        let mut found = Vec::new();
        let mut rest = text;
        while let Some(start) = rest.find("{{") {
            let after = &rest[start..];
            let Some(end) = after.find("}}") else { break };
            found.push(&after[..end + 2]);
            rest = &after[end + 2..];
        }
        found
    }

    #[test]
    fn every_translator_skeleton_carries_the_ten_rule_headings_in_order() {
        for (name, text) in skeletons().iter().take(4) {
            let mut at = 0;
            for heading in RULE_HEADINGS {
                let pos = text[at..]
                    .find(heading)
                    .unwrap_or_else(|| panic!("{name} lacks or misorders {heading:?}"));
                at += pos + heading.len();
            }
            assert_eq!(
                text.matches("\n## Rule ").count(),
                RULE_HEADINGS.len(),
                "{name} carries a rule heading outside the ten"
            );
        }
        assert!(
            !DISPATCHER.lines().any(|line| line.starts_with("## Rule ")),
            "the dispatcher is not a translator skeleton"
        );
    }

    #[test]
    fn skeletons_use_only_known_placeholders_and_the_glossary_block_only_where_it_applies() {
        for (name, text) in skeletons() {
            for placeholder in placeholders_in(text) {
                assert!(
                    PLACEHOLDERS.contains(&placeholder) || placeholder == BUDGET_PER_BOOK,
                    "{name} carries unknown placeholder {placeholder}"
                );
            }
            let has_glossary = text.contains("{{glossary_block}}");
            assert_eq!(
                has_glossary,
                name.ends_with("with_glossary.md"),
                "{name}: glossary block presence must follow the file name"
            );
            if name.starts_with("translator-") {
                for required in [
                    "{{segment_package}}",
                    "{{target_lang}}",
                    "{{chapter_terms}}",
                ] {
                    assert!(text.contains(required), "{name} lacks {required}");
                }
            }
        }
    }

    #[test]
    fn the_dispatcher_names_the_loop_condition_and_the_budget() {
        for required in [
            "translate.list_pending",
            "translate.plan",
            "cost_tokens",
            "{{budget_per_chapter}}",
            BUDGET_PER_BOOK,
            "actor_kind_override",
            "<data_root>/translate/prompts/{{target_lang}}/",
        ] {
            assert!(DISPATCHER.contains(required), "dispatcher lacks {required}");
        }
    }

    #[test]
    fn skeletons_are_english_only_and_the_review_rule_carries_the_note_shape() {
        for (name, text) in skeletons() {
            let offending: Vec<char> = text.chars().filter(|c| (*c as u32) >= 0x2E80).collect();
            assert!(
                offending.is_empty(),
                "{name} carries non-Latin script: {offending:?}"
            );
        }
        for text in [TRANSLATOR_REVIEW_WITH_GLOSSARY, TRANSLATOR_REVIEW_CLEAN] {
            for kind in [
                "mistranslation",
                "translator_choice",
                "source_damage",
                "omission",
                "structure_defect",
                "term_inconsistency",
                "pass",
            ] {
                assert!(
                    text.contains(kind),
                    "review skeleton lacks review kind {kind}"
                );
            }
            assert!(text.contains("intra_lingual"));
        }
        assert_eq!(
            translator_skeleton("review", "clean"),
            Some(TRANSLATOR_REVIEW_CLEAN)
        );
        assert_eq!(translator_skeleton("draft", "verbose"), None);
    }
}
