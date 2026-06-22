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
    Biblio, Block, BlockKind, ExtractError, Extraction, FallbackEvent, Provenance,
    TextLayerQuality, Toc, TocEntry, fallback_kinds,
};
use crate::headings;

const ADAPTER: &str = "txt";

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
    let mut fallbacks = Vec::new();
    let text = decode(&bytes, &mut fallbacks);

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
            fallbacks,
        },
    })
}

/// Decode raw bytes to text, detecting the encoding. Order: BOM, then a
/// strict UTF-8 trial, then GB18030 (covers GBK / GB2312) for legacy
/// Chinese text. Records a [`FallbackEvent`] on the lossy UTF-8 and
/// GB18030 fallthrough paths so the envelope keeps the trace.
fn decode(bytes: &[u8], fallbacks: &mut Vec<FallbackEvent>) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        // The BOM only claims UTF-8 — the rest may still contain
        // invalid sub-sequences `from_utf8_lossy` then replaces with
        // U+FFFD. Strict-check before the lossy decode to detect that.
        if std::str::from_utf8(rest).is_err() {
            FallbackEvent::record(
                fallbacks,
                ADAPTER,
                fallback_kinds::TXT_UTF8_LOSSY_SUBSTITUTION,
                None,
            );
        }
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let (text, had_errors) = encoding_rs::UTF_16LE.decode_without_bom_handling(rest);
        if had_errors {
            FallbackEvent::record(
                fallbacks,
                ADAPTER,
                fallback_kinds::TXT_UTF16LE_LOSSY_SUBSTITUTION,
                None,
            );
        }
        return text.into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let (text, had_errors) = encoding_rs::UTF_16BE.decode_without_bom_handling(rest);
        if had_errors {
            FallbackEvent::record(
                fallbacks,
                ADAPTER,
                fallback_kinds::TXT_UTF16BE_LOSSY_SUBSTITUTION,
                None,
            );
        }
        return text.into_owned();
    }
    let utf8_error = match std::str::from_utf8(bytes) {
        Ok(s) => return s.to_string(),
        Err(err) => err,
    };
    // Not UTF-8 — assume legacy Chinese. GB18030 is a strict superset of
    // GBK and GB2312, so one decoder covers all three.
    FallbackEvent::record(
        fallbacks,
        ADAPTER,
        fallback_kinds::TXT_GB18030,
        Some(utf8_error.to_string()),
    );
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

#[cfg(test)]
mod fallback_tests {
    use super::*;

    #[test]
    fn bom_utf8_lossy_substitution_is_recorded() {
        // BOM, then a stray 0xFF that is not valid UTF-8 — `from_utf8_lossy`
        // will substitute U+FFFD and the adapter must record the fallback.
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.push(0xFF);
        bytes.extend_from_slice(b"text");
        let mut fallbacks = Vec::new();
        let _ = decode(&bytes, &mut fallbacks);
        assert!(
            fallbacks
                .iter()
                .any(|e| e.kind == fallback_kinds::TXT_UTF8_LOSSY_SUBSTITUTION),
            "expected TXT_UTF8_LOSSY_SUBSTITUTION in {fallbacks:?}",
        );
    }

    #[test]
    fn bom_utf8_clean_records_nothing() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"clean ascii");
        let mut fallbacks = Vec::new();
        let _ = decode(&bytes, &mut fallbacks);
        assert!(
            fallbacks.is_empty(),
            "clean BOM-prefixed UTF-8 must record nothing, got {fallbacks:?}",
        );
    }

    #[test]
    fn bom_utf16le_lossy_substitution_is_recorded() {
        // UTF-16LE BOM, then a lone low surrogate (0x00 0xD8) which the
        // decoder must replace with U+FFFD. Followed by an odd trailing
        // byte to keep the input ill-formed at the unit boundary too.
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend_from_slice(&[0x00, 0xD8, 0x41]);
        let mut fallbacks = Vec::new();
        let _ = decode(&bytes, &mut fallbacks);
        assert!(
            fallbacks
                .iter()
                .any(|e| e.kind == fallback_kinds::TXT_UTF16LE_LOSSY_SUBSTITUTION),
            "expected TXT_UTF16LE_LOSSY_SUBSTITUTION in {fallbacks:?}",
        );
    }

    #[test]
    fn bom_utf16le_clean_records_nothing() {
        // UTF-16LE BOM, then "Hi" as two well-formed code units.
        let bytes = [0xFF, 0xFE, b'H', 0x00, b'i', 0x00];
        let mut fallbacks = Vec::new();
        let _ = decode(&bytes, &mut fallbacks);
        assert!(
            fallbacks.is_empty(),
            "clean BOM-prefixed UTF-16LE must record nothing, got {fallbacks:?}",
        );
    }

    #[test]
    fn bom_utf16be_lossy_substitution_is_recorded() {
        // UTF-16BE BOM, then a lone low surrogate (0xD8 0x00) which the
        // decoder must replace with U+FFFD.
        let mut bytes = vec![0xFE, 0xFF];
        bytes.extend_from_slice(&[0xD8, 0x00, 0x00, 0x41]);
        let mut fallbacks = Vec::new();
        let _ = decode(&bytes, &mut fallbacks);
        assert!(
            fallbacks
                .iter()
                .any(|e| e.kind == fallback_kinds::TXT_UTF16BE_LOSSY_SUBSTITUTION),
            "expected TXT_UTF16BE_LOSSY_SUBSTITUTION in {fallbacks:?}",
        );
    }

    #[test]
    fn gb18030_fallthrough_records_kind_and_detail() {
        // A byte sequence that fails strict UTF-8 yet decodes through
        // GB18030. 0xC4 0xE3 is the GB18030 encoding of `\u{4F60}`
        // ("ni" in pinyin), commonly used in legacy Chinese text dumps.
        let bytes = [0xC4, 0xE3];
        let mut fallbacks = Vec::new();
        let _ = decode(&bytes, &mut fallbacks);
        let event = fallbacks
            .iter()
            .find(|e| e.kind == fallback_kinds::TXT_GB18030)
            .expect("expected TXT_GB18030 fallback");
        assert!(
            event.detail.is_some(),
            "GB18030 fallback should record the UTF-8 error detail",
        );
    }
}
