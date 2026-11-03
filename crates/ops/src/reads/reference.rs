// SPDX-License-Identifier: Apache-2.0

//! Reference-book entry lookup.
//!
//! One keyed read over a library's `reference.db`: exact key, then the
//! latin-key fallback, then a single redirect hop, all of it inside
//! [`Refs::lookup`]. This module owns the argument validation and the
//! projection that the MCP `reference.lookup` tool and the control
//! plane's method of the same name both apply, so the two surfaces
//! cannot drift apart in what they accept or return.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

use bookrack_distill::{Catalogs, ParseError};
use bookrack_refs::{LookupResult, Refs, RefsError, ResolvedEntry};
use serde::Deserialize;

/// A keyed lookup request. The field set is the `reference.lookup`
/// argument shape without the library selector, which each surface
/// resolves to a store path before calling in.
#[derive(Debug, Clone, Deserialize)]
pub struct ReferenceLookup {
    /// Book slug to scope the lookup to, or `*` for every registered
    /// book ranked by `authority_rank`.
    pub book: String,
    /// The lookup key, tried as given and then in its latin form.
    pub entry_key: String,
    /// Optional payload-key whitelist. When `Some`, payload keys
    /// outside the list are stripped from each hit.
    pub fields: Option<Vec<String>>,
    /// Optional quality-flag severity floor: `ok`, `info`, `warn` or
    /// `error`. Hits whose flags carry no concern at or above it drop
    /// out.
    pub min_severity: Option<String>,
    /// Book slugs whose entries must not be returned. Only admissible
    /// with `book = "*"`; a single-book scope has nothing to exclude.
    pub exclude_books: Option<Vec<String>>,
}

/// Why a lookup failed.
#[derive(Debug, thiserror::Error)]
pub enum ReferenceLookupError {
    /// A request field is outside what the lookup accepts. The message
    /// names the field.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// The reference store could not be opened or queried.
    #[error("refs error: {0}")]
    Store(#[from] RefsError),

    /// Looking for the store at `path` failed for a reason other than
    /// its absence.
    #[error("cannot inspect the reference store at {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The controlled-vocabulary catalogs embedded in the build did not
    /// parse.
    #[error("catalog error: {0}")]
    Catalog(#[from] ParseError),
}

/// The storage-independent part of a request, checked by
/// [`validate`].
#[derive(Debug, Clone, Copy)]
pub struct Validated<'a> {
    /// `None` for the every-book scope.
    pub book: Option<&'a str>,
    /// Books to leave out; empty when none were named.
    pub exclude: &'a [String],
    /// The severity floor as a rank, when one was asked for.
    pub min_severity: Option<u8>,
}

/// The process-wide controlled vocabulary triad, loaded on first use.
/// A load failure is returned rather than cached, so a later call
/// retries and a corrected catalog recovers without a restart.
pub fn catalogs() -> Result<&'static Catalogs, ReferenceLookupError> {
    static CELL: OnceLock<Catalogs> = OnceLock::new();
    if let Some(c) = CELL.get() {
        return Ok(c);
    }
    let loaded = Catalogs::load_all()?;
    Ok(CELL.get_or_init(|| loaded))
}

/// Check everything about `req` that does not need the store: the
/// scope, the exclusion list against the scope, and the severity
/// word.
pub fn validate(req: &ReferenceLookup) -> Result<Validated<'_>, ReferenceLookupError> {
    let book = scope(req)?;
    let exclude = exclusions(req, book)?;
    let min_severity = severity_floor(req)?;
    Ok(Validated {
        book,
        exclude,
        min_severity,
    })
}

/// Run the lookup against an open store and project the result
/// through the `min_severity` filter and the `fields` whitelist.
///
/// The checks interleave with the query in this order: scope and
/// exclusions before the store is read, the severity word after it.
pub fn lookup(
    refs: &Refs,
    catalogs: &Catalogs,
    req: &ReferenceLookup,
) -> Result<LookupResult, ReferenceLookupError> {
    let book = scope(req)?;
    let exclude = exclusions(req, book)?;

    let mut result = refs.lookup(book, &req.entry_key, exclude)?;

    if let Some(min) = severity_floor(req)? {
        result
            .hits
            .retain(|hit| passes_severity(hit, catalogs, min));
        result.primary_by_authority = (!result.hits.is_empty()).then_some(0);
    }

    if let Some(fields) = req.fields.as_deref() {
        let allowed: BTreeSet<&str> = fields.iter().map(String::as_str).collect();
        for hit in &mut result.hits {
            if let Some(obj) = hit.payload.as_object_mut() {
                obj.retain(|k, _| allowed.contains(k.as_str()));
            }
        }
    }

    Ok(result)
}

