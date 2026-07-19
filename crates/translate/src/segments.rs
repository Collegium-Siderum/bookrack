// SPDX-License-Identifier: Apache-2.0

//! The `translate_segments` table — mutable sentence-level slices.
//!
//! A segment is the actual unit of translation work: a span of source
//! text inside one unit, addressed by a four-part `(start_node_id,
//! start_char_offset, end_node_id, end_char_offset)` span into the
//! corpus. Segments may be re-sliced by an agent while still virgin;
//! `source_text_sha` fingerprints the spanned source text, serving both
//! as a drift sentinel and as the content key that relocates the span
//! after a re-ingest. The `status` column carries the segment
//! lifecycle `draft -> proposed -> sealed`.

use bookrack_dbkit::{ColumnSpec, ForeignKey, IndexSpec, OnDelete, TableSpec};

/// The single source of truth for the `translate_segments` table's
/// schema. The frozen baseline DDL in [`crate::migrate`] is rendered
/// from this spec; `verify_all` pins the two together on every open.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "translate_segments",
    comment: Some("Mutable translation segments; the unit of translation work."),
    columns: &[
        ColumnSpec::int("segment_id").primary_key(),
        ColumnSpec::int("unit_id")
            .not_null()
            .references(ForeignKey::new(
                "translate_units",
                "unit_id",
                OnDelete::NoAction,
            )),
        ColumnSpec::int("start_node_id")
            .not_null()
            .comment("soft reference to the corpus node the span starts in"),
        ColumnSpec::int("start_char_offset").not_null(),
        ColumnSpec::int("end_node_id")
            .not_null()
            .comment("soft reference to the corpus node the span ends in"),
        ColumnSpec::int("end_char_offset").not_null(),
        ColumnSpec::text("source_text_sha")
            .not_null()
            .comment("content fingerprint; drift sentinel and re-anchor key"),
        ColumnSpec::text("status")
            .not_null()
            .check("status IN ('draft', 'proposed', 'sealed')"),
        ColumnSpec::text("draft_text"),
        ColumnSpec::text("reflection_notes").comment("JSON; reflection or review-note payload"),
        ColumnSpec::text("final_text")
            .comment("semantically locked form; other formats derive at export"),
        ColumnSpec::text("source_kind")
            .check("source_kind IN ('human', 'llm-draft', 'llm-reflected', 'edited', 'imported')"),
        ColumnSpec::text("sealed_at"),
        ColumnSpec::int("version").not_null().default("1"),
    ],
    composite_pk: None,
    uniques: &[&[
        "unit_id",
        "start_node_id",
        "start_char_offset",
        "end_node_id",
        "end_char_offset",
    ]],
    table_checks: &[],
    indexes: &[
        IndexSpec::on("seg_by_unit", &["unit_id", "start_char_offset"]),
        IndexSpec::on("seg_by_status", &["status", "sealed_at"]),
    ],
};
