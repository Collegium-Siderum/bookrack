// SPDX-License-Identifier: Apache-2.0

//! User-facing formatting. Every line a command prints for a human lands
//! here, written to **stdout**; progress and diagnostics go through
//! `tracing` to stderr instead, so the result stream and the observation
//! stream never interleave.

use bookrack_audit_profile::AuditProfile;
use bookrack_catalog::{BookPipelineAudit, MetadataAudit};
use bookrack_config::LibraryEntry;
use bookrack_ingest::IngestReport;
use bookrack_metadata::{FieldGrade, FieldReport, MetadataReport};
use bookrack_query::dto::{BookDetail, LibraryStats, ListBooksResult, Toc};
use bookrack_search::Citation;
use bookrack_vectors::VectorsMeta;

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
    if report.no_op {
        println!(
            "Already ingested as intake {}; source and stamps unchanged, nothing to do.",
            report.intake_id,
        );
        println!("  (Pass --force to re-extract, re-chunk, and re-embed anyway.)");
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

/// One row of the `metadata list` listing. Hand-assembled by the
/// command before rendering so the renderer never reaches into the
/// catalog itself.
pub struct MetadataListRow {
    pub intake_id: i64,
    pub title: Option<String>,
    pub confidence: Option<String>,
    pub review_status: Option<String>,
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

/// Print the `metadata list` table as a human-readable listing.
pub fn metadata_list(rows: &[MetadataListRow], total: u64, needs_review: bool) {
    if rows.is_empty() {
        if needs_review {
            println!("No books need review.");
        } else {
            println!("No books.");
        }
        return;
    }
    println!(
        "{:>8}  {:<10}  {:<14}  title",
        "intake", "confidence", "review",
    );
    for row in rows {
        let confidence = row.confidence.as_deref().unwrap_or("-");
        let review = row.review_status.as_deref().unwrap_or("pending");
        let title = row.title.as_deref().unwrap_or("(no title)");
        println!(
            "{:>8}  {:<10}  {:<14}  {}",
            row.intake_id, confidence, review, title,
        );
    }
    let shown = rows.len() as u64;
    if shown < total {
        println!(
            "\n... {} more (showing {} of {})",
            total - shown,
            shown,
            total
        );
    } else {
        println!("\ntotal: {total}");
    }
}

/// Same listing as a JSON object.
pub fn metadata_list_json(rows: &[MetadataListRow], total: u64) {
    let mut out = String::from("{\"total\":");
    out.push_str(&total.to_string());
    out.push_str(",\"rows\":[");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        out.push_str(&format!("\"intake_id\":{}", row.intake_id));
        out.push(',');
        out.push_str("\"title\":");
        write_opt_string(&mut out, row.title.as_deref());
        out.push(',');
        out.push_str("\"confidence\":");
        write_opt_string(&mut out, row.confidence.as_deref());
        out.push(',');
        out.push_str("\"review_status\":");
        write_opt_string(&mut out, row.review_status.as_deref());
        out.push('}');
    }
    out.push_str("]}");
    println!("{out}");
}

/// Print the `metadata_audit` rows for one book, oldest first.
pub fn metadata_audit_trail(book: i64, rows: &[MetadataAudit]) {
    println!("Book {book}: {} metadata audit rows", rows.len());
    for row in rows {
        let actor = row.actor_kind.as_str();
        let detail = row.actor_detail.as_deref().unwrap_or("-");
        let field = row.field.as_deref().unwrap_or("-");
        let old = row.old_value.as_deref().unwrap_or("");
        let new = row.new_value.as_deref().unwrap_or("");
        let reason = row.reason.as_deref().unwrap_or("");
        println!(
            "  [{ts}] {table_name}.{field} {action} by {actor}/{detail}",
            ts = row.changed_at,
            table_name = row.table_name,
            field = field,
            action = row.action,
            actor = actor,
            detail = detail,
        );
        if !old.is_empty() || !new.is_empty() {
            println!("    {old:?} -> {new:?}");
        }
        if !reason.is_empty() {
            println!("    reason: {reason}");
        }
    }
}

/// Same trail as a JSON object.
pub fn metadata_audit_trail_json(book: i64, rows: &[MetadataAudit]) {
    let mut out = String::from("{");
    out.push_str(&format!("\"book\":{book}"));
    out.push_str(",\"rows\":[");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        out.push_str(&format!("\"audit_id\":{}", row.audit_id));
        out.push(',');
        write_string_field(&mut out, "changed_at", &row.changed_at);
        out.push(',');
        write_string_field(&mut out, "table_name", &row.table_name);
        out.push(',');
        write_string_field(&mut out, "action", &row.action);
        out.push(',');
        out.push_str("\"field\":");
        write_opt_string(&mut out, row.field.as_deref());
        out.push(',');
        out.push_str("\"old_value\":");
        write_opt_string(&mut out, row.old_value.as_deref());
        out.push(',');
        out.push_str("\"new_value\":");
        write_opt_string(&mut out, row.new_value.as_deref());
        out.push(',');
        write_string_field(&mut out, "actor_kind", row.actor_kind.as_str());
        out.push(',');
        out.push_str("\"actor_detail\":");
        write_opt_string(&mut out, row.actor_detail.as_deref());
        out.push(',');
        out.push_str("\"reason\":");
        write_opt_string(&mut out, row.reason.as_deref());
        out.push('}');
    }
    out.push_str("]}");
    println!("{out}");
}

