// SPDX-License-Identifier: Apache-2.0

//! Reference-book MCP surface.
//!
//! Backs the `reference.lookup` and `reference.overlay_set` tools.
//! The lookup's validation and projection live in
//! [`bookrack_ops::reads::reference`]; this module holds the MCP
//! argument shapes, the overlay logic, and the forwarding shims the
//! [`crate::BookrackServer`] tool methods call after opening
//! [`Refs`] and reading [`Catalogs`] from the process-wide cache.

use std::io;
use std::path::PathBuf;

use bookrack_distill::Catalogs;
use bookrack_ops::reads::reference::{self as lookup, ReferenceLookup, ReferenceLookupError};
use bookrack_refs::{LookupResult, NewOverlay, Refs};
use rmcp::schemars;
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Argument shape for the `reference.lookup` tool. `library` is
/// `Option<String>` to honour the same default-or-explicit semantics
/// used by every other read tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReferenceLookupArgs {
    pub library: Option<String>,
    /// Book slug to scope the lookup to. Pass `"*"` to query every
    /// registered book and rank cross-book hits by `authority_rank`.
    pub book: String,
    /// The lookup key; resolves directly against
    /// `reference_entries.entry_key`.
    pub entry_key: String,
    /// Optional payload-key whitelist. When `Some`, payload keys
    /// outside the list are stripped from each hit.
    pub fields: Option<Vec<String>>,
    /// Optional quality_flag-severity floor. When set, hits whose
    /// flags do not include at least one flag at or above this
    /// severity drop out. Recognised values: `ok`, `info`, `warn`,
    /// `error`.
    pub min_severity: Option<String>,
    /// Book slugs whose entries must not be returned. Only meaningful
    /// with `book = "*"`, where the lookup spans every registered
    /// book; passing it alongside a single-book scope is an error
    /// rather than a no-op. An excluded book is also skipped as a
    /// redirect target, so it cannot re-enter the result that way.
    pub exclude_books: Option<Vec<String>>,
}

impl ReferenceLookupArgs {
    /// The request without the library selector, in the shape the
    /// ops layer runs.
    pub fn to_lookup(&self) -> ReferenceLookup {
        ReferenceLookup {
            book: self.book.clone(),
            entry_key: self.entry_key.clone(),
            fields: self.fields.clone(),
            min_severity: self.min_severity.clone(),
            exclude_books: self.exclude_books.clone(),
        }
    }
}

/// Argument shape for the `reference.overlay_set` tool. `library` is
/// required for write tools, so this carries a bare `String` rather
/// than `Option<String>`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReferenceOverlaySetArgs {
    pub library: String,
    pub book_slug: String,
    pub entry_key: String,
    /// JSON object whose keys must be present in the property
    /// catalog (`crates/distill/data/property_catalog.toml`).
    pub overlay: JsonValue,
    /// Free-text edit summary recorded on the overlay row.
    pub reason: String,
}

/// Reply shape for `reference.overlay_set`. Always returns a small
/// receipt so MCP clients do not have to special-case empty bodies.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReferenceOverlaySetResult {
    pub book_slug: String,
    pub entry_key: String,
    pub edited_at: String,
}

