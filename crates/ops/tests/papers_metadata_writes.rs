// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the paper-side metadata write ops.
//!
//! What the control-plane tests cannot reach lives here: every call
//! arriving over the daemon's socket carries the same caller, so
//! "a CLI edit and an MCP edit are distinguishable" needs two callers
//! on one `Ops` — which is exactly the shape the daemon runs, and
//! exactly what `with_caller_override` exists for.

use std::future::Future;
use std::path::PathBuf;

use bookrack_catalog::{Catalog, NewContributor, NewIntake};
use bookrack_core::{ItemKind, PartitionIdx};
use bookrack_embed::{Embedder, Result as EmbedResult};
use bookrack_ops::dto::writes::{
    PaperContributorAddRequest, PaperContributorRemoveRequest, PaperSetMetadataFieldRequest,
};
use bookrack_ops::writes::papers_metadata::{
    add_paper_contributor, remove_paper_contributor, set_paper_metadata_field,
};
use bookrack_ops::{Caller, Ops, OpsError, PapersPaths};
use tempfile::TempDir;

/// A constant-vector embedder, so the fixture opens a warm `Library`
/// without a live embedding service.
struct Fake {
    dim: usize,
}

impl Embedder for Fake {
    fn embed_batch(
        &self,
        texts: &[String],
    ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
        let (dim, n) = (self.dim, texts.len());
        async move { Ok(vec![vec![0.25f32; dim]; n]) }
    }
}

struct Fixture {
    _tmp: TempDir,
    ops: Ops<Fake>,
    papers_catalog_db: PathBuf,
}

impl Fixture {
    /// Build an `Ops` with a papers backend attached, under `caller`.
    async fn build_as(caller: Caller) -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let papers_catalog_db = tmp.path().join("papers_catalog.db");
        let papers_corpus_db = tmp.path().join("papers_corpus.db");
        let papers_lancedb = tmp.path().join("lancedb_papers");
        Catalog::open(&papers_catalog_db).expect("seed paper catalog");
        bookrack_corpus::Corpus::open(&papers_corpus_db).expect("seed paper corpus");

        let papers_library = bookrack_query::Library::open(
            papers_corpus_db.clone(),
            papers_catalog_db.clone(),
            &papers_lancedb,
            Fake { dim: 8 },
            "fake-model".to_string(),
            5,
            bookrack_glean::CHUNK_VERSION,
        )
        .await
        .expect("open papers library")
        .with_kind(ItemKind::Paper);

        let ops = Ops::catalog_only(
            tmp.path().join("corpus.db"),
            tmp.path().join("catalog.db"),
            &tmp.path().join("lancedb"),
            tmp.path().join("books"),
            tmp.path().join("backup"),
            caller,
        )
        .with_papers(
            papers_library,
            PapersPaths {
                corpus_db: papers_corpus_db,
                catalog_db: papers_catalog_db.clone(),
                lancedb_dir: papers_lancedb,
                papers_dir: tmp.path().join("papers"),
            },
        );

        Fixture {
            _tmp: tmp,
            ops,
            papers_catalog_db,
        }
    }

    async fn build() -> Fixture {
        Fixture::build_as(Caller::cli()).await
    }

    fn catalog(&self) -> Catalog {
        Catalog::open(&self.papers_catalog_db).expect("open paper catalog")
    }

    fn seed_paper(&self, sha: &str) -> i64 {
        let mut catalog = self.catalog();
        catalog
            .register_intake(ItemKind::Paper, &NewIntake::new(sha).format("pdf"))
            .expect("register intake")
            .into_intake()
            .intake_id
    }

    fn audit_rows(&self, intake_id: i64) -> Vec<bookrack_catalog::MetadataAudit> {
        let node_id = PartitionIdx::new(intake_id).root().get();
        self.catalog()
            .metadata_audit_for_node(node_id)
            .expect("read audit trail")
    }
}

/// The daemon shares one `Ops` across surfaces and installs a
/// task-scope `Caller::mcp()` around each MCP tool call. Two paper
/// edits on that one handle — one inside the scope, one outside — must
/// be told apart in the trail.
///
/// Asserting only "not human" would pass on a handler that hard-coded
/// any single string, so the assertion is that the two rows *differ*.
#[tokio::test]
async fn two_surfaces_editing_one_paper_are_distinguishable_in_the_trail() {
    let fx = Fixture::build().await;
    let intake_id = fx.seed_paper("sha-attribution");

    let direct = set_paper_metadata_field(
        &fx.ops,
        PaperSetMetadataFieldRequest {
            intake_id,
            field: "title".to_string(),
            value: "Edited from the CLI".to_string(),
            reason: None,
            confirmed: false,
        },
    )
    .expect("set outside the override scope");

    let hosted = bookrack_ops::with_caller_override(Caller::mcp(), async {
        set_paper_metadata_field(
            &fx.ops,
            PaperSetMetadataFieldRequest {
                intake_id,
                field: "title".to_string(),
                value: "Edited from MCP".to_string(),
                reason: None,
                confirmed: false,
            },
        )
    })
    .await
    .expect("set inside the override scope");

    assert_ne!(
        direct.actor_kind, hosted.actor_kind,
        "the two surfaces must not report the same actor kind"
    );

    let rows = fx.audit_rows(intake_id);
    assert_eq!(rows.len(), 2, "each edit appends its own row");
    assert_ne!(
        rows[0].actor_kind, rows[1].actor_kind,
        "the trail must tell the two surfaces apart, not just the return value"
    );
    assert_eq!(rows[0].actor_detail.as_deref(), Some("cli"));
    assert_eq!(rows[1].actor_detail.as_deref(), Some("mcp"));
}

