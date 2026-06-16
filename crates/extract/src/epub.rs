// SPDX-License-Identifier: Apache-2.0

//! `EpubAdapter`: an EPUB file → format-neutral [`Extraction`].
//!
//! Three pieces: walk the spine documents into ordered blocks, lift the
//! nav tree into a depth-tagged TOC anchored onto those blocks, and
//! transcribe the OPF Dublin Core into `biblio`.
//!
//! TOC anchoring is the load-bearing step: `EpubTocEntry::href()`
//! yields a resolved href whose path identifies a spine document and
//! whose `fragment()` is matched against the `id` attributes preserved
//! on blocks, so every entry resolves to a block index.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use bookrack_audit_profile::{ExtractToggles, HtmlToggles};
use rbook::Epub;

use crate::contract::{
    Biblio, Block, BlockKind, Contributor, ContributorRole, ExtractError, Extraction,
    FallbackEvent, Provenance, TextLayerQuality, Toc, TocEntry, fallback_kinds,
};
use crate::html_parse;

const ADAPTER: &str = "epub";

/// Behaviour-sensitive extractor versions. Any change here can shift
/// block boundaries, so it is stamped into `Provenance` for downstream
/// dirty-partition detection.
use crate::EXTRACTOR_VERSION;

/// Extract one EPUB file under the given toggle bag and HTML tag
/// configuration.
pub fn extract(
    path: &Path,
    toggles: &ExtractToggles,
    html_toggles: &HtmlToggles,
) -> Result<Extraction, ExtractError> {
    let epub = Epub::open(path).map_err(|e| ExtractError::CorruptFile {
        detail: e.to_string(),
    })?;

    // --- spine documents -> ordered blocks + anchoring indexes -------
    let mut blocks: Vec<Block> = Vec::new();
    // manifest id of each spine document -> its reading-order index.
    let mut unit_of_manifest: HashMap<String, u32> = HashMap::new();
    // First block of each non-empty source unit, ordered by unit so a
    // TOC entry can resolve forward past empty title-page documents.
    let mut first_block_of: BTreeMap<u32, usize> = BTreeMap::new();
    // (source unit, anchor id) -> block index, for fragment hrefs.
    let mut block_of_anchor: HashMap<(u32, String), usize> = HashMap::new();

    for item in epub.reader() {
        let content = item.map_err(|e| ExtractError::MalformedPackage {
            detail: e.to_string(),
        })?;
        let source_unit = content.position() as u32;
        unit_of_manifest.insert(content.manifest_entry().id().to_string(), source_unit);

        let parsed = html_parse::parse_blocks(content.content(), source_unit, html_toggles);
        let base = blocks.len();
        if !parsed.blocks.is_empty() {
            first_block_of.entry(source_unit).or_insert(base);
        }
        for (id, local) in parsed.anchors {
            block_of_anchor
                .entry((source_unit, id))
                .or_insert(base + local);
        }
        blocks.extend(parsed.blocks);
    }

    if !blocks.iter().any(|b| matches!(b.kind, BlockKind::Body)) {
        return Err(ExtractError::EmptyExtraction);
    }

    let mut fallbacks = Vec::new();

    // --- nav tree -> anchored TOC ------------------------------------
    let toc = build_toc(
        &epub,
        &unit_of_manifest,
        &first_block_of,
        &block_of_anchor,
        &mut fallbacks,
    );

    // --- bibliographic metadata --------------------------------------
    let biblio = build_biblio(&epub, toggles, &mut fallbacks);

    Ok(Extraction {
        blocks,
        toc,
        biblio,
        provenance: Provenance {
            adapter: ADAPTER.to_string(),
            extractor_version: EXTRACTOR_VERSION,
            text_layer_quality: TextLayerQuality::BornDigital,
            // Born-digital: a broken spine document aborts the whole
            // file (see ExtractError), so nothing is ever skipped.
            skipped_units: Vec::new(),
            derived_from_sha256: None,
            partial_pages: None,
            source_of_structure: None,
            fallbacks,
        },
    })
}

