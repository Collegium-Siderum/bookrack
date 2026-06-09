// SPDX-License-Identifier: Apache-2.0

//! User-facing formatting. Every line a command prints for a human lands
//! here, written to **stdout**; progress and diagnostics go through
//! `tracing` to stderr instead, so the result stream and the observation
//! stream never interleave.

use bookrack_audit_profile::AuditProfile;
use bookrack_config::LibraryEntry;
use bookrack_ingest::IngestReport;
use bookrack_ingest::ocr::OcrIngestReport;

/// Print the outcome of one `ingest` run.
pub fn ingest(report: &IngestReport) {
    if report.no_op {
        println!(
            "Already ingested as intake {}; source and stamps unchanged, nothing to do.",
            report.intake_id,
        );
        println!("  (Pass --force to re-extract, re-chunk, and re-embed anyway.)");
        print_audit_warning(report);
        return;
    }
    if report.forced {
        println!(
            "Re-ingested intake {} with --force: re-extracted and re-embedded.",
            report.intake_id,
        );
    } else if report.already_registered {
        println!(
            "Refreshed intake {}: a stamp had drifted, so the pipeline re-ran.",
            report.intake_id,
        );
    } else {
        println!("Ingested as intake {}.", report.intake_id);
    }
    println!(
        "  nodes: {} ({} prose leaves)\n  chunks embedded: {}",
        report.nodes_written, report.prose_leaves, report.chunks_written,
    );
    print_audit_warning(report);
}

/// Print the outcome of one `intake ocr` run.
pub fn ocr_intake(report: &OcrIngestReport) {
    if report.no_op {
        println!(
            "OCR intake {} already up to date (source PDF anchored as intake {}, needs_ocr). No work done.",
            report.ocr_intake_id, report.pdf_intake_id,
        );
        return;
    }
    println!(
        "Ingested OCR product as intake {} (source PDF anchored as intake {}, needs_ocr).",
        report.ocr_intake_id, report.pdf_intake_id,
    );
    println!(
        "  pages: {}/{} covered",
        report.ocr_page_count, report.expected_pages,
    );
    if let Some(partial) = report.extraction.provenance.partial_pages.as_deref() {
        let head: String = partial
            .iter()
            .take(10)
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let tail = if partial.len() > 10 {
            format!(", \u{2026} ({} more)", partial.len() - 10)
        } else {
            String::new()
        };
        println!("  \u{26a0} partial ingest: present sheets {head}{tail}");
    }
    println!(
        "  nodes: {} ({} prose leaves)\n  chunks embedded: {}",
        report.nodes_written, report.prose_leaves, report.chunks_written,
    );
    if let Some(path) = &report.envelope_path {
        println!("  envelope: {}", path.display());
    } else {
        println!(
            "  \u{26a0} envelope write failed; `corpus rebuild --only {}` is unavailable.",
            report.ocr_intake_id,
        );
    }
    if report.audit_verdict.as_deref() == Some("needs_work") {
        let confidence = report.audit_confidence.as_deref().unwrap_or("unknown");
        println!(
            "  \u{26a0} metadata audit: needs_work (confidence {confidence}). \
             Run `bookrack exec library.show_metadata_audit '{{\"intake_id\":{}}}'` to inspect.",
            report.ocr_intake_id,
        );
    }
}

/// Surface a `needs_work` audit verdict on stdout so the operator
/// notices it without having to scan stderr. `clean` and the
/// audit-skipped state stay silent — the warning is reserved for the
/// case that actually wants follow-up.
fn print_audit_warning(report: &IngestReport) {
    if report.audit_verdict.as_deref() == Some("needs_work") {
        let confidence = report.audit_confidence.as_deref().unwrap_or("unknown");
        println!(
            "  \u{26a0} metadata audit: needs_work (confidence {confidence}). \
             Run `bookrack exec library.show_metadata_audit '{{\"intake_id\":{}}}'` to inspect.",
            report.intake_id,
        );
    }
}

fn write_string_field(out: &mut String, key: &str, value: &str) {
    out.push('"');
    out.push_str(key);
    out.push_str("\":\"");
    out.push_str(&escape_json(value));
    out.push('"');
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

/// Print every built-in audit-profile name as a JSON array.
pub fn audit_profile_names_json(names: &[&str]) {
    let mut out = String::from("[");
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&escape_json(name));
        out.push('"');
    }
    out.push(']');
    println!("{out}");
}

/// Pretty-print a single audit profile, using `Debug` for the toggle
/// sub-structs so a future field is included without code changes here.
pub fn audit_profile_show(name: &str, profile: &AuditProfile) {
    println!("# {name}");
    println!("audit_enabled: {}", profile.audit_enabled);
    println!("year: {:#?}", profile.year);
    println!("title: {:#?}", profile.title);
    println!("language: {:#?}", profile.language);
    println!("publisher: {:#?}", profile.publisher);
    println!("toc_shape: {:#?}", profile.toc_shape);
    println!("source_prior: {:#?}", profile.source_prior);
    println!("copyright_blocks: {:#?}", profile.copyright_blocks);
    println!("filename_parser: {:#?}", profile.filename_parser);
    println!("extract: {:#?}", profile.extract);
}

