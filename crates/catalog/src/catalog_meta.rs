// SPDX-License-Identifier: Apache-2.0

//! The `catalog_meta` table — database-level scalars.
//!
//! Currently this holds only the schema-version stamp; the helpers are
//! crate-internal, used by the schema-version reconcile on open.

use bookrack_dbkit::{ColumnSpec, TableSpec};

use crate::{Catalog, Result};

/// The single source of truth for the `catalog_meta` table's schema. Its
/// DDL is rendered from this spec.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "catalog_meta",
    comment: Some("Database-level scalars; currently just the schema version stamp."),
    columns: &[
        ColumnSpec::text("key").primary_key(),
        ColumnSpec::text("value").not_null(),
    ],
    composite_pk: None,
    uniques: &[],
    table_checks: &[],
    indexes: &[],
};

impl Catalog {
    /// Read a `catalog_meta` scalar, or `None` if the key is unset.
    pub(crate) fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(bookrack_dbkit::meta_get(&self.conn, SPEC.name, key)?)
    }

    /// Write a `catalog_meta` scalar, replacing any previous value.
    pub(crate) fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        bookrack_dbkit::meta_set(&self.conn, SPEC.name, key, value)?;
        Ok(())
    }
}
