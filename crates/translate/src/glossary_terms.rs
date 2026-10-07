// SPDX-License-Identifier: Apache-2.0

//! The `glossary_terms` table — the concept layer of the glossary.
//!
//! One row states "this source-language term is a concept worth
//! tracking", scoped to the whole library, to one book, or to a
//! reference authority. The candidate renderings live in
//! `glossary_translations`; `primary_choice_id` names the currently
//! preferred one and may be re-pointed or cleared at any time, so
//! competing renderings coexist long-term.

use bookrack_dbkit::{ColumnSpec, TableSpec};
use rusqlite::OptionalExtension;

use crate::{Translate, TranslateError, TranslateResult};

/// Every `scope` a term may carry.
pub const SCOPES: &[&str] = &["authority", "library", "book"];
/// Every `term_kind` a term may carry.
pub const TERM_KINDS: &[&str] = &[
    "term",
    "proper_noun",
    "do_not_translate",
    "common_knowledge",
];

/// One term to insert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTerm<'a> {
    pub scope: &'a str,
    pub scope_ref: Option<&'a str>,
    pub source_lang: &'a str,
    pub source_term: &'a str,
    pub term_kind: &'a str,
}

/// The single source of truth for the `glossary_terms` table's schema.
/// The frozen baseline DDL in [`crate::migrate`] is rendered from this
/// spec; `verify_all` pins the two together on every open.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "glossary_terms",
    comment: Some("Glossary concept layer: one row per tracked source term."),
    columns: &[
        ColumnSpec::int("term_id").primary_key(),
        ColumnSpec::text("scope")
            .not_null()
            .check("scope IN ('authority', 'library', 'book')"),
        ColumnSpec::text("scope_ref")
            .comment("book: intake id; authority: refs book slug; library: NULL"),
        ColumnSpec::text("source_lang").not_null(),
        ColumnSpec::text("source_term").not_null(),
        ColumnSpec::text("source_norm").not_null(),
        ColumnSpec::text("term_kind")
            .not_null()
            .check("term_kind IN ('term', 'proper_noun', 'do_not_translate', 'common_knowledge')"),
        ColumnSpec::int("primary_choice_id")
            .comment("glossary_translations id; no FK, the write path validates"),
    ],
    composite_pk: None,
    uniques: &[&["source_lang", "source_norm", "scope", "scope_ref"]],
    table_checks: &[],
    indexes: &[],
};

/// One `glossary_terms` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermRow {
    pub term_id: i64,
    pub scope: String,
    pub scope_ref: Option<String>,
    pub source_lang: String,
    pub source_term: String,
    pub source_norm: String,
    pub term_kind: String,
    pub primary_choice_id: Option<i64>,
}

/// A glossary term found in a segment's source text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermMatch {
    pub term: TermRow,
    /// Half-open char range of the first occurrence in the source text.
    pub span_in_source: (usize, usize),
}

/// A char that continues a word for boundary purposes: alphanumeric
/// and outside the CJK blocks, which have no word boundaries to
/// respect. Accented Latin letters count as word chars, so a term
/// does not match inside a word that merely starts or ends with one.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() && (c as u32) < 0x2E80
}

/// Case-fold one char to a single char so a folded text keeps a 1:1
/// index mapping with the original. The few chars whose lower-case
/// form is longer than one char fold to its first char.
pub(crate) fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// The identity key of a source term: trimmed, inner whitespace runs
/// collapsed to one space, every char folded the way the matcher folds
/// text. A term therefore always matches its own key.
pub fn source_norm(source_term: &str) -> String {
    let mut out = String::with_capacity(source_term.len());
    let mut pending_space = false;
    for c in source_term.trim().chars() {
        if c.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(fold(c));
    }
    out
}

/// First occurrence of `term` in `text` as a half-open char range, or
/// `None`. Both are compared case-folded; where the term's edge char is
/// a word char, the adjacent text char must not be one.
pub(crate) fn find_term(text: &[char], term: &[char]) -> Option<(usize, usize)> {
    if term.is_empty() || term.len() > text.len() {
        return None;
    }
    let edge_start = is_word_char(term[0]);
    let edge_end = is_word_char(term[term.len() - 1]);
    (0..=text.len() - term.len()).find_map(|at| {
        let end = at + term.len();
        if text[at..end] != *term {
            return None;
        }
        if edge_start && at > 0 && is_word_char(text[at - 1]) {
            return None;
        }
        if edge_end && end < text.len() && is_word_char(text[end]) {
            return None;
        }
        Some((at, end))
    })
}

