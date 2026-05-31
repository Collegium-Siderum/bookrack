// SPDX-License-Identifier: Apache-2.0

//! The `node_contributors` table — contributor roles, many-to-many.
//!
//! One row per (node, role, ordinal, origin): the authors, translators,
//! editors, and so on attributed to a node. `origin` separates what was
//! extracted from what the user supplied. The autoincrement
//! `contributor_id` is a surrogate key so a later per-contributor edit
//! can address a single row; the natural key stays `UNIQUE`.

use bookrack_dbkit::{ColumnSpec, IndexSpec, TableSpec};
use rusqlite::{Row, named_params};

use crate::{Catalog, Result};

/// The single source of truth for the `node_contributors` table's
/// schema. Its DDL is rendered from this spec.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "node_contributors",
    comment: Some("Contributor roles (author / translator / editor / ...), many-to-many."),
    columns: &[
        ColumnSpec::int("contributor_id").pk_autoinc(),
        ColumnSpec::int("node_id").not_null(),
        ColumnSpec::text("role").not_null(),
        ColumnSpec::int("ordinal").not_null(),
        ColumnSpec::text("origin")
            .not_null()
            .comment("extracted / user"),
        ColumnSpec::text("name").not_null(),
        ColumnSpec::text("nationality"),
        ColumnSpec::int("inheritable").not_null().default("1"),
    ],
    composite_pk: None,
    uniques: &[&["node_id", "role", "ordinal", "origin"]],
    table_checks: &[],
    indexes: &[
        IndexSpec::on("idx_contrib_role_name", &["role", "name"]),
        // Covering index for the per-node read path: resolve a node's
        // contributors ordered by role then ordinal. Added as the first
        // real schema migration (migrate::CONTRIBUTOR_INDEX_DDL).
        IndexSpec::on("idx_contrib_node", &["node_id", "role", "ordinal"]),
    ],
};

/// Insert one contributor and return its surrogate id.
const INSERT_SQL: &str = "INSERT INTO node_contributors \
     (node_id, role, ordinal, origin, name, nationality, inheritable) \
     VALUES (:node_id, :role, :ordinal, :origin, :name, :nationality, :inheritable) \
     RETURNING contributor_id";

/// A `SELECT` of every column with `tail` appended; column list from
/// [`SPEC`].
fn select_sql(tail: &str) -> String {
    format!(
        "SELECT {} FROM node_contributors {tail}",
        SPEC.select_list()
    )
}

/// One `node_contributors` row — one contributor in one role on a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeContributor {
    /// Surrogate key, assigned by the database.
    pub contributor_id: i64,
    /// The node this contributor is attributed to.
    pub node_id: i64,
    /// The contribution role (`author`, `translator`, `editor`, ...).
    pub role: String,
    /// Position among the contributors sharing this node and role.
    pub ordinal: i64,
    /// Where this attribution came from (`extracted` / `user`).
    pub origin: String,
    /// The contributor's name.
    pub name: String,
    /// The contributor's nationality, when known.
    pub nationality: Option<String>,
    /// Whether this attribution is inherited by child nodes.
    pub inheritable: bool,
}

impl NodeContributor {
    /// Build a [`NodeContributor`] from a row that includes every column.
    fn from_row(row: &Row<'_>) -> rusqlite::Result<NodeContributor> {
        Ok(NodeContributor {
            contributor_id: row.get("contributor_id")?,
            node_id: row.get("node_id")?,
            role: row.get("role")?,
            ordinal: row.get("ordinal")?,
            origin: row.get("origin")?,
            name: row.get("name")?,
            nationality: row.get("nationality")?,
            inheritable: row.get("inheritable")?,
        })
    }
}

/// A contributor about to be written. The surrogate `contributor_id` is
/// assigned by the database and returned from [`Catalog::add_contributor`].
#[derive(Debug, Clone)]
pub struct NewContributor {
    node_id: i64,
    role: String,
    ordinal: i64,
    origin: String,
    name: String,
    nationality: Option<String>,
    inheritable: bool,
}

