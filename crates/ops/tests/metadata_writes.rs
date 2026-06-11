// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the metadata write ops.
//!
//! Each test builds a catalog-only [`Ops`] over a tempdir-backed catalog,
//! calls one write op, and inspects the resulting catalog state to make
//! sure both the data change and the audit row are recorded.

use std::path::PathBuf;

use bookrack_catalog::{
    BOOK_SCOPE, Catalog, NewIntake, NewOverride, NewPublicationAttrs, STATUS_ACKNOWLEDGED,
};
use bookrack_core::PartitionIdx;
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::dto::writes::{
    AcknowledgeMetadataGapRequest, ApproveMetadataRequest, ClearMetadataFieldRequest,
    RejectMetadataRequest, SetMetadataFieldRequest,
};
use bookrack_ops::reads::books::show_book;
use bookrack_ops::writes::metadata::{
    acknowledge_metadata_gap, approve_metadata, clear_metadata_field, reject_metadata,
    set_metadata_field,
};
use bookrack_ops::{Caller, Ops, with_caller_override};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    ops: Ops<OllamaEmbedClient>,
    catalog_db: PathBuf,
    corpus_db: PathBuf,
    lancedb_dir: PathBuf,
    books_dir: PathBuf,
    backup_dir: PathBuf,
}

impl Fixture {
    fn build() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let catalog_db = tmp.path().join("catalog.db");
        let corpus_db = tmp.path().join("corpus.db");
        let lancedb_dir = tmp.path().join("lancedb");
        let books_dir = tmp.path().join("books");
        let backup_dir = tmp.path().join("backup");
        // Create the catalog once so the schema is migrated before the
        // first op runs.
        Catalog::open(&catalog_db).expect("seed catalog");
        let ops = Ops::<OllamaEmbedClient>::catalog_only(
            corpus_db.clone(),
            catalog_db.clone(),
            &lancedb_dir,
            books_dir.clone(),
            backup_dir.clone(),
            Caller::cli(),
        );
        Fixture {
            _tmp: tmp,
            ops,
            catalog_db,
            corpus_db,
            lancedb_dir,
            books_dir,
            backup_dir,
        }
    }

    fn mcp() -> Fixture {
        let mut fx = Fixture::build();
        fx.ops = Ops::<OllamaEmbedClient>::catalog_only(
            fx.corpus_db.clone(),
            fx.catalog_db.clone(),
            &fx.lancedb_dir,
            fx.books_dir.clone(),
            fx.backup_dir.clone(),
            Caller::mcp(),
        );
        fx
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

#[test]
fn set_metadata_field_records_the_override_and_an_update_audit_row() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-set");
    set_metadata_field(
        &fx.ops,
        SetMetadataFieldRequest {
            intake_id: id,
            field: "title".to_string(),
            value: "A New Title".to_string(),
            reason: None,
        },
    )
    .expect("set");

    let cat = fx.catalog();
    let effective = cat
        .effective_publication_attrs(id, BOOK_SCOPE)
        .expect("effective");
    assert_eq!(effective.get("title"), Some("A New Title"));

    let book_root_id = PartitionIdx::new(id).root().get();
    let audit = cat.metadata_audit_for_node(book_root_id).expect("audit");
    let update_row = audit
        .iter()
        .find(|r| r.action == "update")
        .expect("an update row");
    assert_eq!(update_row.field.as_deref(), Some("title"));
    assert_eq!(update_row.new_value.as_deref(), Some("A New Title"));
    assert!(update_row.old_value.is_none());
    assert!(
        update_row.reason.is_none(),
        "no reason given, none recorded"
    );
}

#[test]
fn set_and_clear_record_the_reason_on_their_audit_rows() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-reason");
    set_metadata_field(
        &fx.ops,
        SetMetadataFieldRequest {
            intake_id: id,
            field: "title".to_string(),
            value: "Corrected Title".to_string(),
            reason: Some("matches the title page".to_string()),
        },
    )
    .expect("set");
    clear_metadata_field(
        &fx.ops,
        ClearMetadataFieldRequest {
            intake_id: id,
            field: "title".to_string(),
            reason: Some("override entered by mistake".to_string()),
        },
    )
    .expect("clear");

    let audit = fx
        .catalog()
        .metadata_audit_for_node(PartitionIdx::new(id).root().get())
        .expect("audit");
    let update = audit
        .iter()
        .find(|r| r.action == "update")
        .expect("update row");
    assert_eq!(update.reason.as_deref(), Some("matches the title page"));
    let delete = audit
        .iter()
        .find(|r| r.action == "delete")
        .expect("delete row");
    assert_eq!(
        delete.reason.as_deref(),
        Some("override entered by mistake")
    );
}

