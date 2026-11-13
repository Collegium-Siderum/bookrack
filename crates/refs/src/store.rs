// SPDX-License-Identifier: Apache-2.0

//! `reference.db` as the open protocol sees it.
//!
//! The specs here describe the tables at the current revision; the
//! migration text in [`crate::migrate`] is the historical record of how
//! a file gets there.

use bookrack_dbkit::{ColumnSpec, OpenError, ReaderStamp, SchemaAxis, StoreSpec, TableSpec};
use rusqlite::Connection;

use crate::RefsError;
use crate::migrate::{TARGET_VERSION, migrations};

/// The `min_reader_version` value this build stamps when writing
/// `reference.db`.
///
/// Bump when a writer-side change produces content an older build could
/// misread. Adding a column or a table does not require a bump.
// setting: internal -- a version stamp; docs/UPGRADE.md's runbook governs a bump
pub const MIN_READER_VERSION: u32 = 1;

/// `reference_meta` key under which [`MIN_READER_VERSION`] is recorded.
const MIN_READER_VERSION_KEY: &str = "min_reader_version";

const BOOKS: TableSpec = TableSpec {
    name: "reference_books",
    comment: Some("Per-book registry: one row per ingested reference book."),
    columns: &[
        ColumnSpec::text("book_slug").primary_key(),
        ColumnSpec::text("schema_name").not_null(),
        ColumnSpec::int("schema_version").not_null(),
        ColumnSpec::text("parser_version").not_null(),
        ColumnSpec::text("title_zh").not_null(),
        ColumnSpec::text("title_en"),
        ColumnSpec::text("edition"),
        ColumnSpec::text("publisher"),
        ColumnSpec::int("year"),
        ColumnSpec::text("isbn"),
        ColumnSpec::int("authority_rank").not_null().default("0"),
        ColumnSpec::text("built_at").not_null(),
        ColumnSpec::int("intake_id"),
        ColumnSpec::int("entry_count").not_null().default("0"),
        ColumnSpec::int("parse_warnings").not_null().default("0"),
    ],
    composite_pk: None,
    uniques: &[],
    table_checks: &[],
    indexes: &[],
};

const OVERLAYS: TableSpec = TableSpec {
    name: "reference_entry_overlays",
    comment: Some("User edits layered on top of the distilled entries."),
    columns: &[
        ColumnSpec::text("book_slug").not_null(),
        ColumnSpec::text("entry_key").not_null(),
        ColumnSpec::text("overlay_json").not_null(),
        ColumnSpec::text("quality_flags"),
        ColumnSpec::text("base_built_at"),
        ColumnSpec::text("edit_reason"),
        ColumnSpec::text("edited_at").not_null(),
    ],
    composite_pk: Some(&["book_slug", "entry_key"]),
    uniques: &[],
    table_checks: &[],
    indexes: &[],
};

const META: TableSpec = TableSpec {
    name: "reference_meta",
    comment: Some("Database-level scalars: the reader version stamp."),
    columns: &[
        ColumnSpec::text("key").primary_key(),
        ColumnSpec::text("value").not_null(),
    ],
    composite_pk: None,
    uniques: &[],
    table_checks: &[],
    indexes: &[],
};

/// The tables whose shape is verified on every open.
///
/// `reference_entries` is absent on purpose, together with the resolved
/// view, the FTS5 sidecar, and its three triggers. Registering a book
/// adds one generated column and one explicit index per `[[indexes]]`
/// entry of its `book.toml` to that table, so its live shape depends on
/// the books the library holds and a static spec cannot describe it. It
/// is the base layer a distill build rewrites in full; the rows a
/// rebuild cannot restore sit in `reference_entry_overlays`, which is
/// verified.
const TABLES: &[&TableSpec] = &[&BOOKS, &OVERLAYS, &META];

pub(crate) const STORE: StoreSpec = StoreSpec {
    label: "reference",
    tables: TABLES,
    schema: SchemaAxis::Migrated {
        target: TARGET_VERSION,
        migrate: migrate_to_latest,
        retired: None,
    },
    reader_stamp: ReaderStamp {
        table: "reference_meta",
        key: MIN_READER_VERSION_KEY,
        write_version: MIN_READER_VERSION,
    },
};

fn migrate_to_latest(conn: &mut Connection) -> Result<(), rusqlite_migration::Error> {
    migrations().to_latest(conn)
}

impl From<OpenError> for RefsError {
    fn from(e: OpenError) -> RefsError {
        match e {
            OpenError::TooOld { found, target } => RefsError::SchemaTooOld {
                found,
                supported: target,
            },
            OpenError::TooNew { found, target } => RefsError::SchemaTooNew {
                found,
                supported: target,
            },
            OpenError::ReaderTooOld { required, current } => {
                RefsError::ReaderTooOld { required, current }
            }
            OpenError::Verify(e) => RefsError::Verify(e),
            OpenError::Migrate(e) => RefsError::Migration(e),
            OpenError::Sqlite(e) => RefsError::Sqlite(e),
            OpenError::Io(e) => RefsError::Io(e),
            OpenError::Rederive { .. } => {
                unreachable!("reference.db is migrated, never rederived")
            }
            OpenError::Retired { .. } => {
                unreachable!("reference.db names no retired-value check")
            }
        }
    }
}