/// First block of the first non-empty source unit at or after `unit`.
/// Lets a TOC entry whose own document is an empty chapter-title page
/// resolve forward to where the chapter's prose actually begins.
fn first_at_or_after(first_block_of: &BTreeMap<u32, usize>, unit: u32) -> Option<usize> {
    first_block_of.range(unit..).next().map(|(_, &idx)| idx)
}

/// Flatten the nav tree, resolving each entry's href to a block index.
/// Records [`fallback_kinds::EPUB_NAV_DEPTH_SATURATE`] the first time
/// a nav entry's reported depth is 0 — `saturating_sub(1)` clamps to
/// 0 there, hiding the depth information for that entry.
fn build_toc(
    epub: &Epub,
    unit_of_manifest: &HashMap<String, u32>,
    first_block_of: &BTreeMap<u32, usize>,
    block_of_anchor: &HashMap<(u32, String), usize>,
    fallbacks: &mut Vec<FallbackEvent>,
) -> Toc {
    let Some(root) = epub.toc().contents() else {
        return Toc::default();
    };
    let mut entries = Vec::new();
    let mut depth_saturate_logged = false;
    for entry in root.flatten() {
        // rbook depth counts the (omitted) root as 0; topmost real
        // entries are 1. The contract wants 0 = topmost.
        let raw_depth = entry.depth();
        if raw_depth == 0 && !depth_saturate_logged {
            FallbackEvent::record(
                fallbacks,
                ADAPTER,
                fallback_kinds::EPUB_NAV_DEPTH_SATURATE,
                None,
            );
            depth_saturate_logged = true;
        }
        let depth = raw_depth.saturating_sub(1).min(u8::MAX as usize) as u8;

        let start_block = entry.manifest_entry().and_then(|manifest_entry| {
            let unit = *unit_of_manifest.get(manifest_entry.id())?;
            let fragment = entry.href().and_then(|href| href.fragment());
            match fragment {
                Some(frag) => block_of_anchor
                    .get(&(unit, frag.to_string()))
                    .copied()
                    .or_else(|| first_at_or_after(first_block_of, unit)),
                None => first_at_or_after(first_block_of, unit),
            }
        });

        entries.push(TocEntry {
            label: entry.label().to_string(),
            depth,
            start_block,
        });
    }
    Toc { entries }
}

/// Transcribe the OPF Dublin Core metadata. Absent fields stay `None` —
/// extract reports only what the file carries; enrichment is METADATA's.
fn build_biblio(
    epub: &Epub,
    toggles: &ExtractToggles,
    fallbacks: &mut Vec<FallbackEvent>,
) -> Biblio {
    let md = epub.metadata();

    let title = md.title().map(|t| t.value().to_string());
    let subtitle = md
        .titles()
        .find(|t| t.kind().is_subtitle())
        .map(|t| t.value().to_string());
    let series = md
        .titles()
        .find(|t| t.kind().is_collection())
        .map(|t| t.value().to_string());
    let publisher = md.publishers().next().map(|p| p.value().to_string());
    let year_entry = md.published_entry().map(|e| e.value().to_string());
    let year = year_entry.as_deref().and_then(|v| parse_year(v, toggles));
    let language = md.language().map(|l| l.value().to_string());
    let isbn = if toggles.epub_isbn_recognition {
        md.identifiers()
            .find_map(|id| as_isbn(id.value(), fallbacks))
    } else {
        None
    };

    let mut contributors = Vec::new();
    for creator in md.creators() {
        contributors.push(Contributor {
            name: creator.value().to_string(),
            // A bare dc:creator with no relator is conventionally the author.
            role: role_from_code(
                creator.main_role().map(|r| r.code()),
                ContributorRole::Author,
                toggles,
            ),
            family: None,
            given: None,
            orcid: None,
        });
    }
    for contributor in md.contributors() {
        contributors.push(Contributor {
            name: contributor.value().to_string(),
            role: role_from_code(
                contributor.main_role().map(|r| r.code()),
                ContributorRole::Other,
                toggles,
            ),
            family: None,
            given: None,
            orcid: None,
        });
    }

    Biblio {
        title,
        subtitle,
        publisher,
        year,
        year_raw: year_entry,
        isbn,
        series,
        language,
        contributors,
        ..Biblio::default()
    }
}

