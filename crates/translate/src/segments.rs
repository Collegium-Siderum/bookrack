// SPDX-License-Identifier: Apache-2.0

//! The `translate_segments` table — mutable sentence-level slices.
//!
//! A segment is the actual unit of translation work: a span of source
//! text inside one unit, addressed by a four-part `(start_node_id,
//! start_char_offset, end_node_id, end_char_offset)` span into the
//! corpus. Segments may be re-sliced by an agent while still virgin;
//! `source_text_sha` fingerprints the spanned source text, serving both
//! as a drift sentinel and as the content key that relocates the span
//! after a re-ingest. The `status` column carries the segment
//! lifecycle `draft -> proposed -> sealed`.

use bookrack_dbkit::{ColumnSpec, ForeignKey, IndexSpec, OnDelete, TableSpec};
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::{Translate, TranslateResult};

/// The single source of truth for the `translate_segments` table's
/// schema. The frozen baseline DDL in [`crate::migrate`] is rendered
/// from this spec; `verify_all` pins the two together on every open.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "translate_segments",
    comment: Some("Mutable translation segments; the unit of translation work."),
    columns: &[
        ColumnSpec::int("segment_id").primary_key(),
        ColumnSpec::int("unit_id")
            .not_null()
            .references(ForeignKey::new(
                "translate_units",
                "unit_id",
                OnDelete::NoAction,
            )),
        ColumnSpec::int("start_node_id")
            .not_null()
            .comment("soft reference to the corpus node the span starts in"),
        ColumnSpec::int("start_char_offset").not_null(),
        ColumnSpec::int("end_node_id")
            .not_null()
            .comment("soft reference to the corpus node the span ends in"),
        ColumnSpec::int("end_char_offset").not_null(),
        ColumnSpec::text("source_text_sha")
            .not_null()
            .comment("content fingerprint; drift sentinel and re-anchor key"),
        ColumnSpec::text("status")
            .not_null()
            .check("status IN ('draft', 'proposed', 'sealed')"),
        ColumnSpec::text("draft_text"),
        ColumnSpec::text("reflection_notes").comment("JSON; reflection or review-note payload"),
        ColumnSpec::text("final_text")
            .comment("semantically locked form; other formats derive at export"),
        ColumnSpec::text("source_kind")
            .check("source_kind IN ('human', 'llm-draft', 'llm-reflected', 'edited', 'imported')"),
        ColumnSpec::text("sealed_at"),
        ColumnSpec::int("version").not_null().default("1"),
    ],
    composite_pk: None,
    uniques: &[&[
        "unit_id",
        "start_node_id",
        "start_char_offset",
        "end_node_id",
        "end_char_offset",
    ]],
    table_checks: &[],
    indexes: &[
        IndexSpec::on("seg_by_unit", &["unit_id", "start_char_offset"]),
        IndexSpec::on("seg_by_status", &["status", "sealed_at"]),
    ],
};

/// One `translate_segments` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentRow {
    pub segment_id: i64,
    pub unit_id: i64,
    pub start_node_id: i64,
    pub start_char_offset: i64,
    pub end_node_id: i64,
    pub end_char_offset: i64,
    pub source_text_sha: String,
    pub status: String,
    pub draft_text: Option<String>,
    pub reflection_notes: Option<String>,
    pub final_text: Option<String>,
    pub source_kind: Option<String>,
    pub sealed_at: Option<String>,
    pub version: i64,
}

impl SegmentRow {
    /// Server-side task-mode inference: text that was imported and not
    /// yet sealed is reviewed; everything else — no text at all, text
    /// an agent drafted, or an imported text already sealed — is
    /// drafted.
    pub fn task_mode(&self) -> &'static str {
        let has_text = self.draft_text.is_some() || self.final_text.is_some();
        if has_text && self.source_kind.as_deref() == Some("imported") && self.status != "sealed" {
            "review"
        } else {
            "draft"
        }
    }
}

/// Canonical fingerprint of a segment's spanned source text: lower-case
/// hex SHA-256 over the UTF-8 bytes. Writers store it in
/// `source_text_sha`; readers recompute it from the corpus text and
/// compare.
pub fn span_sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hex
}

const SELECT_SEGMENT: &str = "SELECT segment_id, unit_id, start_node_id, start_char_offset, \
     end_node_id, end_char_offset, source_text_sha, status, draft_text, reflection_notes, \
     final_text, source_kind, sealed_at, version FROM translate_segments";

fn read_segment(row: &rusqlite::Row<'_>) -> rusqlite::Result<SegmentRow> {
    Ok(SegmentRow {
        segment_id: row.get(0)?,
        unit_id: row.get(1)?,
        start_node_id: row.get(2)?,
        start_char_offset: row.get(3)?,
        end_node_id: row.get(4)?,
        end_char_offset: row.get(5)?,
        source_text_sha: row.get(6)?,
        status: row.get(7)?,
        draft_text: row.get(8)?,
        reflection_notes: row.get(9)?,
        final_text: row.get(10)?,
        source_kind: row.get(11)?,
        sealed_at: row.get(12)?,
        version: row.get(13)?,
    })
}

