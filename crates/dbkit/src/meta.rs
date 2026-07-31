// SPDX-License-Identifier: Apache-2.0

//! Schema application and the key/value `meta` table helpers.
//!
//! Both databases keep a small `(key, value)` table for database-level
//! scalars — the schema-version stamp and, for the corpus, the recorded
//! index-build parameters. The reconcile policy differs per database and
//! stays with each crate; the storage mechanics are shared here.

use rusqlite::{Connection, OptionalExtension, named_params};

use crate::ddl::render_ddl;
use crate::spec::TableSpec;

/// Create every table in `specs` (and its indexes) on `conn`.
///
/// The DDL is rendered from the specs, so this is the one place a
/// database's schema comes into being. Every statement is idempotent, so
/// applying it to an already-initialized database is a no-op.
pub fn apply_schema(conn: &Connection, specs: &[&TableSpec]) -> rusqlite::Result<()> {
    let mut ddl = String::new();
    for spec in specs {
        ddl.push_str(&render_ddl(spec));
        ddl.push('\n');
    }
    conn.execute_batch(&ddl)
}

/// Read a scalar from a `(key, value)` meta table, or `None` if unset.
pub fn meta_get(
    conn: &Connection,
    meta_table: &str,
    key: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        &format!("SELECT value FROM {meta_table} WHERE key = :key"),
        named_params! { ":key": key },
        |row| row.get::<_, String>(0),
    )
    .optional()
}

/// Write a scalar to a `(key, value)` meta table, replacing any previous
/// value for the key.
pub fn meta_set(
    conn: &Connection,
    meta_table: &str,
    key: &str,
    value: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO {meta_table}(key, value) VALUES(:key, :value) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value"
        ),
        named_params! { ":key": key, ":value": value },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{ColumnSpec, TableSpec};

    const META: TableSpec = TableSpec {
        name: "kv",
        comment: None,
        columns: &[
            ColumnSpec::text("key").primary_key(),
            ColumnSpec::text("value").not_null(),
        ],
        composite_pk: None,
        uniques: &[],
        table_checks: &[],
        indexes: &[],
    };

    #[test]
    fn apply_schema_builds_a_usable_table() {
        let conn = Connection::open_in_memory().expect("open");
        apply_schema(&conn, &[&META]).expect("apply schema");

        // The rendered DDL carries the spec's constraints, not just the
        // column names: the key is a primary key and the value is NOT
        // NULL.
        conn.execute("INSERT INTO kv(key, value) VALUES('k', 'v')", [])
            .expect("the table accepts a row");
        conn.execute("INSERT INTO kv(key, value) VALUES('k', 'other')", [])
            .expect_err("the key is a primary key");
        conn.execute("INSERT INTO kv(key, value) VALUES('n', NULL)", [])
            .expect_err("the value is NOT NULL");

        // A second application is a no-op, which means the rows survive
        // it — an application that dropped and recreated the table would
        // also "succeed".
        apply_schema(&conn, &[&META]).expect("re-apply schema");
        let value: Option<String> = meta_get(&conn, "kv", "k").expect("get");
        assert_eq!(value.as_deref(), Some("v"), "re-applying kept the row");
    }

    #[test]
    fn meta_round_trips_and_overwrites() {
        let conn = Connection::open_in_memory().expect("open");
        apply_schema(&conn, &[&META]).expect("apply schema");

        assert_eq!(meta_get(&conn, "kv", "missing").expect("get"), None);
        meta_set(&conn, "kv", "k", "first").expect("set");
        assert_eq!(
            meta_get(&conn, "kv", "k").expect("get"),
            Some("first".to_string())
        );
        meta_set(&conn, "kv", "k", "second").expect("overwrite");
        assert_eq!(
            meta_get(&conn, "kv", "k").expect("get"),
            Some("second".to_string())
        );
    }
}
