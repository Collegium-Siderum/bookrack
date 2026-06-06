// SPDX-License-Identifier: Apache-2.0

//! Text-layer quality assessment — the extract / OCR boundary.
//!
//! `extract` only handles files with a *usable* text layer; files with
//! none, or with a text layer too corrupt to use, belong on the OCR
//! path. The hard case is a dual-layer PDF — a page image with a hidden
//! text layer underneath: a human flipping through sees a clean scan
//! and never notices the text layer's quality. So the verdict is
//! computed from the text and the page composition, never from how the
//! page looks.
//!
//! The verdict is a [`QualityDecision`]:
//! - `RouteToOcr` — no text layer, or one too corrupt to use; the file
//!   never becomes an `Extraction`, it goes to OCR.
//! - `Keep { grade: Doubtful }` — a text layer present and mostly
//!   readable but not trustworthy: most often a dual-layer scan, whose
//!   text layer is itself OCR output of unknown vintage. Extracted, but
//!   flagged.
//! - `Keep { grade: Usable }` — a born-digital text layer that passed
//!   every check.
//!
//! Verdicts lean conservative: a false "usable" feeds garbage into the
//! index, far worse than a false "doubtful" or "route to OCR".

use bookrack_audit_profile::QualityThresholds;
use serde::Serialize;

use crate::contract::TextLayerQuality;

/// What the quality gate decided about a candidate text layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum QualityDecision {
    /// Unusable — route the file to OCR. `reason` records why.
    RouteToOcr { reason: String },
    /// Keep this text layer; `grade` is the confidence to attach to it,
    /// `reason` explains the grade.
    Keep {
        grade: TextLayerQuality,
        reason: String,
    },
}

/// The verdict plus every signal it was computed from — the signals are
/// retained so thresholds can be recalibrated against real books.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QualityReport {
    pub verdict: QualityDecision,
    pub page_count: usize,
    /// Pages carrying more than a trivial amount of text.
    pub pages_with_text: usize,
    /// Pages bearing at least one image object.
    pub image_pages: usize,
    pub total_chars: usize,
    pub chars_per_page: f64,
    /// Share of pages that carry an image — a dual-layer / scan signal.
    pub image_page_ratio: f64,
    /// Share of U+FFFD replacement characters.
    pub replacement_ratio: f64,
    /// Share of Private Use Area code points (broken font cmap symptom).
    pub pua_ratio: f64,
    /// Share of control / non-printable characters (normal whitespace
    /// excluded).
    pub control_ratio: f64,
    /// Share of CJK ideographs that are split by a space from the next
    /// ideograph — a garbled-OCR symptom (clean CJK has no word spaces).
    pub cjk_space_ratio: f64,
    /// Share of ASCII digits — diagnostic.
    pub digit_ratio: f64,
    /// Share of CJK ideographs — diagnostic.
    pub cjk_ratio: f64,
    /// Share of ASCII letters — diagnostic.
    pub latin_ratio: f64,
}

// Thresholds — calibrated against the spike's PDF corpus and exposed
// through [`QualityThresholds`] in `bookrack_audit_profile`. Born-digital
// PDFs in that corpus carry images on <=11% of pages; scans and dual-
// layer PDFs on >=99%, so the dual-layer cut is wide of either cluster.
// The verdict ladder is a behaviour dimension caller code re-extracts
// on; the active values are stamped through `EXTRACTOR_VERSION`.

