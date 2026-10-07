// SPDX-License-Identifier: Apache-2.0

//! The `glossary_translations` table — candidate renderings.
//!
//! One row per proposed rendering of a term into one target language,
//! attributed to a faction or translator and optionally backed by a
//! reference-book entry through `authority_ref`. Candidates never
//! disappear: `status` moves between `candidate`, `active`, `retired`,
//! and `rejected`, so superseded renderings stay on record. A `NULL`
//! `target_term` records a do-not-translate verdict.

use bookrack_dbkit::{ColumnSpec, ForeignKey, IndexSpec, OnDelete, TableSpec};

use rusqlite::OptionalExtension;

use crate::{Translate, TranslateError, TranslateResult};

/// Every `status` a rendering may carry.
pub const TRANSLATION_STATUSES: &[&str] = &["candidate", "active", "retired", "rejected"];

/// One rendering to insert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTranslation<'a> {
    pub term_id: i64,
    pub target_lang: &'a str,
    /// `None` records a do-not-translate verdict.
    pub target_term: Option<&'a str>,
    pub faction: Option<&'a str>,
    pub translator: Option<&'a str>,
    pub citation: Option<&'a str>,
    pub rationale: Option<&'a str>,
    /// One of [`TRANSLATION_STATUSES`].
    pub status: &'a str,
    pub authority_ref: Option<&'a str>,
    /// RFC 3339 UTC, stamped by the caller.
    pub proposed_at: &'a str,
}

const SELECT_TRANSLATION: &str = "SELECT translation_id, term_id, target_lang, target_term, \
     faction, translator, citation, rationale, status, authority_ref, proposed_at, approved_at, \
     version FROM glossary_translations";

fn read_translation(row: &rusqlite::Row<'_>) -> rusqlite::Result<TranslationRow> {
    Ok(TranslationRow {
        translation_id: row.get(0)?,
        term_id: row.get(1)?,
        target_lang: row.get(2)?,
        target_term: row.get(3)?,
        faction: row.get(4)?,
        translator: row.get(5)?,
        citation: row.get(6)?,
        rationale: row.get(7)?,
        status: row.get(8)?,
        authority_ref: row.get(9)?,
        proposed_at: row.get(10)?,
        approved_at: row.get(11)?,
        version: row.get(12)?,
    })
}

/// The single source of truth for the `glossary_translations` table's
/// schema. The frozen baseline DDL in [`crate::migrate`] is rendered
/// from this spec; `verify_all` pins the two together on every open.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "glossary_translations",
    comment: Some("Candidate renderings of glossary terms; superseded rows stay."),
    columns: &[
        ColumnSpec::int("translation_id").primary_key(),
        ColumnSpec::int("term_id")
            .not_null()
            .references(ForeignKey::new(
                "glossary_terms",
                "term_id",
                OnDelete::NoAction,
            )),
        ColumnSpec::text("target_lang").not_null(),
        ColumnSpec::text("target_term").comment("NULL records a do-not-translate verdict"),
        ColumnSpec::text("faction"),
        ColumnSpec::text("translator"),
        ColumnSpec::text("citation"),
        ColumnSpec::text("rationale"),
        ColumnSpec::text("status")
            .not_null()
            .check("status IN ('candidate', 'active', 'retired', 'rejected')"),
        ColumnSpec::text("authority_ref")
            .comment("refs://<book_slug>#<entry_key> URI; library-relative soft reference"),
        ColumnSpec::text("proposed_at").not_null(),
        ColumnSpec::text("approved_at"),
        ColumnSpec::int("version").not_null().default("1"),
    ],
    composite_pk: None,
    uniques: &[],
    table_checks: &[],
    indexes: &[IndexSpec::on(
        "gt_by_term",
        &["term_id", "target_lang", "status"],
    )],
};

/// One `glossary_translations` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslationRow {
    pub translation_id: i64,
    pub term_id: i64,
    pub target_lang: String,
    /// `None` records a do-not-translate verdict.
    pub target_term: Option<String>,
    pub faction: Option<String>,
    pub translator: Option<String>,
    pub citation: Option<String>,
    pub rationale: Option<String>,
    pub status: String,
    pub authority_ref: Option<String>,
    pub proposed_at: String,
    pub approved_at: Option<String>,
    pub version: i64,
}