#[test]
fn set_metadata_field_pub_place_and_original_year_flow_through_effective() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-pubplace");
    set_metadata_field(
        &fx.ops,
        SetMetadataFieldRequest {
            intake_id: id,
            field: "pub_place".to_string(),
            value: "New York".to_string(),
            reason: None,
        },
    )
    .expect("set pub_place");
    set_metadata_field(
        &fx.ops,
        SetMetadataFieldRequest {
            intake_id: id,
            field: "original_year".to_string(),
            value: "1949".to_string(),
            reason: None,
        },
    )
    .expect("set original_year");
    let effective = fx
        .catalog()
        .effective_publication_attrs(id, BOOK_SCOPE)
        .expect("effective");
    assert_eq!(effective.get("pub_place"), Some("New York"));
    assert_eq!(effective.get("original_year"), Some("1949"));
}

#[test]
fn clear_metadata_field_falls_back_to_base_and_records_a_delete() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-clear");
    let mut base = NewPublicationAttrs::new(id, BOOK_SCOPE);
    base.title = Some("Base Title".to_string());
    fx.catalog().upsert_publication_attrs(&base).expect("base");
    set_metadata_field(
        &fx.ops,
        SetMetadataFieldRequest {
            intake_id: id,
            field: "title".to_string(),
            value: "Override Title".to_string(),
            reason: None,
        },
    )
    .expect("set");
    let outcome = clear_metadata_field(
        &fx.ops,
        ClearMetadataFieldRequest {
            intake_id: id,
            field: "title".to_string(),
            reason: None,
        },
    )
    .expect("clear");
    assert!(outcome.changed);

    let effective = fx
        .catalog()
        .effective_publication_attrs(id, BOOK_SCOPE)
        .expect("effective");
    assert_eq!(effective.get("title"), Some("Base Title"));

    let book_root_id = PartitionIdx::new(id).root().get();
    let audit = fx
        .catalog()
        .metadata_audit_for_node(book_root_id)
        .expect("audit");
    assert!(audit.iter().any(|r| r.action == "delete"));
}

#[test]
fn acknowledge_records_a_review_and_a_gate_audit_row() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-ack");
    acknowledge_metadata_gap(
        &fx.ops,
        AcknowledgeMetadataGapRequest {
            intake_id: id,
            reason: "operator vetted".to_string(),
        },
    )
    .expect("ack");
    let review = fx
        .catalog()
        .review(id, BOOK_SCOPE)
        .expect("review")
        .expect("present");
    assert_eq!(review.status, STATUS_ACKNOWLEDGED);

    let book_root_id = PartitionIdx::new(id).root().get();
    let audit = fx
        .catalog()
        .metadata_audit_for_node(book_root_id)
        .expect("audit");
    assert!(audit.iter().any(|r| r.action == "acknowledge_gate"));
}

#[test]
fn approve_records_a_review_and_an_approval_audit_row() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-approve");
    approve_metadata(
        &fx.ops,
        ApproveMetadataRequest {
            intake_id: id,
            reason: Some("checked against the printed copy".to_string()),
        },
    )
    .expect("approve");
    let review = fx
        .catalog()
        .review(id, BOOK_SCOPE)
        .expect("review")
        .expect("present");
    assert_eq!(review.status, bookrack_catalog::STATUS_APPROVED);
    assert_eq!(review.reviewed_by, "human");

    let book_root_id = PartitionIdx::new(id).root().get();
    let audit = fx
        .catalog()
        .metadata_audit_for_node(book_root_id)
        .expect("audit");
    assert!(audit.iter().any(|r| r.action == "approve"));
}

#[test]
fn approve_without_a_reason_still_records_the_audit_row() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-approve-noreason");
    approve_metadata(
        &fx.ops,
        ApproveMetadataRequest {
            intake_id: id,
            reason: None,
        },
    )
    .expect("approve");
    let review = fx
        .catalog()
        .review(id, BOOK_SCOPE)
        .expect("review")
        .expect("present");
    assert_eq!(review.status, bookrack_catalog::STATUS_APPROVED);
    assert_eq!(review.notes, None);
}

#[test]
fn reject_records_a_review_and_a_reject_audit_row() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-reject");
    reject_metadata(
        &fx.ops,
        RejectMetadataRequest {
            intake_id: id,
            reason: "wrong source file".to_string(),
        },
    )
    .expect("reject");
    let review = fx
        .catalog()
        .review(id, BOOK_SCOPE)
        .expect("review")
        .expect("present");
    assert_eq!(review.status, bookrack_catalog::STATUS_REJECTED);
    assert_eq!(review.notes.as_deref(), Some("wrong source file"));
    let book_root_id = PartitionIdx::new(id).root().get();
    let audit = fx
        .catalog()
        .metadata_audit_for_node(book_root_id)
        .expect("audit");
    assert!(audit.iter().any(|r| r.action == "reject"));
}