/// Errors raised by the reference logic helpers. Maps to MCP
/// `internal_error` / `invalid_params` at the tool-method layer.
#[derive(Debug, thiserror::Error)]
pub enum ReferenceError {
    #[error("refs error: {0}")]
    Refs(#[from] bookrack_refs::RefsError),

    #[error("catalog error: {0}")]
    Catalog(#[from] bookrack_distill::ParseError),

    /// Looking for the reference store failed for a reason other than
    /// its absence.
    #[error("cannot inspect the reference store at {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("overlay property {key:?} is not in property_catalog.toml")]
    UnknownOverlayProperty { key: String },
}

impl From<ReferenceLookupError> for ReferenceError {
    fn from(e: ReferenceLookupError) -> Self {
        match e {
            ReferenceLookupError::InvalidArgument(m) => ReferenceError::InvalidArgument(m),
            ReferenceLookupError::Store(e) | ReferenceLookupError::Open { source: e, .. } => {
                ReferenceError::Refs(e)
            }
            ReferenceLookupError::Io { path, source } => ReferenceError::Io { path, source },
            ReferenceLookupError::Catalog(e) => ReferenceError::Catalog(e),
        }
    }
}

/// The process-wide controlled vocabulary triad, shared with the ops
/// layer's lookup.
pub(crate) fn catalogs() -> Result<&'static Catalogs, ReferenceError> {
    Ok(lookup::catalogs()?)
}

// ---------------------------------------------------------------------------
// reference.lookup
// ---------------------------------------------------------------------------

/// Run the ops-layer lookup for `args` against an open store.
///
/// The tool method reads through `lookup_at`, which opens the store
/// itself; this entry serves callers that already hold one open.
#[allow(dead_code)]
pub(crate) fn reference_lookup_logic(
    refs: &Refs,
    catalogs: &Catalogs,
    args: &ReferenceLookupArgs,
) -> Result<LookupResult, ReferenceError> {
    Ok(lookup::lookup(refs, catalogs, &args.to_lookup())?)
}

// ---------------------------------------------------------------------------
// reference.overlay_set
// ---------------------------------------------------------------------------

/// Validate the overlay payload against the property catalog and
/// upsert it. Returns the timestamp stamped onto the row.
pub(crate) fn reference_overlay_set_logic(
    refs: &Refs,
    catalogs: &Catalogs,
    args: &ReferenceOverlaySetArgs,
    edited_at: String,
) -> Result<ReferenceOverlaySetResult, ReferenceError> {
    let obj = args.overlay.as_object().ok_or_else(|| {
        ReferenceError::InvalidArgument(
            "overlay must be a JSON object whose keys are property catalog keys".to_string(),
        )
    })?;
    for key in obj.keys() {
        if !catalogs.properties.entries.contains_key(key) {
            return Err(ReferenceError::UnknownOverlayProperty { key: key.clone() });
        }
    }

    let overlay = NewOverlay {
        book_slug: args.book_slug.clone(),
        entry_key: args.entry_key.clone(),
        overlay: args.overlay.clone(),
        quality_flags: None,
        base_built_at: None,
        edit_reason: Some(args.reason.clone()),
        edited_at: edited_at.clone(),
    };
    refs.upsert_overlay(&overlay)?;

    Ok(ReferenceOverlaySetResult {
        book_slug: args.book_slug.clone(),
        entry_key: args.entry_key.clone(),
        edited_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_refs::{NewBook, NewEntry};
    use serde_json::json;

    fn book(slug: &str, rank: i64, built_at: &str) -> NewBook {
        NewBook {
            book_slug: slug.to_string(),
            schema_name: "name_translation".to_string(),
            schema_version: 1,
            parser_version: "0.1.0".to_string(),
            title_zh: format!("book {slug}"),
            title_en: None,
            edition: None,
            publisher: None,
            year: None,
            isbn: None,
            authority_rank: rank,
            built_at: built_at.to_string(),
            intake_id: None,
        }
    }

    fn entry(slug: &str, key: &str, payload: JsonValue, quality_flags: Vec<&str>) -> NewEntry {
        NewEntry {
            book_slug: slug.to_string(),
            entry_key: key.to_string(),
            headword: key.to_string(),
            aliases: vec![],
            payload,
            fts_text: key.to_string(),
            source: json!({
                "book_slug": slug,
                "page": 1,
                "sheet": 1,
                "distill_run_id": "2026-06-25T00:00:00Z",
            }),
            quality_flags: quality_flags.into_iter().map(String::from).collect(),
        }
    }

    fn lookup(book: &str, entry_key: &str) -> ReferenceLookupArgs {
        ReferenceLookupArgs {
            library: None,
            book: book.to_string(),
            entry_key: entry_key.to_string(),
            fields: None,
            min_severity: None,
            exclude_books: None,
        }
    }

    /// [`lookup`] with an exclusion list attached.
    fn lookup_excluding(book: &str, entry_key: &str, exclude: &[&str]) -> ReferenceLookupArgs {
        ReferenceLookupArgs {
            exclude_books: Some(exclude.iter().map(|s| s.to_string()).collect()),
            ..lookup(book, entry_key)
        }
    }

    #[test]
    fn cross_book_lookup_orders_hits_by_authority_rank() {
        let refs = Refs::open_in_memory().expect("refs");
        refs.upsert_book(&book("high_authority", 10, "2026-06-25T00:00:00Z"))
            .unwrap();
        refs.upsert_book(&book("low_authority", 3, "2026-06-25T00:01:00Z"))
            .unwrap();
        refs.upsert_entry(&entry(
            "high_authority",
            "smith",
            json!({"country": "USA"}),
            vec![],
        ))
        .unwrap();
        refs.upsert_entry(&entry(
            "low_authority",
            "smith",
            json!({"country": "UK"}),
            vec![],
        ))
        .unwrap();

        let cats = Catalogs::load_all().unwrap();
        let result = reference_lookup_logic(&refs, &cats, &lookup("*", "smith")).unwrap();
        assert_eq!(result.hits.len(), 2);
        assert_eq!(result.hits[0].book_slug, "high_authority");
        assert_eq!(result.primary_by_authority, Some(0));
        assert_eq!(result.redirect_followed, None);
    }

    #[test]
    fn lookup_follows_redirect_and_surfaces_the_original_key() {
        let refs = Refs::open_in_memory().expect("refs");
        refs.upsert_book(&book("book_a", 10, "2026-06-25T00:00:00Z"))
            .unwrap();
        refs.upsert_entry(&entry(
            "book_a",
            "redirect_source",
            json!({"redirect_to": "target"}),
            vec![],
        ))
        .unwrap();
        refs.upsert_entry(&entry(
            "book_a",
            "target",
            json!({"country": "USA"}),
            vec![],
        ))
        .unwrap();

        let cats = Catalogs::load_all().unwrap();
        let result =
            reference_lookup_logic(&refs, &cats, &lookup("book_a", "redirect_source")).unwrap();
        assert_eq!(result.entry_key, "redirect_source");
        assert_eq!(result.redirect_followed.as_deref(), Some("redirect_source"));
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].entry_key, "target");
    }

    #[test]
    fn an_empty_book_is_rejected_rather_than_read_as_a_wildcard() {
        let refs = Refs::open_in_memory().expect("refs");
        let cats = Catalogs::load_all().unwrap();
        let err = reference_lookup_logic(&refs, &cats, &lookup("", "smith"))
            .expect_err("an empty book scope is refused");
        assert!(
            matches!(&err, ReferenceError::InvalidArgument(m) if m.contains("empty string")),
            "got {err:?}"
        );
    }

    #[test]
    fn wildcard_lookup_drops_the_excluded_books() {
        let refs = Refs::open_in_memory().expect("refs");
        refs.upsert_book(&book("high_authority", 10, "2026-06-25T00:00:00Z"))
            .unwrap();
        refs.upsert_book(&book("low_authority", 3, "2026-06-25T00:01:00Z"))
            .unwrap();
        refs.upsert_entry(&entry(
            "high_authority",
            "smith",
            json!({"country": "USA"}),
            vec![],
        ))
        .unwrap();
        refs.upsert_entry(&entry(
            "low_authority",
            "smith",
            json!({"country": "UK"}),
            vec![],
        ))
        .unwrap();

        let cats = Catalogs::load_all().unwrap();
        // Excluding the top-ranked book leaves the other one, and the
        // primary index follows it rather than pointing past the end.
        let result = reference_lookup_logic(
            &refs,
            &cats,
            &lookup_excluding("*", "smith", &["high_authority"]),
        )
        .unwrap();
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].book_slug, "low_authority");
        assert_eq!(result.primary_by_authority, Some(0));
    }

    #[test]
    fn exclude_books_is_refused_outside_the_wildcard_scope() {
        let refs = Refs::open_in_memory().expect("refs");
        let cats = Catalogs::load_all().unwrap();
        let err = reference_lookup_logic(
            &refs,
            &cats,
            &lookup_excluding("book_a", "smith", &["book_b"]),
        )
        .expect_err("a single-book scope has nothing to exclude");
        assert!(
            matches!(&err, ReferenceError::InvalidArgument(m) if m.contains("exclude_books")),
            "got {err:?}"
        );

        // An empty list is not a request to exclude anything, so it is
        // admissible in either scope.
        assert!(
            reference_lookup_logic(&refs, &cats, &lookup_excluding("book_a", "smith", &[])).is_ok()
        );
    }

    #[test]
    fn min_severity_warn_drops_info_only_hits_and_keeps_warn_hits() {
        let refs = Refs::open_in_memory().expect("refs");
        refs.upsert_book(&book("book_low", 10, "2026-06-25T00:00:00Z"))
            .unwrap();
        refs.upsert_book(&book("book_high", 5, "2026-06-25T00:01:00Z"))
            .unwrap();
        // book_low's hit has only an info-severity flag; book_high's
        // hit has a warn-severity flag. min_severity=warn must drop
        // the first and keep the second.
        refs.upsert_entry(&entry(
            "book_low",
            "smith",
            json!({"country": "USA"}),
            vec!["spliced_from_orphan"],
        ))
        .unwrap();
        refs.upsert_entry(&entry(
            "book_high",
            "smith",
            json!({"country": "UK"}),
            vec!["pair_mismatch"],
        ))
        .unwrap();

        let cats = Catalogs::load_all().unwrap();
        let mut args = lookup("*", "smith");
        args.min_severity = Some("warn".to_string());
        let result = reference_lookup_logic(&refs, &cats, &args).unwrap();
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].book_slug, "book_high");
        assert_eq!(result.primary_by_authority, Some(0));
    }

    #[test]
    fn fields_whitelist_strips_unrequested_payload_keys() {
        let refs = Refs::open_in_memory().expect("refs");
        refs.upsert_book(&book("book_a", 10, "2026-06-25T00:00:00Z"))
            .unwrap();
        refs.upsert_entry(&entry(
            "book_a",
            "smith",
            json!({"country": "USA", "year_span": {"birth": 1900}, "bio_annotation": "extra"}),
            vec![],
        ))
        .unwrap();

        let cats = Catalogs::load_all().unwrap();
        let mut args = lookup("book_a", "smith");
        args.fields = Some(vec!["country".to_string()]);
        let result = reference_lookup_logic(&refs, &cats, &args).unwrap();
        let payload = result.hits[0].payload.as_object().unwrap();
        assert!(payload.contains_key("country"));
        assert!(!payload.contains_key("year_span"));
        assert!(!payload.contains_key("bio_annotation"));
    }

    #[test]
    fn overlay_set_rejects_a_non_catalog_property_key() {
        let refs = Refs::open_in_memory().expect("refs");
        refs.upsert_book(&book("book_a", 10, "2026-06-25T00:00:00Z"))
            .unwrap();
        refs.upsert_entry(&entry("book_a", "smith", json!({"country": "USA"}), vec![]))
            .unwrap();

        let cats = Catalogs::load_all().unwrap();
        let args = ReferenceOverlaySetArgs {
            library: "lib".to_string(),
            book_slug: "book_a".to_string(),
            entry_key: "smith".to_string(),
            overlay: json!({"random_key": "value"}),
            reason: "test".to_string(),
        };
        let err =
            reference_overlay_set_logic(&refs, &cats, &args, "2026-06-25T01:00:00Z".to_string())
                .unwrap_err();
        match err {
            ReferenceError::UnknownOverlayProperty { key } => {
                assert_eq!(key, "random_key");
            }
            other => panic!("expected UnknownOverlayProperty, got {other:?}"),
        }
    }

    #[test]
    fn overlay_set_accepts_a_catalog_property_and_lookup_reads_the_patch() {
        let refs = Refs::open_in_memory().expect("refs");
        refs.upsert_book(&book("book_a", 10, "2026-06-25T00:00:00Z"))
            .unwrap();
        refs.upsert_entry(&entry("book_a", "smith", json!({"country": "USA"}), vec![]))
            .unwrap();

        let cats = Catalogs::load_all().unwrap();
        let args = ReferenceOverlaySetArgs {
            library: "lib".to_string(),
            book_slug: "book_a".to_string(),
            entry_key: "smith".to_string(),
            overlay: json!({"country": "United States"}),
            reason: "fix the OCR confusion".to_string(),
        };
        let receipt =
            reference_overlay_set_logic(&refs, &cats, &args, "2026-06-25T01:00:00Z".to_string())
                .unwrap();
        assert_eq!(receipt.book_slug, "book_a");
        assert_eq!(receipt.entry_key, "smith");

        let result = reference_lookup_logic(&refs, &cats, &lookup("book_a", "smith")).unwrap();
        assert_eq!(result.hits[0].payload["country"], "United States");
        assert!(result.hits[0].has_overlay);
    }
}
