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
//! - **structure** — lines that look like Chinese chapter / volume
//!   markers become `Heading` blocks, so even a bare `.txt` yields a
//!   usable TOC.
//!
//! The chapter-marker characters are spelled as `\u{...}` escapes, not
//! literal glyphs, so this source file stays ASCII-only.

use std::path::Path;

use bookrack_audit_profile::ExtractToggles;

use crate::contract::{
    Biblio, Block, BlockKind, ExtractError, Extraction, Provenance, TextLayerQuality, Toc, TocEntry,
};

/// Behaviour-sensitive adapter version, stamped into `Provenance`.
const ADAPTER_VERSION: &str = "txt-adapter=1";

/// The ordinal prefix that opens a Chinese chapter / volume marker
/// (U+7B2C).
const MARKER_PREFIX: char = '\u{7b2c}';

/// Volume-class unit characters — U+5377, U+90E8, U+7BC7. A marker
/// ending in one of these is a level-1 heading.
const VOLUME_UNITS: [char; 3] = ['\u{5377}', '\u{90e8}', '\u{7bc7}'];

/// Chapter-class unit characters — U+7AE0, U+8282, U+56DE. A marker
/// ending in one of these is a level-2 heading.
const CHAPTER_UNITS: [char; 3] = ['\u{7ae0}', '\u{8282}', '\u{56de}'];

/// CJK numerals, including the fullwidth digit variants, that may form
/// the number run between a marker's prefix and its unit character.
const CJK_NUMERALS: &str = "\u{96f6}\u{4e00}\u{4e8c}\u{4e09}\u{56db}\u{4e94}\u{516d}\u{4e03}\u{516b}\u{4e5d}\u{5341}\u{767e}\u{5343}\u{4e07}\u{4e24}\u{58f9}\u{8d30}\u{53c1}\u{8086}\u{4f0d}\u{9646}\u{67d2}\u{634c}\u{7396}\u{62fe}\u{ff10}\u{ff11}\u{ff12}\u{ff13}\u{ff14}\u{ff15}\u{ff16}\u{ff17}\u{ff18}\u{ff19}";

/// Extract one plain-text file. `toggles.txt_toc_enabled` gates whether
/// Chinese chapter / volume marker lines are emitted as heading blocks;
/// when off, every non-blank line lands as `BlockKind::Body` and the
/// derived TOC stays empty.
pub fn extract(path: &Path, toggles: &ExtractToggles) -> Result<Extraction, ExtractError> {
    let bytes = std::fs::read(path)?;
    let (text, encoding) = decode(&bytes);

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
            match heading_level(line) {
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
            // The encoding guess is behaviour-sensitive — a different
            // guess yields different text — so it is stamped here.
            extractor_version: format!("{ADAPTER_VERSION};encoding={encoding}"),
            text_layer_quality: TextLayerQuality::BornDigital,
            // A plain-text file is one source unit; nothing to skip.
            skipped_units: Vec::new(),
        },
    })
}

/// Decode raw bytes to text, detecting the encoding. Order: BOM, then a
/// strict UTF-8 trial, then GB18030 (covers GBK / GB2312) for legacy
/// Chinese text. Returns the text and the encoding label.
fn decode(bytes: &[u8]) -> (String, &'static str) {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return (String::from_utf8_lossy(rest).into_owned(), "utf-8-bom");
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let (text, _) = encoding_rs::UTF_16LE.decode_without_bom_handling(rest);
        return (text.into_owned(), "utf-16le");
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let (text, _) = encoding_rs::UTF_16BE.decode_without_bom_handling(rest);
        return (text.into_owned(), "utf-16be");
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return (s.to_string(), "utf-8");
    }
    // Not UTF-8 — assume legacy Chinese. GB18030 is a strict superset of
    // GBK and GB2312, so one decoder covers all three.
    let (text, _, had_errors) = encoding_rs::GB18030.decode(bytes);
    (
        text.into_owned(),
        if had_errors {
            "gb18030(lossy)"
        } else {
            "gb18030"
        },
    )
}

/// Heading level of a Chinese chapter / volume marker line, or `None`.
///
/// A marker is a short line beginning with the ordinal prefix, then a
/// run of ASCII or CJK numerals, then a unit character. Volume-class
/// units are level 1, chapter-class units level 2. The length cap keeps
/// a prose sentence that merely opens with the prefix from matching.
fn heading_level(line: &str) -> Option<u8> {
    if line.chars().count() > 30 {
        return None;
    }
    let mut chars = line.chars();
    if chars.next()? != MARKER_PREFIX {
        return None;
    }
    let mut saw_number = false;
    for c in chars {
        if c.is_ascii_digit() || CJK_NUMERALS.contains(c) {
            saw_number = true;
        } else if saw_number {
            // First non-numeral after the number run decides the kind.
            return if VOLUME_UNITS.contains(&c) {
                Some(1)
            } else if CHAPTER_UNITS.contains(&c) {
                Some(2)
            } else {
                None
            };
        } else {
            // The prefix is not immediately followed by a number — this
            // is ordinary prose opening with it, not a marker.
            return None;
        }
    }
    None
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
