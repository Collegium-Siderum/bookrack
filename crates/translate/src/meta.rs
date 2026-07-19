// SPDX-License-Identifier: Apache-2.0

//! The `translate_meta` table — database-level scalars.
//!
//! A small `(key, value)` table holding the schema-version mirror and
//! the `min_reader_version` stamp, through dbkit's shared meta-table
//! helpers.

use bookrack_dbkit::{ColumnSpec, TableSpec};

use crate::{Translate, TranslateResult};

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
}
