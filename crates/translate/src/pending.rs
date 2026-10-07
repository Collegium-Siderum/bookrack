// SPDX-License-Identifier: Apache-2.0

//! Pending-work aggregation over units and segments.
//!
//! A dispatcher driving translation of one book into one language
//! asks a single question in a loop: which units still carry
//! segments that are not sealed, and which segments are they? The
//! answer is built here from `translate_units` joined to
//! `translate_segments`; an empty answer is the loop's exit
//! condition. Totals over the whole `(intake, target_lang)` scope ride
//! along so progress can be reported without a second call.

use crate::{Translate, TranslateResult};

/// One unit that still has unsealed segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUnit {
    pub unit_id: i64,
    pub node_id: i64,
    pub unit_order: i64,
    pub source_outline: Option<String>,
    pub injection_profile: String,
    /// Segments of this unit in `draft` status.
    pub draft: i64,
    /// Segments of this unit in `proposed` status.
    pub proposed: i64,
    /// Segments of this unit in `sealed` status.
    pub sealed: i64,
    /// Draft and proposed segment ids in `(start_node_id,
    /// start_char_offset)` order.
    pub pending_segment_ids: Vec<i64>,
}

/// Counts over every unit and segment of one `(intake, target_lang)`
/// scope, pending or not.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PendingTotals {
    /// Units in scope, including those with every segment sealed and
    /// those with no segments yet.
    pub units: i64,
    pub segments: i64,
    pub draft: i64,
    pub proposed: i64,
    pub sealed: i64,
}

