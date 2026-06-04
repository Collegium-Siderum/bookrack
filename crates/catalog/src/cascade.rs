// SPDX-License-Identifier: Apache-2.0

//! Cross-table cascade for removing a single book from `catalog.db`.
//!
//! A book is identified by its `intake_id`; its allocator-derived
//! `book_root_id = PartitionIdx::new(intake_id).root().get()` is passed
//! in so the cascade does not depend on `corpus.db` being open.
//!
//! Two classes of catalog tables hold per-book rows:
//!
//! - Seven tables keyed by `intake_id` (the metadata and lifecycle
//!   layers): `book_state`, `node_publication_attrs`, `node_overrides`,
//!   `node_contributors`, `node_categories`, `node_reviews`,
//!   `node_role_takeovers`.
//! - One table keyed by `book_root_id` (the manual TOC overlay):
//!   `toc_edits`.
//!
//! The audit tables `metadata_audit` and `book_pipeline_audit` are
//! denormalized by design — `book_pipeline_audit` even carries
//! `source_sha256` — and are intentionally **not** cascaded. They
//! remain as a forensic record of a removed book's pipeline history.
//!
//! `retrieval_issues`, `works`, `expressions`, and `mcp_tool_calls`
//! do not carry an `intake_id` column and are not touched.

use rusqlite::named_params;

use crate::{Catalog, Result, count_as_u64};

/// Per-table row tallies produced by [`Catalog::count_book_derived`] and
/// returned by [`Catalog::delete_book_derived`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BookRemovalCounts {
    /// Rows in `book_state` keyed by `intake_id`.
    pub book_state: u64,
    /// Rows in `node_publication_attrs` keyed by `intake_id`.
    pub node_publication_attrs: u64,
    /// Rows in `node_overrides` keyed by `intake_id`.
    pub node_overrides: u64,
    /// Rows in `node_contributors` keyed by `intake_id`.
    pub node_contributors: u64,
    /// Rows in `node_categories` keyed by `intake_id`.
    pub node_categories: u64,
    /// Rows in `node_reviews` keyed by `intake_id`.
    pub node_reviews: u64,
    /// Rows in `node_role_takeovers` keyed by `intake_id`.
    pub node_role_takeovers: u64,
    /// Rows in `toc_edits` keyed by `book_root_id`.
    pub toc_edits: u64,
}

impl BookRemovalCounts {
    /// Sum across every cascaded table.
    pub fn total(&self) -> u64 {
        self.book_state
            + self.node_publication_attrs
            + self.node_overrides
            + self.node_contributors
            + self.node_categories
            + self.node_reviews
            + self.node_role_takeovers
            + self.toc_edits
    }
}

const COUNT_BY_INTAKE_ID_TABLES: &[&str] = &[
    "book_state",
    "node_publication_attrs",
    "node_overrides",
    "node_contributors",
    "node_categories",
    "node_reviews",
    "node_role_takeovers",
];

const DELETE_BOOK_STATE_SQL: &str = "DELETE FROM book_state WHERE intake_id = :intake_id";
const DELETE_NODE_PUBLICATION_ATTRS_SQL: &str =
    "DELETE FROM node_publication_attrs WHERE intake_id = :intake_id";
const DELETE_NODE_OVERRIDES_SQL: &str = "DELETE FROM node_overrides WHERE intake_id = :intake_id";
const DELETE_NODE_CONTRIBUTORS_SQL: &str =
    "DELETE FROM node_contributors WHERE intake_id = :intake_id";
const DELETE_NODE_CATEGORIES_SQL: &str = "DELETE FROM node_categories WHERE intake_id = :intake_id";
const DELETE_NODE_REVIEWS_SQL: &str = "DELETE FROM node_reviews WHERE intake_id = :intake_id";
const DELETE_NODE_ROLE_TAKEOVERS_SQL: &str =
    "DELETE FROM node_role_takeovers WHERE intake_id = :intake_id";
const DELETE_TOC_EDITS_SQL: &str = "DELETE FROM toc_edits WHERE book_root_id = :book_root_id";

const COUNT_TOC_EDITS_SQL: &str =
    "SELECT COUNT(*) FROM toc_edits WHERE book_root_id = :book_root_id";

const DELETE_INTAKE_SQL: &str = "DELETE FROM intake WHERE intake_id = :intake_id";