fn scope(req: &ReferenceLookup) -> Result<Option<&str>, ReferenceLookupError> {
    if req.book == "*" {
        Ok(None)
    } else if req.book.is_empty() {
        Err(ReferenceLookupError::InvalidArgument(
            "`book` must be a slug or `*`; got empty string".to_string(),
        ))
    } else {
        Ok(Some(req.book.as_str()))
    }
}

fn exclusions<'a>(
    req: &'a ReferenceLookup,
    book: Option<&str>,
) -> Result<&'a [String], ReferenceLookupError> {
    let exclude = req.exclude_books.as_deref().unwrap_or(&[]);
    if !exclude.is_empty() && book.is_some() {
        return Err(ReferenceLookupError::InvalidArgument(format!(
            "`exclude_books` only applies with book=\"*\"; got book={:?}",
            req.book
        )));
    }
    Ok(exclude)
}

fn severity_floor(req: &ReferenceLookup) -> Result<Option<u8>, ReferenceLookupError> {
    req.min_severity
        .as_deref()
        .map(|word| {
            severity_level(word).ok_or_else(|| {
                ReferenceLookupError::InvalidArgument(format!(
                    "unknown min_severity {word:?}; \
                     expected ok | info | warn | error"
                ))
            })
        })
        .transpose()
}

fn passes_severity(hit: &ResolvedEntry, catalogs: &Catalogs, min: u8) -> bool {
    if hit.quality_flags.is_empty() {
        // Unflagged hits carry no concerns; they survive every
        // severity floor because there is nothing to filter against.
        return true;
    }
    hit.quality_flags.iter().any(|flag| {
        catalogs
            .quality_flags
            .entries
            .get(flag)
            .map(|spec| severity_level(&spec.severity).unwrap_or(0))
            .unwrap_or(0)
            >= min
    })
}

fn severity_level(s: &str) -> Option<u8> {
    match s {
        "ok" => Some(0),
        "info" => Some(1),
        "warn" => Some(2),
        "error" => Some(3),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(book: &str, entry_key: &str) -> ReferenceLookup {
        ReferenceLookup {
            book: book.to_string(),
            entry_key: entry_key.to_string(),
            fields: None,
            min_severity: None,
            exclude_books: None,
        }
    }

    fn invalid_argument(err: ReferenceLookupError) -> String {
        match err {
            ReferenceLookupError::InvalidArgument(message) => message,
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn validate_refuses_an_empty_book() {
        let message = invalid_argument(validate(&request("", "smith")).unwrap_err());
        assert_eq!(message, "`book` must be a slug or `*`; got empty string");
    }

    #[test]
    fn validate_refuses_exclusions_on_a_single_book_scope() {
        let mut req = request("book_a", "smith");
        req.exclude_books = Some(vec!["book_b".to_string()]);
        let message = invalid_argument(validate(&req).unwrap_err());
        assert_eq!(
            message,
            "`exclude_books` only applies with book=\"*\"; got book=\"book_a\""
        );

        // An empty list is not a request to exclude anything.
        req.exclude_books = Some(vec![]);
        assert!(validate(&req).is_ok());
    }

    #[test]
    fn validate_refuses_an_unknown_severity_word() {
        let mut req = request("*", "smith");
        req.min_severity = Some("severe".to_string());
        let message = invalid_argument(validate(&req).unwrap_err());
        assert_eq!(
            message,
            "unknown min_severity \"severe\"; expected ok | info | warn | error"
        );
    }

    #[test]
    fn validate_ranks_the_severity_words() {
        for (word, rank) in [("ok", 0), ("info", 1), ("warn", 2), ("error", 3)] {
            let mut req = request("*", "smith");
            req.min_severity = Some(word.to_string());
            let validated = validate(&req).unwrap();
            assert_eq!(validated.min_severity, Some(rank), "{word}");
            assert_eq!(validated.book, None);
            assert!(validated.exclude.is_empty());
        }
    }
}
