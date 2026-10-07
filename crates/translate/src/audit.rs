// SPDX-License-Identifier: Apache-2.0

//! The `translate_audit` table — the append-only action log.
//!
//! One row per state-changing action on a segment, term, or
//! translation: the audit trail is a recording of the state machine,
//! not the state machine itself. `actor_kind` reuses the catalog's
//! [`bookrack_catalog::ActorKind`] closed set, pinned by the same
//! `CHECK` constraint every audit table in the workspace carries.
//! `payload_json` snapshots the action's inputs and outputs;
//! `cost_tokens` is a bare numeric column so budget queries can `SUM`
//! it without parsing JSON.

use bookrack_catalog::ActorKind;
use bookrack_dbkit::{ColumnSpec, TableSpec};

use crate::{Translate, TranslateResult};

/// The single source of truth for the `translate_audit` table's schema.
/// The frozen baseline DDL in [`crate::migrate`] is rendered from this
/// spec; `verify_all` pins the two together on every open.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "translate_audit",
    comment: Some("Append-only audit of translation actions; a recording, not the state machine."),
    columns: &[
        ColumnSpec::int("audit_id").primary_key(),
        ColumnSpec::int("segment_id")
            .comment("subject: at most one of the three id columns is set"),
        ColumnSpec::int("term_id"),
        ColumnSpec::int("translation_id"),
        ColumnSpec::text("action").not_null(),
        ColumnSpec::text("actor_kind")
            .not_null()
            .check("actor_kind IN ('human', 'llm', 'import', 'pipeline', 'system')"),
        ColumnSpec::text("actor_detail"),
        ColumnSpec::text("session_id"),
        ColumnSpec::text("reason"),
        ColumnSpec::text("payload_json").comment("snapshot of the action's inputs and outputs"),
        ColumnSpec::int("cost_tokens").comment("bare numeric so budget queries can SUM"),
        ColumnSpec::text("changed_at").not_null(),
    ],
    composite_pk: None,
    uniques: &[],
    table_checks: &[],
    indexes: &[],
};

/// Every action a write surface records, one per write.
pub const ACTIONS: &[&str] = &[
    "plan",
    "propose_draft",
    "propose_reflection",
    "propose_final",
    "seal",
    "import",
    "resegment",
    "term_create",
    "translation_add",
    "set_primary",
];

/// What an audit row is about: at most one of the three id columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditSubject {
    /// A segment write, or a plan / resegment over a unit (recorded
    /// without a subject).
    None,
    Segment(i64),
    Term(i64),
    Translation(i64),
}

/// One audit row to append.
#[derive(Debug, Clone)]
pub struct NewAudit<'a> {
    pub subject: AuditSubject,
    /// One of [`ACTIONS`].
    pub action: &'a str,
    pub actor_kind: ActorKind,
    pub actor_detail: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub reason: Option<&'a str>,
    /// Snapshot of the write's inputs and outputs.
    pub payload: Option<&'a serde_json::Value>,
    pub cost_tokens: Option<i64>,
    /// RFC 3339 UTC, stamped by the caller.
    pub changed_at: &'a str,
}

/// One `translate_audit` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    pub audit_id: i64,
    pub segment_id: Option<i64>,
    pub term_id: Option<i64>,
    pub translation_id: Option<i64>,
    pub action: String,
    pub actor_kind: String,
    pub actor_detail: Option<String>,
    pub session_id: Option<String>,
    pub reason: Option<String>,
    pub payload_json: Option<String>,
    pub cost_tokens: Option<i64>,
    pub changed_at: String,
}

impl Translate {
    /// Append one audit row and return its id.
    pub fn append_audit(&self, row: &NewAudit<'_>) -> TranslateResult<i64> {
        debug_assert!(
            ACTIONS.contains(&row.action),
            "unknown audit action {:?}",
            row.action
        );
        let (segment_id, term_id, translation_id) = match row.subject {
            AuditSubject::None => (None, None, None),
            AuditSubject::Segment(id) => (Some(id), None, None),
            AuditSubject::Term(id) => (None, Some(id), None),
            AuditSubject::Translation(id) => (None, None, Some(id)),
        };
        let payload = row.payload.map(serde_json::Value::to_string);
        Ok(self.conn.query_row(
            "INSERT INTO translate_audit (segment_id, term_id, translation_id, action, \
             actor_kind, actor_detail, session_id, reason, payload_json, cost_tokens, \
             changed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
             RETURNING audit_id",
            rusqlite::params![
                segment_id,
                term_id,
                translation_id,
                row.action,
                row.actor_kind.as_str(),
                row.actor_detail,
                row.session_id,
                row.reason,
                payload,
                row.cost_tokens,
                row.changed_at,
            ],
            |r| r.get(0),
        )?)
    }

