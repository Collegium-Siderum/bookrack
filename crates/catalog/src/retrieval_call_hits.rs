// SPDX-License-Identifier: Apache-2.0

//! The `retrieval_call_hits` table — per-hit detail of a retrieval
//! call.
//!
//! One row per returned passage, keyed `(call_id, ord)` where `ord` is
//! the 0-based rank in the returned hit list. The `passage_id` index
//! answers the cross-call question "how often was this passage hit"
//! with a single aggregate. Rows live and die with their
//! `retrieval_calls` parent.

use bookrack_dbkit::{ColumnSpec, ForeignKey, IndexSpec, OnDelete, TableSpec};
use rusqlite::{Row, named_params};

use crate::{Catalog, Result};

/// The single source of truth for the `retrieval_call_hits` table's
/// schema. Its DDL is rendered from this spec.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "retrieval_call_hits",
    comment: Some(
        "Observability: per-hit detail of a retrieval call, one row per returned passage.",
    ),
    columns: &[
        ColumnSpec::int("call_id")
            .not_null()
            .references(ForeignKey::new(
                "retrieval_calls",
                "call_id",
                OnDelete::Cascade,
            )),
        ColumnSpec::int("ord")
            .not_null()
            .comment("0-based rank in the returned hit list"),
        ColumnSpec::text("passage_id").not_null(),
        ColumnSpec::real("distance").not_null(),
    ],
    composite_pk: Some(&["call_id", "ord"]),
    uniques: &[],
    table_checks: &[],
    indexes: &[IndexSpec::on(
        "idx_retrieval_call_hits_passage",
        &["passage_id"],
    )],
};

/// One `retrieval_call_hits` row — a single returned passage.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalCallHit {
    /// The retrieval call the hit belongs to.
    pub call_id: i64,
    /// 0-based rank in the returned hit list.
    pub ord: i64,
    /// The passage that came back.
    pub passage_id: String,
    /// The vector distance the store reported for the hit.
    pub distance: f64,
}

impl RetrievalCallHit {
    /// Build a [`RetrievalCallHit`] from a row that includes every
    /// column.
    fn from_row(row: &Row<'_>) -> rusqlite::Result<RetrievalCallHit> {
        Ok(RetrievalCallHit {
            call_id: row.get("call_id")?,
            ord: row.get("ord")?,
            passage_id: row.get("passage_id")?,
            distance: row.get("distance")?,
        })
    }
}

impl Catalog {
    /// Every hit of one retrieval call, in rank order.
    pub fn retrieval_hits(&self, call_id: i64) -> Result<Vec<RetrievalCallHit>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM retrieval_call_hits WHERE call_id = :call_id ORDER BY ord",
            SPEC.select_list()
        ))?;
        let rows = stmt
            .query_map(
                named_params! { ":call_id": call_id },
                RetrievalCallHit::from_row,
            )?
            .collect::<rusqlite::Result<Vec<RetrievalCallHit>>>()?;
        Ok(rows)
    }
}