impl Translate {
    /// Units of one `(intake_id, target_lang)` that still carry
    /// non-sealed segments, in `unit_order`, with totals over the whole
    /// scope. A unit whose segments are all sealed, or which has no
    /// segments at all, is absent from the list but counted in the
    /// totals.
    pub fn list_pending(
        &self,
        intake_id: i64,
        target_lang: &str,
    ) -> TranslateResult<(Vec<PendingUnit>, PendingTotals)> {
        let mut units = {
            let mut stmt = self.conn.prepare(
                "SELECT u.unit_id, u.node_id, u.unit_order, u.source_outline, \
                        u.injection_profile, \
                        SUM(s.status = 'draft'), SUM(s.status = 'proposed'), \
                        SUM(s.status = 'sealed') \
                 FROM translate_units u \
                 JOIN translate_segments s ON s.unit_id = u.unit_id \
                 WHERE u.intake_id = ?1 AND u.target_lang = ?2 \
                 GROUP BY u.unit_id \
                 HAVING SUM(s.status != 'sealed') > 0 \
                 ORDER BY u.unit_order, u.unit_id",
            )?;
            let rows = stmt.query_map(rusqlite::params![intake_id, target_lang], |row| {
                Ok(PendingUnit {
                    unit_id: row.get(0)?,
                    node_id: row.get(1)?,
                    unit_order: row.get(2)?,
                    source_outline: row.get(3)?,
                    injection_profile: row.get(4)?,
                    draft: row.get(5)?,
                    proposed: row.get(6)?,
                    sealed: row.get(7)?,
                    pending_segment_ids: Vec::new(),
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut ids = self.conn.prepare(
            "SELECT segment_id FROM translate_segments \
             WHERE unit_id = ?1 AND status != 'sealed' \
             ORDER BY start_node_id, start_char_offset",
        )?;
        for unit in &mut units {
            unit.pending_segment_ids = ids
                .query_map([unit.unit_id], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?;
        }

        let totals = self.conn.query_row(
            "SELECT COUNT(DISTINCT u.unit_id), COUNT(s.segment_id), \
                    COALESCE(SUM(s.status = 'draft'), 0), \
                    COALESCE(SUM(s.status = 'proposed'), 0), \
                    COALESCE(SUM(s.status = 'sealed'), 0) \
             FROM translate_units u \
             LEFT JOIN translate_segments s ON s.unit_id = u.unit_id \
             WHERE u.intake_id = ?1 AND u.target_lang = ?2",
            rusqlite::params![intake_id, target_lang],
            |row| {
                Ok(PendingTotals {
                    units: row.get(0)?,
                    segments: row.get(1)?,
                    draft: row.get(2)?,
                    proposed: row.get(3)?,
                    sealed: row.get(4)?,
                })
            },
        )?;

        Ok((units, totals))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seed;

    #[test]
    fn only_units_with_unsealed_segments_are_listed_and_totals_cover_the_scope() {
        let t = seed::fresh();
        let done = seed::unit(&t, 1, "zh", 10, 0);
        seed::segment(&t, done, 10, 0, 5, "sealed");
        seed::segment(&t, done, 10, 5, 9, "sealed");

        let mixed = seed::unit(&t, 1, "zh", 11, 1);
        let mixed_proposed = seed::segment(&t, mixed, 11, 5, 9, "proposed");
        seed::segment(&t, mixed, 11, 0, 5, "sealed");
        let mixed_draft = seed::segment(&t, mixed, 11, 9, 12, "draft");

        let fresh = seed::unit(&t, 1, "zh", 12, 2);
        let fresh_b = seed::segment(&t, fresh, 12, 5, 9, "draft");
        let fresh_a = seed::segment(&t, fresh, 12, 0, 5, "draft");

        // Planned but not yet segmented: not pending, still in scope.
        seed::unit(&t, 1, "zh", 13, 3);

        let (units, totals) = t.list_pending(1, "zh").expect("list");
        assert_eq!(
            units,
            vec![
                PendingUnit {
                    unit_id: mixed,
                    node_id: 11,
                    unit_order: 1,
                    source_outline: None,
                    injection_profile: "default".into(),
                    draft: 1,
                    proposed: 1,
                    sealed: 1,
                    pending_segment_ids: vec![mixed_proposed, mixed_draft],
                },
                PendingUnit {
                    unit_id: fresh,
                    node_id: 12,
                    unit_order: 2,
                    source_outline: None,
                    injection_profile: "default".into(),
                    draft: 2,
                    proposed: 0,
                    sealed: 0,
                    pending_segment_ids: vec![fresh_a, fresh_b],
                },
            ]
        );
        assert_eq!(
            totals,
            PendingTotals {
                units: 4,
                segments: 7,
                draft: 3,
                proposed: 1,
                sealed: 3,
            }
        );
    }

    #[test]
    fn units_are_listed_in_unit_order_not_insertion_order() {
        let t = seed::fresh();
        let second = seed::unit(&t, 1, "zh", 11, 1);
        seed::segment(&t, second, 11, 0, 5, "draft");
        let first = seed::unit(&t, 1, "zh", 10, 0);
        seed::segment(&t, first, 10, 0, 5, "draft");

        let (units, _) = t.list_pending(1, "zh").expect("list");
        let ids: Vec<i64> = units.iter().map(|u| u.unit_id).collect();
        assert_eq!(ids, vec![first, second]);
    }

    #[test]
    fn scope_is_one_intake_and_one_target_language() {
        let t = seed::fresh();
        let wanted = seed::unit(&t, 1, "zh", 10, 0);
        seed::segment(&t, wanted, 10, 0, 5, "draft");
        let other_lang = seed::unit(&t, 1, "de", 10, 0);
        seed::segment(&t, other_lang, 10, 0, 5, "draft");
        let other_book = seed::unit(&t, 2, "zh", 10, 0);
        seed::segment(&t, other_book, 10, 0, 5, "draft");

        let (units, totals) = t.list_pending(1, "zh").expect("list");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].unit_id, wanted);
        assert_eq!(totals.units, 1);
        assert_eq!(totals.segments, 1);
    }

    #[test]
    fn an_unknown_scope_reads_as_empty_with_zero_totals() {
        let t = seed::fresh();
        let (units, totals) = t.list_pending(9, "zh").expect("list");
        assert!(units.is_empty());
        assert_eq!(totals, PendingTotals::default());
    }
}
