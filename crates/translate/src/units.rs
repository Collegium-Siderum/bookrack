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
use rusqlite::OptionalExtension;

use crate::{Translate, TranslateResult};

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

/// One `translate_units` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitRow {
    pub unit_id: i64,
    pub intake_id: i64,
    pub target_lang: String,
    pub node_id: i64,
    pub unit_order: i64,
    pub source_outline: Option<String>,
    pub injection_profile: String,
}

const SELECT_UNIT: &str = "SELECT unit_id, intake_id, target_lang, node_id, unit_order, \
     source_outline, injection_profile FROM translate_units";

fn read_unit(row: &rusqlite::Row<'_>) -> rusqlite::Result<UnitRow> {
    Ok(UnitRow {
        unit_id: row.get(0)?,
        intake_id: row.get(1)?,
        target_lang: row.get(2)?,
        node_id: row.get(3)?,
        unit_order: row.get(4)?,
        source_outline: row.get(5)?,
        injection_profile: row.get(6)?,
    })
}

impl Translate {
    /// The unit with `unit_id`, or `None` if no such row exists.
    pub fn unit(&self, unit_id: i64) -> TranslateResult<Option<UnitRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("{SELECT_UNIT} WHERE unit_id = ?1"),
                [unit_id],
                read_unit,
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use crate::seed;

    #[test]
    fn a_unit_reads_back_every_column() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 3, "zh", 40, 2);
        t.conn
            .execute(
                "UPDATE translate_units SET source_outline = 'I > 2', \
                 injection_profile = 'academic' WHERE unit_id = ?1",
                [unit_id],
            )
            .expect("update");

        let row = t.unit(unit_id).expect("read").expect("row");
        assert_eq!(row.unit_id, unit_id);
        assert_eq!(row.intake_id, 3);
        assert_eq!(row.target_lang, "zh");
        assert_eq!(row.node_id, 40);
        assert_eq!(row.unit_order, 2);
        assert_eq!(row.source_outline.as_deref(), Some("I > 2"));
        assert_eq!(row.injection_profile, "academic");
    }

    #[test]
    fn a_fresh_unit_carries_the_default_profile_and_no_outline() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);
        let row = t.unit(unit_id).expect("read").expect("row");
        assert_eq!(row.injection_profile, "default");
        assert_eq!(row.source_outline, None);
    }

    #[test]
    fn an_unknown_unit_reads_as_none() {
        let t = seed::fresh();
        assert_eq!(t.unit(404).expect("read"), None);
    }
}