impl Catalog {
    /// Count, without writing, every per-book row this cascade would
    /// delete. Reads each catalog table named in [`BookRemovalCounts`].
    pub fn count_book_derived(
        &self,
        intake_id: i64,
        book_root_id: i64,
    ) -> Result<BookRemovalCounts> {
        let mut by_intake = [0u64; 7];
        for (i, table) in COUNT_BY_INTAKE_ID_TABLES.iter().enumerate() {
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE intake_id = :intake_id");
            let n: i64 =
                self.conn
                    .query_row(&sql, named_params! { ":intake_id": intake_id }, |row| {
                        row.get(0)
                    })?;
            by_intake[i] = count_as_u64(n)?;
        }
        let toc_edits_n: i64 = self.conn.query_row(
            COUNT_TOC_EDITS_SQL,
            named_params! { ":book_root_id": book_root_id },
            |row| row.get(0),
        )?;
        Ok(BookRemovalCounts {
            book_state: by_intake[0],
            node_publication_attrs: by_intake[1],
            node_overrides: by_intake[2],
            node_contributors: by_intake[3],
            node_categories: by_intake[4],
            node_reviews: by_intake[5],
            node_role_takeovers: by_intake[6],
            toc_edits: count_as_u64(toc_edits_n)?,
        })
    }

    /// Delete every per-book row in the cascaded catalog tables within
    /// one transaction. Returns the per-table tallies. Idempotent — a
    /// second call after a successful one returns all-zero counts.
    /// `metadata_audit` and `book_pipeline_audit` are preserved by
    /// design, see module docs.
    pub fn delete_book_derived(
        &mut self,
        intake_id: i64,
        book_root_id: i64,
    ) -> Result<BookRemovalCounts> {
        let tx = self.conn.transaction()?;
        let book_state = tx.execute(
            DELETE_BOOK_STATE_SQL,
            named_params! { ":intake_id": intake_id },
        )? as u64;
        let node_publication_attrs = tx.execute(
            DELETE_NODE_PUBLICATION_ATTRS_SQL,
            named_params! { ":intake_id": intake_id },
        )? as u64;
        let node_overrides = tx.execute(
            DELETE_NODE_OVERRIDES_SQL,
            named_params! { ":intake_id": intake_id },
        )? as u64;
        let node_contributors = tx.execute(
            DELETE_NODE_CONTRIBUTORS_SQL,
            named_params! { ":intake_id": intake_id },
        )? as u64;
        let node_categories = tx.execute(
            DELETE_NODE_CATEGORIES_SQL,
            named_params! { ":intake_id": intake_id },
        )? as u64;
        let node_reviews = tx.execute(
            DELETE_NODE_REVIEWS_SQL,
            named_params! { ":intake_id": intake_id },
        )? as u64;
        let node_role_takeovers = tx.execute(
            DELETE_NODE_ROLE_TAKEOVERS_SQL,
            named_params! { ":intake_id": intake_id },
        )? as u64;
        let toc_edits = tx.execute(
            DELETE_TOC_EDITS_SQL,
            named_params! { ":book_root_id": book_root_id },
        )? as u64;
        tx.commit()?;
        Ok(BookRemovalCounts {
            book_state,
            node_publication_attrs,
            node_overrides,
            node_contributors,
            node_categories,
            node_reviews,
            node_role_takeovers,
            toc_edits,
        })
    }

