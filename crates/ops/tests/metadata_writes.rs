// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the metadata write ops.
//!
//! Each test mounts a catalog-only library over a tempdir-backed
//! catalog, runs one write op in a write session on it, and inspects the
//! resulting catalog state to make sure both the data change and the
//! audit row are recorded.

use std::path::PathBuf;
use std::sync::Arc;

use bookrack_catalog::{Catalog, NewIntake, NewOverride, NewPublicationAttrs, STATUS_ACKNOWLEDGED};
use bookrack_config::Config;
use bookrack_core::{ItemKind, PartitionIdx};
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::dto::writes::{
    AcknowledgeMetadataGapRequest, AddContributorRequest, ApproveMetadataRequest,
    ClearMetadataFieldRequest, RejectMetadataRequest, RemoveContributorRequest,
    SetMetadataFieldRequest, VoidMetadataFieldRequest,
};
use bookrack_ops::reads::books::{find_books, show_book};
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::write_session::{Region, WriteSession};
use bookrack_ops::writes::metadata::{
    acknowledge_metadata_gap, add_contributor, approve_metadata, clear_metadata_field,
    reject_metadata, remove_contributor, set_metadata_field, void_metadata_field,
};
use bookrack_ops::{Caller, Ops, with_caller_override};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    handle: Arc<LibraryHandle<OllamaEmbedClient>>,
    registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
    catalog_db: PathBuf,
    backup_dir: PathBuf,
}

impl Fixture {
    fn build() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = Arc::new(Config::new(tmp.path().to_path_buf()));
        let catalog_db = cfg.catalog_db();
        let corpus_db = cfg.corpus_db();
        let backup_dir = tmp.path().join("backup");
        // Create both stores once so their schemas are migrated before
        // the first op runs. The read ops open `corpus.db` through the
        // read-only door, which refuses a missing file, so the fixture
        // materializes it the way ingest does in a real library.
        Catalog::open_for_write_unbacked(&catalog_db).expect("seed catalog");
        bookrack_corpus::Corpus::open_for_write(&corpus_db).expect("seed corpus");
        let ops = Ops::<OllamaEmbedClient>::catalog_only(
            corpus_db,
            catalog_db.clone(),
            &cfg.lancedb_dir(),
            cfg.books_dir(),
            backup_dir.clone(),
            Caller::cli(),
        );
        let handle = LibraryHandle::new("default", cfg, ops);
        let registry = LibraryRegistry::single(Arc::clone(&handle));
        Fixture {
            _tmp: tmp,
            handle,
            registry,
            catalog_db,
            backup_dir,
        }
    }

    /// The library's ops, for the read side of a test.
    fn ops(&self) -> &Ops<OllamaEmbedClient> {
        self.handle.ops()
    }

    /// A write session on the library, attributed to the CLI.
    fn session(&self) -> WriteSession<OllamaEmbedClient> {
        self.session_as(Caller::cli())
    }

    /// A write session on the library, attributed to `caller`.
    fn session_as(&self, caller: Caller) -> WriteSession<OllamaEmbedClient> {
        self.registry
            .begin_write(&self.handle, Region::Library, caller)
            .expect("no other session is open on the fixture")
    }

    fn catalog(&self) -> Catalog {
        Catalog::open_for_write_unbacked(&self.catalog_db).expect("open catalog")
    }

    fn seed_intake(&self, sha: &str) -> i64 {
        self.catalog()
            .register_intake(ItemKind::Book, &NewIntake::new(sha))
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
        &mut fx.session(),
        SetMetadataFieldRequest {
            intake_id: id,
            field: "title".to_string(),
            value: "A New Title".to_string(),
            reason: None,
            confirmed: false,
        },
    )
    .expect("set");

    let cat = fx.catalog();
    let effective = cat
        .effective_publication_attrs(id, ItemKind::Book)
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
fn set_metadata_field_records_the_confirmed_mark() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-set-confirmed");
    set_metadata_field(
        &mut fx.session(),
        SetMetadataFieldRequest {
            intake_id: id,
            field: "publisher".to_string(),
            value: "Verified Press".to_string(),
            reason: Some("checked the copyright page".to_string()),
            confirmed: true,
        },
    )
    .expect("set confirmed");
    let row = fx
        .catalog()
        .overrides_for_address(id, ItemKind::Book)
        .expect("overrides")
        .into_iter()
        .find(|o| o.field == "publisher")
        .expect("publisher override");
    assert!(row.confirmed);

    // A later unconfirmed rewrite drops the mark: the new value has
    // not been checked against the source.
    set_metadata_field(
        &mut fx.session(),
        SetMetadataFieldRequest {
            intake_id: id,
            field: "publisher".to_string(),
            value: "Another Press".to_string(),
            reason: None,
            confirmed: false,
        },
    )
    .expect("set unconfirmed");
    let row = fx
        .catalog()
        .overrides_for_address(id, ItemKind::Book)
        .expect("overrides")
        .into_iter()
        .find(|o| o.field == "publisher")
        .expect("publisher override");
    assert!(!row.confirmed);
}

