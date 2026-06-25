// SPDX-License-Identifier: Apache-2.0

//! Reference-book MCP surface.
//!
//! Mirrors mother doc §5.6 (`reference.lookup`) and §5.8
//! (`reference.overlay_set`). The two `pub(super)` logic helpers run
//! the actual queries against an open [`Refs`] handle; the
//! [`crate::BookrackServer`] tool methods are thin shims that open
//! [`Refs`], read [`Catalogs`] from the process-wide cache, dispatch
//! to one of these helpers, and serialize the result.

use std::sync::OnceLock;

use bookrack_distill::Catalogs;
use bookrack_refs::{LookupResult, NewOverlay, Refs, ResolvedEntry};
use rmcp::schemars;
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Argument shape for the `reference.lookup` tool. Mirrors mother doc
/// §5.6 / §5.10. `library` is `Option<String>` to honour the same
/// default-or-explicit semantics used by every other read tool.
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
    /// `error` (mother doc §5.11).
    pub min_severity: Option<String>,
}

/// Argument shape for the `reference.overlay_set` tool. Mirrors
/// mother doc §5.8. `library` is required for write tools, so this
/// carries a bare `String` rather than `Option<String>`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReferenceOverlaySetArgs {
    pub library: String,
    pub book_slug: String,
    pub entry_key: String,
    /// JSON object whose keys must be present in the property
    /// catalog (`crates/distill/data/property_catalog.toml`).
    pub overlay: JsonValue,
    /// Free-text edit reason recorded on the overlay row (mother
    /// doc §5.8 borrowed from MediaWiki edit summary).
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

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("overlay property {key:?} is not in property_catalog.toml")]
    UnknownOverlayProperty { key: String },
}

/// The process-wide controlled vocabulary triad. Loaded lazily on
/// first MCP request that needs it. A load failure is returned as
/// `ReferenceError::Catalog` rather than panicking, and is not
/// cached: a subsequent request retries the load so a corrected
/// `property_catalog.toml` can recover without a daemon restart.
pub(crate) fn catalogs() -> Result<&'static Catalogs, ReferenceError> {
    static CELL: OnceLock<Catalogs> = OnceLock::new();
    if let Some(c) = CELL.get() {
        return Ok(c);
    }
    let loaded = Catalogs::load_all()?;
    Ok(CELL.get_or_init(|| loaded))
}

// ---------------------------------------------------------------------------
// reference.lookup
// ---------------------------------------------------------------------------

/// Run `Refs::lookup` and project the result through the `fields`
/// whitelist and the `min_severity` filter declared on `args`.
pub(crate) fn reference_lookup_logic(
    refs: &Refs,
    catalogs: &Catalogs,
    args: &ReferenceLookupArgs,
) -> Result<LookupResult, ReferenceError> {
    let book = if args.book == "*" {
        None
    } else if args.book.is_empty() {
        return Err(ReferenceError::InvalidArgument(
            "`book` must be a slug or `*`; got empty string".to_string(),
        ));
    } else {
        Some(args.book.as_str())
    };

    let mut result = refs.lookup(book, &args.entry_key)?;

    if let Some(min_severity) = args.min_severity.as_deref() {
        let min = severity_level(min_severity).ok_or_else(|| {
            ReferenceError::InvalidArgument(format!(
                "unknown min_severity {min_severity:?}; \
                 expected ok | info | warn | error"
            ))
        })?;
        result
            .hits
            .retain(|hit| passes_severity(hit, catalogs, min));
        result.primary_by_authority = (!result.hits.is_empty()).then_some(0);
    }

    if let Some(fields) = args.fields.as_deref() {
        let allowed: std::collections::BTreeSet<&str> = fields.iter().map(String::as_str).collect();
        for hit in &mut result.hits {
            if let Some(obj) = hit.payload.as_object_mut() {
                obj.retain(|k, _| allowed.contains(k.as_str()));
            }
        }
    }

    Ok(result)
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
