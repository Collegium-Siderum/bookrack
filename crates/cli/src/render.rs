// SPDX-License-Identifier: Apache-2.0

//! User-facing formatting. Every line a command prints for a human lands
//! here, written to **stdout**; progress and diagnostics go through
//! `tracing` to stderr instead, so the result stream and the observation
//! stream never interleave.

use bookrack_ingest::IngestReport;
use bookrack_search::Citation;

/// First `max` characters of `text`, collapsed to a single line, with an
/// ellipsis when truncated.
fn snippet(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(max).collect();
    if flat.chars().count() > max {
        out.push('\u{2026}');
    }
    out
}

/// Print the outcome of one `ingest` run.
pub fn ingest(report: &IngestReport) {
    if report.already_registered {
        println!(
            "Already ingested (intake {}); refreshed in place.",
            report.intake_id
        );
    } else {
        println!("Ingested as intake {}.", report.intake_id);
    }
    println!(
        "  nodes: {} ({} prose leaves)\n  chunks embedded: {}",
        report.nodes_written, report.prose_leaves, report.chunks_written,
    );
}

/// Print the cited passages a `query` returned, best match first.
pub fn citations(hits: &[Citation]) {
    if hits.is_empty() {
        println!("No matching passages.");
        return;
    }
    for (i, hit) in hits.iter().enumerate() {
        let trail = if hit.breadcrumb.is_empty() {
            "(untitled location)"
        } else {
            &hit.breadcrumb
        };
        println!("{}. [{:.3}] {}", i + 1, hit.distance, trail);
        println!("   {}", snippet(&hit.text, 200));
    }
}
