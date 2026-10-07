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

use crate::{Translate, TranslateError, TranslateResult};

/// Every `role` a witness may carry.
pub const ROLES: &[&str] = &["alt_source", "translation_witness", "prior_translation"];

/// One witness to anchor on a unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWitness<'a> {
    pub witness_intake_id: i64,
    pub witness_node_id: i64,
    pub lang: &'a str,
    pub role: &'a str,
    pub note: Option<&'a str>,
}

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

/// One `translate_unit_witnesses` row: a pointer to the witness text,
/// never the text itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WitnessRow {
    pub witness_intake_id: i64,
    pub witness_node_id: i64,
    pub lang: String,
    pub role: String,
    pub note: Option<String>,
}

impl Translate {
    /// Every witness anchored on `unit_id`, in insertion order; empty
    /// for a unit without witnesses or an unknown unit.
    pub fn witnesses_for_unit(&self, unit_id: i64) -> TranslateResult<Vec<WitnessRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT witness_intake_id, witness_node_id, lang, role, note \
             FROM translate_unit_witnesses WHERE unit_id = ?1 ORDER BY witness_id",
        )?;
        let rows = stmt.query_map([unit_id], |row| {
            Ok(WitnessRow {
                witness_intake_id: row.get(0)?,
                witness_node_id: row.get(1)?,
                lang: row.get(2)?,
                role: row.get(3)?,
                note: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

impl Translate {
    /// Anchor a witness on `unit_id`, replacing the row for the same
    /// witness book if there is one. The unit must exist and the role
    /// must be one of [`ROLES`].
    pub fn put_witness(&self, unit_id: i64, w: &NewWitness<'_>) -> TranslateResult<i64> {
        if self.unit(unit_id)?.is_none() {
            return Err(TranslateError::UnknownUnit { unit_id });
        }
        if !ROLES.contains(&w.role) {
            return Err(TranslateError::UnknownValue {
                what: "role",
                value: w.role.to_owned(),
                known: ROLES,
            });
        }
        Ok(self.conn.query_row(
            "INSERT INTO translate_unit_witnesses (unit_id, witness_intake_id, witness_node_id, \
             lang, role, note) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(unit_id, witness_intake_id) DO UPDATE SET \
             witness_node_id = excluded.witness_node_id, lang = excluded.lang, \
             role = excluded.role, note = excluded.note \
             RETURNING witness_id",
            rusqlite::params![
                unit_id,
                w.witness_intake_id,
                w.witness_node_id,
                w.lang,
                w.role,
                w.note
            ],
            |row| row.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seed;

    #[test]
    fn witnesses_read_back_every_column_in_insertion_order() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);
        let other = seed::unit(&t, 1, "zh", 11, 1);
        seed::witness(&t, unit_id, 5, "alt_source");
        let second = seed::witness(&t, unit_id, 6, "prior_translation");
        seed::witness(&t, other, 9, "translation_witness");
        t.conn
            .execute(
                "UPDATE translate_unit_witnesses SET note = 'second edition' WHERE witness_id = ?1",
                [second],
            )
            .expect("update");

        let rows = t.witnesses_for_unit(unit_id).expect("read");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].witness_intake_id, 5);
        assert_eq!(rows[0].witness_node_id, 7);
        assert_eq!(rows[0].lang, "en");
        assert_eq!(rows[0].role, "alt_source");
        assert_eq!(rows[0].note, None);
        assert_eq!(rows[1].witness_intake_id, 6);
        assert_eq!(rows[1].role, "prior_translation");
        assert_eq!(rows[1].note.as_deref(), Some("second edition"));
    }

    #[test]
    fn a_unit_without_witnesses_reads_as_empty() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);
        assert!(t.witnesses_for_unit(unit_id).expect("read").is_empty());
        assert!(t.witnesses_for_unit(404).expect("read").is_empty());
    }

    #[test]
    fn put_witness_upserts_per_witness_book_and_checks_unit_and_role() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);
        let first = NewWitness {
            witness_intake_id: 5,
            witness_node_id: 70,
            lang: "en",
            role: "alt_source",
            note: None,
        };
        let id = t.put_witness(unit_id, &first).expect("insert");
        let same = t
            .put_witness(
                unit_id,
                &NewWitness {
                    witness_node_id: 71,
                    role: "prior_translation",
                    note: Some("1999 edition"),
                    ..first
                },
            )
            .expect("replace");
        assert_eq!(same, id, "the same witness book keeps its row");
        let rows = t.witnesses_for_unit(unit_id).expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (
                rows[0].witness_node_id,
                rows[0].role.as_str(),
                rows[0].note.as_deref()
            ),
            (71, "prior_translation", Some("1999 edition"))
        );

        let err = t
            .put_witness(
                unit_id,
                &NewWitness {
                    role: "bystander",
                    ..first
                },
            )
            .expect_err("role");
        assert!(
            matches!(err, TranslateError::UnknownValue { what: "role", .. }),
            "{err:?}"
        );
        let err = t.put_witness(404, &first).expect_err("unit");
        assert!(
            matches!(err, TranslateError::UnknownUnit { unit_id: 404 }),
            "{err:?}"
        );
    }
}