/// Print the `book_pipeline_audit` rows for one book, oldest first.
pub fn pipeline_trail(book: i64, rows: &[BookPipelineAudit]) {
    println!("Book {book}: {} pipeline audit rows", rows.len());
    for row in rows {
        let adapter = row.adapter.as_deref().unwrap_or("-");
        let duration = row
            .duration_ms
            .map(|d| format!("{d}ms"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  [{ts}] {stage}/{sub} {outcome} adapter={adapter} dur={duration} run={run}",
            ts = row.ts,
            stage = row.stage,
            sub = row.sub_step,
            outcome = row.outcome,
            run = row.pipeline_run_id,
        );
        if let Some(err) = &row.error_message {
            println!("    error: {err}");
        }
        if let Some(metrics) = &row.metric_summary {
            println!("    metrics: {metrics}");
        }
    }
}

/// Same trail as a JSON object.
pub fn pipeline_trail_json(book: i64, rows: &[BookPipelineAudit]) {
    let mut out = String::from("{");
    out.push_str(&format!("\"book\":{book}"));
    out.push_str(",\"rows\":[");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        out.push_str(&format!("\"audit_id\":{}", row.audit_id));
        out.push(',');
        write_string_field(&mut out, "ts", &row.ts);
        out.push(',');
        write_string_field(&mut out, "stage", &row.stage);
        out.push(',');
        write_string_field(&mut out, "sub_step", &row.sub_step);
        out.push(',');
        write_string_field(&mut out, "outcome", &row.outcome);
        out.push(',');
        write_string_field(&mut out, "pipeline_run_id", &row.pipeline_run_id);
        out.push(',');
        out.push_str("\"adapter\":");
        write_opt_string(&mut out, row.adapter.as_deref());
        out.push(',');
        out.push_str("\"duration_ms\":");
        match row.duration_ms {
            Some(d) => out.push_str(&d.to_string()),
            None => out.push_str("null"),
        }
        out.push(',');
        out.push_str("\"error_message\":");
        write_opt_string(&mut out, row.error_message.as_deref());
        out.push(',');
        out.push_str("\"metric_summary\":");
        write_opt_string(&mut out, row.metric_summary.as_deref());
        out.push('}');
    }
    out.push_str("]}");
    println!("{out}");
}

/// Write an `Option<&str>` as a JSON string or `null`.
fn write_opt_string(out: &mut String, value: Option<&str>) {
    match value {
        Some(v) => {
            out.push('"');
            out.push_str(&escape_json(v));
            out.push('"');
        }
        None => out.push_str("null"),
    }
}

