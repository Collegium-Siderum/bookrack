// SPDX-License-Identifier: Apache-2.0

//! The `index_meta` table — index-level scalars.
//!
//! `index_meta` records the parameters an index was built with —
//! embedding model, vector dimension, chunk and normalization versions,
//! the schema version — so a daemon can refuse to serve an index that no
//! longer matches its compiled-in constants.

use bookrack_dbkit::{ColumnSpec, TableSpec};

use crate::{Corpus, Result};

/// The single source of truth for the `index_meta` table's schema. Its
/// DDL is rendered from this spec.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "index_meta",
    comment: Some("Index-level scalars: the parameters an index was built with."),
    columns: &[
        ColumnSpec::text("key").primary_key(),
        ColumnSpec::text("value").not_null(),
    ],
    composite_pk: None,
    uniques: &[],
    table_checks: &[],
    indexes: &[],
};

impl Corpus {
    /// Read an `index_meta` scalar, or `None` if the key is unset.
    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(bookrack_dbkit::meta_get(&self.conn, SPEC.name, key)?)
    }

    /// Write an `index_meta` scalar, replacing any previous value.
    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        bookrack_dbkit::meta_set(&self.conn, SPEC.name, key, value)?;
        Ok(())
    }
}
