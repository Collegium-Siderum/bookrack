// SPDX-License-Identifier: Apache-2.0

//! Conformance checking — comparing a live database against its specs.
//!
//! The DDL is rendered from a [`TableSpec`], so a freshly built database
//! always matches its spec. This check earns its keep on a database file
//! built by an *older* binary: a column added to a spec without a
//! rebuild, or any other drift between the compiled-in specs and the
//! schema on disk, is reported here instead of failing silently later.
//!
//! It runs as a dedicated test (always) and as a debug-build assertion
//! when a database is opened.

use rusqlite::Connection;

use crate::spec::{OnDelete, TableSpec};

/// A live table whose schema does not match its [`TableSpec`].
#[derive(Debug)]
pub struct SchemaMismatch {
    /// The table that failed conformance.
    pub table: String,
    /// One human-readable line per discrepancy.
    pub diffs: Vec<String>,
}

impl std::fmt::Display for SchemaMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "schema conformance failure in table `{}`:", self.table)?;
        for diff in &self.diffs {
            writeln!(f, "  {diff}")?;
        }
        write!(
            f,
            "hint: the database was built by an older SCHEMA_VERSION, or a \
             TableSpec changed without a rebuild"
        )
    }
}

impl std::error::Error for SchemaMismatch {}

/// Why a conformance check could not be completed or did not pass.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// A database error occurred while introspecting the schema.
    #[error("database error during schema verification: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The live schema does not match the spec.
    #[error("{0}")]
    Mismatch(#[from] SchemaMismatch),
}

/// Verify that the table named by `spec` exists in `conn` with exactly
/// the columns, primary key, indexes, and foreign keys the spec declares.
pub fn verify_table(conn: &Connection, spec: &TableSpec) -> Result<(), VerifyError> {
    let mut diffs = Vec::new();
    verify_columns(conn, spec, &mut diffs)?;
    verify_primary_key(conn, spec, &mut diffs)?;
    verify_indexes(conn, spec, &mut diffs)?;
    verify_foreign_keys(conn, spec, &mut diffs)?;

    if diffs.is_empty() {
        Ok(())
    } else {
        Err(SchemaMismatch {
            table: spec.name.to_string(),
            diffs,
        }
        .into())
    }
}

/// Verify every spec in `specs`, returning the first failure.
pub fn verify_all(conn: &Connection, specs: &[&TableSpec]) -> Result<(), VerifyError> {
    for spec in specs {
        verify_table(conn, spec)?;
    }
    Ok(())
}

// ── Column conformance ──────────────────────────────────────────────

/// One row of `PRAGMA table_info`.
struct LiveColumn {
    name: String,
    sql_type: String,
    notnull: bool,
    default: Option<String>,
    /// Position in the primary key, or 0 if the column is not part of it.
    pk: i64,
}

fn read_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<LiveColumn>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info('{table}')"))?;
    let rows = stmt.query_map([], |row| {
        Ok(LiveColumn {
            name: row.get("name")?,
            sql_type: row.get("type")?,
            notnull: row.get::<_, i64>("notnull")? != 0,
            default: row.get("dflt_value")?,
            pk: row.get("pk")?,
        })
    })?;
    rows.collect()
}

fn verify_columns(
    conn: &Connection,
    spec: &TableSpec,
    diffs: &mut Vec<String>,
) -> rusqlite::Result<()> {
    let live = read_columns(conn, spec.name)?;
    for col in spec.columns {
        let Some(found) = live.iter().find(|l| l.name == col.name) else {
            diffs.push(format!(
                "column `{}`: present in TableSpec, missing from database",
                col.name
            ));
            continue;
        };
        let want_type = col.sql_type.as_sql();
        if !found.sql_type.eq_ignore_ascii_case(want_type) {
            diffs.push(format!(
                "column `{}`: type mismatch — TableSpec says {want_type}, database says {}",
                col.name, found.sql_type
            ));
        }
        let want_notnull = !col.nullable;
        if found.notnull != want_notnull {
            diffs.push(format!(
                "column `{}`: NOT NULL mismatch — TableSpec says {want_notnull}, database says {}",
                col.name, found.notnull
            ));
        }
        if found.default.as_deref() != col.default {
            diffs.push(format!(
                "column `{}`: DEFAULT mismatch — TableSpec says {:?}, database says {:?}",
                col.name, col.default, found.default
            ));
        }
    }
    for found in &live {
        if !spec.columns.iter().any(|c| c.name == found.name) {
            diffs.push(format!(
                "column `{}`: present in database, missing from TableSpec",
                found.name
            ));
        }
    }
    Ok(())
}