impl TranslationRow {
    /// The one-line form a prompt quotes for this rendering: the
    /// target term, then ` # ` and the faction, translator and
    /// citation that are set, joined by `; `. A do-not-translate
    /// verdict reads `keep original`. All separators are ASCII; any
    /// other script comes from the stored values.
    pub fn inject_hint(&self) -> String {
        let Some(target_term) = self.target_term.as_deref() else {
            return "keep original".to_owned();
        };
        let provenance: Vec<&str> = [&self.faction, &self.translator, &self.citation]
            .into_iter()
            .filter_map(|field| field.as_deref())
            .filter(|s| !s.is_empty())
            .collect();
        if provenance.is_empty() {
            target_term.to_owned()
        } else {
            format!("{target_term} # {}", provenance.join("; "))
        }
    }
}

impl Translate {
    /// The active and candidate renderings of `term_id` into
    /// `target_lang`, active first, then by id. Retired and rejected
    /// rows stay on record but are not renderings a prompt may use.
    pub fn renderings_for_term(
        &self,
        term_id: i64,
        target_lang: &str,
    ) -> TranslateResult<Vec<TranslationRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT translation_id, term_id, target_lang, target_term, faction, translator, \
                    citation, rationale, status, authority_ref, proposed_at, approved_at, version \
             FROM glossary_translations \
             WHERE term_id = ?1 AND target_lang = ?2 AND status IN ('active', 'candidate') \
             ORDER BY CASE status WHEN 'active' THEN 0 ELSE 1 END, translation_id",
        )?;
        let rows = stmt.query_map(rusqlite::params![term_id, target_lang], |row| {
            Ok(TranslationRow {
                translation_id: row.get(0)?,
                term_id: row.get(1)?,
                target_lang: row.get(2)?,
                target_term: row.get(3)?,
                faction: row.get(4)?,
                translator: row.get(5)?,
                citation: row.get(6)?,
                rationale: row.get(7)?,
                status: row.get(8)?,
                authority_ref: row.get(9)?,
                proposed_at: row.get(10)?,
                approved_at: row.get(11)?,
                version: row.get(12)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

impl Translate {
    /// The rendering with `translation_id`, or `None`.
    pub fn translation(&self, translation_id: i64) -> TranslateResult<Option<TranslationRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("{SELECT_TRANSLATION} WHERE translation_id = ?1"),
                [translation_id],
                read_translation,
            )
            .optional()?)
    }

    /// The rendering of `term_id` into `target_lang` that reads
    /// `target_term`, in any status, or `None`. `None` as the term
    /// finds a recorded do-not-translate verdict.
    pub fn find_translation(
        &self,
        term_id: i64,
        target_lang: &str,
        target_term: Option<&str>,
    ) -> TranslateResult<Option<TranslationRow>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "{SELECT_TRANSLATION} WHERE term_id = ?1 AND target_lang = ?2 \
                     AND target_term IS ?3 ORDER BY translation_id LIMIT 1"
                ),
                rusqlite::params![term_id, target_lang, target_term],
                read_translation,
            )
            .optional()?)
    }

    /// Insert a rendering. The term must exist and the status must be
    /// one of [`TRANSLATION_STATUSES`]; `authority_ref` is stored as
    /// given, never resolved.
    pub fn insert_translation(&self, tr: &NewTranslation<'_>) -> TranslateResult<i64> {
        if self.term(tr.term_id)?.is_none() {
            return Err(TranslateError::UnknownTerm {
                term_id: tr.term_id,
            });
        }
        if !TRANSLATION_STATUSES.contains(&tr.status) {
            return Err(TranslateError::UnknownValue {
                what: "status",
                value: tr.status.to_owned(),
                known: TRANSLATION_STATUSES,
            });
        }
        Ok(self.conn.query_row(
            "INSERT INTO glossary_translations (term_id, target_lang, target_term, faction, \
             translator, citation, rationale, status, authority_ref, proposed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) RETURNING translation_id",
            rusqlite::params![
                tr.term_id,
                tr.target_lang,
                tr.target_term,
                tr.faction,
                tr.translator,
                tr.citation,
                tr.rationale,
                tr.status,
                tr.authority_ref,
                tr.proposed_at,
            ],
            |row| row.get(0),
        )?)
    }

    /// Make `translation_id` the primary rendering of `term_id`. The
    /// rendering must belong to the term and be active or a candidate;
    /// it becomes active, and the previous primary keeps its status.
    pub fn set_primary(&self, term_id: i64, translation_id: i64) -> TranslateResult<()> {
        if self.term(term_id)?.is_none() {
            return Err(TranslateError::UnknownTerm { term_id });
        }
        let eligible = self.translation(translation_id)?.is_some_and(|r| {
            r.term_id == term_id && (r.status == "active" || r.status == "candidate")
        });
        if !eligible {
            return Err(TranslateError::UnknownTranslation {
                term_id,
                translation_id,
            });
        }
        self.conn.execute(
            "UPDATE glossary_translations SET status = 'active', version = version + 1 \
             WHERE translation_id = ?1 AND status <> 'active'",
            [translation_id],
        )?;
        self.conn.execute(
            "UPDATE glossary_terms SET primary_choice_id = ?1 WHERE term_id = ?2",
            rusqlite::params![translation_id, term_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seed;

    fn rendering(target_term: Option<&str>, provenance: [Option<&str>; 3]) -> TranslationRow {
        TranslationRow {
            translation_id: 1,
            term_id: 1,
            target_lang: "zh".into(),
            target_term: target_term.map(str::to_owned),
            faction: provenance[0].map(str::to_owned),
            translator: provenance[1].map(str::to_owned),
            citation: provenance[2].map(str::to_owned),
            rationale: None,
            status: "active".into(),
            authority_ref: None,
            proposed_at: "2026-01-01T00:00:00Z".into(),
            approved_at: None,
            version: 1,
        }
    }

    #[test]
    fn the_hint_joins_the_target_term_to_whatever_provenance_is_set() {
        assert_eq!(
            rendering(
                Some("ci zai"),
                [Some("phenomenology"), Some("CJY"), Some("1987")]
            )
            .inject_hint(),
            "ci zai # phenomenology; CJY; 1987"
        );
        assert_eq!(
            rendering(Some("ci zai"), [None, Some("CJY"), None]).inject_hint(),
            "ci zai # CJY"
        );
        assert_eq!(
            rendering(Some("ci zai"), [None, Some(""), None]).inject_hint(),
            "ci zai"
        );
    }

    #[test]
    fn a_do_not_translate_verdict_reads_keep_original() {
        assert_eq!(
            rendering(None, [Some("phenomenology"), None, None]).inject_hint(),
            "keep original"
        );
    }

    #[test]
    fn a_rendering_reads_back_every_column() {
        let t = seed::fresh();
        let term = seed::term(&t, "library", None, "de", "Dasein", "dasein", "term");
        let id = seed::translation(&t, term, "zh", Some("ci zai"), "active");
        t.conn
            .execute(
                "UPDATE glossary_translations SET faction = 'f', translator = 'tr', \
                 citation = 'c', rationale = 'r', authority_ref = 'refs://lex#dasein', \
                 approved_at = '2026-02-02T00:00:00Z', version = 3 WHERE translation_id = ?1",
                [id],
            )
            .expect("update");

        let rows = t.renderings_for_term(term, "zh").expect("read");
        assert_eq!(
            rows,
            vec![TranslationRow {
                translation_id: id,
                term_id: term,
                target_lang: "zh".into(),
                target_term: Some("ci zai".into()),
                faction: Some("f".into()),
                translator: Some("tr".into()),
                citation: Some("c".into()),
                rationale: Some("r".into()),
                status: "active".into(),
                authority_ref: Some("refs://lex#dasein".into()),
                proposed_at: "2026-01-01T00:00:00Z".into(),
                approved_at: Some("2026-02-02T00:00:00Z".into()),
                version: 3,
            }]
        );
    }

    fn new_translation(
        term_id: i64,
        target_term: Option<&'static str>,
        status: &'static str,
    ) -> NewTranslation<'static> {
        NewTranslation {
            term_id,
            target_lang: "zh",
            target_term,
            faction: Some("faction-a"),
            translator: None,
            citation: None,
            rationale: Some("why"),
            status,
            authority_ref: Some("refs://dict#entry"),
            proposed_at: "2026-01-01T00:00:00Z",
        }
    }

    #[test]
    fn an_inserted_rendering_reads_back_and_is_found_by_its_text() {
        let t = seed::fresh();
        let term_id = seed::term(&t, "library", None, "de", "Traum", "traum", "term");
        let id = t
            .insert_translation(&new_translation(term_id, Some("TRAUM-A"), "candidate"))
            .expect("insert");
        let row = t.translation(id).expect("read").expect("row");
        assert_eq!(
            (
                row.target_term.as_deref(),
                row.faction.as_deref(),
                row.status.as_str(),
                row.authority_ref.as_deref(),
                row.version
            ),
            (
                Some("TRAUM-A"),
                Some("faction-a"),
                "candidate",
                Some("refs://dict#entry"),
                1
            )
        );
        assert_eq!(
            t.find_translation(term_id, "zh", Some("TRAUM-A"))
                .expect("find")
                .map(|r| r.translation_id),
            Some(id)
        );
        assert_eq!(
            t.find_translation(term_id, "zh", Some("other"))
                .expect("find"),
            None
        );
        assert_eq!(
            t.find_translation(term_id, "fr", Some("TRAUM-A"))
                .expect("find"),
            None
        );

        let keep = t
            .insert_translation(&new_translation(term_id, None, "candidate"))
            .expect("do-not-translate");
        assert_eq!(
            t.find_translation(term_id, "zh", None)
                .expect("find")
                .map(|r| r.translation_id),
            Some(keep)
        );
    }

    #[test]
    fn a_rendering_needs_an_existing_term_and_a_known_status() {
        let t = seed::fresh();
        let err = t
            .insert_translation(&new_translation(404, Some("x"), "candidate"))
            .expect_err("term");
        assert!(
            matches!(err, TranslateError::UnknownTerm { term_id: 404 }),
            "{err:?}"
        );
        let term_id = seed::term(&t, "library", None, "de", "Traum", "traum", "term");
        let err = t
            .insert_translation(&new_translation(term_id, Some("x"), "maybe"))
            .expect_err("status");
        assert!(
            matches!(err, TranslateError::UnknownValue { what: "status", .. }),
            "{err:?}"
        );
    }

    #[test]
    fn set_primary_activates_the_chosen_rendering_and_checks_ownership() {
        let t = seed::fresh();
        let term_id = seed::term(&t, "library", None, "de", "Traum", "traum", "term");
        let other_term = seed::term(&t, "library", None, "de", "Raum", "raum", "term");
        let first = seed::translation(&t, term_id, "zh", Some("A"), "active");
        seed::set_primary(&t, term_id, first);
        let second = seed::translation(&t, term_id, "zh", Some("B"), "candidate");
        let retired = seed::translation(&t, term_id, "zh", Some("C"), "retired");
        let foreign = seed::translation(&t, other_term, "zh", Some("D"), "active");

        t.set_primary(term_id, second).expect("switch");
        let term = t.term(term_id).expect("read").expect("row");
        assert_eq!(term.primary_choice_id, Some(second));
        let second_row = t.translation(second).expect("read").expect("row");
        assert_eq!(
            (second_row.status.as_str(), second_row.version),
            ("active", 2)
        );
        let first_row = t.translation(first).expect("read").expect("row");
        assert_eq!(
            (first_row.status.as_str(), first_row.version),
            ("active", 1),
            "the old primary is left as it was"
        );

        for (candidate, what) in [
            (retired, "retired"),
            (foreign, "another term's"),
            (404, "unknown"),
        ] {
            let err = t.set_primary(term_id, candidate).expect_err(what);
            assert!(
                matches!(err, TranslateError::UnknownTranslation { translation_id, .. } if translation_id == candidate),
                "{what}: {err:?}"
            );
        }
        assert_eq!(
            t.term(term_id)
                .expect("read")
                .expect("row")
                .primary_choice_id,
            Some(second)
        );
        let err = t.set_primary(404, second).expect_err("term");
        assert!(
            matches!(err, TranslateError::UnknownTerm { term_id: 404 }),
            "{err:?}"
        );
    }
}
