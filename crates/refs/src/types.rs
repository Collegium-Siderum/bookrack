// SPDX-License-Identifier: Apache-2.0

//! POD value types crossing the `Refs` surface.
//!
//! The inputs (`NewBook`, `NewEntry`, `NewOverlay`, `IndexSpec`) carry
//! payloads as `serde_json::Value`; the read shapes (`ResolvedEntry`,
//! `LookupResult`) project them back from the resolved view. None of
//! these types touch the database; they are translated to and from row
//! bindings in `lib.rs`.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Registration record for a single reference book.
///
/// `entry_count` and `parse_warnings` are not exposed: they are
/// maintenance counters owned by `Refs` and updated as entries are
/// upserted. `built_at` is the caller's responsibility (the distill
/// run's ISO-8601 timestamp).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewBook {
    pub book_slug: String,
    pub schema_name: String,
    pub schema_version: i64,
    pub parser_version: String,
    pub title_zh: String,
    pub title_en: Option<String>,
    pub edition: Option<String>,
    pub publisher: Option<String>,
    pub year: Option<i64>,
    pub isbn: Option<String>,
    pub authority_rank: i64,
    pub built_at: String,
    pub intake_id: Option<i64>,
}

/// One distilled entry, as written by the distill pipeline into the
/// base layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewEntry {
    pub book_slug: String,
    pub entry_key: String,
    pub headword: String,
    pub aliases: Vec<String>,
    pub payload: JsonValue,
    pub fts_text: String,
    pub source: JsonValue,
    pub quality_flags: Vec<String>,
}

/// One user-authored overlay record layered on top of a base entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewOverlay {
    pub book_slug: String,
    pub entry_key: String,
    pub overlay: JsonValue,
    pub quality_flags: Option<Vec<String>>,
    pub base_built_at: Option<String>,
    pub edit_reason: Option<String>,
    pub edited_at: String,
}

/// One row of the `reference_entries_resolved` view: base payload
/// merged with the overlay through `json_patch`, with the overlay's
/// presence surfaced as `has_overlay`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedEntry {
    pub book_slug: String,
    pub entry_key: String,
    pub headword: String,
    pub aliases: Vec<String>,
    pub payload: JsonValue,
    pub source: JsonValue,
    pub quality_flags: Vec<String>,
    pub edit_reason: Option<String>,
    pub edited_at: Option<String>,
    pub has_overlay: bool,
}

/// The reply shape of [`crate::Refs::lookup`]: a disambiguation array
/// borrowed from Wikipedia's dab page. Always returned even for a
/// single-hit lookup, so callers do not branch on cardinality.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LookupResult {
    /// The `entry_key` the caller asked for. If a redirect was
    /// followed, this remains the original key (the followed-to key
    /// appears as each hit's `entry_key`).
    pub entry_key: String,
    pub hits: Vec<ResolvedEntry>,
    /// Index into `hits` of the highest-authority entry, or `None`
    /// when `hits` is empty.
    pub primary_by_authority: Option<usize>,
    /// `Some(original_key)` when the lookup followed a redirect hop;
    /// `None` for direct hits or unresolved redirect loops.
    pub redirect_followed: Option<String>,
}

/// A single per-book physical lookup path: one path inside
/// `payload_json` is exposed as a `VIRTUAL` generated column with a
/// partial index keyed by `book_slug`.
///
/// `book.toml`'s `[[indexes]]` array is deserialised straight into a
/// `Vec<IndexSpec>` at register time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexSpec {
    /// Dotted path inside `payload_json`. e.g. `country`,
    /// `year_span.birth`. Validated by [`crate::indexes::apply`].
    pub field: String,
    #[serde(default)]
    pub kind: IndexKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexKind {
    #[default]
    Btree,
}