impl Translate {
    /// The segment with `segment_id`, or `None` if no such row exists.
    pub fn segment(&self, segment_id: i64) -> TranslateResult<Option<SegmentRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("{SELECT_SEGMENT} WHERE segment_id = ?1"),
                [segment_id],
                read_segment,
            )
            .optional()?)
    }

    /// Every segment of `unit_id`. Rows come back in
    /// `(start_node_id, start_char_offset)` order; document-order
    /// sorting against corpus positions is the assembler's job, since
    /// node ids are not ordering keys.
    pub fn segments_in_unit(&self, unit_id: i64) -> TranslateResult<Vec<SegmentRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_SEGMENT} WHERE unit_id = ?1 ORDER BY start_node_id, start_char_offset"
        ))?;
        let rows = stmt.query_map([unit_id], read_segment)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seed;

    fn set_text(t: &Translate, segment_id: i64, column: &str, value: &str) {
        t.conn
            .execute(
                &format!("UPDATE translate_segments SET {column} = ?1 WHERE segment_id = ?2"),
                rusqlite::params![value, segment_id],
            )
            .expect("update");
    }

    #[test]
    fn a_segment_reads_back_every_column() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);
        let id = seed::segment_in(&t, unit_id, (10, 3), (12, 8), "proposed");
        set_text(&t, id, "draft_text", "draft");
        set_text(&t, id, "reflection_notes", "{\"notes\":[]}");
        set_text(&t, id, "final_text", "final");
        set_text(&t, id, "source_kind", "llm-draft");
        set_text(&t, id, "sealed_at", "2026-01-01T00:00:00Z");
        t.conn
            .execute(
                "UPDATE translate_segments SET version = 4 WHERE segment_id = ?1",
                [id],
            )
            .expect("update");

        let row = t.segment(id).expect("read").expect("row");
        assert_eq!(
            row,
            SegmentRow {
                segment_id: id,
                unit_id,
                start_node_id: 10,
                start_char_offset: 3,
                end_node_id: 12,
                end_char_offset: 8,
                source_text_sha: "sha".into(),
                status: "proposed".into(),
                draft_text: Some("draft".into()),
                reflection_notes: Some("{\"notes\":[]}".into()),
                final_text: Some("final".into()),
                source_kind: Some("llm-draft".into()),
                sealed_at: Some("2026-01-01T00:00:00Z".into()),
                version: 4,
            }
        );
    }

    #[test]
    fn an_unknown_segment_reads_as_none() {
        let t = seed::fresh();
        assert_eq!(t.segment(404).expect("read"), None);
    }

    #[test]
    fn segments_in_unit_order_by_start_node_then_offset_and_stay_within_the_unit() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);
        let other = seed::unit(&t, 1, "zh", 11, 1);
        let later_node = seed::segment(&t, unit_id, 12, 0, 5, "draft");
        let same_node_later = seed::segment(&t, unit_id, 10, 40, 60, "draft");
        let first = seed::segment(&t, unit_id, 10, 0, 40, "draft");
        seed::segment(&t, other, 9, 0, 5, "draft");

        let ids: Vec<i64> = t
            .segments_in_unit(unit_id)
            .expect("read")
            .iter()
            .map(|row| row.segment_id)
            .collect();
        assert_eq!(ids, vec![first, same_node_later, later_node]);
    }

    #[test]
    fn task_mode_reviews_only_imported_text_that_is_not_yet_sealed() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);

        let empty = seed::segment(&t, unit_id, 10, 0, 5, "draft");

        let imported_proposed = seed::segment(&t, unit_id, 10, 5, 10, "proposed");
        set_text(&t, imported_proposed, "draft_text", "imported text");
        set_text(&t, imported_proposed, "source_kind", "imported");

        let imported_sealed = seed::segment(&t, unit_id, 10, 10, 15, "sealed");
        set_text(&t, imported_sealed, "final_text", "imported text");
        set_text(&t, imported_sealed, "source_kind", "imported");

        let llm_drafted = seed::segment(&t, unit_id, 10, 15, 20, "proposed");
        set_text(&t, llm_drafted, "draft_text", "machine text");
        set_text(&t, llm_drafted, "source_kind", "llm-draft");

        let mode = |id: i64| t.segment(id).expect("read").expect("row").task_mode();
        assert_eq!(mode(empty), "draft");
        assert_eq!(mode(imported_proposed), "review");
        assert_eq!(mode(imported_sealed), "draft");
        assert_eq!(mode(llm_drafted), "draft");
    }

    #[test]
    fn span_sha256_hex_matches_known_vectors() {
        assert_eq!(
            span_sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            span_sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Multi-byte input hashes its UTF-8 bytes, not its chars.
        assert_ne!(span_sha256_hex("\u{e9}"), span_sha256_hex("e"));
    }
}
