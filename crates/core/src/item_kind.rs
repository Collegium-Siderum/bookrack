// SPDX-License-Identifier: Apache-2.0

//! The pipeline kind of one ingested item.
//!
//! `ItemKind` tags every row in the catalog's per-item tables — book
//! ingest, paper glean, and reference-book distill land into the same
//! physical tables, keyed by the logical address `(intake_id, scope)`,
//! and the scope value disambiguates the pipelines. The enum supersedes
//! the previous stringly-typed `"book"` constant so a stray literal
//! cannot reach the catalog from a caller.
//!
//! This type is **not** the same as [`crate::Scope`], which addresses a
//! position inside one item's node tree (root / partition / leaf). The
//! two are deliberately distinct and live in separate modules.

use serde::{Deserialize, Serialize};

/// Which pipeline produced an ingested item.
///
/// The serde representation is `"book"` / `"paper"` / `"reference"`
/// (the same string the catalog writes into its `scope` column), so a
/// [`ItemKind`] round-trips through any JSON-shaped wire format without
/// a custom derive on the consumer side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    /// A book ingested through the `ingest` pipeline. Default so that
    /// `#[serde(default)]` on a queue-job kind field reads a v1
    /// queue document — written before the field existed — as a
    /// book job.
    #[default]
    Book,
    /// A paper gleaned through the `glean` pipeline.
    Paper,
    /// A reference book distilled through the `distill` pipeline. Its
    /// rows live in `reference.db` rather than `corpus.db` / the vector
    /// store; the catalog still carries its intake + audit metadata.
    Reference,
}

impl ItemKind {
    /// The string the catalog writes into its `scope` column. Returned
    /// as `&'static str` so callers can bind it directly into prepared
    /// SQL parameters or pass it where a `&str` is expected.
    pub const fn as_scope_str(&self) -> &'static str {
        match self {
            ItemKind::Book => "book",
            ItemKind::Paper => "paper",
            ItemKind::Reference => "reference",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_strings_match_the_catalog_column_values() {
        assert_eq!(ItemKind::Book.as_scope_str(), "book");
        assert_eq!(ItemKind::Paper.as_scope_str(), "paper");
        assert_eq!(ItemKind::Reference.as_scope_str(), "reference");
    }

    #[test]
    fn default_is_book() {
        assert_eq!(ItemKind::default(), ItemKind::Book);
    }

    #[test]
    fn reference_round_trips_through_serde() {
        let s = serde_json::to_string(&ItemKind::Reference).unwrap();
        assert_eq!(s, "\"reference\"");
        let back: ItemKind = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ItemKind::Reference);
    }
}
