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
