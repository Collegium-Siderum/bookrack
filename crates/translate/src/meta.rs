// SPDX-License-Identifier: Apache-2.0

//! The `translate_meta` table — database-level scalars.
//!
//! A small `(key, value)` table holding the schema-version mirror, the
//! `min_reader_version` stamp and the build stamps of the translation
//! memory, through dbkit's shared meta-table helpers.

use bookrack_dbkit::{ColumnSpec, TableSpec};

use crate::{Translate, TranslateError, TranslateResult};

/// `translate_meta` key recording the row layout the translation
/// memory was written with.
pub const TM_SCHEMA_VERSION_KEY: &str = "tm_schema_version";
/// `translate_meta` key recording the embedding model that wrote the
/// translation memory.
pub const TM_EMBED_MODEL_KEY: &str = "tm_embed_model";
/// `translate_meta` key recording the vector width of the translation
/// memory.
pub const TM_VECTOR_DIM_KEY: &str = "tm_vector_dim";

/// The build parameters of the translation memory derived from this
/// store. Stamped when the memory is first written or rebuilt, and
/// compared by every path that reads or extends it: a memory written
/// by another model or row layout is rebuilt, never searched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmStamps {
    pub schema_version: u32,
    pub embed_model: String,
    pub vector_dim: u32,
}

/// The single source of truth for the `translate_meta` table's schema.
/// The frozen baseline DDL in [`crate::migrate`] is rendered from this
/// spec; `verify_all` pins the two together on every open.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "translate_meta",
    comment: Some("Key/value scalars: schema-version mirror and reader-version stamp."),
    columns: &[
        ColumnSpec::text("key").primary_key(),
        ColumnSpec::text("value").not_null(),
    ],
    composite_pk: None,
    uniques: &[],
    table_checks: &[],
    indexes: &[],
};

impl Translate {
    /// Read a scalar from `translate_meta`, or `None` if unset.
    pub(crate) fn meta_get(&self, key: &str) -> TranslateResult<Option<String>> {
        Ok(bookrack_dbkit::meta_get(&self.conn, SPEC.name, key)?)
    }

    /// Write a scalar to `translate_meta`, replacing any previous value.
    pub(crate) fn meta_set(&self, key: &str, value: &str) -> TranslateResult<()> {
        bookrack_dbkit::meta_set(&self.conn, SPEC.name, key, value)?;
        Ok(())
    }