/// Map a MARC relator code to a contributor role, falling back to
/// `default` when no code is present. With `marc_role_mapping = false`
/// every code (and the bare-no-code path for non-creator entries)
/// collapses onto `Other`.
fn role_from_code(
    code: Option<&str>,
    default: ContributorRole,
    toggles: &ExtractToggles,
) -> ContributorRole {
    if !toggles.marc_role_mapping {
        return default;
    }
    match code {
        None => default,
        Some(c) => match c {
            "aut" => ContributorRole::Author,
            "trl" | "trc" => ContributorRole::Translator,
            "edt" => ContributorRole::Editor,
            _ => ContributorRole::Other,
        },
    }
}

/// Leading year of a date string such as `2011-05-01T00:00:00Z`.
///
/// Implausible years are rejected: a published-date sentinel some tools
/// emit for "unknown" (`0101-01-01`) would otherwise transcribe as 101.
/// When `epub_year_range_check` is off the bounds check is skipped and
/// any four-digit prefix passes.
fn parse_year(value: &str, toggles: &ExtractToggles) -> Option<i32> {
    let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
    let year: i32 = digits.get(..4)?.parse().ok()?;
    if !toggles.epub_year_range_check {
        return Some(year);
    }
    (toggles.epub_year_min..=toggles.epub_year_max)
        .contains(&year)
        .then_some(year)
}

/// Recognize an ISBN inside an identifier value (`urn:isbn:...`, or a
/// bare 10/13-digit number). Returns the digit string, hyphens stripped.
/// Records [`fallback_kinds::EPUB_ISBN_SUBSTRING_FALLBACK`] when the
/// match came from a value carrying `isbn` somewhere in the string
/// without the canonical `urn:isbn:` prefix.
fn as_isbn(value: &str, fallbacks: &mut Vec<FallbackEvent>) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let tail = lower
        .rsplit("isbn")
        .next()
        .unwrap_or(&lower)
        .trim_start_matches([':', ' ', '-']);
    let digits: String = tail
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == 'x' || *c == 'X')
        .collect();
    if digits.len() == 10 || digits.len() == 13 {
        if lower.contains("isbn") && !lower.starts_with("urn:isbn:") {
            FallbackEvent::record(
                fallbacks,
                ADAPTER,
                fallback_kinds::EPUB_ISBN_SUBSTRING_FALLBACK,
                Some(value.chars().take(64).collect()),
            );
        }
        Some(digits)
    } else {
        None
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::*;

    #[test]
    fn urn_isbn_prefix_records_nothing() {
        let mut fallbacks = Vec::new();
        let isbn = as_isbn("urn:isbn:9780000000000", &mut fallbacks);
        assert_eq!(isbn.as_deref(), Some("9780000000000"));
        assert!(
            fallbacks.is_empty(),
            "canonical urn:isbn: prefix must record nothing, got {fallbacks:?}",
        );
    }

    #[test]
    fn isbn_substring_without_urn_prefix_records_fallback() {
        let mut fallbacks = Vec::new();
        let isbn = as_isbn("ISBN 978-0-00-000000-0", &mut fallbacks);
        assert_eq!(isbn.as_deref(), Some("9780000000000"));
        assert!(
            fallbacks
                .iter()
                .any(|e| e.kind == fallback_kinds::EPUB_ISBN_SUBSTRING_FALLBACK),
            "expected EPUB_ISBN_SUBSTRING_FALLBACK in {fallbacks:?}",
        );
    }

    #[test]
    fn bare_digit_identifier_records_nothing() {
        let mut fallbacks = Vec::new();
        let isbn = as_isbn("9780000000000", &mut fallbacks);
        assert_eq!(isbn.as_deref(), Some("9780000000000"));
        assert!(
            fallbacks.is_empty(),
            "bare-digit identifier without isbn substring must record nothing, got {fallbacks:?}",
        );
    }

    #[test]
    fn non_isbn_value_returns_none_and_records_nothing() {
        let mut fallbacks = Vec::new();
        let isbn = as_isbn("not-an-identifier", &mut fallbacks);
        assert!(isbn.is_none());
        assert!(fallbacks.is_empty(), "unexpected events: {fallbacks:?}");
    }
}
