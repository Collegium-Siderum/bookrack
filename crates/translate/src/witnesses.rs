// SPDX-License-Identifier: Apache-2.0

//! The `translate_unit_witnesses` table — witness-text anchoring.
//!
//! A witness is a parallel text consulted during translation: an
//! alternative source edition, a translation into a third language, or
//! a prior translation into the target language. Witnesses anchor at
//! the unit level — chapter-to-chapter alignment set once from the TOC
//! structure — and finer alignment is done ad hoc by the reading agent,
//! not stored here. `witness_intake_id` and `witness_node_id` are soft
//! cross-database references into the catalog and corpus.

use bookrack_dbkit::{ColumnSpec, ForeignKey, OnDelete, TableSpec};

/// The single source of truth for the `translate_unit_witnesses`
/// table's schema. The frozen baseline DDL in [`crate::migrate`] is
/// rendered from this spec; `verify_all` pins the two together on
/// every open.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "translate_unit_witnesses",
    comment: Some("Witness texts anchored per unit; chapter-to-chapter alignment."),
    columns: &[
        ColumnSpec::int("witness_id").primary_key(),
        ColumnSpec::int("unit_id")
            .not_null()
            .references(ForeignKey::new(
                "translate_units",
                "unit_id",
                OnDelete::NoAction,
            )),
        ColumnSpec::int("witness_intake_id")
            .not_null()
            .comment("soft reference to the catalog intake; no cascade"),
        ColumnSpec::int("witness_node_id").not_null(),
        ColumnSpec::text("lang").not_null(),
        ColumnSpec::text("role")
            .not_null()
            .check("role IN ('alt_source', 'translation_witness', 'prior_translation')"),
        ColumnSpec::text("note").comment("free-form witness credentials"),
    ],
    composite_pk: None,
    uniques: &[&["unit_id", "witness_intake_id"]],
    table_checks: &[],
    indexes: &[],
};
