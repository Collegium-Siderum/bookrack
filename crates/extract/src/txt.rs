// SPDX-License-Identifier: Apache-2.0

//! `TxtAdapter`: a plain-text file → [`Extraction`].
//!
//! Plain text is the weakest-fidelity source: no markup, no metadata,
//! not even a declared encoding. The adapter does three things and is
//! honest about the limits of each:
//!
//! - **encoding detection** — a BOM if present, else a strict UTF-8
//!   trial, else a GB18030 (a GBK / GB2312 superset) fallback for
//!   legacy Chinese text. The guess is stamped into `Provenance`, since
//!   it determines the bytes-to-text mapping.
//! - **segmentation** — one non-blank line is one block. This fits the
//!   common one-paragraph-per-line text (web-novel dumps and the like);
//!   hard-wrapped text would instead need a blank-line paragraph-join.
//! - **structure** — lines that match a chapter / volume marker in any
//!   supported language family become `Heading` blocks, so even a bare
//!   `.txt` yields a usable TOC. See [`crate::headings`] for the family
//!   definitions; the dispatcher covers Sino (Chinese / Japanese /
//!   Hangul-via-Sino-shape), Latin (English / French / Spanish /
//!   Italian), and German.

use std::path::Path;

use bookrack_audit_profile::{ExtractToggles, HeadingPatterns};

use crate::EXTRACTOR_VERSION;
use crate::contract::{
    Biblio, Block, BlockKind, ExtractError, Extraction, Provenance, TextLayerQuality, Toc, TocEntry,
};
use crate::headings;

/// Extract one plain-text file. `toggles.txt_toc_enabled` gates whether
/// matching lines become `Heading` blocks; when off, every non-blank
/// line lands as `BlockKind::Body` and the derived TOC stays empty.
/// `heading_patterns` carries the multi-language marker grammar the
/// dispatcher consults when the gate is on.
pub fn extract(
    path: &Path,
    toggles: &ExtractToggles,
    heading_patterns: &HeadingPatterns,
) -> Result<Extraction, ExtractError> {
    let bytes = std::fs::read(path)?;
    let text = decode(&bytes);

    // One non-blank line is one block; `str::lines` also strips the
    // `\r` of CRLF endings, and `trim` drops the leading ideographic
    // indent (U+3000) that Chinese text uses.
    let mut blocks = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let kind = if toggles.txt_toc_enabled {
            match headings::heading_level(line, heading_patterns) {
                Some(level) => BlockKind::Heading { level },
                None => BlockKind::Body,
            }
        } else {
            BlockKind::Body
        };
        blocks.push(Block {
            kind,
            text: line.to_string(),
            source_unit: 0,
            style: None,
        });
    }

    if !blocks.iter().any(|b| matches!(b.kind, BlockKind::Body)) {
        return Err(ExtractError::EmptyExtraction);
    }

    let toc = toc_from_headings(&blocks);

    Ok(Extraction {
        blocks,
        toc,
        // A bare .txt carries no reliable bibliographic metadata.
        biblio: Biblio::default(),
        provenance: Provenance {
            adapter: "txt".to_string(),
            extractor_version: EXTRACTOR_VERSION,
            text_layer_quality: TextLayerQuality::BornDigital,
            // A plain-text file is one source unit; nothing to skip.
            skipped_units: Vec::new(),
            derived_from_sha256: None,
            partial_pages: None,
            source_of_structure: None,
        },
    })
}

/// Decode raw bytes to text, detecting the encoding. Order: BOM, then a
/// strict UTF-8 trial, then GB18030 (covers GBK / GB2312) for legacy
/// Chinese text.
fn decode(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let (text, _) = encoding_rs::UTF_16LE.decode_without_bom_handling(rest);
        return text.into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let (text, _) = encoding_rs::UTF_16BE.decode_without_bom_handling(rest);
        return text.into_owned();
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    // Not UTF-8 — assume legacy Chinese. GB18030 is a strict superset of
    // GBK and GB2312, so one decoder covers all three.
    let (text, _, _) = encoding_rs::GB18030.decode(bytes);
    text.into_owned()
}

/// Build a TOC from the heading blocks, each anchored to its own block.
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
