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

use crate::{Translate, TranslateResult};

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
}