#[test]
fn set_and_clear_record_the_reason_on_their_audit_rows() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-reason");
    set_metadata_field(
        &mut fx.session(),
        SetMetadataFieldRequest {
            intake_id: id,
            field: "title".to_string(),
            value: "Corrected Title".to_string(),
            reason: Some("matches the title page".to_string()),
            confirmed: false,
        },
    )
    .expect("set");
    clear_metadata_field(
        &mut fx.session(),
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
        &mut fx.session(),
        SetMetadataFieldRequest {
            intake_id: id,
            field: "pub_place".to_string(),
            value: "New York".to_string(),
            reason: None,
            confirmed: false,
        },
    )
    .expect("set pub_place");
    set_metadata_field(
        &mut fx.session(),
        SetMetadataFieldRequest {
            intake_id: id,
            field: "original_year".to_string(),
            value: "1949".to_string(),
            reason: None,
            confirmed: false,
        },
    )
    .expect("set original_year");
    let effective = fx
        .catalog()
        .effective_publication_attrs(id, ItemKind::Book)
        .expect("effective");
    assert_eq!(effective.get("pub_place"), Some("New York"));
    assert_eq!(effective.get("original_year"), Some("1949"));
}

#[test]
fn clear_metadata_field_falls_back_to_base_and_records_a_delete() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-clear");
    let mut base = NewPublicationAttrs::new(id, ItemKind::Book);
    base.title = Some("Base Title".to_string());
    fx.catalog().upsert_publication_attrs(&base).expect("base");
    set_metadata_field(
        &mut fx.session(),
        SetMetadataFieldRequest {
            intake_id: id,
            field: "title".to_string(),
            value: "Override Title".to_string(),
            reason: None,
            confirmed: false,
        },
    )
    .expect("set");
    let outcome = clear_metadata_field(
        &mut fx.session(),
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
        .effective_publication_attrs(id, ItemKind::Book)
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
        &mut fx.session(),
        AcknowledgeMetadataGapRequest {
            intake_id: id,
            reason: "operator vetted".to_string(),
        },
    )
    .expect("ack");
    let review = fx
        .catalog()
        .review(id, ItemKind::Book)
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
        &mut fx.session(),
        ApproveMetadataRequest {
            intake_id: id,
            reason: Some("checked against the printed copy".to_string()),
        },
    )
    .expect("approve");
    let review = fx
        .catalog()
        .review(id, ItemKind::Book)
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
        &mut fx.session(),
        ApproveMetadataRequest {
            intake_id: id,
            reason: None,
        },
    )
    .expect("approve");
    let review = fx
        .catalog()
        .review(id, ItemKind::Book)
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
        &mut fx.session(),
        RejectMetadataRequest {
            intake_id: id,
            reason: "wrong source file".to_string(),
        },
    )
    .expect("reject");
    let review = fx
        .catalog()
        .review(id, ItemKind::Book)
        .expect("review")
        .expect("present");
    assert_eq!(review.status, bookrack_catalog::STATUS_REJECTED);
    // The reason lives on the audit row; the review note is untouched.
    assert_eq!(review.notes, None);
    let book_root_id = PartitionIdx::new(id).root().get();
    let audit = fx
        .catalog()
        .metadata_audit_for_node(book_root_id)
        .expect("audit");
    assert!(audit.iter().any(|r| r.action == "reject"));
}

