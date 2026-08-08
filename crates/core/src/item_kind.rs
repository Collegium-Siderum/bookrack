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
    /// Every pipeline, in declaration order. Lets a caller walk the
    /// vocabulary instead of transcribing it, so a new pipeline reaches
    /// the walkers without a second edit.
    pub const ALL: [ItemKind; 3] = [ItemKind::Book, ItemKind::Paper, ItemKind::Reference];

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

    /// The kind a catalog `scope` string names, or `None` when the
    /// string is not one this build writes. Exact inverse of
    /// [`ItemKind::as_scope_str`].
    ///
    /// Derived by walking [`ItemKind::ALL`] rather than by a second
    /// `match`, so the accepted spellings cannot drift from what the
    /// catalog column holds. The comparison is exact: no alias, no
    /// plural, no case folding — a caller that wants to be generous
    /// about near misses classifies them itself, where it can say what
    /// it guessed.
    pub fn from_scope_str(s: &str) -> Option<ItemKind> {
        ItemKind::ALL
            .into_iter()
            .find(|kind| kind.as_scope_str() == s)
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

    /// The two directions agree on every kind. The `match` is
    /// exhaustive on purpose: adding a pipeline breaks the build here
    /// rather than silently leaving its rows unaddressable.
    #[test]
    fn a_scope_string_reads_back_as_the_kind_that_wrote_it() {
        for kind in ItemKind::ALL {
            let scope = match kind {
                ItemKind::Book => "book",
                ItemKind::Paper => "paper",
                ItemKind::Reference => "reference",
            };
            assert_eq!(scope, kind.as_scope_str());
            assert_eq!(ItemKind::from_scope_str(scope), Some(kind));
        }
    }

    /// Only the exact column values name a kind.
    ///
    /// The vocabulary is what an id prefix is parsed against and what a
    /// listing row prints, so a spelling accepted here becomes a
    /// spelling the whole surface accepts. `books` is the command
    /// namespace and `Book` is the variant name — both are near misses
    /// an implementation is tempted to be generous about, and the
    /// generosity belongs where the guess can be reported, not here.
    #[test]
    fn a_near_miss_names_no_kind() {
        for miss in [
            "books",
            "papers",
            "Book",
            "BOOK",
            "",
            " book",
            "reference/x",
        ] {
            assert_eq!(
                ItemKind::from_scope_str(miss),
                None,
                "{miss:?} was accepted as a kind"
            );
        }
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