/// Print a `books list` / `books find` page as a human-readable table.
pub fn books_list(result: &ListBooksResult) {
    if result.books.is_empty() {
        println!("No books.");
        return;
    }
    println!(
        "{:>8}  {:<10}  {:<25}  title",
        "intake", "status", "contributor",
    );
    for book in &result.books {
        let format = book.format.as_deref().unwrap_or("-");
        let contributor = book.top_contributor.as_deref().unwrap_or("-");
        let title = book.title.as_deref().unwrap_or("(no title)");
        println!(
            "{:>8}  {:<10}  {:<25}  [{format}] {title}",
            book.intake_id, book.status, contributor,
        );
    }
    let shown = result.books.len() as u64;
    let suffix = if result.truncated { " (truncated)" } else { "" };
    if shown < result.total {
        println!(
            "\n... {} more (showing {} of {}{})",
            result.total - shown,
            shown,
            result.total,
            suffix,
        );
    } else {
        println!("\ntotal: {}{}", result.total, suffix);
    }
}

/// Same page as a JSON object, using `serde_json` over the DTO.
pub fn books_list_json(result: &ListBooksResult) {
    let s = serde_json::to_string(result).expect("ListBooksResult serializes");
    println!("{s}");
}

/// Print one book's full bibliographic record.
pub fn books_show(detail: &BookDetail) {
    let title = detail.title.as_deref().unwrap_or("(no title)");
    let format = detail.format.as_deref().unwrap_or("-");
    println!(
        "Book {} [{format}] {} {title}",
        detail.intake_id, detail.status,
    );
    if !detail.effective_biblio.is_empty() {
        println!("  effective metadata:");
        for (key, value) in &detail.effective_biblio {
            println!("    {key}: {value}");
        }
    }
    if !detail.contributors.is_empty() {
        println!("  contributors:");
        for c in &detail.contributors {
            let nat = c
                .nationality
                .as_deref()
                .map(|n| format!(" ({n})"))
                .unwrap_or_default();
            println!(
                "    [{}#{}] {}{nat} via {}",
                c.role, c.ordinal, c.name, c.origin,
            );
        }
    }
}

/// Same record as a JSON object.
pub fn books_show_json(detail: &BookDetail) {
    let s = serde_json::to_string(detail).expect("BookDetail serializes");
    println!("{s}");
}

/// Print a book's table of contents, depth-first.
pub fn books_toc(toc: &Toc) {
    println!(
        "Book {}: {} TOC nodes{}",
        toc.intake_id,
        toc.nodes.len(),
        if toc.truncated { " (truncated)" } else { "" },
    );
    for node in &toc.nodes {
        let indent: String = std::iter::repeat_n("  ", node.depth as usize).collect();
        let title = node.title.as_deref().unwrap_or("(untitled)");
        println!("{indent}- [{}] {title}", node.node_id);
    }
}

/// Same TOC as a JSON object.
pub fn books_toc_json(toc: &Toc) {
    let s = serde_json::to_string(toc).expect("Toc serializes");
    println!("{s}");
}

/// Print aggregate library counts.
pub fn books_stats(stats: &LibraryStats) {
    print_count_map("intake by status", &stats.intake_counts_by_status);
    print_count_map("intake by format", &stats.intake_count_by_format);
    print_count_map("book state by stage", &stats.book_state_counts_by_stage);
    print_count_map(
        "retrieval issue by status",
        &stats.retrieval_issue_counts_by_status,
    );
}

/// Same stats as a JSON object.
pub fn books_stats_json(stats: &LibraryStats) {
    let s = serde_json::to_string(stats).expect("LibraryStats serializes");
    println!("{s}");
}

fn print_count_map(label: &str, map: &std::collections::BTreeMap<String, u64>) {
    if map.is_empty() {
        return;
    }
    println!("{label}:");
    for (key, value) in map {
        println!("  {key:>16}  {value}");
    }
}

/// Aggregated view the `bookrack info` command assembles before
/// rendering. Pure data — the renderer never reads from the catalog or
/// the filesystem itself, so it stays trivially testable.
pub struct InfoSnapshot {
    pub data_dir: String,
    pub library: Option<String>,
    pub source: &'static str,
    pub ollama_url: String,
    pub embed_model_configured: String,
    pub corpus_schema_version_expected: u32,
    pub catalog_schema_version_expected: u32,
    pub corpus_stamps: CorpusStamps,
    pub vectors_meta: Option<VectorsMeta>,
    pub intake_count: Option<u64>,
    pub ready_book_count: Option<u64>,
    pub disk: DiskUsage,
}

