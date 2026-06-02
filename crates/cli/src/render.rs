// SPDX-License-Identifier: Apache-2.0

//! User-facing formatting. Every line a command prints for a human lands
//! here, written to **stdout**; progress and diagnostics go through
//! `tracing` to stderr instead, so the result stream and the observation
//! stream never interleave.

use bookrack_ingest::IngestReport;
use bookrack_metadata::{FieldGrade, FieldReport, MetadataReport};
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

/// Print the metadata audit report for one book as a human-readable
/// listing, leading with the human/LLM review status and the audit's
/// own plausibility verdict, then one line per field.
///
/// `review_status` is the value from `node_reviews.status` — `pending`
/// until a human or LLM advances it — or `None` if the book has no
/// review row at all. The audit verdict is kept on a separate line so
/// the reader cannot mistake a `clean` audit for an `approved` review.
pub fn metadata_show(book: i64, report: &MetadataReport, review_status: Option<&str>) {
    let status = review_status.unwrap_or("(no review row)");
    println!("Book {book}: review status {status}");
    println!(
        "  audit verdict {} (confidence {})",
        report.verdict.as_token(),
        report.confidence.as_str()
    );
    for field in &report.fields {
        let grade = grade_label(field.grade);
        let flags = if field.flags.is_empty() {
            String::new()
        } else {
            let tokens: Vec<&str> = field.flags.iter().map(|f| f.token()).collect();
            format!(" [{}]", tokens.join(", "))
        };
        println!("  {:>10}  {grade}{flags}", field.field);
        if !field.hint.is_empty() {
            println!("              {}", field.hint);
        }
    }
    if !report.copyright_blocks.is_empty() {
        let blocks: Vec<String> = report
            .copyright_blocks
            .iter()
            .map(|b| b.to_string())
            .collect();
        println!("  copyright-page candidates: {}", blocks.join(", "));
    }
}

/// Print the same report as a single-line JSON object. Hand-emitted
/// so the metadata crate stays free of a serde dependency.
pub fn metadata_show_json(book: i64, report: &MetadataReport, review_status: Option<&str>) {
    let mut out = String::new();
    out.push('{');
    write_string_field(&mut out, "review_status", review_status.unwrap_or(""));
    out.push(',');
    write_string_field(&mut out, "audit_verdict", report.verdict.as_token());
    out.push(',');
    write_string_field(&mut out, "confidence", report.confidence.as_str());
    out.push(',');
    out.push_str(&format!("\"book\":{book}"));
    out.push(',');
    out.push_str("\"fields\":[");
    for (i, field) in report.fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&field_to_json(field));
    }
    out.push(']');
    out.push(',');
    let blocks: Vec<String> = report
        .copyright_blocks
        .iter()
        .map(|b| b.to_string())
        .collect();
    out.push_str(&format!("\"copyright_blocks\":[{}]", blocks.join(",")));
    out.push('}');
    println!("{out}");
}

fn grade_label(grade: FieldGrade) -> &'static str {
    match grade {
        FieldGrade::Missing => "missing",
        FieldGrade::Weak => "weak",
        FieldGrade::Medium => "medium",
        FieldGrade::Strong => "strong",
    }
}

fn write_string_field(out: &mut String, key: &str, value: &str) {
    out.push('"');
    out.push_str(key);
    out.push_str("\":\"");
    out.push_str(&escape_json(value));
    out.push('"');
}

fn field_to_json(field: &FieldReport) -> String {
    let mut out = String::from("{");
    write_string_field(&mut out, "field", &field.field);
    out.push(',');
    write_string_field(&mut out, "grade", grade_label(field.grade));
    out.push(',');
    out.push_str("\"flags\":[");
    for (i, flag) in field.flags.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(flag.token());
        out.push('"');
    }
    out.push(']');
    out.push(',');
    write_string_field(&mut out, "hint", &field.hint);
    out.push('}');
    out
}

/// Quote-escape a string for embedding in a JSON literal.
fn escape_json(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
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
