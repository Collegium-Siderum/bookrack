// SPDX-License-Identifier: Apache-2.0

//! The `node_categories` table — category tags, many-to-many.
//!
//! One row per (node, category). A node may carry several categories,
//! at most a few of them flagged primary; `source` records whether the
//! tag was user-set, suggested by an LLM, or inferred.

use bookrack_dbkit::{ColumnSpec, IndexSpec, TableSpec};
use rusqlite::{Row, named_params};

use crate::{Catalog, Result};

/// The single source of truth for the `node_categories` table's schema.
/// Its DDL is rendered from this spec.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "node_categories",
    comment: Some("Category tags, many-to-many."),
    columns: &[
        ColumnSpec::int("node_id").not_null(),
        ColumnSpec::text("category").not_null(),
        ColumnSpec::int("is_primary").not_null().default("0"),
        ColumnSpec::text("source")
            .not_null()
            .comment("user / llm_suggested / inferred"),
        ColumnSpec::int("confirmed").not_null().default("0"),
        ColumnSpec::text("curated_at").not_null(),
        ColumnSpec::text("curated_by").not_null(),
    ],
    composite_pk: Some(&["node_id", "category"]),
    uniques: &[],
    table_checks: &[],
    indexes: &[IndexSpec::on("idx_cat_cat", &["category"])],
};

/// Insert or replace one (node, category) tag. `curated_at` is generated
/// by SQLite so the whole crate shares one timestamp source.
const UPSERT_SQL: &str = "INSERT INTO node_categories \
     (node_id, category, is_primary, source, confirmed, curated_at, curated_by) \
     VALUES (:node_id, :category, :is_primary, :source, :confirmed, \
             strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), :curated_by) \
     ON CONFLICT(node_id, category) DO UPDATE SET \
       is_primary = excluded.is_primary, \
       source = excluded.source, \
       confirmed = excluded.confirmed, \
       curated_at = excluded.curated_at, \
       curated_by = excluded.curated_by";

/// A `SELECT` of every column with `tail` appended; column list from
/// [`SPEC`].
fn select_sql(tail: &str) -> String {
    format!("SELECT {} FROM node_categories {tail}", SPEC.select_list())
}

/// One `node_categories` row — one category tag on one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeCategory {
    /// The tagged node.
    pub node_id: i64,
    /// The category.
    pub category: String,
    /// Whether this is a primary category of the node.
    pub is_primary: bool,
    /// Where the tag came from (`user` / `llm_suggested` / `inferred`).
    pub source: String,
    /// Whether the user has confirmed the tag.
    pub confirmed: bool,
    /// When the tag was last curated, ISO-8601 UTC.
    pub curated_at: String,
    /// Who curated the tag.
    pub curated_by: String,
}

impl NodeCategory {
    /// Build a [`NodeCategory`] from a row that includes every column.
    fn from_row(row: &Row<'_>) -> rusqlite::Result<NodeCategory> {
        Ok(NodeCategory {
            node_id: row.get("node_id")?,
            category: row.get("category")?,
            is_primary: row.get("is_primary")?,
            source: row.get("source")?,
            confirmed: row.get("confirmed")?,
            curated_at: row.get("curated_at")?,
            curated_by: row.get("curated_by")?,
        })
    }
}

/// A category tag about to be written.
#[derive(Debug, Clone)]
pub struct NewCategory {
    node_id: i64,
    category: String,
    is_primary: bool,
    source: String,
    confirmed: bool,
    curated_by: String,
}

impl NewCategory {
    /// A tag of `category` on `node_id`, from `source`. Secondary and
    /// unconfirmed until the builder methods say otherwise.
    pub fn new(
        node_id: i64,
        category: impl Into<String>,
        source: impl Into<String>,
        curated_by: impl Into<String>,
    ) -> NewCategory {
        NewCategory {
            node_id,
            category: category.into(),
            is_primary: false,
            source: source.into(),
            confirmed: false,
            curated_by: curated_by.into(),
        }
    }

    /// Set whether this is a primary category of the node.
    pub fn primary(mut self, is_primary: bool) -> NewCategory {
        self.is_primary = is_primary;
        self
    }

    /// Mark the tag confirmed (or not).
    pub fn confirmed(mut self, confirmed: bool) -> NewCategory {
        self.confirmed = confirmed;
        self
    }
}

impl Catalog {
    /// Insert or replace one (node, category) tag.
    pub fn add_category(&self, new: &NewCategory) -> Result<()> {
        self.conn.execute(
            UPSERT_SQL,
            named_params! {
                ":node_id": new.node_id,
                ":category": new.category,
                ":is_primary": new.is_primary,
                ":source": new.source,
                ":confirmed": new.confirmed,
                ":curated_by": new.curated_by,
            },
        )?;
        Ok(())
    }

    /// Every category on `node_id`, ordered by category name.
    pub fn categories_for_node(&self, node_id: i64) -> Result<Vec<NodeCategory>> {
        let mut stmt = self
            .conn
            .prepare(&select_sql("WHERE node_id = :node_id ORDER BY category"))?;
        let rows = stmt
            .query_map(
                named_params! { ":node_id": node_id },
                NodeCategory::from_row,
            )?
            .collect::<rusqlite::Result<Vec<NodeCategory>>>()?;
        Ok(rows)
    }

    /// Remove a category tag. Returns whether a row existed.
    pub fn remove_category(&self, node_id: i64, category: &str) -> Result<bool> {
        let affected = self.conn.execute(
            "DELETE FROM node_categories WHERE node_id = :node_id AND category = :category",
            named_params! { ":node_id": node_id, ":category": category },
        )?;
        Ok(affected > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_category_round_trips_every_field() {
        let catalog = Catalog::open_in_memory().expect("open");
        catalog
            .add_category(
                &NewCategory::new(100_000_001, "philosophy", "user", "human")
                    .primary(true)
                    .confirmed(true),
            )
            .expect("write");

        let all = catalog.categories_for_node(100_000_001).expect("read");
        assert_eq!(all.len(), 1);
        let row = &all[0];
        assert_eq!(row.node_id, 100_000_001);
        assert_eq!(row.category, "philosophy");
        assert!(row.is_primary);
        assert_eq!(row.source, "user");
        assert!(row.confirmed);
        assert!(!row.curated_at.is_empty());
        assert_eq!(row.curated_by, "human");
    }

    #[test]
    fn categories_can_be_added_listed_and_removed() {
        let catalog = Catalog::open_in_memory().expect("open");
        catalog
            .add_category(&NewCategory::new(
                100_000_001,
                "history",
                "inferred",
                "pipeline",
            ))
            .expect("add");
        catalog
            .add_category(&NewCategory::new(
                100_000_001,
                "biography",
                "llm_suggested",
                "llm",
            ))
            .expect("add");
        let names: Vec<String> = catalog
            .categories_for_node(100_000_001)
            .expect("read")
            .into_iter()
            .map(|c| c.category)
            .collect();
        assert_eq!(names, ["biography", "history"]);

        assert!(
            catalog
                .remove_category(100_000_001, "history")
                .expect("remove")
        );
        assert_eq!(
            catalog
                .categories_for_node(100_000_001)
                .expect("read")
                .len(),
            1
        );
        assert!(
            !catalog
                .remove_category(100_000_001, "history")
                .expect("miss")
        );
    }
}