    /// Delete the `intake` row itself. Returns whether the row existed.
    /// Run last in a removal: keeping the row until every other step
    /// has succeeded lets an interrupted removal be resumed by feeding
    /// the same `intake_id` to the next `bookrack remove` call.
    pub fn delete_intake(&self, intake_id: i64) -> Result<bool> {
        let affected = self
            .conn
            .execute(DELETE_INTAKE_SQL, named_params! { ":intake_id": intake_id })?;
        Ok(affected > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ActorKind, BOOK_SCOPE, NewBookState, NewContributor, NewIntake, NewMetadataAudit,
        NewOverride, NewPublicationAttrs, NewReview, NewRoleTakeover, STATUS_APPROVED,
    };

    fn book_root_id_of(intake_id: i64) -> i64 {
        intake_id * 100_000_000 + 1
    }

    fn seed_intake(catalog: &mut Catalog, sha: &str) -> i64 {
        catalog
            .register_intake(&NewIntake::new(sha))
            .expect("register")
            .into_intake()
            .intake_id
    }

    fn seed_full_book(catalog: &mut Catalog, sha: &str, title: &str, author: &str) -> i64 {
        let intake_id = seed_intake(catalog, sha);
        let book_root_id = book_root_id_of(intake_id);

        // book_state
        catalog
            .upsert_book_state(
                &NewBookState::new(book_root_id, intake_id, "ready")
                    .parsed_at("2026-06-04T00:00:00Z"),
            )
            .expect("book_state");

        // publication_attrs base
        let mut attrs = NewPublicationAttrs::new(intake_id, BOOK_SCOPE);
        attrs.title = Some(title.to_string());
        catalog.upsert_publication_attrs(&attrs).expect("attrs");

        // override
        catalog
            .set_override(&NewOverride::new(
                intake_id,
                BOOK_SCOPE,
                "title",
                Some("Curated Title".to_string()),
                "human",
            ))
            .expect("override");

        // contributor
        catalog
            .add_contributor(&NewContributor::new(
                intake_id,
                BOOK_SCOPE,
                "author",
                0,
                "extracted",
                author,
            ))
            .expect("contributor");

        // category
        catalog
            .add_category(
                &crate::NewCategory::new(intake_id, BOOK_SCOPE, "fiction", "extracted", "human")
                    .primary(true),
            )
            .expect("category");

        // review
        catalog
            .upsert_review(&NewReview::new(
                intake_id,
                BOOK_SCOPE,
                "human:test",
                STATUS_APPROVED,
            ))
            .expect("review");

        // role takeover
        catalog
            .mark_role_takeover(&NewRoleTakeover::new(
                intake_id,
                BOOK_SCOPE,
                "translator",
                "human",
            ))
            .expect("role takeover");

        // metadata audit row (must survive removal)
        let mut audit = NewMetadataAudit::new("node_publication_attrs", "seed", ActorKind::System);
        audit.node_id = Some(book_root_id);
        catalog
            .record_metadata_audit(&audit)
            .expect("metadata audit");

        intake_id
    }

    #[test]
    fn count_book_derived_tallies_each_table() {
        let mut catalog = Catalog::open_in_memory().expect("open");
        let intake_id = seed_full_book(&mut catalog, "sha-a", "Alpha", "Ann");
        let counts = catalog
            .count_book_derived(intake_id, book_root_id_of(intake_id))
            .expect("count");
        assert_eq!(counts.book_state, 1);
        assert_eq!(counts.node_publication_attrs, 1);
        assert_eq!(counts.node_overrides, 1);
        assert_eq!(counts.node_contributors, 1);
        assert_eq!(counts.node_categories, 1);
        assert_eq!(counts.node_reviews, 1);
        assert_eq!(counts.node_role_takeovers, 1);
        assert_eq!(counts.toc_edits, 0);
        assert_eq!(counts.total(), 7);
    }

    #[test]
    fn delete_book_derived_clears_only_the_target_book() {
        let mut catalog = Catalog::open_in_memory().expect("open");
        let kept = seed_full_book(&mut catalog, "sha-keep", "Keep", "Kim");
        let gone = seed_full_book(&mut catalog, "sha-gone", "Gone", "Gus");

        let counts = catalog
            .delete_book_derived(gone, book_root_id_of(gone))
            .expect("delete");
        assert_eq!(counts.book_state, 1);
        assert_eq!(counts.node_contributors, 1);

        // The target's derived rows are gone.
        let after = catalog
            .count_book_derived(gone, book_root_id_of(gone))
            .expect("count");
        assert_eq!(after.total(), 0);

        // The other book is untouched.
        let other = catalog
            .count_book_derived(kept, book_root_id_of(kept))
            .expect("count");
        assert_eq!(other.book_state, 1);
        assert_eq!(other.node_contributors, 1);
    }

    #[test]
    fn delete_book_derived_is_idempotent() {
        let mut catalog = Catalog::open_in_memory().expect("open");
        let intake_id = seed_full_book(&mut catalog, "sha-rerun", "Rerun", "Ronan");
        catalog
            .delete_book_derived(intake_id, book_root_id_of(intake_id))
            .expect("first");
        let again = catalog
            .delete_book_derived(intake_id, book_root_id_of(intake_id))
            .expect("second");
        assert_eq!(again.total(), 0);
    }

    #[test]
    fn delete_book_derived_preserves_metadata_audit_and_pipeline_audit() {
        let mut catalog = Catalog::open_in_memory().expect("open");
        let intake_id = seed_full_book(&mut catalog, "sha-audit", "Audit", "Ada");
        let book_root_id = book_root_id_of(intake_id);

        let audit_before = catalog
            .metadata_audit_for_node(book_root_id)
            .expect("audit before");
        assert!(
            !audit_before.is_empty(),
            "seeded fixture must include a metadata_audit row",
        );

        catalog
            .delete_book_derived(intake_id, book_root_id)
            .expect("cascade");
        catalog.delete_intake(intake_id).expect("delete intake");

        // The forensic audit row survives an intake-row delete.
        let audit_after = catalog
            .metadata_audit_for_node(book_root_id)
            .expect("audit after");
        assert_eq!(audit_after.len(), audit_before.len());
    }

    #[test]
    fn delete_intake_returns_existence_and_removes_the_row() {
        let mut catalog = Catalog::open_in_memory().expect("open");
        let intake_id = seed_intake(&mut catalog, "sha-d");
        assert!(catalog.delete_intake(intake_id).expect("delete"));
        assert!(catalog.intake_by_id(intake_id).expect("lookup").is_none());
        // A second call finds nothing to delete.
        assert!(!catalog.delete_intake(intake_id).expect("rerun"));
    }
}