impl NewContributor {
    /// A contributor in `role` at `ordinal` on `node_id`, from `origin`.
    /// Inheritable and nationality-free until the builder says otherwise.
    pub fn new(
        node_id: i64,
        role: impl Into<String>,
        ordinal: i64,
        origin: impl Into<String>,
        name: impl Into<String>,
    ) -> NewContributor {
        NewContributor {
            node_id,
            role: role.into(),
            ordinal,
            origin: origin.into(),
            name: name.into(),
            nationality: None,
            inheritable: true,
        }
    }

    /// Record the contributor's nationality.
    pub fn nationality(mut self, nationality: impl Into<String>) -> NewContributor {
        self.nationality = Some(nationality.into());
        self
    }

    /// Set whether the attribution is inherited by child nodes.
    pub fn inheritable(mut self, inheritable: bool) -> NewContributor {
        self.inheritable = inheritable;
        self
    }
}

impl Catalog {
    /// Insert one contributor, returning its assigned `contributor_id`.
    ///
    /// Fails with a database error if it duplicates an existing
    /// (node, role, ordinal, origin) — that natural key is `UNIQUE`.
    pub fn add_contributor(&self, new: &NewContributor) -> Result<i64> {
        let id = self.conn.query_row(
            INSERT_SQL,
            named_params! {
                ":node_id": new.node_id,
                ":role": new.role,
                ":ordinal": new.ordinal,
                ":origin": new.origin,
                ":name": new.name,
                ":nationality": new.nationality,
                ":inheritable": new.inheritable,
            },
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Every contributor on `node_id`, ordered by role then ordinal.
    pub fn contributors_for_node(&self, node_id: i64) -> Result<Vec<NodeContributor>> {
        let mut stmt = self.conn.prepare(&select_sql(
            "WHERE node_id = :node_id ORDER BY role, ordinal",
        ))?;
        let rows = stmt
            .query_map(
                named_params! { ":node_id": node_id },
                NodeContributor::from_row,
            )?
            .collect::<rusqlite::Result<Vec<NodeContributor>>>()?;
        Ok(rows)
    }

    /// Remove one contributor by its surrogate id. Returns whether a row
    /// existed.
    pub fn remove_contributor(&self, contributor_id: i64) -> Result<bool> {
        let affected = self.conn.execute(
            "DELETE FROM node_contributors WHERE contributor_id = :id",
            named_params! { ":id": contributor_id },
        )?;
        Ok(affected > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_contributor_round_trips_every_field() {
        let catalog = Catalog::open_in_memory().expect("open");
        let id = catalog
            .add_contributor(
                &NewContributor::new(100_000_001, "translator", 0, "user", "A Translator")
                    .nationality("fr")
                    .inheritable(false),
            )
            .expect("add");
        assert!(id > 0);

        let all = catalog.contributors_for_node(100_000_001).expect("read");
        assert_eq!(all.len(), 1);
        let row = &all[0];
        assert_eq!(row.contributor_id, id);
        assert_eq!(row.node_id, 100_000_001);
        assert_eq!(row.role, "translator");
        assert_eq!(row.ordinal, 0);
        assert_eq!(row.origin, "user");
        assert_eq!(row.name, "A Translator");
        assert_eq!(row.nationality.as_deref(), Some("fr"));
        assert!(!row.inheritable);
    }

    #[test]
    fn contributors_come_back_ordered_by_role_then_ordinal() {
        let catalog = Catalog::open_in_memory().expect("open");
        // Insert out of order to prove the query sorts.
        catalog
            .add_contributor(&NewContributor::new(
                100_000_001,
                "author",
                1,
                "extracted",
                "Second",
            ))
            .expect("add");
        catalog
            .add_contributor(&NewContributor::new(
                100_000_001,
                "author",
                0,
                "extracted",
                "First",
            ))
            .expect("add");
        let names: Vec<String> = catalog
            .contributors_for_node(100_000_001)
            .expect("read")
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["First", "Second"]);
    }

    #[test]
    fn a_contributor_can_be_removed() {
        let catalog = Catalog::open_in_memory().expect("open");
        let id = catalog
            .add_contributor(&NewContributor::new(
                100_000_001,
                "editor",
                0,
                "user",
                "An Editor",
            ))
            .expect("add");
        assert!(catalog.remove_contributor(id).expect("remove"));
        assert!(
            catalog
                .contributors_for_node(100_000_001)
                .expect("read")
                .is_empty()
        );
        assert!(!catalog.remove_contributor(id).expect("miss"));
    }
}
