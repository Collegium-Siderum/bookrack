// SPDX-License-Identifier: Apache-2.0

//! Summarise the reference store (`reference.db`): one row of build
//! provenance per distilled book plus the entry and overlay counts.

use std::path::Path;

use bookrack_config::Config;
use bookrack_refs::{Refs, RefsError};
use serde::Serialize;

use crate::Result;
use crate::scrub::Scrubber;

/// Write `<bundle>/refs/summary.json`. The store is opened through
/// its read-only door, which refuses a schema this build does not
/// match in either direction; a reference.db that is missing or fails
/// to open writes `open-error.json` in place of the summary, with the
/// flattened refusal so a newer schema reads differently from a
/// corrupt file.
///
/// The summary carries what a maintainer needs to reproduce a distill
/// build — the schema and parser that produced each book, when, from
/// which intake, and how many entries and warnings it left — and none
/// of the bibliographic columns (title, publisher, ISBN), which name a
/// real book and have no diagnostic value.
pub fn collect(cfg: &Config, bundle_dir: &Path, scrubber: &Scrubber) -> Result<()> {
    let dst = bundle_dir.join("refs");
    std::fs::create_dir_all(&dst)?;

    let reference_db = cfg.reference_db();
    if !reference_db.exists() {
        return super::write_open_error(&dst, &reference_db, None);
    }
    let refs = match Refs::open_read_only(&reference_db) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "diagnose: could not open reference.db read-only");
            return super::write_open_error(
                &dst,
                &reference_db,
                Some(&bookrack_core::error_chain(&e)),
            );
        }
    };

    let summary = summarise(&refs).map_err(RefsError::from)?;
    let mut value = serde_json::to_value(&summary)?;
    scrubber.scrub_value(&mut value);
    let mut text = serde_json::to_string_pretty(&value)?;
    text.push('\n');
    std::fs::write(dst.join("summary.json"), text)?;
    Ok(())
}

/// The build provenance of one distilled reference book.
#[derive(Serialize)]
struct BookBuild {
    book_slug: String,
    schema_name: String,
    schema_version: i64,
    parser_version: String,
    built_at: String,
    intake_id: Option<i64>,
    entry_count: i64,
    parse_warnings: i64,
}

/// The whole-store summary written to `summary.json`.
#[derive(Serialize)]
struct Summary {
    schema_version_on_disk: i64,
    books: Vec<BookBuild>,
    entry_count: i64,
    overlay_count: i64,
}

fn summarise(refs: &Refs) -> std::result::Result<Summary, rusqlite::Error> {
    let conn = refs.connection();
    let schema_version_on_disk: i64 =
        conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let mut stmt = conn.prepare(
        "SELECT book_slug, schema_name, schema_version, parser_version, built_at, \
                intake_id, entry_count, parse_warnings \
         FROM reference_books ORDER BY book_slug",
    )?;
    let books = stmt
        .query_map([], |row| {
            Ok(BookBuild {
                book_slug: row.get(0)?,
                schema_name: row.get(1)?,
                schema_version: row.get(2)?,
                parser_version: row.get(3)?,
                built_at: row.get(4)?,
                intake_id: row.get(5)?,
                entry_count: row.get(6)?,
                parse_warnings: row.get(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let entry_count: i64 = conn.query_row("SELECT COUNT(*) FROM reference_entries", [], |row| {
        row.get(0)
    })?;
    let overlay_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM reference_entry_overlays", [], |row| {
            row.get(0)
        })?;
    Ok(Summary {
        schema_version_on_disk,
        books,
        entry_count,
        overlay_count,
    })
}