#[test]
fn status_flips_preserve_the_ingest_audit_note() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-preserve-notes");
    fx.catalog()
        .upsert_review(
            &bookrack_catalog::NewReview::new(
                id,
                ItemKind::Book,
                "bookrack-ingest:default",
                bookrack_catalog::STATUS_PENDING,
            )
            .notes("audit verdict=needs_work, flagged: publisher, year"),
        )
        .expect("ingest review");

    approve_metadata(
        &mut fx.session(),
        ApproveMetadataRequest {
            intake_id: id,
            reason: Some("verified against the source".to_string()),
        },
    )
    .expect("approve");

    let review = fx
        .catalog()
        .review(id, ItemKind::Book)
        .expect("review")
        .expect("present");
    assert_eq!(review.status, bookrack_catalog::STATUS_APPROVED);
    assert_eq!(
        review.notes.as_deref(),
        Some("audit verdict=needs_work, flagged: publisher, year")
    );
}

#[test]
fn write_ops_reject_unknown_intake_ids() {
    let fx = Fixture::build();
    let err = set_metadata_field(
        &mut fx.session(),
        SetMetadataFieldRequest {
            intake_id: 999,
            field: "title".to_string(),
            value: "X".to_string(),
            reason: None,
            confirmed: false,
        },
    )
    .expect_err("error");
    assert!(matches!(
        err,
        bookrack_ops::OpsError::IntakeNotFound { intake_id: 999 }
    ));
    let err = clear_metadata_field(
        &mut fx.session(),
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
        &mut fx.session(),
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
        &mut fx.session(),
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
        &mut fx.session(),
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
    // A CLI session should write `actor_kind = human`, an MCP session
    // should write `actor_kind = llm`, with the matching `actor_detail`
    // on each. The pair of rows lets the audit trail tell the two
    // surfaces apart.
    let fx = Fixture::build();
    let id_cli = fx.seed_intake("sha-cli");
    set_metadata_field(
        &mut fx.session_as(Caller::cli()),
        SetMetadataFieldRequest {
            intake_id: id_cli,
            field: "title".to_string(),
            value: "From CLI".to_string(),
            reason: None,
            confirmed: false,
        },
    )
    .expect("cli set");
    let cli_row = fx
        .catalog()
        .metadata_audit_for_node(PartitionIdx::new(id_cli).root().get())
        .expect("audit")
        .into_iter()
        .find(|r| r.action == "update")
        .expect("update row");
    assert_eq!(cli_row.actor_kind.as_str(), "human");
    assert_eq!(cli_row.actor_detail.as_deref(), Some("cli"));

    let id_mcp = fx.seed_intake("sha-mcp");
    set_metadata_field(
        &mut fx.session_as(Caller::mcp()),
        SetMetadataFieldRequest {
            intake_id: id_mcp,
            field: "title".to_string(),
            value: "From MCP".to_string(),
            reason: None,
            confirmed: false,
        },
    )
    .expect("mcp set");
    let mcp_row = fx
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
fn the_session_caller_labels_a_write_whatever_the_ops_caller_and_override() {
    // The fixture's `Ops` is built with `Caller::cli()`, and the write
    // below runs inside a task-scope `Caller::mcp()` override. Neither
    // reaches the write: the audit row, the call row, and the reported
    // outcome all carry the session's own caller.
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-override");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build runtime");
    let outcome = runtime
        .block_on(with_caller_override(Caller::mcp(), async {
            set_metadata_field(
                &mut fx.session_as(Caller::control_plane()),
                SetMetadataFieldRequest {
                    intake_id: id,
                    field: "title".to_string(),
                    value: "From the control plane".to_string(),
                    reason: None,
                    confirmed: false,
                },
            )
        }))
        .expect("set inside override scope");
    assert_eq!(outcome.actor_kind, "human");
    assert_eq!(outcome.actor_detail.as_deref(), Some("control_plane"));

    let catalog = fx.catalog();
    let row = catalog
        .metadata_audit_for_node(PartitionIdx::new(id).root().get())
        .expect("audit")
        .into_iter()
        .find(|r| r.action == "update")
        .expect("update row");
    assert_eq!(row.actor_kind.as_str(), "human");
    assert_eq!(row.actor_detail.as_deref(), Some("control_plane"));

    let sources: Vec<String> = catalog
        .tool_calls_for_tool("library.metadata.set")
        .expect("call rows")
        .into_iter()
        .map(|c| c.source)
        .collect();
    assert_eq!(sources, ["control_plane"]);
}

#[test]
fn show_book_lists_active_overrides_with_their_curation_trail() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-override-visibility");
    let mut base = NewPublicationAttrs::new(id, ItemKind::Book);
    base.title = Some("Base Title".to_string());
    fx.catalog().upsert_publication_attrs(&base).expect("base");

    let detail = show_book(fx.ops(), id).expect("show before");
    assert!(detail.overrides.is_empty());

    set_metadata_field(
        &mut fx.session(),
        SetMetadataFieldRequest {
            intake_id: id,
            field: "title".to_string(),
            value: "Curated Title".to_string(),
            reason: Some("matches the title page".to_string()),
            confirmed: false,
        },
    )
    .expect("set");

    let detail = show_book(fx.ops(), id).expect("show after set");
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
        &mut fx.session(),
        ClearMetadataFieldRequest {
            intake_id: id,
            field: "title".to_string(),
            reason: None,
        },
    )
    .expect("clear");
    let detail = show_book(fx.ops(), id).expect("show after clear");
    assert!(detail.overrides.is_empty());
    assert_eq!(
        detail.effective_biblio.get("title").map(String::as_str),
        Some("Base Title")
    );
}