/// Four `index_meta` stamps from `corpus.db`, plus the on-disk schema
/// version. Each is optional — a fresh library has none of them.
#[derive(Default)]
pub struct CorpusStamps {
    pub embed_model: Option<String>,
    pub vector_dim: Option<String>,
    pub chunk_version: Option<String>,
    pub normalize_version: Option<String>,
    pub schema_version_on_disk: Option<String>,
}

/// Sizes of the three persisted stores. Each is `None` when the file or
/// directory is absent — a freshly-bootstrapped library reports nothing.
pub struct DiskUsage {
    pub catalog_db: Option<u64>,
    pub corpus_db: Option<u64>,
    pub lancedb_dir: Option<u64>,
}

/// Print the one-screen status card.
pub fn info(snapshot: &InfoSnapshot) {
    println!("data_dir:        {}", snapshot.data_dir);
    let library = snapshot.library.as_deref().unwrap_or("(unnamed)");
    println!("library:         {library}  via {}", snapshot.source);
    println!("ollama_url:      {}", snapshot.ollama_url);
    println!(
        "embed_model:     {} (configured)",
        snapshot.embed_model_configured
    );

    println!();
    println!("corpus.db:");
    println!(
        "  schema_version: binary {}, on-disk {}",
        snapshot.corpus_schema_version_expected,
        snapshot
            .corpus_stamps
            .schema_version_on_disk
            .as_deref()
            .unwrap_or("(empty)"),
    );
    println!(
        "  embed_model:    {}",
        snapshot
            .corpus_stamps
            .embed_model
            .as_deref()
            .unwrap_or("(empty)"),
    );
    println!(
        "  vector_dim:     {}",
        snapshot
            .corpus_stamps
            .vector_dim
            .as_deref()
            .unwrap_or("(empty)"),
    );
    println!(
        "  chunk_version:  {}",
        snapshot
            .corpus_stamps
            .chunk_version
            .as_deref()
            .unwrap_or("(empty)"),
    );
    println!(
        "  normalize_ver:  {}",
        snapshot
            .corpus_stamps
            .normalize_version
            .as_deref()
            .unwrap_or("(empty)"),
    );

    println!();
    println!("catalog.db:");
    println!(
        "  schema_version: binary {} (run a write to refresh the on-disk row)",
        snapshot.catalog_schema_version_expected,
    );
    if let Some(n) = snapshot.intake_count {
        println!("  intakes:        {n}");
    } else {
        println!("  intakes:        (catalog unreadable)");
    }
    if let Some(n) = snapshot.ready_book_count {
        println!("  ready books:    {n}");
    }

    println!();
    println!("vectors:");
    if let Some(meta) = &snapshot.vectors_meta {
        println!("  ann kind:       {}", meta.kind);
        println!("  num_partitions: {}", meta.num_partitions);
        println!("  index name:     {}", meta.lance_index_name);
        println!("  built_at:       {}", meta.built_at);
        println!("  chunks built:   {}", meta.built_at_chunk_count);
        println!("  churn:          {}", meta.churn_since_rebuild);
    } else {
        println!("  (no vectors_meta.json — never built)");
    }

    println!();
    println!("disk:");
    println!(
        "  catalog.db:     {}",
        format_size(snapshot.disk.catalog_db)
    );
    println!("  corpus.db:      {}", format_size(snapshot.disk.corpus_db));
    println!(
        "  lancedb/:       {}",
        format_size(snapshot.disk.lancedb_dir)
    );
}

/// Render a byte count as a short human-readable string, or `(absent)`
/// when the source could not be read.
fn format_size(bytes: Option<u64>) -> String {
    let Some(n) = bytes else {
        return "(absent)".to_string();
    };
    const KIB: f64 = 1024.0;
    let n = n as f64;
    if n < KIB {
        return format!("{n} B");
    }
    if n < KIB * KIB {
        return format!("{:.1} KiB", n / KIB);
    }
    if n < KIB * KIB * KIB {
        return format!("{:.1} MiB", n / (KIB * KIB));
    }
    format!("{:.2} GiB", n / (KIB * KIB * KIB))
}

/// Per-store findings the `bookrack verify` command accumulates before
/// rendering. Every field is optional: an unverifiable store leaves its
/// schema flag false and its error populated, and the rest skip the
/// counts that depend on it.
#[derive(Default)]
pub struct VerifyReport {
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
