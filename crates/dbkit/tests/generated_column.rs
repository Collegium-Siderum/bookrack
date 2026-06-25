// SPDX-License-Identifier: Apache-2.0

//! Confirm the bundled SQLite supports `VIRTUAL` generated columns
//! added by `ALTER TABLE ... ADD COLUMN` and indexed by a subsequent
//! `CREATE INDEX`.
//!
//! The `refs` crate's `Refs::register_book` relies on this combination
//! to attach per-book physical lookup paths to the shared
//! `reference_entries` table at registration time: each book.toml
//! `[[indexes]]` entry becomes a generated column projecting one path
//! out of `payload_json` plus a partial index keyed by `book_slug`.
//!
//! SQLite's `ALTER TABLE ... ADD COLUMN` only accepts `VIRTUAL`
//! generated columns (`STORED` raises "cannot add a STORED column" at
//! DDL time), so the column is computed at write time but the
//! `CREATE INDEX` on it persists the projected values into a B-tree
//! that point lookups hit directly. Performance for indexed lookups is
//! therefore identical to a stored column. The smallest path that
//! exercises both pieces is captured here so a future SQLite bump that
//! changes the support surface fails at the dbkit gate, not deep inside
//! a refs migration.

use rusqlite::Connection;

#[test]
fn bundled_sqlite_supports_alter_add_virtual_generated_column_with_index() {
    let conn = Connection::open_in_memory().expect("open in-memory db");

    // `VIRTUAL GENERATED ALWAYS AS` on `ALTER TABLE ADD COLUMN` has
    // been supported since SQLite 3.31 (2020); the bundled rusqlite
    // version is well past that. `json_extract` from JSON1 is the
    // projection refs uses against `payload_json`. The partial index
    // mirrors the per-book WHERE clause refs emits per `[[indexes]]`
    // entry.
    conn.execute_batch(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, j TEXT NOT NULL);\n\
         INSERT INTO t (j) VALUES ('{\"x\": 1}'), ('{\"x\": 2}');\n\
         ALTER TABLE t ADD COLUMN c INTEGER \
             GENERATED ALWAYS AS (json_extract(j, '$.x')) VIRTUAL;\n\
         CREATE INDEX ix_t_c ON t(c) WHERE id > 0;",
    )
    .expect("bundled SQLite must support VIRTUAL gencol on ALTER + partial INDEX");

    // The generated column materializes for the rows that were already
    // present when the ALTER ran, and stays in sync on subsequent
    // inserts.
    let stored: i64 = conn
        .query_row("SELECT c FROM t WHERE id = 1", [], |row| row.get(0))
        .expect("read generated value");
    assert_eq!(stored, 1);

    conn.execute("INSERT INTO t (j) VALUES ('{\"x\": 3}')", [])
        .expect("insert row past the ALTER");
    let stored_new: i64 = conn
        .query_row("SELECT c FROM t WHERE j = '{\"x\": 3}'", [], |row| {
            row.get(0)
        })
        .expect("read generated value of post-ALTER row");
    assert_eq!(stored_new, 3);

    // The partial index appears under the requested name with the
    // recorded WHERE clause, so refs can rely on `CREATE INDEX IF NOT
    // EXISTS` for idempotency.
    let index_sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'ix_t_c'",
            [],
            |row| row.get(0),
        )
        .expect("read index ddl");
    assert!(
        index_sql.contains("WHERE id > 0"),
        "partial index must persist the WHERE clause: {index_sql}"
    );
}
