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

use crate::{Translate, TranslateError, TranslateResult};

/// `status` of a segment nobody has written to yet.
pub const STATUS_DRAFT: &str = "draft";
/// `status` once a final text is recorded or imported.
pub const STATUS_PROPOSED: &str = "proposed";
/// `status` once the final text is locked.
pub const STATUS_SEALED: &str = "sealed";
/// Every `status` value, in lifecycle order.
pub const STATUSES: &[&str] = &[STATUS_DRAFT, STATUS_PROPOSED, STATUS_SEALED];
/// Every `source_kind` value a sealed or imported segment may carry.
pub const SOURCE_KINDS: &[&str] = &["human", "llm-draft", "llm-reflected", "edited", "imported"];
/// `source_kind` an import stamps.
pub const SOURCE_KIND_IMPORTED: &str = "imported";

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

/// One segment to insert: a span and the fingerprint of the text it
/// covers. Status starts at `draft`, version at 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSegment {
    pub unit_id: i64,
    pub start_node_id: i64,
    pub start_char_offset: i64,
    pub end_node_id: i64,
    pub end_char_offset: i64,
    pub source_text_sha: String,
}

/// The three writes of a proposal, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposeStage {
    /// Records `draft_text`.
    Draft,
    /// Records `reflection_notes`, and a revised `draft_text` when given.
    Reflection,
    /// Records `final_text` and moves the segment to `proposed`.
    Final,
}

impl ProposeStage {
    /// The audit action this stage records.
    pub const fn action(self) -> &'static str {
        match self {
            ProposeStage::Draft => "propose_draft",
            ProposeStage::Reflection => "propose_reflection",
            ProposeStage::Final => "propose_final",
        }
    }

    const fn name(self) -> &'static str {
        match self {
            ProposeStage::Draft => "draft",
            ProposeStage::Reflection => "reflection",
            ProposeStage::Final => "final",
        }
    }
}

impl Translate {
    fn segment_or_unknown(&self, segment_id: i64) -> TranslateResult<SegmentRow> {
        self.segment(segment_id)?
            .ok_or(TranslateError::UnknownSegment { segment_id })
    }

    fn expect_version(row: &SegmentRow, expected: i64) -> TranslateResult<()> {
        if row.version != expected {
            return Err(TranslateError::VersionConflict {
                segment_id: row.segment_id,
                expected,
                current: row.version,
            });
        }
        Ok(())
    }

