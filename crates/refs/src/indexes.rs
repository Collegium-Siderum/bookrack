// SPDX-License-Identifier: Apache-2.0

//! Per-book physical lookup paths attached to `reference_entries`.
//!
//! Each [`crate::types::IndexSpec`] from a book.toml `[[indexes]]`
//! entry becomes:
//!
//! 1. A `VIRTUAL` generated column on `reference_entries` named
//!    `gencol_<slug>_<field>` (`.` separators in `field` flatten to
//!    `_`), projecting one path out of `payload_json` via
//!    `json_extract`. `STORED` would be preferred but bundled SQLite's
//!    `ALTER TABLE ADD COLUMN` rejects `STORED` (see
//!    `crates/dbkit/tests/generated_column.rs`); `VIRTUAL` is
//!    functionally equivalent here because the `CREATE INDEX` below
//!    materializes the projected values into a B-tree that point
//!    lookups hit directly.
//!
//! 2. A partial B-tree index `ix_ref_<slug>_<field>` over that column,
//!    restricted to `WHERE book_slug = '<slug>'` so the index pages
//!    cover only rows from the registering book.
//!
//! The function is idempotent: re-registering the same book is a
//! no-op. Re-registering with one fewer index leaves the dropped
//! column and index in place (DROP is out of scope for v1; a future
//! migration will manage column removal explicitly).

use rusqlite::Connection;

use crate::RefsError;
use crate::types::{IndexKind, IndexSpec};

/// Apply the index set for `book_slug` against the connection.
///
/// Idempotent by construction: `ALTER TABLE ADD COLUMN` is attempted
/// unconditionally and the `duplicate column name` error is treated as
/// a no-op signal, and `CREATE INDEX IF NOT EXISTS` is naturally a
/// no-op when the index already exists. The introspection-based
/// pre-check that would otherwise gate the ALTER is unreliable —
/// `pragma_table_info` is served from rusqlite's compiled-schema cache
/// and does not reflect columns added by an earlier ALTER on the same
/// connection.
pub fn apply(conn: &Connection, book_slug: &str, specs: &[IndexSpec]) -> Result<(), RefsError> {
    validate_slug(book_slug)?;

    for spec in specs {
        validate_field(&spec.field)?;
        let column = column_name(book_slug, &spec.field);
        let json_path = format!("$.{}", spec.field);
        // No bound parameters here: SQLite refuses parameters in DDL.
        // The identifiers are validated above to a strict
        // `[a-z0-9_]+` alphabet, and the JSON path is built from the
        // same validated input plus a literal `$.` prefix, so the
        // interpolation cannot widen the grammar.
        let alter = format!(
            "ALTER TABLE reference_entries \
             ADD COLUMN {column} \
             GENERATED ALWAYS AS (json_extract(payload_json, '{json_path}')) VIRTUAL"
        );
        match conn.execute_batch(&alter) {
            Ok(()) => {}
            Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                if msg.contains("duplicate column name") => {}
            Err(e) => return Err(e.into()),
        }

        let index = index_name(book_slug, &spec.field);
        match spec.kind {
            IndexKind::Btree => {
                let create = format!(
                    "CREATE INDEX IF NOT EXISTS {index} \
                     ON reference_entries({column}) \
                     WHERE book_slug = '{book_slug}'"
                );
                conn.execute_batch(&create)?;
            }
        }
    }

    Ok(())
}

/// `gencol_<slug>_<field with . -> _>`.
fn column_name(book_slug: &str, field: &str) -> String {
    format!("gencol_{book_slug}_{}", field.replace('.', "_"))
}

/// `ix_ref_<slug>_<field with . -> _>`.
fn index_name(book_slug: &str, field: &str) -> String {
    format!("ix_ref_{book_slug}_{}", field.replace('.', "_"))
}

/// A slug must start with a lowercase letter and contain only
/// `[a-z0-9_]`. Mirrors the rule the `audit-profile` crate applies to
/// catalog keys.
fn validate_slug(slug: &str) -> Result<(), RefsError> {
    let mut chars = slug.chars();
    let first = chars.next().ok_or_else(|| invalid_ident("empty slug"))?;
    if !first.is_ascii_lowercase() {
        return Err(invalid_ident(format!(
            "slug must start with a lowercase letter: {slug:?}"
        )));
    }
    for ch in chars {
        if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_') {
            return Err(invalid_ident(format!(
                "slug must match [a-z][a-z0-9_]*: {slug:?}"
            )));
        }
    }
    Ok(())
}

/// A field is a dotted path of slug-shaped segments. Empty segments
/// and trailing dots are rejected.
fn validate_field(field: &str) -> Result<(), RefsError> {
    if field.is_empty() {
        return Err(invalid_ident("empty field"));
    }
    for segment in field.split('.') {
        validate_slug(segment).map_err(|_| {
            invalid_ident(format!(
                "field segments must match [a-z][a-z0-9_]*: {field:?}"
            ))
        })?;
    }
    Ok(())
}

fn invalid_ident<S: Into<String>>(msg: S) -> RefsError {
    RefsError::InvalidIdentifier(msg.into())
}
