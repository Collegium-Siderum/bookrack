// SPDX-License-Identifier: Apache-2.0

//! `HtmlAdapter`: a standalone HTML document → [`Extraction`].
//!
//! A loose HTML file has no spine, no nav, no OPF — the single document
//! *is* the whole book. So, unlike EPUB:
//!
//! - it parses to one source unit through the shared `html_parse` DOM
//!   walk (the same walk EPUB spine documents use);
//! - the TOC has nothing to lift, so it is *inferred* from the heading
//!   hierarchy — every `<h1>`–`<h6>` becomes an entry anchored to its
//!   own block;
//! - biblio is only what the `<head>` carries, which for HTML exported
//!   by a reader app is often just a numeric id, not a real title.
//!
//! Loose HTML is rare in real corpora, so this adapter stays
//! deliberately minimal: it does not separate CSS-styled footnotes,
//! and it reads the `<head>` only shallowly.

use std::path::Path;

use bookrack_audit_profile::HtmlToggles;

use crate::EXTRACTOR_VERSION;
use crate::contract::{
    Biblio, Block, BlockKind, Contributor, ContributorRole, ExtractError, Extraction,
    FallbackEvent, Provenance, TextLayerQuality, Toc, TocEntry, fallback_kinds,
};
use crate::html_parse;

const ADAPTER: &str = "html";

/// Upper bound on the `<head>` scan window. Loose HTML can be huge,
/// but the metadata sits at the start; capping the slice keeps the
/// scan from walking the body.
const HEAD_WINDOW_BYTES: usize = 256 * 1024;

/// Extract one standalone HTML file.
pub fn extract(path: &Path, html_toggles: &HtmlToggles) -> Result<Extraction, ExtractError> {
    let bytes = std::fs::read(path)?;
    let mut fallbacks = Vec::new();
    let content = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            // A `<meta charset>` other than UTF-8 would need real decoding;
            // this adapter falls back to lossy rather than fail outright.
            FallbackEvent::record(
                &mut fallbacks,
                ADAPTER,
                fallback_kinds::HTML_UTF8_LOSSY,
                Some(e.utf8_error().to_string()),
            );
            String::from_utf8_lossy(e.as_bytes()).into_owned()
        }
    };

    // The whole file is one source unit — there is no spine to index.
    let parsed = html_parse::parse_blocks(&content, 0, html_toggles);
    if !parsed
        .blocks
        .iter()
        .any(|b| matches!(b.kind, BlockKind::Body))
    {
        return Err(ExtractError::EmptyExtraction);
    }

    let toc = toc_from_headings(&parsed.blocks);
    let biblio = build_biblio(&content, &mut fallbacks);

    Ok(Extraction {
        blocks: parsed.blocks,
        toc,
        biblio,
        provenance: Provenance {
            adapter: ADAPTER.to_string(),
            extractor_version: EXTRACTOR_VERSION,
            text_layer_quality: TextLayerQuality::BornDigital,
            // A standalone HTML file is one source unit; there is no
            // sub-unit to skip.
            skipped_units: Vec::new(),
            derived_from_sha256: None,
            partial_pages: None,
            source_of_structure: None,
            fallbacks,
        },
    })
}

/// Infer a TOC from the heading blocks. With no nav document, every
/// `<h1>`–`<h6>` becomes an entry anchored to its own block, its depth
/// taken straight from the heading level (`<h1>` → depth 0).
///
/// This trusts the document's heading levels as-is; it does not re-base
/// them (a book that starts at `<h2>` yields a topmost depth of 1).
/// Honest to the source — loose HTML often has no consistent heading
/// scheme to normalize against.
fn toc_from_headings(blocks: &[Block]) -> Toc {
    let mut entries = Vec::new();
    for (idx, block) in blocks.iter().enumerate() {
        if let BlockKind::Heading { level } = block.kind {
            entries.push(TocEntry {
                label: block.text.clone(),
                depth: level.saturating_sub(1),
                start_block: Some(idx),
            });
        }
    }
    Toc { entries }
}

