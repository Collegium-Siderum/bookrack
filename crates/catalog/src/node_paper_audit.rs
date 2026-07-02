// SPDX-License-Identifier: Apache-2.0

//! The `node_paper_audit` table — the SQL-dimension face of glean's
//! `PaperReport`.
//!
//! One row per `(intake_id, scope)` and rewritten on each pass. Mirrors
//! [`crate::node_reviews::SPEC`] keying so a join on the same key is
//! trivial. `scope` carries `'paper'` today; the column ships now so a
//! later kind extension does not need a schema bump.
//!
//! The audit's `notes` JSON in `node_reviews` and the
//! `node_publication_attrs.audit_verdict / confidence` rollup stay the
//! sources of truth for free-form text and the read-side rollup
//! respectively. This table is the **write-side** projection that lets
//! per-field grade distributions, per-flag frequencies, and profile ×
//! verdict crosses be answered with one `GROUP BY` instead of a JSON
//! scan.
//!
//! Column shape: eight header columns, nine `grade_<field>` columns
//! holding one of `missing` / `weak` / `medium` / `strong`, and one
//! `flag_<token>` boolean per [`bookrack-glean`]'s `PaperFlag` enum
//! token. Booleans are stored as `INTEGER NOT NULL DEFAULT 0` so a
//! flag absent from a report writes as `0` and a `SUM(flag_*)` over
//! the table answers per-flag frequency directly.
//!
//! `profile_name` carries the audit profile short name
//! (`default` / `trust-source` / `strict`); `profile_fingerprint` and
//! `profile_toggle_summary` (M[14]) pin the effective profile the row
//! was judged with, independent of what the name resolves to later.

use bookrack_dbkit::{ColumnSpec, IndexSpec, TableSpec};
use rusqlite::{OptionalExtension, Row, named_params};

use crate::{Catalog, Result};

/// The single source of truth for the `node_paper_audit` table's
/// schema. Its DDL is rendered from this spec.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "node_paper_audit",
    comment: Some("Per-paper audit projection: grades, flags, verdict."),
    columns: &[
        ColumnSpec::int("intake_id").not_null(),
        ColumnSpec::text("scope").not_null(),
        ColumnSpec::text("profile_name").not_null(),
        ColumnSpec::text("verdict")
            .not_null()
            .comment("clean / needs_work"),
        ColumnSpec::text("confidence")
            .not_null()
            .comment("low / medium / high"),
        ColumnSpec::text("csl_type").comment("driving CSL-type; NULL when none was inferred"),
        ColumnSpec::text("audited_at")
            .not_null()
            .comment("ISO-8601 UTC"),
        ColumnSpec::text("extractor_version").not_null(),
        // Nine per-field grade columns: missing / weak / medium / strong.
        ColumnSpec::text("grade_title").not_null(),
        ColumnSpec::text("grade_year").not_null(),
        ColumnSpec::text("grade_doi").not_null(),
        ColumnSpec::text("grade_arxiv").not_null(),
        ColumnSpec::text("grade_issn").not_null(),
        ColumnSpec::text("grade_container").not_null(),
        ColumnSpec::text("grade_abstract").not_null(),
        ColumnSpec::text("grade_author").not_null(),
        ColumnSpec::text("grade_language").not_null(),
        // Per-flag boolean columns: 0/1. Names track `PaperFlag::as_token()`.
        ColumnSpec::int("flag_doi_invalid_format")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_arxiv_id_invalid_format")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_issn_invalid_checksum")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_orcid_invalid_checksum")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_no_stable_identifier")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_empty").not_null().default("0"),
        ColumnSpec::int("flag_voided").not_null().default("0"),
        ColumnSpec::int("flag_placeholder_value")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_equals_filename")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_source_watermark")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_purely_numeric")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_year_out_of_range")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_date_looks_like_timestamp")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_pdf_year_likely_file_date")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_lang_mismatches_body")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_non_bcp47").not_null().default("0"),
        ColumnSpec::int("flag_source_prior_weak")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_doubtful_text_layer")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_abstract_too_short")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_venue_not_in_list")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_author_list_empty")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_author_list_single_word")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_title_echoes_arxiv_banner")
            .not_null()
            .default("0"),
        ColumnSpec::int("flag_contributor_sentinel_name")
            .not_null()
            .default("0"),
        ColumnSpec::text("pipeline_run_id")
            .comment("run group; NULL for rows written before M[12]"),
        ColumnSpec::text("profile_fingerprint")
            .comment("stable fingerprint of the effective profile; NULL before M[14]"),
        ColumnSpec::text("profile_toggle_summary")
            .comment("JSON toggle summary of the effective profile; NULL before M[14]"),
    ],
    composite_pk: Some(&["intake_id", "scope"]),
    uniques: &[],
    table_checks: &[],
    indexes: &[
        IndexSpec::on("idx_node_paper_audit_profile", &["profile_name"]),
        IndexSpec::on(
            "idx_node_paper_audit_verdict_confidence",
            &["verdict", "confidence"],
        ),
        IndexSpec::on("idx_node_paper_audit_run", &["pipeline_run_id"]),
    ],
};