    /// Every audit row about `segment_id`, oldest first.
    pub fn audit_for_segment(&self, segment_id: i64) -> TranslateResult<Vec<AuditRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT audit_id, segment_id, term_id, translation_id, action, actor_kind, \
             actor_detail, session_id, reason, payload_json, cost_tokens, changed_at \
             FROM translate_audit WHERE segment_id = ?1 ORDER BY audit_id",
        )?;
        let rows = stmt.query_map([segment_id], |row| {
            Ok(AuditRow {
                audit_id: row.get(0)?,
                segment_id: row.get(1)?,
                term_id: row.get(2)?,
                translation_id: row.get(3)?,
                action: row.get(4)?,
                actor_kind: row.get(5)?,
                actor_detail: row.get(6)?,
                session_id: row.get(7)?,
                reason: row.get(8)?,
                payload_json: row.get(9)?,
                cost_tokens: row.get(10)?,
                changed_at: row.get(11)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Sum of `cost_tokens` over every audit row about a segment of
    /// `(intake_id, target_lang)`; rows without a cost count zero.
    pub fn sum_cost_tokens(&self, intake_id: i64, target_lang: &str) -> TranslateResult<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(a.cost_tokens), 0) FROM translate_audit a \
             JOIN translate_segments s ON s.segment_id = a.segment_id \
             JOIN translate_units u ON u.unit_id = s.unit_id \
             WHERE u.intake_id = ?1 AND u.target_lang = ?2",
            rusqlite::params![intake_id, target_lang],
            |row| row.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seed;

    /// The `actor_kind` CHECK is a string literal; this pins it to the
    /// catalog's closed actor set so the two cannot drift apart.
    #[test]
    fn actor_kind_check_pins_the_catalog_actor_set() {
        let check = SPEC
            .columns
            .iter()
            .find(|c| c.name == "actor_kind")
            .expect("actor_kind column")
            .check
            .expect("actor_kind CHECK");
        for kind in ActorKind::ALL {
            assert!(
                check.contains(&format!("'{}'", kind.as_str())),
                "CHECK must list actor kind {:?}: {check}",
                kind.as_str()
            );
        }
    }

    fn audit(subject: AuditSubject, action: &'static str, cost: Option<i64>) -> NewAudit<'static> {
        NewAudit {
            subject,
            action,
            actor_kind: ActorKind::Llm,
            actor_detail: Some("mcp"),
            session_id: Some("s-1"),
            reason: Some("because"),
            payload: None,
            cost_tokens: cost,
            changed_at: "2026-01-01T00:00:00Z",
        }
    }

    #[test]
    fn an_audit_row_reads_back_every_column_with_the_subject_in_its_own_column() {
        let t = seed::fresh();
        let unit_id = seed::unit(&t, 1, "zh", 10, 0);
        let segment_id = seed::segment(&t, unit_id, 10, 0, 5, "draft");
        let payload = serde_json::json!({ "stage": "draft", "text": "x" });
        let mut row = audit(
            AuditSubject::Segment(segment_id),
            "propose_draft",
            Some(120),
        );
        row.payload = Some(&payload);
        let audit_id = t.append_audit(&row).expect("append");

        let rows = t.audit_for_segment(segment_id).expect("read");
        assert_eq!(
            rows,
            vec![AuditRow {
                audit_id,
                segment_id: Some(segment_id),
                term_id: None,
                translation_id: None,
                action: "propose_draft".into(),
                actor_kind: "llm".into(),
                actor_detail: Some("mcp".into()),
                session_id: Some("s-1".into()),
                reason: Some("because".into()),
                payload_json: Some(payload.to_string()),
                cost_tokens: Some(120),
                changed_at: "2026-01-01T00:00:00Z".into(),
            }]
        );

        let term_row = audit(AuditSubject::Term(3), "term_create", None);
        let term_audit = t.append_audit(&term_row).expect("append");
        let (seg, term): (Option<i64>, Option<i64>) = t
            .conn
            .query_row(
                "SELECT segment_id, term_id FROM translate_audit WHERE audit_id = ?1",
                [term_audit],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("read");
        assert_eq!((seg, term), (None, Some(3)));
    }

    #[test]
    fn cost_tokens_sum_per_book_and_language_and_skip_null_rows() {
        let t = seed::fresh();
        let zh = seed::unit(&t, 1, "zh", 10, 0);
        let fr = seed::unit(&t, 1, "fr", 10, 0);
        let other_book = seed::unit(&t, 2, "zh", 20, 0);
        let a = seed::segment(&t, zh, 10, 0, 5, "draft");
        let b = seed::segment(&t, zh, 10, 5, 9, "draft");
        let c = seed::segment(&t, fr, 10, 0, 5, "draft");
        let d = seed::segment(&t, other_book, 20, 0, 5, "draft");
        for (segment, cost) in [
            (a, Some(100)),
            (a, None),
            (b, Some(25)),
            (c, Some(7)),
            (d, Some(9)),
        ] {
            t.append_audit(&audit(
                AuditSubject::Segment(segment),
                "propose_draft",
                cost,
            ))
            .expect("append");
        }
        assert_eq!(t.sum_cost_tokens(1, "zh").expect("sum"), 125);
        assert_eq!(t.sum_cost_tokens(1, "fr").expect("sum"), 7);
        assert_eq!(t.sum_cost_tokens(3, "zh").expect("sum"), 0);
    }
}
