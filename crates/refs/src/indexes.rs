// SPDX-License-Identifier: Apache-2.0

//! Per-book physical lookup paths attached to `reference_entries`.
//!
//! Each [`crate::types::IndexSpec`] from a book.toml `[[indexes]]`
//! entry becomes:
//!
//! 1. A `VIRTUAL` generated column on `reference_entries` named
//!    `gencol__<enc_slug>__<enc_field>`, projecting one path out of
//!    `payload_json` via `json_extract`. `STORED` would be preferred
//!    but bundled SQLite's `ALTER TABLE ADD COLUMN` rejects `STORED`
//!    (see `crates/dbkit/tests/generated_column.rs`); `VIRTUAL` is
//!    functionally equivalent here because the `CREATE INDEX` below
//!    materializes the projected values into a B-tree that point
//!    lookups hit directly.
//!
//! 2. A partial B-tree index `ix_ref__<enc_slug>__<enc_field>` over
//!    that column, restricted to `WHERE book_slug = '<slug>'` so the
//!    index pages cover only rows from the registering book.
//!
//! `enc_slug` and `enc_field` are the slug and field passed through
//! a char-level encoding that maps `_` to `_u` and `.` to `_d`,
//! leaving the rest untouched. The encoding is injective and its
//! output never contains the substring `__`, so the `__` joiner is
//! an unambiguous section boundary. This keeps the produced physical
//! names within `[a-z0-9_]` (so they need no SQL quoting) while
//! preventing the previous collision where `field = "a.b"` and
//! `field = "a_b"`, or `slug = "a"` + `field = "b_c"` and
//! `slug = "a_b"` + `field = "c"`, mapped to the same physical name
//! and the second declaration was silently dropped by the
//! `duplicate column name` branch.
//!
//! The function is idempotent: re-registering the same book is a
//! no-op. Re-registering with one fewer index leaves the dropped
//! column and index in place (DROP is out of scope for v1; a future
//! migration will manage column removal explicitly).

use std::collections::HashSet;

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
///
/// A second declaration of the same `field` within `specs` is rejected
/// with [`RefsError::DuplicateIndex`]: the previous encoding would
/// have masked it as a `duplicate column name` no-op and silently
/// dropped the second spec.
pub fn apply(conn: &Connection, book_slug: &str, specs: &[IndexSpec]) -> Result<(), RefsError> {
    validate_slug(book_slug)?;

    let mut seen: HashSet<&str> = HashSet::with_capacity(specs.len());
    for spec in specs {
        validate_field(&spec.field)?;
        if !seen.insert(spec.field.as_str()) {
            return Err(RefsError::DuplicateIndex(spec.field.clone()));
        }
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

/// `gencol__<enc_slug>__<enc_field>`.
fn column_name(book_slug: &str, field: &str) -> String {
    format!(
        "gencol__{}__{}",
        encode_segment(book_slug),
        encode_segment(field)
    )
}

/// `ix_ref__<enc_slug>__<enc_field>`.
fn index_name(book_slug: &str, field: &str) -> String {
    format!(
        "ix_ref__{}__{}",
        encode_segment(book_slug),
        encode_segment(field)
    )
}

/// Char-level encoding used to splice a `book_slug` and an
/// `IndexSpec::field` into a single SQL identifier without losing
/// information across the boundary.
///
/// - `'_'` becomes `"_u"` (literal underscore in input).
/// - `'.'` becomes `"_d"` (segment separator in dotted field paths).
/// - Everything else passes through.
///
/// `validate_slug` guarantees the input is a subset of `[a-z0-9_]`,
/// and `validate_field` extends that to `[a-z0-9_]` plus `.`, so the
/// `match` covers every character that can reach this function. The
/// output is in `[a-z0-9_]` and, by construction, contains no `__`
/// substring: every `_` in the output is immediately followed by
/// either `u` or `d`. The caller can therefore use `__` as an
/// unambiguous section joiner between encoded segments.
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '_' => out.push_str("_u"),
            '.' => out.push_str("_d"),
            _ => out.push(c),
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pairs that collided under the previous `replace('.', "_")`
    /// scheme. The fix is an injective encoding, so each pair must
    /// produce distinct physical names.
    #[test]
    fn column_name_separates_known_collision_pairs() {
        // Within one book, a dotted vs. underscored field.
        assert_ne!(column_name("book", "a.b"), column_name("book", "a_b"));
        assert_ne!(
            column_name("book", "foo.bar.baz"),
            column_name("book", "foo_bar_baz")
        );

        // Cross-book: the boundary between encoded slug and encoded
        // field must stay unambiguous even when one side carries an
        // underscore and the other does not.
        assert_ne!(column_name("a_b", "c"), column_name("a", "b_c"));
        assert_ne!(
            column_name("book_a", "year_span.birth"),
            column_name("book", "a_year_span_birth")
        );

        // The same pairs apply to the index name builder.
        assert_ne!(index_name("book", "a.b"), index_name("book", "a_b"));
        assert_ne!(index_name("a_b", "c"), index_name("a", "b_c"));
    }

    /// The encoded form must never contain the `__` joiner substring;
    /// otherwise the section boundary loses its meaning.
    #[test]
    fn encode_segment_never_emits_double_underscore() {
        for s in [
            "a",
            "a_b",
            "a__b",
            "a.b",
            "a..b",
            "year_span.birth",
            "country",
            "book_slug_with_lots_of_underscores",
            "deeply.nested.path.with_underscore",
        ] {
            let enc = encode_segment(s);
            assert!(
                !enc.contains("__"),
                "encode_segment({s:?}) = {enc:?} contains __"
            );
        }
    }

    /// Exhaustive uniqueness check over a small alphabet. This stands
    /// in for the property test called for by the 0.8 bug-fix plan
    /// without pulling a new dev-dependency into the workspace.
    #[test]
    fn column_name_is_injective_on_small_alphabet() {
        let alphabet = ["a", "ab", "a_b", "a.b", "ab_c", "a_b_c", "a.b.c"];
        let mut seen: HashSet<String> = HashSet::new();
        for slug in &alphabet {
            for field in &alphabet {
                if validate_slug(slug).is_err() || validate_field(field).is_err() {
                    continue;
                }
                let name = column_name(slug, field);
                assert!(
                    seen.insert(name.clone()),
                    "collision: column_name({slug:?}, {field:?}) = {name:?}"
                );
            }
        }
    }

    #[test]
    fn apply_rejects_duplicate_field_in_specs() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        conn.execute_batch(
            "CREATE TABLE reference_entries (\
                 book_slug TEXT NOT NULL, \
                 payload_json TEXT NOT NULL\
             );",
        )
        .expect("create reference_entries");

        let specs = vec![
            IndexSpec {
                field: "country".to_string(),
                kind: IndexKind::Btree,
            },
            IndexSpec {
                field: "country".to_string(),
                kind: IndexKind::Btree,
            },
        ];

        let err = apply(&conn, "book", &specs).expect_err("duplicate must error");
        match err {
            RefsError::DuplicateIndex(field) => assert_eq!(field, "country"),
            other => panic!("expected DuplicateIndex, got {other:?}"),
        }
    }
}
