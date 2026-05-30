// SPDX-License-Identifier: Apache-2.0

//! A connection wrapper that times the statements run directly on it.

use std::ops::{Deref, DerefMut};
use std::time::{Duration, Instant};

use rusqlite::{Connection, Params, Row};

/// Threshold above which a single direct statement is logged as slow.
///
/// Direct SQLite statements run in well under a millisecond at the corpus
/// sizes the pipeline produces; a query crossing this bound signals
/// pathology — a missing index, an unexpected full scan — worth a `WARN`,
/// not routine load.
pub const DEFAULT_SLOW_QUERY_THRESHOLD: Duration = Duration::from_millis(100);

/// A SQLite [`Connection`] that times the one-shot statements run directly
/// on it and logs a `WARN` when one exceeds its slow threshold.
///
/// It derefs to the wrapped connection, so the prepared-statement,
/// transaction, and batch APIs remain available unchanged; only the direct
/// [`execute`](TimedConnection::execute) and
/// [`query_row`](TimedConnection::query_row) entry points are timed, since
/// those are where a single slow statement surfaces. Prepared statements
/// and transactions run their own SQL out of this type's view and are not
/// timed here.
pub struct TimedConnection {
    conn: Connection,
    /// Names the database in slow-query logs (e.g. `"corpus"`).
    label: &'static str,
    slow_threshold: Duration,
}

impl TimedConnection {
    /// Wrap `conn`, labelling it `label` in slow-query logs, with the
    /// [`DEFAULT_SLOW_QUERY_THRESHOLD`].
    pub fn new(conn: Connection, label: &'static str) -> TimedConnection {
        TimedConnection {
            conn,
            label,
            slow_threshold: DEFAULT_SLOW_QUERY_THRESHOLD,
        }
    }

    /// Wrap `conn` with an explicit slow threshold, for tests that need to
    /// force the slow-query path without an artificially slow query.
    pub fn with_threshold(
        conn: Connection,
        label: &'static str,
        slow_threshold: Duration,
    ) -> TimedConnection {
        TimedConnection {
            conn,
            label,
            slow_threshold,
        }
    }

    /// Run a one-shot statement, timing it and logging a `WARN` if it is
    /// slow. Mirrors [`Connection::execute`].
    pub fn execute<P: Params>(&self, sql: &str, params: P) -> rusqlite::Result<usize> {
        let start = Instant::now();
        let result = self.conn.execute(sql, params);
        self.note(sql, start.elapsed());
        result
    }

    /// Run a single-row query, timing it and logging a `WARN` if it is
    /// slow. Mirrors [`Connection::query_row`].
    pub fn query_row<T, P, F>(&self, sql: &str, params: P, f: F) -> rusqlite::Result<T>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> rusqlite::Result<T>,
    {
        let start = Instant::now();
        let result = self.conn.query_row(sql, params, f);
        self.note(sql, start.elapsed());
        result
    }

    /// Emit a slow-query `WARN` when `elapsed` crosses the threshold.
    fn note(&self, sql: &str, elapsed: Duration) {
        if elapsed >= self.slow_threshold {
            tracing::warn!(
                db = self.label,
                elapsed_ms = elapsed.as_secs_f64() * 1e3,
                threshold_ms = self.slow_threshold.as_secs_f64() * 1e3,
                sql = sql_summary(sql),
                "slow database query",
            );
        }
    }
}

impl Deref for TimedConnection {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.conn
    }
}

impl DerefMut for TimedConnection {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

/// Reduce a SQL statement to a single, length-capped line for logging, so
/// a slow-query event names the statement without dumping a multi-line
/// query into the log.
fn sql_summary(sql: &str) -> String {
    const MAX: usize = 120;
    let flat = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(MAX).collect();
    if flat.chars().count() > MAX {
        out.push('\u{2026}');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timed_execute_and_query_row_pass_through() {
        let conn = TimedConnection::new(Connection::open_in_memory().expect("open"), "test");
        conn.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)",
            [],
        )
        .expect("create");
        let affected = conn
            .execute("INSERT INTO t (v) VALUES (?1)", ["hello"])
            .expect("insert");
        assert_eq!(affected, 1);
        let v: String = conn
            .query_row("SELECT v FROM t WHERE id = 1", [], |row| row.get(0))
            .expect("query");
        assert_eq!(v, "hello");
    }

    #[test]
    fn deref_exposes_the_prepared_statement_api() {
        let conn = TimedConnection::new(Connection::open_in_memory().expect("open"), "test");
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .expect("batch");
        let mut stmt = conn
            .prepare("INSERT INTO t (id) VALUES (?1)")
            .expect("prepare");
        stmt.execute([7]).expect("exec");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 1);
    }

    #[test]
    fn deref_mut_exposes_transactions() {
        let mut conn = TimedConnection::new(Connection::open_in_memory().expect("open"), "test");
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .expect("batch");
        let tx = conn.transaction().expect("begin");
        tx.execute("INSERT INTO t (id) VALUES (1)", [])
            .expect("insert");
        tx.commit().expect("commit");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 1);
    }

    #[test]
    fn sql_summary_flattens_and_caps() {
        let summarized = sql_summary("SELECT *\n  FROM t\n  WHERE id = 1");
        assert_eq!(summarized, "SELECT * FROM t WHERE id = 1");
        let long = "x".repeat(200);
        assert!(sql_summary(&long).chars().count() <= 121);
    }

    /// A `MakeWriter` collecting all subscriber output into a shared buffer
    /// so a test can assert on what was logged.
    #[derive(Clone, Default)]
    struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;

        fn make_writer(&'a self) -> CaptureWriter {
            self.clone()
        }
    }

    #[test]
    fn a_query_over_the_threshold_logs_a_warning() {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(CaptureWriter(buffer.clone()))
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            // A zero threshold makes every statement count as slow.
            let conn = TimedConnection::with_threshold(
                Connection::open_in_memory().expect("open"),
                "test",
                Duration::ZERO,
            );
            conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", [])
                .expect("create");
        });

        let logged = String::from_utf8(buffer.lock().expect("lock").clone()).expect("utf8");
        assert!(logged.contains("slow database query"), "got: {logged}");
        assert!(logged.contains("db=\"test\""), "got: {logged}");
    }
}
