// SPDX-License-Identifier: Apache-2.0

//! The `catalog.db` schema migration sequence.
//!
//! `catalog.db` is source-of-truth and cannot be rebuilt, so its schema
//! evolves through migrations rather than a drop-and-recreate. The applied
//! revision lives in SQLite's `user_version`, advanced by
//! `rusqlite_migration`.
//!
//! `M[0]` is the baseline: the entire current schema, rendered from the
//! same [`TableSpec`](bookrack_dbkit::TableSpec)s `apply_schema` uses, so
//! the schema has a single source of truth and is never transcribed by
//! hand. Real schema changes append `M[1]`, `M[2]`, …, each owning its own
//! SQL; none is pre-written. The sequence is forward-only — a desktop
//! downgrade restores a backup rather than running a `down` step.

use std::sync::OnceLock;

use rusqlite_migration::{M, Migrations};

use crate::db::{PENDING_TABLES_DDL, SPECS};

/// The `user_version` a fully-migrated `catalog.db` carries: the number of
/// migrations defined. The `catalog_meta.schema_version` mirror is kept
/// equal to it.
pub(crate) const TARGET_VERSION: i64 = 1;

/// The baseline DDL: every spec's rendered `CREATE TABLE` followed by the
/// tables that have no spec yet.
///
/// Rendered once and held for the program's lifetime, since [`M::up`]
/// borrows its SQL for `'static`. `render_ddl` emits
/// `CREATE TABLE IF NOT EXISTS`, so applying the baseline to a database
/// that already holds these tables is a no-op — the basis for adopting a
/// pre-migration database in place.
fn baseline_sql() -> &'static str {
    static SQL: OnceLock<String> = OnceLock::new();
    SQL.get_or_init(|| {
        let mut sql = String::new();
        for spec in SPECS {
            sql.push_str(&bookrack_dbkit::render_ddl(spec));
            sql.push('\n');
        }
        sql.push_str(PENDING_TABLES_DDL);
        sql
    })
    .as_str()
}

/// The migration sequence applied to `catalog.db` on open.
pub(crate) fn migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(baseline_sql())])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn the_migration_set_is_well_formed() {
        migrations().validate().expect("migrations must validate");
    }

    #[test]
    fn applying_the_baseline_reaches_the_target_version() {
        let mut conn = Connection::open_in_memory().expect("open");
        migrations()
            .to_latest(&mut conn)
            .expect("baseline must apply");
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, TARGET_VERSION);
    }

    #[test]
    fn applying_the_baseline_twice_is_idempotent() {
        let mut conn = Connection::open_in_memory().expect("open");
        migrations().to_latest(&mut conn).expect("first apply");
        // Re-running against an already-migrated database is a no-op, not
        // an error: the basis for in-place adoption.
        migrations().to_latest(&mut conn).expect("second apply");
    }
}