    /// Insert a segment, or find the one already on its span. The
    /// five-column span is unique per unit, so a repeated plan lands on
    /// the existing row; the flag says whether this call inserted.
    pub fn insert_segment(&self, s: &NewSegment) -> TranslateResult<(i64, bool)> {
        let inserted: Option<i64> = self
            .conn
            .query_row(
                "INSERT OR IGNORE INTO translate_segments (unit_id, start_node_id, \
                 start_char_offset, end_node_id, end_char_offset, source_text_sha, status) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'draft') RETURNING segment_id",
                rusqlite::params![
                    s.unit_id,
                    s.start_node_id,
                    s.start_char_offset,
                    s.end_node_id,
                    s.end_char_offset,
                    s.source_text_sha,
                ],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = inserted {
            return Ok((id, true));
        }
        let existing: i64 = self.conn.query_row(
            "SELECT segment_id FROM translate_segments WHERE unit_id = ?1 AND start_node_id = ?2 \
             AND start_char_offset = ?3 AND end_node_id = ?4 AND end_char_offset = ?5",
            rusqlite::params![
                s.unit_id,
                s.start_node_id,
                s.start_char_offset,
                s.end_node_id,
                s.end_char_offset,
            ],
            |row| row.get(0),
        )?;
        Ok((existing, false))
    }

    /// Record one stage of a proposal on a draft or proposed segment.
    ///
    /// `expected_version` must match the row; every stage advances the
    /// version by one. `Draft` and `Final` need `text`, `Reflection`
    /// needs `notes` and takes `text` as a revised draft when given.
    /// `Final` moves the segment to `proposed`; `source_kind` is left to
    /// the seal. A sealed segment refuses every stage.
    pub fn apply_propose(
        &self,
        segment_id: i64,
        expected_version: i64,
        stage: ProposeStage,
        text: Option<&str>,
        notes: Option<&str>,
    ) -> TranslateResult<SegmentRow> {
        let row = self.segment_or_unknown(segment_id)?;
        if row.status == STATUS_SEALED {
            return Err(TranslateError::WrongStatus {
                segment_id,
                status: row.status,
                wanted: "draft or proposed",
            });
        }
        Translate::expect_version(&row, expected_version)?;
        let missing = |field| TranslateError::MissingField {
            stage: stage.name(),
            field,
        };
        match stage {
            ProposeStage::Draft => {
                let text = text.ok_or_else(|| missing("text"))?;
                self.conn.execute(
                    "UPDATE translate_segments SET draft_text = ?1, version = version + 1 \
                     WHERE segment_id = ?2",
                    rusqlite::params![text, segment_id],
                )?;
            }
            ProposeStage::Reflection => {
                let notes = notes.ok_or_else(|| missing("notes"))?;
                self.conn.execute(
                    "UPDATE translate_segments SET reflection_notes = ?1, \
                     draft_text = COALESCE(?2, draft_text), version = version + 1 \
                     WHERE segment_id = ?3",
                    rusqlite::params![notes, text, segment_id],
                )?;
            }
            ProposeStage::Final => {
                let text = text.ok_or_else(|| missing("text"))?;
                self.conn.execute(
                    "UPDATE translate_segments SET final_text = ?1, status = 'proposed', \
                     version = version + 1 WHERE segment_id = ?2",
                    rusqlite::params![text, segment_id],
                )?;
            }
        }
        self.segment_or_unknown(segment_id)
    }

    /// Lock a proposed segment's final text.
    ///
    /// Requires `status = proposed`, a recorded `final_text`, the
    /// expected version, and a `source_kind` from [`SOURCE_KINDS`].
    /// Stamps `sealed_at` and advances the version.
    pub fn seal_segment(
        &self,
        segment_id: i64,
        expected_version: i64,
        source_kind: &str,
        sealed_at: &str,
    ) -> TranslateResult<SegmentRow> {
        let row = self.segment_or_unknown(segment_id)?;
        if row.status != STATUS_PROPOSED {
            return Err(TranslateError::WrongStatus {
                segment_id,
                status: row.status,
                wanted: "proposed",
            });
        }
        Translate::expect_version(&row, expected_version)?;
        if row.final_text.is_none() {
            return Err(TranslateError::MissingField {
                stage: "seal",
                field: "final_text",
            });
        }
        if !SOURCE_KINDS.contains(&source_kind) {
            return Err(TranslateError::UnknownValue {
                what: "source_kind",
                value: source_kind.to_owned(),
                known: SOURCE_KINDS,
            });
        }
        self.conn.execute(
            "UPDATE translate_segments SET status = 'sealed', source_kind = ?1, sealed_at = ?2, \
             version = version + 1 WHERE segment_id = ?3",
            rusqlite::params![source_kind, sealed_at, segment_id],
        )?;
        self.segment_or_unknown(segment_id)
    }

    /// Fill a draft segment that carries no text with an existing
    /// translation: `final_text` is set, the segment moves to
    /// `proposed` with `source_kind = imported`, and the version
    /// advances.
    pub fn import_fill(&self, segment_id: i64, final_text: &str) -> TranslateResult<SegmentRow> {
        let row = self.segment_or_unknown(segment_id)?;
        if row.status != STATUS_DRAFT {
            return Err(TranslateError::WrongStatus {
                segment_id,
                status: row.status,
                wanted: "draft",
            });
        }
        if row.draft_text.is_some() || row.reflection_notes.is_some() || row.final_text.is_some() {
            return Err(TranslateError::NotEmpty { segment_id });
        }
        self.conn.execute(
            "UPDATE translate_segments SET final_text = ?1, status = 'proposed', \
             source_kind = ?2, version = version + 1 WHERE segment_id = ?3",
            rusqlite::params![final_text, SOURCE_KIND_IMPORTED, segment_id],
        )?;
        self.segment_or_unknown(segment_id)
    }

    /// Whether a segment has never been written to: draft, version 1,
    /// no text in any column. Only such a segment may be re-sliced.
    pub fn is_virgin(row: &SegmentRow) -> bool {
        row.status == STATUS_DRAFT
            && row.version == 1
            && row.draft_text.is_none()
            && row.reflection_notes.is_none()
            && row.final_text.is_none()
    }

    /// Delete one segment row.
    pub fn delete_segment(&self, segment_id: i64) -> TranslateResult<()> {
        let n = self.conn.execute(
            "DELETE FROM translate_segments WHERE segment_id = ?1",
            [segment_id],
        )?;
        if n == 0 {
            return Err(TranslateError::UnknownSegment { segment_id });
        }
        Ok(())
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

    fn new_segment(unit_id: i64, node: i64, start: i64, end: i64) -> NewSegment {
        NewSegment {
            unit_id,
            start_node_id: node,
            start_char_offset: start,
            end_node_id: node,
            end_char_offset: end,
            source_text_sha: span_sha256_hex("text"),
        }
    }

    #[test]
    fn status_and_source_kind_vocabularies_pin_the_check_constraints() {
        let check_of = |column: &str| {
            SPEC.columns
                .iter()
                .find(|c| c.name == column)
                .and_then(|c| c.check)
                .expect("CHECK")
        };
        let status_check = check_of("status");
        for status in STATUSES {
            assert!(
                status_check.contains(&format!("'{status}'")),
                "{status_check}"
            );
        }
        assert_eq!(status_check.matches('\'').count(), STATUSES.len() * 2);
        let kind_check = check_of("source_kind");
        for kind in SOURCE_KINDS {
            assert!(kind_check.contains(&format!("'{kind}'")), "{kind_check}");
        }
        assert_eq!(kind_check.matches('\'').count(), SOURCE_KINDS.len() * 2);
    }

    #[test]
    fn inserting_the_same_span_twice_returns_the_first_row_without_inserting() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);
        let (first, inserted) = t
            .insert_segment(&new_segment(unit_id, 10, 0, 5))
            .expect("insert");
        assert!(inserted);
        let (again, inserted) = t
            .insert_segment(&new_segment(unit_id, 10, 0, 5))
            .expect("insert");
        assert!(!inserted);
        assert_eq!(again, first);
        let (other, inserted) = t
            .insert_segment(&new_segment(unit_id, 10, 5, 9))
            .expect("insert");
        assert!(inserted);
        assert_ne!(other, first);
        let row = t.segment(first).expect("read").expect("row");
        assert_eq!((row.status.as_str(), row.version), ("draft", 1));
    }

