// SPDX-License-Identifier: Apache-2.0

//! The retired labels of `catalog.db`.
//!
//! A label is retired when no path of this build writes or reads it.
//! A catalog behind this build that holds a row carrying one is refused
//! as it opens; the migration that passes the retirement runs the same
//! search inside its transaction, so a row written between the two
//! rolls the migration back.

use bookrack_dbkit::RetiredHit;
use rusqlite::Connection;

/// The revision whose migration renames `book_state` to `item_state`.
// setting: internal -- a schema revision of a past migration
const ITEM_STATE_SINCE: i64 = 8;

/// The revision whose migration adds `intake.derived_from_sha256`.
// setting: internal -- a schema revision of a past migration
const DERIVED_FROM_SINCE: i64 = 14;

/// Count the rows of a catalog at revision `found` that carry a retired
/// label, one entry per label that occurs.
///
/// A catalog at revision 0 has no tables and holds none. Each later
/// revision is searched through the tables and columns it has.
pub(crate) fn retired_labels(conn: &Connection, found: i64) -> rusqlite::Result<Vec<RetiredHit>> {
    if found < 1 {
        return Ok(Vec::new());
    }
    let state = if found < ITEM_STATE_SINCE {
        "book_state"
    } else {
        "item_state"
    };
    let state_sql =
        format!("SELECT count(*) FROM {state} WHERE ocr_marker_finished_at IS NOT NULL");
    let mut searches: Vec<(&'static str, &str)> = vec![
        (
            "format = ocr-markdown",
            "SELECT count(*) FROM intake WHERE format = 'ocr-markdown'",
        ),
        (
            "adapter = ocr-pages",
            "SELECT count(*) FROM intake WHERE adapter = 'ocr-pages'",
        ),
        (
            "status = aborted",
            "SELECT count(*) FROM intake WHERE status = 'aborted'",
        ),
        (
            "source = ocr_marker",
            "SELECT count(*) FROM node_publication_attrs WHERE source = 'ocr_marker'",
        ),
        ("ocr_marker_finished_at", &state_sql),
    ];
    if found >= DERIVED_FROM_SINCE {
        searches.push((
            "derived_from_sha256",
            "SELECT count(*) FROM intake WHERE derived_from_sha256 IS NOT NULL",
        ));
    }
    let mut hits = Vec::new();
    for (marker, sql) in searches {
        let count = conn
            .query_row(sql, [], |row| row.get::<_, i64>(0))?
            .unsigned_abs();
        if count > 0 {
            hits.push(RetiredHit { marker, count });
        }
    }
    Ok(hits)
}

/// The retired labels as one line: `marker (count)`, comma-separated.
pub(crate) fn list(hits: &[RetiredHit]) -> String {
    hits.iter()
        .map(RetiredHit::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}