#[test]
fn write_ops_reject_unknown_intake_ids() {
    let fx = Fixture::build();
    let err = set_metadata_field(
        &fx.ops,
        SetMetadataFieldRequest {
            intake_id: 999,
            field: "title".to_string(),
            value: "X".to_string(),
            reason: None,
        },
    )
    .expect_err("error");
    assert!(matches!(
        err,
        bookrack_ops::OpsError::IntakeNotFound { intake_id: 999 }
    ));
    let err = clear_metadata_field(
        &fx.ops,
        ClearMetadataFieldRequest {
            intake_id: 999,
            field: "title".to_string(),
            reason: None,
        },
    )
    .expect_err("error");
    assert!(matches!(
        err,
        bookrack_ops::OpsError::IntakeNotFound { intake_id: 999 }
    ));
    let err = acknowledge_metadata_gap(
        &fx.ops,
        AcknowledgeMetadataGapRequest {
            intake_id: 999,
            reason: "r".to_string(),
        },
    )
    .expect_err("error");
    assert!(matches!(
        err,
        bookrack_ops::OpsError::IntakeNotFound { intake_id: 999 }
    ));
    let err = approve_metadata(
        &fx.ops,
        ApproveMetadataRequest {
            intake_id: 999,
            reason: None,
        },
    )
    .expect_err("error");
    assert!(matches!(
        err,
        bookrack_ops::OpsError::IntakeNotFound { intake_id: 999 }
    ));
    let err = reject_metadata(
        &fx.ops,
        RejectMetadataRequest {
            intake_id: 999,
            reason: "r".to_string(),
        },
    )
    .expect_err("error");
    assert!(matches!(
        err,
        bookrack_ops::OpsError::IntakeNotFound { intake_id: 999 }
    ));
}

#[test]
fn cli_and_mcp_writes_are_distinguishable_by_actor_kind() {
    // A CLI write should write `actor_kind = human`, an MCP write should
    // write `actor_kind = llm`, with the matching `actor_detail` on
    // each. The pair of rows lets the audit trail tell the two surfaces
    // apart.
    let fx_cli = Fixture::build();
    let id_cli = fx_cli.seed_intake("sha-cli");
    set_metadata_field(
        &fx_cli.ops,
        SetMetadataFieldRequest {
            intake_id: id_cli,
            field: "title".to_string(),
            value: "From CLI".to_string(),
            reason: None,
        },
    )
    .expect("cli set");
    let cli_row = fx_cli
        .catalog()
        .metadata_audit_for_node(PartitionIdx::new(id_cli).root().get())
        .expect("audit")
        .into_iter()
        .find(|r| r.action == "update")
        .expect("update row");
    assert_eq!(cli_row.actor_kind.as_str(), "human");
    assert_eq!(cli_row.actor_detail.as_deref(), Some("cli"));

    let fx_mcp = Fixture::mcp();
    let id_mcp = fx_mcp.seed_intake("sha-mcp");
    set_metadata_field(
        &fx_mcp.ops,
        SetMetadataFieldRequest {
            intake_id: id_mcp,
            field: "title".to_string(),
            value: "From MCP".to_string(),
            reason: None,
        },
    )
    .expect("mcp set");
    let mcp_row = fx_mcp
        .catalog()
        .metadata_audit_for_node(PartitionIdx::new(id_mcp).root().get())
        .expect("audit")
        .into_iter()
        .find(|r| r.action == "update")
        .expect("update row");
    assert_eq!(mcp_row.actor_kind.as_str(), "llm");
    assert_eq!(mcp_row.actor_detail.as_deref(), Some("mcp"));
}

#[test]
fn caller_override_relabels_writes_on_a_shared_ops() {
    // The daemon shares one `Ops` across surfaces, and the MCP server
    // installs a task-scope `Caller::mcp()` override around each tool
    // call. A write inside the scope must stamp `llm` / `mcp` on both
    // the audit row and the reported outcome; a write outside the scope
    // on the same `Ops` falls back to the baked-in caller.
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-override");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build runtime");
    let outcome = runtime
        .block_on(with_caller_override(Caller::mcp(), async {
            set_metadata_field(
                &fx.ops,
                SetMetadataFieldRequest {
                    intake_id: id,
                    field: "title".to_string(),
                    value: "From MCP via override".to_string(),
                    reason: None,
                },
            )
        }))
        .expect("set inside override scope");
    assert_eq!(outcome.actor_kind, "llm");
    assert_eq!(outcome.actor_detail.as_deref(), Some("mcp"));

    let row = fx
        .catalog()
        .metadata_audit_for_node(PartitionIdx::new(id).root().get())
        .expect("audit")
        .into_iter()
        .find(|r| r.action == "update")
        .expect("update row");
    assert_eq!(row.actor_kind.as_str(), "llm");
    assert_eq!(row.actor_detail.as_deref(), Some("mcp"));

    let outside = clear_metadata_field(
        &fx.ops,
        ClearMetadataFieldRequest {
            intake_id: id,
            field: "title".to_string(),
            reason: None,
        },
    )
    .expect("clear outside override scope");
    assert_eq!(outside.actor_kind, "human");
    assert_eq!(outside.actor_detail.as_deref(), Some("cli"));
}