/// The reason a curator supplies reaches the audit row, and the row
/// carries the value the edit replaced.
#[tokio::test]
async fn a_paper_edit_records_its_reason_and_the_value_it_replaced() {
    let fx = Fixture::build().await;
    let intake_id = fx.seed_paper("sha-reason");

    for (value, reason) in [
        ("First", "the extracted title was the running header"),
        ("Second", "checked against the published version"),
    ] {
        set_paper_metadata_field(
            &fx.ops,
            PaperSetMetadataFieldRequest {
                intake_id,
                field: "title".to_string(),
                value: value.to_string(),
                reason: Some(reason.to_string()),
                confirmed: false,
            },
        )
        .expect("set");
    }

    let rows = fx.audit_rows(intake_id);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].reason.as_deref(),
        Some("the extracted title was the running header")
    );
    assert_eq!(rows[1].old_value.as_deref(), Some("First"));
    assert_eq!(rows[1].new_value.as_deref(), Some("Second"));
}

/// A field outside the paper editable set is refused, and the refusal
/// names the paper set rather than the book one — `isbn` is editable on
/// a book and is not a paper column, so a message listing it would send
/// the caller at a write this surface refuses.
#[tokio::test]
async fn an_unknown_paper_field_is_refused_and_names_the_paper_set() {
    let fx = Fixture::build().await;
    let intake_id = fx.seed_paper("sha-field");

    let err = set_paper_metadata_field(
        &fx.ops,
        PaperSetMetadataFieldRequest {
            intake_id,
            field: "isbn".to_string(),
            value: "irrelevant".to_string(),
            reason: None,
            confirmed: false,
        },
    )
    .expect_err("isbn is not a paper field");
    assert!(matches!(err, OpsError::UnknownMetadataField { .. }));
    let message = err.to_string();
    assert!(
        message.contains("container_title"),
        "the message must list the paper set: {message}"
    );
    assert!(
        !message.contains("isbn, "),
        "the message must not offer a book-only field as a repair: {message}"
    );
    assert!(
        fx.audit_rows(intake_id).is_empty(),
        "a refused field must not leave an audit row"
    );
}

/// Removing a contributor by surrogate id is bounded by the paper the
/// caller named: the id alone addresses a row anywhere in the catalog.
#[tokio::test]
async fn removing_a_contributor_is_bounded_by_the_paper_that_owns_it() {
    let fx = Fixture::build().await;
    let owner = fx.seed_paper("sha-owner");
    let bystander = fx.seed_paper("sha-bystander");

    let added = add_paper_contributor(
        &fx.ops,
        PaperContributorAddRequest {
            intake_id: owner,
            role: "author".to_string(),
            name: "Alex Sample".to_string(),
            family: Some("Sample".to_string()),
            given: Some("Alex".to_string()),
            orcid: None,
            reason: None,
        },
    )
    .expect("add contributor");

    let err = remove_paper_contributor(
        &fx.ops,
        PaperContributorRemoveRequest {
            intake_id: bystander,
            contributor_id: added.contributor_id,
            reason: None,
        },
    )
    .expect_err("a row on another paper must not be removable");
    assert!(matches!(err, OpsError::ContributorNotFound { .. }));

    assert_eq!(
        fx.catalog()
            .contributors_for_address(owner, ItemKind::Paper)
            .expect("read contributors")
            .len(),
        1,
        "the refused removal must leave the row in place"
    );

    // The same call against the owning paper succeeds, so the refusal
    // above is the ownership check and not a broken lookup.
    let outcome = remove_paper_contributor(
        &fx.ops,
        PaperContributorRemoveRequest {
            intake_id: owner,
            contributor_id: added.contributor_id,
            reason: Some("wrong attribution".to_string()),
        },
    )
    .expect("the owning paper may remove its own row");
    assert!(outcome.changed);
}

/// A contributor added after a removal takes the next free ordinal, so
/// the insert does not collide on the UNIQUE key.
#[tokio::test]
async fn a_contributor_added_after_a_removal_takes_the_next_free_ordinal() {
    let fx = Fixture::build().await;
    let intake_id = fx.seed_paper("sha-ordinal");

    let catalog = fx.catalog();
    let mut ids = Vec::new();
    for (ordinal, name) in [(0, "a"), (1, "b"), (2, "c")] {
        ids.push(
            catalog
                .add_contributor(&NewContributor::new(
                    intake_id,
                    ItemKind::Paper,
                    "author",
                    ordinal,
                    "user",
                    name,
                ))
                .expect("seed contributor"),
        );
    }
    remove_paper_contributor(
        &fx.ops,
        PaperContributorRemoveRequest {
            intake_id,
            contributor_id: ids[0],
            reason: None,
        },
    )
    .expect("remove the first row");

    add_paper_contributor(
        &fx.ops,
        PaperContributorAddRequest {
            intake_id,
            role: "author".to_string(),
            name: "d".to_string(),
            family: None,
            given: None,
            orcid: None,
            reason: None,
        },
    )
    .expect("add after remove must not collide on the UNIQUE key");
}