    /// The translation memory's build stamps, or `None` unless all
    /// three are recorded. A partial set reads as absent: it is what an
    /// interrupted rebuild leaves behind, and it certifies nothing.
    pub fn tm_stamps(&self) -> TranslateResult<Option<TmStamps>> {
        let (Some(schema), Some(model), Some(dim)) = (
            self.meta_get(TM_SCHEMA_VERSION_KEY)?,
            self.meta_get(TM_EMBED_MODEL_KEY)?,
            self.meta_get(TM_VECTOR_DIM_KEY)?,
        ) else {
            return Ok(None);
        };
        let parse = |key: &'static str, value: &str| {
            value
                .parse::<u32>()
                .map_err(|_| TranslateError::TmStampMismatch {
                    key,
                    stored: value.to_owned(),
                    expected: "an unsigned integer".to_owned(),
                })
        };
        Ok(Some(TmStamps {
            schema_version: parse(TM_SCHEMA_VERSION_KEY, &schema)?,
            embed_model: model,
            vector_dim: parse(TM_VECTOR_DIM_KEY, &dim)?,
        }))
    }

    /// Record `expected` as the memory's build stamps when none are
    /// recorded, or refuse when the recorded ones differ in any key.
    /// A partial set is treated as none: a rebuild that stamps at its
    /// end overwrites whatever an interrupted one left.
    pub fn reconcile_tm_stamps(&self, expected: &TmStamps) -> TranslateResult<()> {
        if let Some(stored) = self.tm_stamps()? {
            for (key, stored, wanted) in [
                (
                    TM_SCHEMA_VERSION_KEY,
                    stored.schema_version.to_string(),
                    expected.schema_version.to_string(),
                ),
                (
                    TM_EMBED_MODEL_KEY,
                    stored.embed_model.clone(),
                    expected.embed_model.clone(),
                ),
                (
                    TM_VECTOR_DIM_KEY,
                    stored.vector_dim.to_string(),
                    expected.vector_dim.to_string(),
                ),
            ] {
                if stored != wanted {
                    return Err(TranslateError::TmStampMismatch {
                        key,
                        stored,
                        expected: wanted,
                    });
                }
            }
            return Ok(());
        }
        self.meta_set(TM_SCHEMA_VERSION_KEY, &expected.schema_version.to_string())?;
        self.meta_set(TM_EMBED_MODEL_KEY, &expected.embed_model)?;
        self.meta_set(TM_VECTOR_DIM_KEY, &expected.vector_dim.to_string())?;
        Ok(())
    }

    /// Remove the memory's build stamps, so the next rebuild stamps
    /// afresh. Absent keys are accepted.
    pub fn clear_tm_stamps(&self) -> TranslateResult<()> {
        self.conn.execute(
            &format!("DELETE FROM {} WHERE key IN (?1, ?2, ?3)", SPEC.name),
            [TM_SCHEMA_VERSION_KEY, TM_EMBED_MODEL_KEY, TM_VECTOR_DIM_KEY],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seed;

    fn stamps(model: &str, dim: u32) -> TmStamps {
        TmStamps {
            schema_version: 1,
            embed_model: model.to_owned(),
            vector_dim: dim,
        }
    }

    #[test]
    fn tm_stamps_are_absent_until_reconciled_and_then_read_back() {
        let t = seed::fresh();
        assert_eq!(t.tm_stamps().expect("read"), None);
        t.reconcile_tm_stamps(&stamps("model-a", 4)).expect("stamp");
        assert_eq!(t.tm_stamps().expect("read"), Some(stamps("model-a", 4)));
        // Reconciling the same stamps again is a no-op.
        t.reconcile_tm_stamps(&stamps("model-a", 4)).expect("same");
    }

    #[test]
    fn reconcile_refuses_a_different_model_or_width_and_names_the_key() {
        let t = seed::fresh();
        t.reconcile_tm_stamps(&stamps("model-a", 4)).expect("stamp");
        let err = t
            .reconcile_tm_stamps(&stamps("model-b", 4))
            .expect_err("other model");
        assert!(
            matches!(&err, TranslateError::TmStampMismatch { key, stored, expected }
                if *key == TM_EMBED_MODEL_KEY && stored == "model-a" && expected == "model-b"),
            "{err:?}"
        );
        let err = t
            .reconcile_tm_stamps(&stamps("model-a", 8))
            .expect_err("other width");
        assert!(
            matches!(&err, TranslateError::TmStampMismatch { key, stored, expected }
                if *key == TM_VECTOR_DIM_KEY && stored == "4" && expected == "8"),
            "{err:?}"
        );
        let mut other_layout = stamps("model-a", 4);
        other_layout.schema_version = 2;
        let err = t
            .reconcile_tm_stamps(&other_layout)
            .expect_err("other layout");
        assert!(
            matches!(&err, TranslateError::TmStampMismatch { key, .. }
                if *key == TM_SCHEMA_VERSION_KEY),
            "{err:?}"
        );
        // The refusal left the stored stamps as they were.
        assert_eq!(t.tm_stamps().expect("read"), Some(stamps("model-a", 4)));
    }

    #[test]
    fn a_partial_stamp_set_reads_as_absent_and_is_overwritten_by_the_next_reconcile() {
        let t = seed::fresh();
        t.reconcile_tm_stamps(&stamps("model-a", 4)).expect("stamp");
        t.conn
            .execute(
                "DELETE FROM translate_meta WHERE key = ?1",
                [TM_VECTOR_DIM_KEY],
            )
            .expect("drop one key");
        assert_eq!(t.tm_stamps().expect("read"), None);
        t.reconcile_tm_stamps(&stamps("model-b", 8))
            .expect("a partial set does not refuse");
        assert_eq!(t.tm_stamps().expect("read"), Some(stamps("model-b", 8)));
    }

    #[test]
    fn clear_then_reconcile_restamps() {
        let t = seed::fresh();
        t.clear_tm_stamps().expect("clearing nothing is fine");
        t.reconcile_tm_stamps(&stamps("model-a", 4)).expect("stamp");
        t.clear_tm_stamps().expect("clear");
        assert_eq!(t.tm_stamps().expect("read"), None);
        t.reconcile_tm_stamps(&stamps("model-b", 8))
            .expect("restamp");
        assert_eq!(t.tm_stamps().expect("read"), Some(stamps("model-b", 8)));
    }
}