fn verify_primary_key(
    conn: &Connection,
    spec: &TableSpec,
    diffs: &mut Vec<String>,
) -> rusqlite::Result<()> {
    let live = read_columns(conn, spec.name)?;
    let mut live_pk: Vec<&LiveColumn> = live.iter().filter(|l| l.pk > 0).collect();
    live_pk.sort_by_key(|l| l.pk);
    let live_names: Vec<&str> = live_pk.iter().map(|l| l.name.as_str()).collect();
    let want: Vec<&str> = spec.primary_key_columns();
    if live_names != want {
        diffs.push(format!(
            "primary key mismatch — TableSpec says {want:?}, database says {live_names:?}"
        ));
    }
    Ok(())
}

// ── Index conformance ───────────────────────────────────────────────

/// One row of `PRAGMA index_list`.
struct LiveIndex {
    name: String,
    unique: bool,
    /// `'c'` for `CREATE INDEX`, `'u'`/`'pk'` for constraint-backed
    /// indexes the database creates implicitly.
    origin: String,
    partial: bool,
}

fn read_indexes(conn: &Connection, table: &str) -> rusqlite::Result<Vec<LiveIndex>> {
    let mut stmt = conn.prepare(&format!("PRAGMA index_list('{table}')"))?;
    let rows = stmt.query_map([], |row| {
        Ok(LiveIndex {
            name: row.get("name")?,
            unique: row.get::<_, i64>("unique")? != 0,
            origin: row.get("origin")?,
            partial: row.get::<_, i64>("partial")? != 0,
        })
    })?;
    rows.collect()
}

fn read_index_columns(conn: &Connection, index: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA index_info('{index}')"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>("name"))?;
    rows.collect()
}

fn verify_indexes(
    conn: &Connection,
    spec: &TableSpec,
    diffs: &mut Vec<String>,
) -> rusqlite::Result<()> {
    // Constraint-backed indexes (UNIQUE / PRIMARY KEY) are verified
    // through the column and key checks; only explicit `CREATE INDEX`
    // entries are matched against the spec's index list.
    let live: Vec<LiveIndex> = read_indexes(conn, spec.name)?
        .into_iter()
        .filter(|i| i.origin == "c")
        .collect();
    for index in spec.indexes {
        let Some(found) = live.iter().find(|l| l.name == index.name) else {
            diffs.push(format!(
                "index `{}`: present in TableSpec, missing from database",
                index.name
            ));
            continue;
        };
        if found.unique != index.unique {
            diffs.push(format!(
                "index `{}`: UNIQUE mismatch — TableSpec says {}, database says {}",
                index.name, index.unique, found.unique
            ));
        }
        let want_partial = index.where_clause.is_some();
        if found.partial != want_partial {
            diffs.push(format!(
                "index `{}`: partial mismatch — TableSpec says {want_partial}, database says {}",
                index.name, found.partial
            ));
        }
        let live_cols = read_index_columns(conn, index.name)?;
        if live_cols
            .iter()
            .map(String::as_str)
            .ne(index.columns.iter().copied())
        {
            diffs.push(format!(
                "index `{}`: column mismatch — TableSpec says {:?}, database says {:?}",
                index.name, index.columns, live_cols
            ));
        }
    }
    for found in &live {
        if !spec.indexes.iter().any(|i| i.name == found.name) {
            diffs.push(format!(
                "index `{}`: present in database, missing from TableSpec",
                found.name
            ));
        }
    }
    Ok(())
}

// ── Foreign-key conformance ─────────────────────────────────────────

/// One row of `PRAGMA foreign_key_list`.
struct LiveForeignKey {
    from: String,
    to_table: String,
    to_column: String,
    on_delete: String,
}