    #[test]
    fn a_proposal_walks_the_three_stages_and_advances_the_version_each_time() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);
        let id = seed::segment(&t, unit_id, 10, 0, 5, "draft");

        let row = t
            .apply_propose(id, 1, ProposeStage::Draft, Some("d1"), None)
            .expect("draft");
        assert_eq!(
            (row.draft_text.as_deref(), row.status.as_str(), row.version),
            (Some("d1"), "draft", 2)
        );

        let row = t
            .apply_propose(
                id,
                2,
                ProposeStage::Reflection,
                Some("d2"),
                Some("{\"ok\":true}"),
            )
            .expect("reflection");
        assert_eq!(
            row.draft_text.as_deref(),
            Some("d2"),
            "a revised draft travels with the notes"
        );
        assert_eq!(row.reflection_notes.as_deref(), Some("{\"ok\":true}"));
        assert_eq!((row.status.as_str(), row.version), ("draft", 3));

        let row = t
            .apply_propose(id, 3, ProposeStage::Final, Some("f"), None)
            .expect("final");
        assert_eq!(
            (row.final_text.as_deref(), row.status.as_str(), row.version),
            (Some("f"), "proposed", 4)
        );
        assert_eq!(row.source_kind, None, "source_kind is the seal's to set");
    }

    #[test]
    fn a_stale_version_is_refused_and_leaves_the_row_untouched() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);
        let id = seed::segment(&t, unit_id, 10, 0, 5, "draft");
        t.apply_propose(id, 1, ProposeStage::Draft, Some("d1"), None)
            .expect("draft");
        let err = t
            .apply_propose(id, 1, ProposeStage::Draft, Some("d2"), None)
            .expect_err("stale version");
        assert!(
            matches!(err, TranslateError::VersionConflict { segment_id, expected: 1, current: 2 } if segment_id == id),
            "{err:?}"
        );
        let row = t.segment(id).expect("read").expect("row");
        assert_eq!((row.draft_text.as_deref(), row.version), (Some("d1"), 2));
    }

    #[test]
    fn a_stage_without_its_field_and_a_sealed_segment_are_refused() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);
        let id = seed::segment(&t, unit_id, 10, 0, 5, "draft");
        let err = t
            .apply_propose(id, 1, ProposeStage::Reflection, None, None)
            .expect_err("no notes");
        assert!(
            matches!(
                err,
                TranslateError::MissingField {
                    stage: "reflection",
                    field: "notes"
                }
            ),
            "{err:?}"
        );
        let err = t
            .apply_propose(id, 1, ProposeStage::Final, None, None)
            .expect_err("no text");
        assert!(
            matches!(
                err,
                TranslateError::MissingField {
                    stage: "final",
                    field: "text"
                }
            ),
            "{err:?}"
        );

        let sealed = seed::segment(&t, unit_id, 10, 5, 9, "sealed");
        let err = t
            .apply_propose(sealed, 1, ProposeStage::Draft, Some("x"), None)
            .expect_err("sealed");
        assert!(
            matches!(
                err,
                TranslateError::WrongStatus {
                    wanted: "draft or proposed",
                    ..
                }
            ),
            "{err:?}"
        );

        let err = t
            .apply_propose(404, 1, ProposeStage::Draft, Some("x"), None)
            .expect_err("unknown");
        assert!(
            matches!(err, TranslateError::UnknownSegment { segment_id: 404 }),
            "{err:?}"
        );
    }

    #[test]
    fn a_proposed_imported_segment_takes_a_reflection_in_review_mode() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);
        let id = seed::segment(&t, unit_id, 10, 0, 5, "draft");
        let row = t.import_fill(id, "imported text").expect("import");
        assert_eq!(row.task_mode(), "review");
        let row = t
            .apply_propose(id, row.version, ProposeStage::Reflection, None, Some("[]"))
            .expect("reflection on proposed");
        assert_eq!(
            (row.status.as_str(), row.reflection_notes.as_deref()),
            ("proposed", Some("[]"))
        );
    }

    #[test]
    fn sealing_requires_proposed_status_a_final_text_and_a_known_source_kind() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);
        let id = seed::segment(&t, unit_id, 10, 0, 5, "draft");

        let err = t
            .seal_segment(id, 1, "llm-draft", "2026-01-01T00:00:00Z")
            .expect_err("draft");
        assert!(
            matches!(
                err,
                TranslateError::WrongStatus {
                    wanted: "proposed",
                    ..
                }
            ),
            "{err:?}"
        );

        t.conn
            .execute(
                "UPDATE translate_segments SET status = 'proposed' WHERE segment_id = ?1",
                [id],
            )
            .expect("force proposed without final text");
        let err = t
            .seal_segment(id, 1, "llm-draft", "2026-01-01T00:00:00Z")
            .expect_err("no final text");
        assert!(
            matches!(
                err,
                TranslateError::MissingField {
                    stage: "seal",
                    field: "final_text"
                }
            ),
            "{err:?}"
        );

        set_text(&t, id, "final_text", "f");
        let err = t
            .seal_segment(id, 1, "robot", "2026-01-01T00:00:00Z")
            .expect_err("unknown kind");
        assert!(
            matches!(
                err,
                TranslateError::UnknownValue {
                    what: "source_kind",
                    ..
                }
            ),
            "{err:?}"
        );
        let err = t
            .seal_segment(id, 2, "llm-draft", "2026-01-01T00:00:00Z")
            .expect_err("stale");
        assert!(
            matches!(err, TranslateError::VersionConflict { .. }),
            "{err:?}"
        );

        let row = t
            .seal_segment(id, 1, "llm-reflected", "2026-01-01T00:00:00Z")
            .expect("seal");
        assert_eq!(
            (
                row.status.as_str(),
                row.source_kind.as_deref(),
                row.sealed_at.as_deref(),
                row.version
            ),
            (
                "sealed",
                Some("llm-reflected"),
                Some("2026-01-01T00:00:00Z"),
                2
            )
        );
        let err = t
            .seal_segment(id, 2, "llm-reflected", "2026-01-01T00:00:00Z")
            .expect_err("twice");
        assert!(matches!(err, TranslateError::WrongStatus { .. }), "{err:?}");
    }

    #[test]
    fn an_import_fills_only_an_empty_draft_segment() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);
        let empty = seed::segment(&t, unit_id, 10, 0, 5, "draft");
        let row = t
            .import_fill(empty, "existing translation")
            .expect("import");
        assert_eq!(
            (
                row.final_text.as_deref(),
                row.status.as_str(),
                row.source_kind.as_deref(),
                row.version
            ),
            (
                Some("existing translation"),
                "proposed",
                Some("imported"),
                2
            )
        );
        let err = t.import_fill(empty, "again").expect_err("already proposed");
        assert!(
            matches!(
                err,
                TranslateError::WrongStatus {
                    wanted: "draft",
                    ..
                }
            ),
            "{err:?}"
        );

        let drafted = seed::segment(&t, unit_id, 10, 5, 9, "draft");
        set_text(&t, drafted, "draft_text", "d");
        let err = t.import_fill(drafted, "x").expect_err("has text");
        assert!(
            matches!(err, TranslateError::NotEmpty { segment_id } if segment_id == drafted),
            "{err:?}"
        );
    }

    #[test]
    fn virginity_needs_draft_status_version_one_and_no_text() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);
        let virgin = seed::segment(&t, unit_id, 10, 0, 5, "draft");
        assert!(Translate::is_virgin(
            &t.segment(virgin).expect("read").expect("row")
        ));

        let drafted = seed::segment(&t, unit_id, 10, 5, 9, "draft");
        t.apply_propose(drafted, 1, ProposeStage::Draft, Some("d"), None)
            .expect("draft");
        assert!(!Translate::is_virgin(
            &t.segment(drafted).expect("read").expect("row")
        ));

        let bumped = seed::segment(&t, unit_id, 10, 9, 12, "draft");
        t.conn
            .execute(
                "UPDATE translate_segments SET version = 2 WHERE segment_id = ?1",
                [bumped],
            )
            .expect("bump");
        assert!(!Translate::is_virgin(
            &t.segment(bumped).expect("read").expect("row")
        ));
    }

    #[test]
    fn deleting_a_segment_removes_it_and_refuses_an_unknown_id() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);
        let id = seed::segment(&t, unit_id, 10, 0, 5, "draft");
        t.delete_segment(id).expect("delete");
        assert_eq!(t.segment(id).expect("read"), None);
        let err = t.delete_segment(id).expect_err("gone");
        assert!(matches!(err, TranslateError::UnknownSegment { segment_id } if segment_id == id));
    }
}
