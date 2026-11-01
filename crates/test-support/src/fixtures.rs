// SPDX-License-Identifier: Apache-2.0

//! Synthetic inputs shared by the suites and the sandbox tool's
//! presets, kept in one place so a fixture has one definition.

/// A two-headword reference book whose pipeline exercises every stage
/// kind the distill runner reports on: its `book.toml`. The slug is
/// `tiny`; a caller wanting another replaces the quoted `"tiny"`.
pub const DISTILL_TINY_BOOK_TOML: &str = r#"
book_slug      = "tiny"
schema_name    = "name_translation"
schema_version = 1
parser_version = "0.1.0"
authority_rank = 10

[parser]
writes_properties = []
stages = [
  "split_pages",
  { stage = "one_block_per_page", lang = "latin" },
  { stage = "walk_anchors",
    anchor = "latin_headword",
    splice_orphans_to_prev_block = false },
  "split_headline_only",
  { stage = "to_entry_draft",
    key_normalizer = "normalize_latin_key" },
]
"#;

/// The source of [`DISTILL_TINY_BOOK_TOML`]: one page, two headwords.
pub const DISTILL_TINY_SOURCE: &str = "<!-- page 1 (sheet 1) -->\nSmith\nJones\n";