/// Every `flag_*` column name on `node_paper_audit`, in declaration
/// order. Kept here so the writer and downstream readers share one
/// canonical list rather than rebuilding it from the schema.
pub const FLAG_COLUMNS: &[&str] = &[
    "flag_doi_invalid_format",
    "flag_arxiv_id_invalid_format",
    "flag_issn_invalid_checksum",
    "flag_orcid_invalid_checksum",
    "flag_no_stable_identifier",
    "flag_empty",
    "flag_voided",
    "flag_placeholder_value",
    "flag_equals_filename",
    "flag_source_watermark",
    "flag_purely_numeric",
    "flag_year_out_of_range",
    "flag_date_looks_like_timestamp",
    "flag_pdf_year_likely_file_date",
    "flag_lang_mismatches_body",
    "flag_non_bcp47",
    "flag_source_prior_weak",
    "flag_doubtful_text_layer",
    "flag_abstract_too_short",
    "flag_venue_not_in_list",
    "flag_author_list_empty",
    "flag_author_list_single_word",
    "flag_title_echoes_arxiv_banner",
    "flag_contributor_sentinel_name",
];

/// Every `grade_*` column name on `node_paper_audit`, paired with the
/// `PaperReport.fields` key it projects from.
pub const GRADE_COLUMNS: &[(&str, &str)] = &[
    ("grade_title", "title"),
    ("grade_year", "year"),
    ("grade_doi", "doi"),
    ("grade_arxiv", "arxiv"),
    ("grade_issn", "issn"),
    ("grade_container", "container"),
    ("grade_abstract", "abstract"),
    ("grade_author", "author"),
    ("grade_language", "language"),
];

/// One row about to be written to `node_paper_audit`. Built from a
/// `PaperReport` in `bookrack-glean`; the catalog crate keeps the type
/// transport-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewNodePaperAudit {
    pub intake_id: i64,
    pub scope: String,
    pub profile_name: String,
    pub verdict: String,
    pub confidence: String,
    pub csl_type: Option<String>,
    pub audited_at: String,
    pub extractor_version: String,
    /// `grade_*` values, indexed by [`GRADE_COLUMNS`] position. Each
    /// holds one of `missing` / `weak` / `medium` / `strong`.
    pub grades: [String; GRADE_COLUMNS.len()],
    /// `flag_*` values, indexed by [`FLAG_COLUMNS`] position. `1` if
    /// the flag was emitted by the audit, `0` otherwise.
    pub flags: [u8; FLAG_COLUMNS.len()],
    /// The `pipeline_runs.pipeline_run_id` that grouped this audit, or
    /// `None` when the writer is not running inside an opened run.
    pub pipeline_run_id: Option<String>,
    /// Stable fingerprint of the effective audit profile, or `None`
    /// when the writer could not compute one.
    pub profile_fingerprint: Option<String>,
    /// JSON summary of the profile's boolean toggles, or `None` when
    /// the writer could not compute one.
    pub profile_toggle_summary: Option<String>,
}

/// One `node_paper_audit` row, read back from the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePaperAudit {
    pub intake_id: i64,
    pub scope: String,
    pub profile_name: String,
    pub verdict: String,
    pub confidence: String,
    pub csl_type: Option<String>,
    pub audited_at: String,
    pub extractor_version: String,
    pub grades: [String; GRADE_COLUMNS.len()],
    pub flags: [u8; FLAG_COLUMNS.len()],
    pub pipeline_run_id: Option<String>,
    pub profile_fingerprint: Option<String>,
    pub profile_toggle_summary: Option<String>,
}

impl NodePaperAudit {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<NodePaperAudit> {
        let mut grades: [String; GRADE_COLUMNS.len()] = Default::default();
        for (i, (col, _)) in GRADE_COLUMNS.iter().enumerate() {
            grades[i] = row.get(*col)?;
        }
        let mut flags: [u8; FLAG_COLUMNS.len()] = [0; FLAG_COLUMNS.len()];
        for (i, col) in FLAG_COLUMNS.iter().enumerate() {
            let v: i64 = row.get(*col)?;
            flags[i] = if v == 0 { 0 } else { 1 };
        }
        Ok(NodePaperAudit {
            intake_id: row.get("intake_id")?,
            scope: row.get("scope")?,
            profile_name: row.get("profile_name")?,
            verdict: row.get("verdict")?,
            confidence: row.get("confidence")?,
            csl_type: row.get("csl_type")?,
            audited_at: row.get("audited_at")?,
            extractor_version: row.get("extractor_version")?,
            grades,
            flags,
            pipeline_run_id: row.get("pipeline_run_id")?,
            profile_fingerprint: row.get("profile_fingerprint")?,
            profile_toggle_summary: row.get("profile_toggle_summary")?,
        })
    }
}

