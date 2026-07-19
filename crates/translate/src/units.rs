// SPDX-License-Identifier: Apache-2.0

//! The `translate_units` table — immutable logical structure.
//!
//! One row per corpus node selected for translation into one target
//! language: a chapter, section, or paragraph-level container. Units
//! mirror the corpus structure and are never split or merged by an
//! agent; the mutable sentence-level slicing lives in
//! `translate_segments`. `intake_id` and `node_id` are soft
//! cross-database references; when a re-ingest renumbers the corpus,
//! units are re-anchored through the `source_outline` snapshot rather
//! than cascaded.

use bookrack_dbkit::{ColumnSpec, IndexSpec, TableSpec};

/// The single source of truth for the `translate_units` table's schema.
/// The frozen baseline DDL in [`crate::migrate`] is rendered from this
/// spec; `verify_all` pins the two together on every open.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "translate_units",
    comment: Some("Immutable translation units mirroring corpus structure."),
    columns: &[
        ColumnSpec::int("unit_id").primary_key(),
        ColumnSpec::int("intake_id")
            .not_null()
            .comment("soft reference to the catalog intake; no cascade"),
        ColumnSpec::text("target_lang").not_null(),
        ColumnSpec::int("node_id")
            .not_null()
            .comment("soft reference to the corpus node; re-anchored via source_outline"),
        ColumnSpec::int("unit_order").not_null(),
        ColumnSpec::text("source_outline")
            .comment("chapter-path snapshot; drives re-anchoring and TOC backfill"),
        ColumnSpec::text("injection_profile")
            .not_null()
            .default("'default'"),
    ],
    composite_pk: None,
    uniques: &[&["intake_id", "target_lang", "node_id"]],
    table_checks: &[],
    indexes: &[IndexSpec::on(
        "unit_by_intake",
        &["intake_id", "target_lang", "unit_order"],
    )],
};
