// SPDX-License-Identifier: Apache-2.0

//! The `toc_edits` table — the authoritative log of manual TOC edits.
//!
//! The `corpus.db` node tree is a materialized projection of the
//! extracted skeleton plus this overlay, so a corpus rebuild replays
//! these verbs and never loses an edit. The table is defined here as a
//! spec only; the verbs and their replay live in a later milestone. The
//! spec exists so the schema is gated through the same `TableSpec`
//! pipeline as every other catalog table and a future schema change can
//! ride the standard migration path.

use bookrack_dbkit::{ColumnSpec, TableSpec};

/// The single source of truth for the `toc_edits` table's schema. Its
/// DDL is rendered from this spec.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "toc_edits",
    comment: Some("Authoritative log of manual TOC edits (replayed on corpus rebuild)."),
    columns: &[
        ColumnSpec::int("edit_id").pk_autoinc(),
        ColumnSpec::int("book_root_id")
            .not_null()
            .comment("soft reference to corpus.nodes"),
        ColumnSpec::int("seq")
            .not_null()
            .comment("per-book edit order; replay sorts by this"),
        ColumnSpec::text("verb")
            .not_null()
            .comment("split / merge / set_range / rename / set_type / new / rm"),
        ColumnSpec::text("args")
            .not_null()
            .comment("JSON arguments"),
        ColumnSpec::text("target_anchor").comment("content fingerprint, to re-locate on replay"),
        ColumnSpec::int("new_node_id").comment("id of an org node created by new/split"),
        ColumnSpec::text("actor_kind")
            .not_null()
            .check("actor_kind IN ('human', 'llm', 'import', 'pipeline', 'system')"),
        ColumnSpec::text("actor_detail"),
        ColumnSpec::text("edited_at").not_null(),
        ColumnSpec::text("session_id"),
    ],
    composite_pk: None,
    uniques: &[&["book_root_id", "seq"]],
    table_checks: &[],
    indexes: &[],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Catalog;

    #[test]
    fn the_baseline_schema_conforms_to_the_toc_edits_spec() {
        let catalog = Catalog::open_in_memory().expect("open");
        bookrack_dbkit::verify_table(&catalog.conn, &SPEC)
            .expect("the migration baseline must conform to the toc_edits spec");
    }
}