/// Build the `INSERT … ON CONFLICT DO UPDATE` statement once. Column
/// list and assignment list both come from the same source, so a new
/// flag or grade column reaches the writer through the spec alone.
fn upsert_sql() -> String {
    let mut cols: Vec<&'static str> = vec![
        "intake_id",
        "scope",
        "profile_name",
        "verdict",
        "confidence",
        "csl_type",
        "audited_at",
        "extractor_version",
    ];
    for (col, _) in GRADE_COLUMNS {
        cols.push(col);
    }
    for col in FLAG_COLUMNS {
        cols.push(col);
    }
    cols.push("pipeline_run_id");
    cols.push("profile_fingerprint");
    cols.push("profile_toggle_summary");
    let placeholders: Vec<String> = cols.iter().map(|c| format!(":{c}")).collect();
    let assignments: Vec<String> = cols
        .iter()
        .filter(|c| **c != "intake_id" && **c != "scope")
        .map(|c| format!("{c} = excluded.{c}"))
        .collect();
    format!(
        "INSERT INTO node_paper_audit ({cols}) VALUES ({values}) \
         ON CONFLICT(intake_id, scope) DO UPDATE SET {assigns}",
        cols = cols.join(", "),
        values = placeholders.join(", "),
        assigns = assignments.join(", "),
    )
}

/// A `SELECT` of every column with `tail` appended; column list from
/// [`SPEC`].
fn select_sql(tail: &str) -> String {
    format!("SELECT {} FROM node_paper_audit {tail}", SPEC.select_list())
}

impl Catalog {
    /// Insert or replace one `node_paper_audit` row, keyed by
    /// `(intake_id, scope)`. The whole row is overwritten on conflict.
    pub fn upsert_node_paper_audit(&self, new: &NewNodePaperAudit) -> Result<()> {
        let sql = upsert_sql();
        let mut stmt = self.conn.prepare(&sql)?;
        let csl_type = new.csl_type.as_deref();
        let mut params: Vec<(String, &dyn rusqlite::ToSql)> =
            Vec::with_capacity(8 + 9 + FLAG_COLUMNS.len() + 3);
        params.push((":intake_id".to_string(), &new.intake_id));
        params.push((":scope".to_string(), &new.scope));
        params.push((":profile_name".to_string(), &new.profile_name));
        params.push((":verdict".to_string(), &new.verdict));
        params.push((":confidence".to_string(), &new.confidence));
        params.push((":csl_type".to_string(), &csl_type));
        params.push((":audited_at".to_string(), &new.audited_at));
        params.push((":extractor_version".to_string(), &new.extractor_version));
        for (i, (col, _)) in GRADE_COLUMNS.iter().enumerate() {
            params.push((format!(":{col}"), &new.grades[i]));
        }
        for (i, col) in FLAG_COLUMNS.iter().enumerate() {
            params.push((format!(":{col}"), &new.flags[i]));
        }
        let pipeline_run_id = new.pipeline_run_id.as_deref();
        params.push((":pipeline_run_id".to_string(), &pipeline_run_id));
        let profile_fingerprint = new.profile_fingerprint.as_deref();
        params.push((":profile_fingerprint".to_string(), &profile_fingerprint));
        let profile_toggle_summary = new.profile_toggle_summary.as_deref();
        params.push((
            ":profile_toggle_summary".to_string(),
            &profile_toggle_summary,
        ));
        let refs: Vec<(&str, &dyn rusqlite::ToSql)> =
            params.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        stmt.execute(refs.as_slice())?;
        Ok(())
    }