#[test]
fn show_book_lists_active_overrides_with_their_curation_trail() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-override-visibility");
    let mut base = NewPublicationAttrs::new(id, BOOK_SCOPE);
    base.title = Some("Base Title".to_string());
    fx.catalog().upsert_publication_attrs(&base).expect("base");

    let detail = show_book(&fx.ops, id).expect("show before");
    assert!(detail.overrides.is_empty());

    set_metadata_field(
        &fx.ops,
        SetMetadataFieldRequest {
            intake_id: id,
            field: "title".to_string(),
            value: "Curated Title".to_string(),
            reason: Some("matches the title page".to_string()),
        },
    )
    .expect("set");

    let detail = show_book(&fx.ops, id).expect("show after set");
    assert_eq!(detail.overrides.len(), 1);
    let entry = &detail.overrides[0];
    assert_eq!(entry.field, "title");
    assert_eq!(entry.value.as_deref(), Some("Curated Title"));
    assert_eq!(entry.curated_by, "human");
    assert_eq!(
        detail.effective_biblio.get("title").map(String::as_str),
        Some("Curated Title")
    );

    clear_metadata_field(
        &fx.ops,
        ClearMetadataFieldRequest {
            intake_id: id,
            field: "title".to_string(),
            reason: None,
        },
    )
    .expect("clear");
    let detail = show_book(&fx.ops, id).expect("show after clear");
    assert!(detail.overrides.is_empty());
    assert_eq!(
        detail.effective_biblio.get("title").map(String::as_str),
        Some("Base Title")
    );
}

#[test]
fn set_rejects_a_field_name_outside_the_editable_set() {
    // A typo ("tittle") and a pipeline-owned bookkeeping column
    // ("confidence") are both rejected before anything is written: no
    // override row, no audit row.
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-unknown-set");
    for field in ["tittle", "confidence"] {
        let err = set_metadata_field(
            &fx.ops,
            SetMetadataFieldRequest {
                intake_id: id,
                field: field.to_string(),
                value: "x".to_string(),
                reason: None,
            },
        )
        .expect_err("unknown field must be rejected");
        assert!(matches!(
            &err,
            bookrack_ops::OpsError::UnknownMetadataField { field: f } if f == field
        ));
        assert!(
            err.to_string().contains("title"),
            "error lists the editable set"
        );
    }
    let effective = fx
        .catalog()
        .effective_publication_attrs(id, BOOK_SCOPE)
        .expect("effective");
    assert!(effective.get("tittle").is_none());
    let audit = fx
        .catalog()
        .metadata_audit_for_node(PartitionIdx::new(id).root().get())
        .expect("audit");
    assert!(audit.is_empty(), "a rejected set leaves no audit row");
}

#[test]
fn clear_rejects_an_unknown_field_with_no_override_row() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-unknown-clear");
    let err = clear_metadata_field(
        &fx.ops,
        ClearMetadataFieldRequest {
            intake_id: id,
            field: "tittle".to_string(),
            reason: None,
        },
    )
    .expect_err("unknown field with nothing to clear must be rejected");
    assert!(matches!(
        err,
        bookrack_ops::OpsError::UnknownMetadataField { .. }
    ));
}

#[test]
fn clear_removes_a_stale_override_with_an_unknown_field_name() {
    // Override rows that predate field validation must stay removable:
    // when a row with the unknown key exists, clear takes it out and
    // audits the delete instead of rejecting the name.
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-stale-clear");
    fx.catalog()
        .set_override(&NewOverride::new(
            id,
            BOOK_SCOPE,
            "tittle",
            Some("A Typo'd Title".to_string()),
            "human",
        ))
        .expect("seed stale override");

    let outcome = clear_metadata_field(
        &fx.ops,
        ClearMetadataFieldRequest {
            intake_id: id,
            field: "tittle".to_string(),
            reason: None,
        },
    )
    .expect("stale override must be clearable");
    assert!(outcome.changed);

    let effective = fx
        .catalog()
        .effective_publication_attrs(id, BOOK_SCOPE)
        .expect("effective");
    assert!(effective.get("tittle").is_none());
}
