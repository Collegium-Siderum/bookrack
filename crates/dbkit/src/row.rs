// SPDX-License-Identifier: Apache-2.0

//! Row-decoding helpers shared by every table module.

use rusqlite::Row;
use rusqlite::types::Type;

/// Read a text column and decode it through `parse`.
///
/// For columns whose value is one of a closed set encoded as text — a
/// node type, a status, an actor kind. An unrecognized string means the
/// database was written by something other than this code base; it
/// surfaces as a conversion failure naming the offending column and
/// value, rather than a silent or misleading result.
pub fn decode<T>(row: &Row<'_>, column: &str, parse: fn(&str) -> Option<T>) -> rusqlite::Result<T> {
    let raw: String = row.get(column)?;
    parse(&raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            Type::Text,
            format!("column {column:?}: unrecognized value {raw:?}").into(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn parse_yes_no(s: &str) -> Option<bool> {
        match s {
            "yes" => Some(true),
            "no" => Some(false),
            _ => None,
        }
    }

    #[test]
    fn decodes_a_known_value_and_rejects_an_unknown_one() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch("CREATE TABLE t(v TEXT); INSERT INTO t(v) VALUES ('yes'), ('maybe')")
            .expect("seed");
        let mut stmt = conn
            .prepare("SELECT v FROM t ORDER BY rowid")
            .expect("prepare");
        let mut rows = stmt.query([]).expect("query");

        let known = rows.next().expect("step").expect("row present");
        assert!(decode(known, "v", parse_yes_no).expect("known value decodes"));

        let unknown = rows.next().expect("step").expect("row present");
        assert!(decode(unknown, "v", parse_yes_no).is_err());
    }
}