    /// Fetch the audit row at `(intake_id, scope)`, or `None` when no
    /// row has been written.
    pub fn node_paper_audit(&self, intake_id: i64, scope: &str) -> Result<Option<NodePaperAudit>> {
        let mut stmt = self.conn.prepare(&select_sql(
            "WHERE intake_id = :intake_id AND scope = :scope",
        ))?;
        let row = stmt
            .query_row(
                named_params! { ":intake_id": intake_id, ":scope": scope },
                NodePaperAudit::from_row,
            )
            .optional()?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(intake_id: i64) -> NewNodePaperAudit {
        let mut grades: [String; GRADE_COLUMNS.len()] = Default::default();
        for g in grades.iter_mut() {
            *g = "medium".to_string();
        }
        NewNodePaperAudit {
            intake_id,
            scope: "paper".to_string(),
            profile_name: "default".to_string(),
            verdict: "clean".to_string(),
            confidence: "medium".to_string(),
            csl_type: Some("article-journal".to_string()),
            audited_at: "2026-06-28T10:00:00Z".to_string(),
            extractor_version: "0.0.0-test".to_string(),
            grades,
            flags: [0; FLAG_COLUMNS.len()],
            pipeline_run_id: None,
            profile_fingerprint: None,
            profile_toggle_summary: None,
        }
    }

    #[test]
    fn a_paper_audit_round_trips_every_column() {
        let catalog = Catalog::open_in_memory().expect("open");
        let mut row = fixture(1);
        row.grades[0] = "strong".to_string();
        row.flags[0] = 1;
        row.flags[4] = 1;
        catalog.upsert_node_paper_audit(&row).expect("write");

        let read = catalog
            .node_paper_audit(1, "paper")
            .expect("read")
            .expect("present");
        assert_eq!(read.profile_name, "default");
        assert_eq!(read.verdict, "clean");
        assert_eq!(read.confidence, "medium");
        assert_eq!(read.csl_type.as_deref(), Some("article-journal"));
        assert_eq!(read.grades[0], "strong");
        assert_eq!(read.grades[1], "medium");
        assert_eq!(read.flags[0], 1);
        assert_eq!(read.flags[1], 0);
        assert_eq!(read.flags[4], 1);
        assert_eq!(read.pipeline_run_id, None);
    }

    #[test]
    fn a_paper_audit_round_trips_with_a_pipeline_run_id() {
        let catalog = Catalog::open_in_memory().expect("open");
        let mut row = fixture(1);
        row.pipeline_run_id = Some("glean_review-2026-06-28T10:00:00Z-deadbeef".to_string());
        catalog.upsert_node_paper_audit(&row).expect("write");
        let read = catalog
            .node_paper_audit(1, "paper")
            .expect("read")
            .expect("present");
        assert_eq!(
            read.pipeline_run_id.as_deref(),
            Some("glean_review-2026-06-28T10:00:00Z-deadbeef")
        );
    }

    #[test]
    fn a_paper_audit_round_trips_fingerprint_and_summary() {
        let catalog = Catalog::open_in_memory().expect("open");
        let mut row = fixture(1);
        row.profile_fingerprint = Some("0123456789abcdef".to_string());
        row.profile_toggle_summary =
            Some(r#"[{"enabled":true,"name":"identifier.require_any"}]"#.to_string());
        catalog.upsert_node_paper_audit(&row).expect("write");
        let read = catalog
            .node_paper_audit(1, "paper")
            .expect("read")
            .expect("present");
        assert_eq!(
            read.profile_fingerprint.as_deref(),
            Some("0123456789abcdef")
        );
        assert_eq!(
            read.profile_toggle_summary.as_deref(),
            Some(r#"[{"enabled":true,"name":"identifier.require_any"}]"#),
        );
    }

    #[test]
    fn upsert_overwrites_an_existing_row() {
        let catalog = Catalog::open_in_memory().expect("open");
        catalog.upsert_node_paper_audit(&fixture(1)).expect("first");
        let mut updated = fixture(1);
        updated.verdict = "needs_work".to_string();
        updated.flags[4] = 1;
        catalog.upsert_node_paper_audit(&updated).expect("second");
        let read = catalog
            .node_paper_audit(1, "paper")
            .expect("read")
            .expect("present");
        assert_eq!(read.verdict, "needs_work");
        assert_eq!(read.flags[4], 1);
    }

    #[test]
    fn a_missing_row_reads_as_none() {
        let catalog = Catalog::open_in_memory().expect("open");
        assert!(
            catalog
                .node_paper_audit(404, "paper")
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn the_built_schema_conforms_to_the_spec() {
        let catalog = Catalog::open_in_memory().expect("open");
        bookrack_dbkit::verify_table(&catalog.conn, &SPEC)
            .expect("the migration baseline must conform to the spec");
    }

    #[test]
    fn flag_and_grade_column_lists_match_the_spec() {
        // Every name in FLAG_COLUMNS and GRADE_COLUMNS appears in SPEC.
        let cols: Vec<&str> = SPEC.columns.iter().map(|c| c.name).collect();
        for f in FLAG_COLUMNS {
            assert!(cols.contains(f), "missing flag column {f}");
        }
        for (g, _) in GRADE_COLUMNS {
            assert!(cols.contains(g), "missing grade column {g}");
        }
    }
}