/// Assess the per-page text of a candidate text layer. `image_pages` is
/// how many of those pages carry an image object.
pub fn assess(
    pages: &[String],
    image_pages: usize,
    thresholds: &QualityThresholds,
) -> QualityReport {
    let page_count = pages.len();
    let mut total = 0usize;
    let (mut repl, mut pua, mut control) = (0usize, 0usize, 0usize);
    let (mut digit, mut cjk, mut latin) = (0usize, 0usize, 0usize);
    let mut cjk_space = 0usize;
    let mut pages_with_text = 0usize;

    for page in pages {
        let chars: Vec<char> = page.chars().collect();
        if chars.len() >= 20 {
            pages_with_text += 1;
        }
        for (i, &ch) in chars.iter().enumerate() {
            total += 1;
            if ch == '\u{FFFD}' {
                repl += 1;
            } else if is_pua(ch) {
                pua += 1;
            } else if ch.is_control() && !matches!(ch, '\n' | '\r' | '\t') {
                control += 1;
            }
            if ch.is_ascii_digit() {
                digit += 1;
            } else if is_cjk(ch) {
                cjk += 1;
            } else if ch.is_ascii_alphabetic() {
                latin += 1;
            }
            // A space wedged between two ideographs.
            if ch == ' '
                && i > 0
                && i + 1 < chars.len()
                && is_cjk(chars[i - 1])
                && is_cjk(chars[i + 1])
            {
                cjk_space += 1;
            }
        }
    }

    let ratio = |n: usize, d: usize| if d == 0 { 0.0 } else { n as f64 / d as f64 };
    let chars_per_page = if page_count == 0 {
        0.0
    } else {
        total as f64 / page_count as f64
    };
    let mut report = QualityReport {
        // A placeholder; decide() replaces it once every signal field
        // below is populated, since decide() reads those fields.
        verdict: QualityDecision::Keep {
            grade: TextLayerQuality::Usable,
            reason: String::new(),
        },
        page_count,
        pages_with_text,
        image_pages,
        total_chars: total,
        chars_per_page,
        image_page_ratio: ratio(image_pages, page_count),
        replacement_ratio: ratio(repl, total),
        pua_ratio: ratio(pua, total),
        control_ratio: ratio(control, total),
        cjk_space_ratio: ratio(cjk_space, cjk),
        digit_ratio: ratio(digit, total),
        cjk_ratio: ratio(cjk, total),
        latin_ratio: ratio(latin, total),
    };
    report.verdict = decide(&report, thresholds);
    report
}

/// Apply the verdict ladder: unusable layers first, then the flags that
/// merely demote a layer to `Doubtful`.
fn decide(r: &QualityReport, t: &QualityThresholds) -> QualityDecision {
    use QualityDecision::{Keep, RouteToOcr};
    use TextLayerQuality::{Doubtful, Usable};

    // --- unusable: route to OCR --------------------------------------
    if r.total_chars == 0 {
        return RouteToOcr {
            reason: "no extractable text — no text layer".to_string(),
        };
    }
    if r.chars_per_page < t.chars_per_page_ocr() {
        return RouteToOcr {
            reason: format!(
                "only {:.0} chars/page — a scan with no text layer",
                r.chars_per_page
            ),
        };
    }
    if r.replacement_ratio >= t.replacement_ocr() {
        return RouteToOcr {
            reason: format!(
                "{:.1}% replacement characters — encoding corruption",
                r.replacement_ratio * 100.0
            ),
        };
    }
    if r.pua_ratio >= t.pua_ocr() {
        return RouteToOcr {
            reason: format!(
                "{:.1}% Private Use Area glyphs — broken font cmap",
                r.pua_ratio * 100.0
            ),
        };
    }
    if r.control_ratio >= t.control_ocr() {
        return RouteToOcr {
            reason: format!("{:.1}% control characters", r.control_ratio * 100.0),
        };
    }

    // --- present but not fully trustworthy: extract, but flag --------
    if r.image_page_ratio >= t.dual_layer() {
        return Keep {
            grade: Doubtful,
            reason: format!(
                "dual-layer scan ({:.0}% of pages are images) — the text layer is \
                 itself OCR output; verify before trusting",
                r.image_page_ratio * 100.0
            ),
        };
    }
    if r.cjk_space_ratio >= t.cjk_space_doubt() {
        return Keep {
            grade: Doubtful,
            reason: format!(
                "{:.1}% of ideographs are split by spaces — OCR-grade text layer",
                r.cjk_space_ratio * 100.0
            ),
        };
    }
    if r.chars_per_page < t.chars_per_page_doubt() {
        return Keep {
            grade: Doubtful,
            reason: format!(
                "sparse text ({:.0} chars/page) — possibly a partial text layer",
                r.chars_per_page
            ),
        };
    }
    if r.pua_ratio >= t.pua_doubt() || r.replacement_ratio > 0.0 {
        return Keep {
            grade: Doubtful,
            reason: "minor encoding anomalies in the text layer".to_string(),
        };
    }
    Keep {
        grade: Usable,
        reason: "clean born-digital text layer".to_string(),
    }
}

/// A Private Use Area code point. Broken font subsetting commonly maps
/// real glyphs into the PUA, leaving extractable text that renders fine
/// but carries no meaningful Unicode.
pub fn is_pua(ch: char) -> bool {
    matches!(ch as u32, 0xE000..=0xF8FF | 0xF_0000..=0xF_FFFD | 0x10_0000..=0x10_FFFD)
}

/// A CJK ideograph (the common ranges).
pub fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
        | 0x2_0000..=0x2_A6DF | 0x2_A700..=0x2_EBEF)
}