impl Translate {
    /// Scan `source_text` for glossary terms visible to `intake_id`
    /// and return first-occurrence matches, ordered by position.
    ///
    /// Visible terms are the book's own (`scope = 'book'` with the
    /// intake id as `scope_ref`), the library's, and every authority's.
    /// Terms sharing a `source_norm` collapse to the most specific
    /// scope, book over library over authority. Matching is a
    /// case-folded char-level substring scan; where a term's edge char
    /// is alphanumeric and not CJK, the adjacent text char must not be
    /// one either, so `art` does not hit inside `particular` and a term
    /// does not hit inside an accented word, while CJK neighbours never
    /// block a hit. The source language is not filtered on.
    pub fn match_terms(
        &self,
        intake_id: i64,
        source_text: &str,
    ) -> TranslateResult<Vec<TermMatch>> {
        let mut stmt = self.conn.prepare(
            "SELECT term_id, scope, scope_ref, source_lang, source_term, source_norm, \
                    term_kind, primary_choice_id \
             FROM glossary_terms \
             WHERE (scope = 'book' AND scope_ref = ?1) OR scope IN ('library', 'authority') \
             ORDER BY CASE scope WHEN 'book' THEN 0 WHEN 'library' THEN 1 ELSE 2 END, term_id",
        )?;
        let rows = stmt.query_map([intake_id.to_string()], |row| {
            Ok(TermRow {
                term_id: row.get(0)?,
                scope: row.get(1)?,
                scope_ref: row.get(2)?,
                source_lang: row.get(3)?,
                source_term: row.get(4)?,
                source_norm: row.get(5)?,
                term_kind: row.get(6)?,
                primary_choice_id: row.get(7)?,
            })
        })?;

        let text: Vec<char> = source_text.chars().map(fold).collect();
        let mut seen = std::collections::HashSet::new();
        let mut matches = Vec::new();
        for row in rows {
            let term = row?;
            if !seen.insert(term.source_norm.clone()) {
                continue;
            }
            let needle: Vec<char> = term.source_term.chars().map(fold).collect();
            if let Some(span_in_source) = find_term(&text, &needle) {
                matches.push(TermMatch {
                    term,
                    span_in_source,
                });
            }
        }
        matches.sort_by_key(|m| (m.span_in_source.0, m.term.term_id));
        Ok(matches)
    }
}

impl Translate {
    /// The term with `term_id`, or `None`.
    pub fn term(&self, term_id: i64) -> TranslateResult<Option<TermRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT term_id, scope, scope_ref, source_lang, source_term, source_norm, \
                 term_kind, primary_choice_id FROM glossary_terms WHERE term_id = ?1",
                [term_id],
                read_term,
            )
            .optional()?)
    }

    /// Insert a term. `Ok(Ok(id))` when inserted; `Ok(Err(existing))`
    /// when a term with the same `(source_lang, source_norm, scope,
    /// scope_ref)` is already there, naming it. The key is compared
    /// with `IS`, so two library-scoped terms (whose `scope_ref` is
    /// NULL) collide the way the schema intends. Scope and kind must
    /// come from [`SCOPES`] and [`TERM_KINDS`].
    pub fn insert_term(&self, t: &NewTerm<'_>) -> TranslateResult<Result<i64, i64>> {
        if !SCOPES.contains(&t.scope) {
            return Err(TranslateError::UnknownValue {
                what: "scope",
                value: t.scope.to_owned(),
                known: SCOPES,
            });
        }
        if !TERM_KINDS.contains(&t.term_kind) {
            return Err(TranslateError::UnknownValue {
                what: "term_kind",
                value: t.term_kind.to_owned(),
                known: TERM_KINDS,
            });
        }
        let norm = source_norm(t.source_term);
        // The key is checked here rather than left to the UNIQUE index:
        // `scope_ref` is NULL for library-scoped terms, and SQLite treats
        // NULLs in a unique index as distinct from one another.
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT term_id FROM glossary_terms WHERE source_lang = ?1 AND source_norm = ?2 \
                 AND scope = ?3 AND scope_ref IS ?4",
                rusqlite::params![t.source_lang, norm, t.scope, t.scope_ref],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok(Err(id));
        }
        let id: i64 = self.conn.query_row(
            "INSERT INTO glossary_terms (scope, scope_ref, source_lang, source_term, \
             source_norm, term_kind) VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING term_id",
            rusqlite::params![
                t.scope,
                t.scope_ref,
                t.source_lang,
                t.source_term,
                norm,
                t.term_kind
            ],
            |row| row.get(0),
        )?;
        Ok(Ok(id))
    }
}

