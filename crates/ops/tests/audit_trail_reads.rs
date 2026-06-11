// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `show_audit_trail` and `show_pipeline_trail`.
//!
//! These reads must surface preserved audit rows even after `bookrack
//! remove` has dropped the `intake` row, and must still report a true
//! ghost id (no rows, no intake) as `IntakeNotFound`.

use std::path::PathBuf;

use bookrack_catalog::{ActorKind, Catalog, NewIntake, NewItemPipelineAudit, NewMetadataAudit};
use bookrack_core::PartitionIdx;
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::reads::metadata::show_audit_trail;
use bookrack_ops::reads::pipeline::show_pipeline_trail;
use bookrack_ops::{Caller, Ops, OpsError};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    ops: Ops<OllamaEmbedClient>,
    catalog_db: PathBuf,
}

impl Fixture {
    fn build() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let catalog_db = tmp.path().join("catalog.db");
        let corpus_db = tmp.path().join("corpus.db");
        let lancedb_dir = tmp.path().join("lancedb");
        Catalog::open(&catalog_db).expect("seed catalog");
        let books_dir = tmp.path().join("books");
        let backup_dir = tmp.path().join("backup");
        let ops = Ops::<OllamaEmbedClient>::catalog_only(
            corpus_db,
            catalog_db.clone(),
            &lancedb_dir,
            books_dir,
            backup_dir,
            Caller::cli(),
        );
        Fixture {
            _tmp: tmp,
            ops,
            catalog_db,
        }
    }

    fn catalog(&self) -> Catalog {
        Catalog::open(&self.catalog_db).expect("open catalog")
    }

    fn seed_intake(&self, sha: &str) -> i64 {
        self.catalog()
            .register_intake(&NewIntake::new(sha))
            .expect("register intake")
            .into_intake()
            .intake_id
    }
}

/// Append a metadata-audit row and a pipeline-audit row for `intake_id`.
fn seed_audit_rows(fx: &Fixture, intake_id: i64) {
    let book_root_id = PartitionIdx::new(intake_id).root().get();
    let catalog = fx.catalog();

    let mut meta = NewMetadataAudit::new("node_publication_attrs", "seed", ActorKind::System);
    meta.node_id = Some(book_root_id);
    catalog
        .record_metadata_audit(&meta)
        .expect("record metadata audit");

    let mut pipe =
        NewItemPipelineAudit::new("structure", "parse_toc", "ok", "run-1", ActorKind::Pipeline);
    pipe.book_root_id = Some(book_root_id);
    catalog
        .record_pipeline_audit(&pipe)
        .expect("record pipeline audit");
}

#[test]
fn audit_trail_reads_survive_a_removed_intake() {
    // Seed an intake, write one row to each audit table, then drop the
    // intake row to simulate a completed `bookrack remove`. Both reads
    // must still surface the preserved rows.
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-preserved");
    seed_audit_rows(&fx, id);

    let existed = fx.catalog().delete_intake(id).expect("delete intake");
    assert!(existed, "delete_intake reports the row existed");
    assert!(
        fx.catalog().intake_by_id(id).expect("lookup").is_none(),
        "intake row is gone after delete"
    );

    let meta_trail = show_audit_trail(&fx.ops, id).expect("audit trail after remove");
    assert_eq!(
        meta_trail.len(),
        1,
        "metadata_audit row preserved across remove"
    );

    let pipe_trail = show_pipeline_trail(&fx.ops, id).expect("pipeline trail after remove");
    assert_eq!(
        pipe_trail.len(),
        1,
        "book_pipeline_audit row preserved across remove"
    );
}

#[test]
fn audit_trail_reads_report_intake_not_found_for_a_ghost_id() {
    // A `intake_id` that was never registered AND has no audit rows
    // is a true ghost. Both reads must report it as `IntakeNotFound`
    // rather than returning an empty trail.
    let fx = Fixture::build();
    assert!(matches!(
        show_audit_trail(&fx.ops, 999),
        Err(OpsError::IntakeNotFound { intake_id: 999 })
    ));
    assert!(matches!(
        show_pipeline_trail(&fx.ops, 999),
        Err(OpsError::IntakeNotFound { intake_id: 999 })
    ));
}

#[test]
fn audit_trail_reads_return_empty_for_a_registered_intake_with_no_history() {
    // An intake that exists but has no audit rows yet is a legitimate
    // state — the reads must return an empty trail, not an error.
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-quiet");

    let meta_trail = show_audit_trail(&fx.ops, id).expect("audit trail for quiet intake");
    assert!(meta_trail.is_empty());

    let pipe_trail = show_pipeline_trail(&fx.ops, id).expect("pipeline trail for quiet intake");
    assert!(pipe_trail.is_empty());
}
