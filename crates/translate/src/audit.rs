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

use bookrack_dbkit::{ColumnSpec, TableSpec};

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

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_catalog::ActorKind;

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
}