/// Read what little metadata the `<head>` carries: `<title>`, the
/// `<html lang>` attribute, and a `<meta name="author">` if present.
/// Records [`fallback_kinds::HTML_HEAD_TRUNCATED_256K`] when the
/// bounded scan window did not reach a real `</head>`.
fn build_biblio(content: &str, fallbacks: &mut Vec<FallbackEvent>) -> Biblio {
    // The head sits at the start; scan only a bounded prefix so a large
    // body is never walked here. Back the limit off to a char boundary
    // so the slice never splits a multi-byte character.
    let mut limit = content.len().min(HEAD_WINDOW_BYTES);
    while !content.is_char_boundary(limit) {
        limit -= 1;
    }
    let head = &content[..limit];

    // If the bounded slice was used (i.e. the body extended past
    // HEAD_WINDOW_BYTES) and that slice does not contain `</head>`,
    // any `<head>` metadata past the window was not consulted. Record
    // it so a downstream consumer can see when the cap mattered.
    if content.len() > HEAD_WINDOW_BYTES && ci_find(head, "</head").is_none() {
        FallbackEvent::record(
            fallbacks,
            ADAPTER,
            fallback_kinds::HTML_HEAD_TRUNCATED_256K,
            None,
        );
    }

    let title = inner_text(head, "title").filter(|s| !s.is_empty());
    let language = tag_attr(head, "html", "lang").filter(|s| !s.is_empty());

    let mut contributors = Vec::new();
    if let Some(author) = meta_content(head, "author").filter(|s| !s.is_empty()) {
        contributors.push(Contributor {
            name: author,
            role: ContributorRole::Author,
            family: None,
            given: None,
            orcid: None,
        });
    }

    Biblio {
        title,
        language,
        contributors,
        ..Biblio::default()
    }
}

/// Case-insensitive byte-wise substring search over a bounded slice.
fn ci_find(haystack: &str, needle: &str) -> Option<usize> {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| {
        h[i..i + n.len()]
            .iter()
            .zip(n)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

/// Text between `<tag ...>` and `</tag>`, entity-decoded and trimmed.
fn inner_text(region: &str, tag: &str) -> Option<String> {
    let open = ci_find(region, &format!("<{tag}"))?;
    let gt = region[open..].find('>')? + open + 1;
    let close = ci_find(&region[gt..], &format!("</{tag}"))? + gt;
    Some(decode_entities(region[gt..close].trim()))
}

/// Value of `attr` on the first `<tag ...>` start tag in `region`.
fn tag_attr(region: &str, tag: &str, attr: &str) -> Option<String> {
    let open = ci_find(region, &format!("<{tag}"))?;
    let gt = region[open..].find('>')? + open;
    attr_value(&region[open..gt], attr)
}

/// `content` of the first `<meta name="...">` whose name matches.
fn meta_content(region: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(rel) = ci_find(&region[from..], "<meta") {
        let start = from + rel;
        let gt = region[start..].find('>').map(|g| start + g)?;
        let tag = &region[start..gt];
        if attr_value(tag, "name").is_some_and(|v| v.eq_ignore_ascii_case(name)) {
            return attr_value(tag, "content");
        }
        from = gt + 1;
    }
    None
}

/// Extract a quoted `attr="value"` (or single-quoted) from a start tag.
fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let mut from = 0;
    while let Some(rel) = ci_find(&tag[from..], attr) {
        let at = from + rel;
        // Reject a substring hit inside a longer attribute name.
        let before_ok = tag[..at]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace);
        let rest = tag[at + attr.len()..].trim_start();
        if let (true, Some(after)) = (before_ok, rest.strip_prefix('=')) {
            let after = after.trim_start();
            let quote = after.chars().next()?;
            if quote == '"' || quote == '\'' {
                let end = after[1..].find(quote)? + 1;
                return Some(decode_entities(&after[1..end]));
            }
        }
        from = at + attr.len();
    }
    None
}

/// Decode the handful of XML entities a `<head>` value may carry.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

#[cfg(test)]
mod fallback_tests {
    use super::*;

    #[test]
    fn oversize_body_without_head_close_records_truncation() {
        // `<head>` is unclosed and the body extends past the window —
        // the slice never reaches `</head>`, so truncation must record.
        let mut content = String::from("<html><head><title>t</title>");
        content.push_str(&"x".repeat(HEAD_WINDOW_BYTES + 8));
        let mut fallbacks = Vec::new();
        let _ = build_biblio(&content, &mut fallbacks);
        assert!(
            fallbacks
                .iter()
                .any(|e| e.kind == fallback_kinds::HTML_HEAD_TRUNCATED_256K),
            "expected HTML_HEAD_TRUNCATED_256K in {fallbacks:?}",
        );
    }

    #[test]
    fn small_document_with_real_head_close_records_nothing() {
        let content = "<html><head><title>t</title></head><body>hi</body></html>";
        let mut fallbacks = Vec::new();
        let _ = build_biblio(content, &mut fallbacks);
        assert!(
            fallbacks.is_empty(),
            "well-formed small document must record nothing, got {fallbacks:?}",
        );
    }

    #[test]
    fn oversize_document_with_head_close_inside_window_records_nothing() {
        // The `</head>` lands inside the window, so even an oversize
        // body does not trigger the truncation signal.
        let mut content = String::from("<html><head><title>t</title></head><body>");
        content.push_str(&"x".repeat(HEAD_WINDOW_BYTES + 8));
        content.push_str("</body></html>");
        let mut fallbacks = Vec::new();
        let _ = build_biblio(&content, &mut fallbacks);
        assert!(
            fallbacks.is_empty(),
            "oversize body with head closed inside the window must record nothing, got {fallbacks:?}",
        );
    }
}