fn read_foreign_keys(conn: &Connection, table: &str) -> rusqlite::Result<Vec<LiveForeignKey>> {
    let mut stmt = conn.prepare(&format!("PRAGMA foreign_key_list('{table}')"))?;
    let rows = stmt.query_map([], |row| {
        Ok(LiveForeignKey {
            from: row.get("from")?,
            to_table: row.get("table")?,
            to_column: row.get("to")?,
            on_delete: row.get("on_delete")?,
        })
    })?;
    rows.collect()
}

fn verify_foreign_keys(
    conn: &Connection,
    spec: &TableSpec,
    diffs: &mut Vec<String>,
) -> rusqlite::Result<()> {
    let live = read_foreign_keys(conn, spec.name)?;
    let want: Vec<(&str, OnDelete, &str, &str)> = spec
        .columns
        .iter()
        .filter_map(|c| {
            c.references
                .map(|fk| (c.name, fk.on_delete, fk.table, fk.column))
        })
        .collect();
    for &(from, on_delete, to_table, to_column) in &want {
        let Some(found) = live.iter().find(|l| l.from == from) else {
            diffs.push(format!(
                "foreign key on `{from}`: present in TableSpec, missing from database"
            ));
            continue;
        };
        if found.to_table != to_table || found.to_column != to_column {
            diffs.push(format!(
                "foreign key on `{from}`: target mismatch — TableSpec says {to_table}({to_column}), \
                 database says {}({})",
                found.to_table, found.to_column
            ));
        }
        if found.on_delete != on_delete.pragma_str() {
            diffs.push(format!(
                "foreign key on `{from}`: ON DELETE mismatch — TableSpec says {}, database says {}",
                on_delete.pragma_str(),
                found.on_delete
            ));
        }
    }
    for found in &live {
        if !want.iter().any(|&(from, ..)| found.from == from) {
            diffs.push(format!(
                "foreign key on `{}`: present in database, missing from TableSpec",
                found.from
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_ddl;
    use crate::spec::{ColumnSpec, ForeignKey, IndexSpec};

    const SPEC: TableSpec = TableSpec {
        name: "sample",
        comment: None,
        columns: &[
            ColumnSpec::int("sample_id").primary_key(),
            ColumnSpec::int("owner_id").references(ForeignKey::new(
                "sample",
                "sample_id",
                OnDelete::Cascade,
            )),
            ColumnSpec::text("label").not_null(),
            ColumnSpec::int("flag").not_null().default("0"),
        ],
        composite_pk: None,
        uniques: &[],
        table_checks: &[],
        indexes: &[
            IndexSpec::on("idx_sample_label", &["label"]),
            IndexSpec::on("idx_sample_flag", &["flag"]).partial("flag = 1"),
        ],
    };

    fn conn_from(ddl: &str) -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(ddl).expect("apply ddl");
        conn
    }

    #[test]
    fn a_rendered_schema_conforms_to_its_spec() {
        let conn = conn_from(&render_ddl(&SPEC));
        verify_table(&conn, &SPEC).expect("rendered schema must conform to its spec");
    }

    #[test]
    fn an_extra_database_column_is_reported() {
        let conn = conn_from(&render_ddl(&SPEC));
        conn.execute_batch("ALTER TABLE sample ADD COLUMN stray TEXT")
            .expect("alter");
        let err = verify_table(&conn, &SPEC).expect_err("must detect the stray column");
        let VerifyError::Mismatch(mismatch) = err else {
            panic!("expected a schema mismatch");
        };
        assert!(
            mismatch
                .diffs
                .iter()
                .any(|d| d.contains("`stray`") && d.contains("missing from TableSpec"))
        );
    }

    #[test]
    fn a_missing_index_is_reported() {
        // Build the table but none of its indexes.
        let conn = conn_from(
            "CREATE TABLE sample (\
            sample_id INTEGER PRIMARY KEY, owner_id INTEGER, label TEXT NOT NULL, \
            flag INTEGER NOT NULL DEFAULT 0)",
        );
        let err = verify_table(&conn, &SPEC).expect_err("must detect the missing indexes");
        let VerifyError::Mismatch(mismatch) = err else {
            panic!("expected a schema mismatch");
        };
        assert!(
            mismatch
                .diffs
                .iter()
                .any(|d| d.contains("idx_sample_label") && d.contains("missing from database"))
        );
    }
}