fn read_term(row: &rusqlite::Row<'_>) -> rusqlite::Result<TermRow> {
    Ok(TermRow {
        term_id: row.get(0)?,
        scope: row.get(1)?,
        scope_ref: row.get(2)?,
        source_lang: row.get(3)?,
        source_term: row.get(4)?,
        source_norm: row.get(5)?,
        term_kind: row.get(6)?,
        primary_choice_id: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seed;

    fn span(text: &str, term: &str) -> Option<(usize, usize)> {
        let text: Vec<char> = text.chars().map(fold).collect();
        let term: Vec<char> = term.chars().map(fold).collect();
        find_term(&text, &term)
    }

    #[test]
    fn a_word_edge_does_not_match_inside_a_longer_word() {
        assert_eq!(span("a particular case", "art"), None);
        assert_eq!(span("the art of war", "art"), Some((4, 7)));
        assert_eq!(span("the objet a is", "objet a"), Some((4, 11)));
    }

    #[test]
    fn accented_latin_neighbours_block_a_hit() {
        assert_eq!(span("Lacan's \u{c9}crits", "crits"), None);
        assert_eq!(span("das \u{fc}ber-Ich", "ber"), None);
        assert_eq!(span("\u{c9}crits", "\u{e9}crits"), Some((0, 6)));
    }

    #[test]
    fn matching_folds_case_including_accented_letters() {
        assert_eq!(span("DASEIN", "Dasein"), Some((0, 6)));
        assert_eq!(span("\u{c9}CRITS", "\u{e9}crits"), Some((0, 6)));
    }

    #[test]
    fn spans_count_chars_not_bytes_and_report_the_first_occurrence() {
        // Two multi-byte chars precede the first hit.
        assert_eq!(span("\u{e9}\u{e9} art art", "art"), Some((3, 6)));
    }

    #[test]
    fn cjk_neighbours_never_block_a_hit() {
        // Term: two ideographs; text: the same two inside a run of ideographs.
        let text = "\u{4e3b}\u{4f53}\u{6027}\u{7684}";
        assert_eq!(span(text, "\u{4f53}\u{6027}"), Some((1, 3)));
        // A Latin term directly against ideographs still hits.
        assert_eq!(span("\u{4e3b}Dasein\u{7684}", "Dasein"), Some((1, 7)));
    }

    #[test]
    fn an_empty_term_never_matches() {
        assert_eq!(span("anything", ""), None);
    }

    #[test]
    fn visible_terms_are_the_books_own_the_librarys_and_every_authoritys() {
        let t = seed::fresh();
        let mine = seed::term(&t, "book", Some("1"), "de", "Dasein", "dasein", "term");
        seed::term(&t, "book", Some("2"), "de", "Sorge", "sorge", "term");
        let lib = seed::term(&t, "library", None, "de", "Angst", "angst", "term");
        let auth = seed::term(
            &t,
            "authority",
            Some("lexicon"),
            "de",
            "Welt",
            "welt",
            "term",
        );

        let ids: Vec<i64> = t
            .match_terms(1, "Dasein, Sorge, Angst, Welt")
            .expect("match")
            .iter()
            .map(|m| m.term.term_id)
            .collect();
        assert_eq!(ids, vec![mine, lib, auth]);
    }

    #[test]
    fn terms_sharing_a_norm_collapse_to_the_most_specific_scope() {
        let t = seed::fresh();
        seed::term(
            &t,
            "authority",
            Some("lexicon"),
            "de",
            "Dasein",
            "dasein",
            "term",
        );
        seed::term(&t, "library", None, "de", "Dasein", "dasein", "term");
        let book = seed::term(&t, "book", Some("1"), "de", "Dasein", "dasein", "term");

        let matches = t.match_terms(1, "Dasein").expect("match");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].term.term_id, book);
        assert_eq!(matches[0].term.scope, "book");
    }

    #[test]
    fn matches_come_back_in_text_order_with_every_column() {
        let t = seed::fresh();
        let later = seed::term(&t, "library", None, "de", "Sorge", "sorge", "proper_noun");
        let earlier = seed::term(&t, "book", Some("1"), "de", "Dasein", "dasein", "term");
        seed::set_primary(&t, earlier, 77);

        let matches = t.match_terms(1, "Dasein und Sorge").expect("match");
        assert_eq!(
            matches,
            vec![
                TermMatch {
                    term: TermRow {
                        term_id: earlier,
                        scope: "book".into(),
                        scope_ref: Some("1".into()),
                        source_lang: "de".into(),
                        source_term: "Dasein".into(),
                        source_norm: "dasein".into(),
                        term_kind: "term".into(),
                        primary_choice_id: Some(77),
                    },
                    span_in_source: (0, 6),
                },
                TermMatch {
                    term: TermRow {
                        term_id: later,
                        scope: "library".into(),
                        scope_ref: None,
                        source_lang: "de".into(),
                        source_term: "Sorge".into(),
                        source_norm: "sorge".into(),
                        term_kind: "proper_noun".into(),
                        primary_choice_id: None,
                    },
                    span_in_source: (11, 16),
                },
            ]
        );
    }

    #[test]
    fn source_norm_trims_collapses_whitespace_and_folds_case() {
        assert_eq!(source_norm("  Objet   petit\ta "), "objet petit a");
        assert_eq!(source_norm("Dasein"), "dasein");
        assert_eq!(source_norm("\u{c9}crits"), "\u{e9}crits");
    }

    #[test]
    fn a_created_term_matches_itself_in_a_text() {
        let t = seed::fresh();
        let id = t
            .insert_term(&NewTerm {
                scope: "book",
                scope_ref: Some("1"),
                source_lang: "de",
                source_term: "Das Ding",
                term_kind: "term",
            })
            .expect("insert")
            .expect("new");
        let row = t.term(id).expect("read").expect("row");
        assert_eq!(
            (row.source_term.as_str(), row.source_norm.as_str()),
            ("Das Ding", "das ding")
        );
        let matches = t
            .match_terms(1, "Hier ist das Ding selbst.")
            .expect("match");
        assert_eq!(
            matches.iter().map(|m| m.term.term_id).collect::<Vec<_>>(),
            vec![id]
        );
    }

    #[test]
    fn inserting_a_term_on_an_existing_key_names_the_existing_row_instead_of_failing() {
        let t = seed::fresh();
        let first = NewTerm {
            scope: "library",
            scope_ref: None,
            source_lang: "de",
            source_term: "Traum",
            term_kind: "term",
        };
        let id = t.insert_term(&first).expect("insert").expect("new");
        let again = t
            .insert_term(&NewTerm {
                source_term: " TRAUM ",
                term_kind: "proper_noun",
                ..first
            })
            .expect("repeat");
        assert_eq!(again, Err(id), "same key folds to the same row");
        let other_scope = t
            .insert_term(&NewTerm {
                scope: "book",
                scope_ref: Some("1"),
                ..first
            })
            .expect("other scope")
            .expect("new");
        assert_ne!(other_scope, id);
    }

    #[test]
    fn scope_and_kind_outside_the_vocabularies_are_refused() {
        let t = seed::fresh();
        let good = NewTerm {
            scope: "library",
            scope_ref: None,
            source_lang: "de",
            source_term: "Traum",
            term_kind: "term",
        };
        let err = t
            .insert_term(&NewTerm {
                scope: "planet",
                ..good
            })
            .expect_err("scope");
        assert!(
            matches!(err, TranslateError::UnknownValue { what: "scope", .. }),
            "{err:?}"
        );
        let err = t
            .insert_term(&NewTerm {
                term_kind: "verb",
                ..good
            })
            .expect_err("kind");
        assert!(
            matches!(
                err,
                TranslateError::UnknownValue {
                    what: "term_kind",
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(
            t.term(1).expect("read"),
            None,
            "a refused insert leaves no row"
        );
    }
}
