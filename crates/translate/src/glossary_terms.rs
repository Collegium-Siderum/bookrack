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