/// Pretty-print the per-section differences between two audit profiles.
///
/// Two profiles are walked section by section: when a section differs
/// in `PartialEq`, both sides are pretty-printed under that section's
/// header. Sections that match are skipped. The `name` of the profile
/// is excluded — by construction the two carry their own names and that
/// always differs.
pub fn audit_profile_diff(a_name: &str, a: &AuditProfile, b_name: &str, b: &AuditProfile) {
    let mut differs = false;
    macro_rules! diff_section {
        ($field:ident) => {
            if a.$field != b.$field {
                differs = true;
                println!("# {} differs:", stringify!($field));
                println!("--- {a_name}");
                println!("{:#?}", a.$field);
                println!("+++ {b_name}");
                println!("{:#?}", b.$field);
            }
        };
    }
    if a.audit_enabled != b.audit_enabled {
        differs = true;
        println!("# audit_enabled differs:");
        println!("  {a_name}: {}", a.audit_enabled);
        println!("  {b_name}: {}", b.audit_enabled);
    }
    diff_section!(year);
    diff_section!(title);
    diff_section!(language);
    diff_section!(publisher);
    diff_section!(toc_shape);
    diff_section!(source_prior);
    diff_section!(copyright_blocks);
    diff_section!(filename_parser);
    diff_section!(extract);
    if !differs {
        println!("(no differences between {a_name} and {b_name})");
    }
}

/// Per-store findings the `bookrack verify` command accumulates before
/// rendering. Every field is optional: an unverifiable store leaves its
/// schema flag false and its error populated, and the rest skip the
/// counts that depend on it.
#[derive(Default, serde::Serialize)]
pub struct VerifyReport {
    /// Set when the data directory has no `catalog.db` yet — verify
    /// short-circuits in that case and reports nothing else.
    pub not_initialised: bool,
    pub catalog_schema_ok: bool,
    pub catalog_schema_error: Option<String>,
    pub corpus_schema_ok: bool,
    pub corpus_schema_error: Option<String>,
    pub intake_count: Option<u64>,
    pub missing_intake_files: Option<Vec<i64>>,
    pub vectors_built_at_chunk_count: Option<u64>,
    pub vectors_churn: Option<u64>,
}

/// Print the `bookrack verify` report. Quiet on success, loud on
/// failure: the report itself decides what landed, the renderer only
/// translates.
pub fn verify(report: &VerifyReport) {
    if report.not_initialised {
        println!("data directory not initialised yet.");
        println!("  no catalog.db / corpus.db / lancedb on disk;");
        println!("  run `bookrack ingest <path>` to create them, then verify again.");
        return;
    }
    println!("catalog.db:");
    if report.catalog_schema_ok {
        println!("  schema:         ok");
    } else if let Some(err) = &report.catalog_schema_error {
        println!("  schema:         FAILED");
        for line in err.lines() {
            println!("    {line}");
        }
    }
    if let Some(n) = report.intake_count {
        println!("  intakes:        {n}");
    }
    if let Some(missing) = &report.missing_intake_files {
        if missing.is_empty() {
            println!("  intake files:   every stored_path is present on disk");
        } else {
            println!("  intake files:   {} missing under books/:", missing.len());
            for id in missing {
                println!("    intake {id}");
            }
        }
    }

    println!();
    println!("corpus.db:");
    if report.corpus_schema_ok {
        println!("  schema:         ok");
    } else if let Some(err) = &report.corpus_schema_error {
        println!("  schema:         FAILED");
        for line in err.lines() {
            println!("    {line}");
        }
    }

    if report.vectors_built_at_chunk_count.is_some() || report.vectors_churn.is_some() {
        println!();
        println!("vectors:");
        if let Some(n) = report.vectors_built_at_chunk_count {
            println!("  chunks_at_build: {n}");
        }
        if let Some(n) = report.vectors_churn {
            println!("  churn:           {n}");
        }
    }
}

/// Print the library registry as a human-readable listing. A `None`
/// argument means the registry is not configured at all — surfaced as
/// a single explanatory line rather than an empty body.
pub fn libraries_list(entries: Option<&[LibraryEntry]>) {
    let Some(entries) = entries else {
        println!("No registry configured (set BOOKRACK_REGISTRY).");
        return;
    };
    if entries.is_empty() {
        println!("Registry has no library entries.");
        return;
    }
    println!("{:<20}  {:<10}  data_dir", "name", "default");
    for entry in entries {
        let default_mark = if entry.is_default { "yes" } else { "" };
        println!(
            "{:<20}  {:<10}  {}",
            entry.name,
            default_mark,
            entry.data_dir.display(),
        );
    }
}

/// Same registry as a JSON array — `null` when no registry is set.
pub fn libraries_list_json(entries: Option<&[LibraryEntry]>) {
    let Some(entries) = entries else {
        println!("null");
        return;
    };
    let mut out = String::from("[");
    for (i, entry) in entries.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        write_string_field(&mut out, "name", &entry.name);
        out.push(',');
        write_string_field(&mut out, "data_dir", &entry.data_dir.display().to_string());
        out.push(',');
        out.push_str(&format!("\"is_default\":{}", entry.is_default));
        out.push('}');
    }
    out.push(']');
    println!("{out}");
}