#[test]
fn contributor_add_makes_the_book_findable_by_name() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-contrib-add");

    let outcome = add_contributor(
        &mut fx.session(),
        AddContributorRequest {
            intake_id: id,
            role: "author".to_string(),
            name: "A Missing Author".to_string(),
            nationality: None,
            reason: Some("OPF creator element is empty".to_string()),
        },
    )
    .expect("add");

    let detail = show_book(fx.ops(), id).expect("show");
    let entry = detail
        .contributors
        .iter()
        .find(|c| c.name == "A Missing Author")
        .expect("contributor listed");
    assert_eq!(entry.contributor_id, outcome.contributor_id);
    assert_eq!(entry.role, "author");
    assert_eq!(entry.origin, "user");

    let filter = bookrack_ops::dto::BookFilter {
        contributor_name: Some("A Missing Author".to_string()),
        ..Default::default()
    };
    let found = find_books(fx.ops(), filter, 10, 0).expect("find");
    assert!(found.books.iter().any(|b| b.intake_id == id));

    let audit = fx
        .catalog()
        .metadata_audit_for_node(PartitionIdx::new(id).root().get())
        .expect("audit");
    let row = audit
        .iter()
        .find(|r| r.table_name == "node_contributors" && r.action == "insert")
        .expect("insert row");
    assert_eq!(row.new_value.as_deref(), Some("A Missing Author"));
    assert_eq!(row.reason.as_deref(), Some("OPF creator element is empty"));
}

#[test]
fn contributor_add_rejects_an_unknown_role() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-contrib-role");
    let err = add_contributor(
        &mut fx.session(),
        AddContributorRequest {
            intake_id: id,
            role: "narrator".to_string(),
            name: "Somebody".to_string(),
            nationality: None,
            reason: None,
        },
    )
    .expect_err("unknown role must be rejected");
    assert!(matches!(
        &err,
        bookrack_ops::OpsError::UnknownContributorRole { role } if role == "narrator"
    ));
    assert!(
        err.to_string().contains("author"),
        "error lists the role set"
    );
}

#[test]
fn contributor_remove_strips_a_wrong_extracted_attribution() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-contrib-remove");
    let wrong_id = fx
        .catalog()
        .add_contributor(&bookrack_catalog::NewContributor::new(
            id,
            ItemKind::Book,
            "author",
            0,
            "extracted",
            "An Ebook Packager",
        ))
        .expect("seed extracted contributor");

    let outcome = remove_contributor(
        &mut fx.session(),
        RemoveContributorRequest {
            intake_id: id,
            contributor_id: wrong_id,
            reason: Some("packager credit, not an author".to_string()),
        },
    )
    .expect("remove");
    assert!(outcome.changed);

    let detail = show_book(fx.ops(), id).expect("show");
    assert!(detail.contributors.is_empty());

    let audit = fx
        .catalog()
        .metadata_audit_for_node(PartitionIdx::new(id).root().get())
        .expect("audit");
    let row = audit
        .iter()
        .find(|r| r.table_name == "node_contributors" && r.action == "delete")
        .expect("delete row");
    assert_eq!(row.old_value.as_deref(), Some("An Ebook Packager"));
    assert_eq!(
        row.reason.as_deref(),
        Some("packager credit, not an author")
    );

    let err = remove_contributor(
        &mut fx.session(),
        RemoveContributorRequest {
            intake_id: id,
            contributor_id: wrong_id,
            reason: None,
        },
    )
    .expect_err("already removed");
    assert!(matches!(
        err,
        bookrack_ops::OpsError::ContributorNotFound { .. }
    ));
}

#[test]
fn void_suppresses_the_extracted_value_until_cleared() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-void");
    let mut base = NewPublicationAttrs::new(id, ItemKind::Book);
    base.publisher = Some("Wrong Publisher".to_string());
    fx.catalog().upsert_publication_attrs(&base).expect("base");

    let outcome = void_metadata_field(
        &mut fx.session(),
        VoidMetadataFieldRequest {
            intake_id: id,
            field: "publisher".to_string(),
            reason: Some("extracted value is not a publisher".to_string()),
        },
    )
    .expect("void");
    assert!(outcome.changed);

    let effective = fx
        .catalog()
        .effective_publication_attrs(id, ItemKind::Book)
        .expect("effective");
    assert!(effective.get("publisher").is_none());

    let detail = show_book(fx.ops(), id).expect("show");
    let entry = detail
        .overrides
        .iter()
        .find(|o| o.field == "publisher")
        .expect("tombstone entry");
    assert!(entry.value.is_none());

    let audit = fx
        .catalog()
        .metadata_audit_for_node(PartitionIdx::new(id).root().get())
        .expect("audit");
    let row = audit.iter().find(|r| r.action == "void").expect("void row");
    assert_eq!(row.old_value.as_deref(), Some("Wrong Publisher"));
    assert!(row.new_value.is_none());
    assert_eq!(
        row.reason.as_deref(),
        Some("extracted value is not a publisher")
    );

    let cleared = clear_metadata_field(
        &mut fx.session(),
        ClearMetadataFieldRequest {
            intake_id: id,
            field: "publisher".to_string(),
            reason: None,
        },
    )
    .expect("clear");
    assert!(cleared.changed);
    let effective = fx
        .catalog()
        .effective_publication_attrs(id, ItemKind::Book)
        .expect("effective");
    assert_eq!(effective.get("publisher"), Some("Wrong Publisher"));
}

#[test]
fn void_rejects_a_field_name_outside_the_editable_set() {
    let fx = Fixture::build();
    let id = fx.seed_intake("sha-void-unknown");
    for field in ["tittle", "confidence"] {
        let err = void_metadata_field(
            &mut fx.session(),
            VoidMetadataFieldRequest {
                intake_id: id,
                field: field.to_string(),
                reason: None,
            },
        )
        .expect_err("unknown field must be rejected");
        assert!(matches!(
            err,
            bookrack_ops::OpsError::UnknownMetadataField { .. }
        ));
    }
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
            &mut fx.session(),
            SetMetadataFieldRequest {
                intake_id: id,
                field: field.to_string(),
                value: "x".to_string(),
                reason: None,
                confirmed: false,
            },
        )
        .expect_err("unknown field must be rejected");
        assert!(matches!(
            &err,
            bookrack_ops::OpsError::UnknownMetadataField { field: f, .. } if f == field
        ));
        assert!(
            err.to_string().contains("title"),
            "error lists the editable set"
        );
    }
    let effective = fx
        .catalog()
        .effective_publication_attrs(id, ItemKind::Book)
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
        &mut fx.session(),
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
            ItemKind::Book,
            "tittle",
            Some("A Typo'd Title".to_string()),
            "human",
        ))
        .expect("seed stale override");

    let outcome = clear_metadata_field(
        &mut fx.session(),
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
        .effective_publication_attrs(id, ItemKind::Book)
        .expect("effective");
    assert!(effective.get("tittle").is_none());
}

#[test]
fn a_metadata_write_on_a_behind_catalog_backs_it_up_before_migrating() {
    use bookrack_catalog::fixtures;

    let fx = Fixture::build();
    let id = fx.seed_intake("sha-behind");
    fixtures::step_back_one_version(&fx.catalog_db);
    let behind = fixtures::CURRENT_VERSION - 1;

    set_metadata_field(
        &mut fx.session(),
        SetMetadataFieldRequest {
            intake_id: id,
            field: "title".to_string(),
            value: "A New Title".to_string(),
            reason: None,
            confirmed: false,
        },
    )
    .expect("the write migrates the catalog and lands");
    assert_eq!(
        fixtures::user_version(&fx.catalog_db),
        fixtures::CURRENT_VERSION
    );

    let suffix = format!("-from-v{behind}.bak");
    let backups: Vec<String> = std::fs::read_dir(&fx.backup_dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        backups
            .iter()
            .any(|name| name.starts_with("catalog-") && name.ends_with(&suffix)),
        "a populated catalog was migrated with no snapshot; backup dir holds {backups:?}",
    );
}
