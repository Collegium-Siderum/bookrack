// SPDX-License-Identifier: Apache-2.0

//! `bookrack papers remove` — drop one paper from every paper-side store.
//!
//! Mirrors [`crate::cmd::remove`] against the paper cluster
//! (`papers_catalog.db`, `papers_corpus.db`, `lancedb_papers`,
//! `papers_dir`). The catalog cascade is schema-identical to the book
//! side, so `Catalog::count_book_derived` and `delete_book_derived`
//! run against `papers_catalog.db` without modification; the only
//! paper-specific cleanup is deleting `intake.source_pdf_path`, the
//! archived source PDF that lives alongside the envelope under
//! `papers_dir/paper-{intake_id}.pdf`.
//!
//! Order matches the book side: catalog rows, corpus partition, vector
//! partition, envelope file, source PDF, intake row. Audit tables
//! (`metadata_audit`, `item_pipeline_audit`) are preserved.

use bookrack_catalog::{Catalog, Intake, ItemRemovalCounts};
use bookrack_config::Config;
use bookrack_core::{NodeId, PartitionIdx};
use bookrack_corpus::Corpus;
use bookrack_vectors::ChunkStore;
use eyre::{Context, Result};
use sha2::{Digest, Sha256};

use crate::cmd::input_error::CmdInputError;
use crate::cmd::remove::ExpectedFingerprint;

/// What produces the layer `papers remove` needs before it can
/// resolve anything.
const GLEAN_FIRST: &str = "Glean a paper into this library first.";

/// Inputs `Cli` collects for a `bookrack papers remove` invocation.
pub struct RemovePaperArgs {
    /// Positional intake id. `None` means the caller passed `--sha`.
    pub intake_id: Option<i64>,
    /// `--sha <hex>` alternative to the positional id.
    pub sha: Option<String>,
    /// Print the plan and exit without writing.
    pub dry_run: bool,
    /// Skip the destructive-action confirmation prompt.
    pub yes: bool,
}

/// Plan a paper remove without writing: resolve the intake, count
/// everything the execute step would delete, and report whether the
/// envelope file and source PDF are on disk. Consumed by the
/// control-plane handler's dry-run leg.
///
/// Every store is opened read-only, so the plan neither creates a
/// database nor migrates one nor materializes the lancedb layout. A
/// missing catalog is an error — there is no intake to resolve; a
/// missing corpus or vector store is reported as empty.
pub async fn plan_remove(cfg: &Config, args: &RemovePaperArgs) -> Result<RemovePaperPlan> {
    if args.intake_id.is_none() && args.sha.is_none() {
        eyre::bail!("pass an intake id (positional) or --sha <hex>");
    }
    // Only the absent file is caller input; see the book-side peer.
    if !cfg.papers_catalog_db().exists() {
        return Err(CmdInputError::NotIngested {
            what: "papers catalog",
            hint: GLEAN_FIRST,
        }
        .into());
    }
    let catalog =
        Catalog::open_read_only(&cfg.papers_catalog_db()).context("open papers catalog")?;
    let intake = resolve_intake(&catalog, args)?;
    derive_remove_plan(cfg, &catalog, intake).await
}

/// Compute the plan body for an already-resolved paper intake against
/// the caller's catalog handle. Both legs pass a handle they already
/// hold, so deriving a plan never opens the catalog a second time —
/// which is what keeps the drift check inside
/// [`execute_remove_from_plan`] from rewriting the backup that
/// [`Catalog::open_with_backup`] stamps on first open.
async fn derive_remove_plan(
    cfg: &Config,
    catalog: &Catalog,
    intake: Intake,
) -> Result<RemovePaperPlan> {
    let intake_id = intake.intake_id;
    let partition = PartitionIdx::new(intake_id);
    let paper_root_node_id: NodeId = partition.root();
    let paper_root_id = paper_root_node_id.get();

    let counts = catalog
        .count_book_derived(intake_id, paper_root_id)
        .context("count catalog rows")?;

    let envelope_path = intake.stored_path.clone();
    let envelope_exists = envelope_path
        .as_deref()
        .map(|p| std::path::Path::new(p).exists())
        .unwrap_or(false);
    let source_pdf_path = intake.source_pdf_path.clone();
    let source_pdf_exists = source_pdf_path
        .as_deref()
        .map(|p| std::path::Path::new(p).exists())
        .unwrap_or(false);

    // `try_open` reads the dimension off the table's own schema and
    // reports a store that is not on disk as absent, so the plan
    // needs neither the corpus stamp nor a creating open.
    let vector_rows = match ChunkStore::try_open(&cfg.papers_lancedb_dir())
        .await
        .context("open papers vector store")?
    {
        Some(store) => count_vector_rows(&store, partition).await.ok(),
        None => None,
    };

    let corpus_nodes = read_corpus_node_count(cfg, paper_root_node_id)?;

    Ok(RemovePaperPlan {
        intake,
        counts,
        vector_rows,
        corpus_nodes,
        envelope_path,
        envelope_exists,
        source_pdf_path,
        source_pdf_exists,
    })
}

/// Execute the remove sequence for a paper intake pinned by an
/// earlier [`plan_remove`] call. Strict: the intake must still
/// resolve in the catalog, else the call aborts without writing.
///
/// When `expected_fingerprint` is [`ExpectedFingerprint::Required`],
/// the plan body is re-derived against current state and its
/// fingerprint must match before any deletion runs; this is the
/// drift guard for the two-RPC control-plane path.
pub async fn execute_remove_from_plan(
    cfg: &Config,
    intake_id: i64,
    expected_fingerprint: ExpectedFingerprint<'_>,
) -> Result<RemovePaperOutcome> {
    let mut catalog = Catalog::open_with_backup(&cfg.papers_catalog_db(), &cfg.backup_dir())
        .context("open papers catalog")?;
    let intake = catalog
        .intake_by_id(intake_id)
        .context("look up intake")?
        .ok_or_else(|| CmdInputError::TargetDrifted {
            intake_id,
            detail: "The paper intake the plan was minted against is no longer in the catalog."
                .to_string(),
        })?;

    if let ExpectedFingerprint::Required(expected) = expected_fingerprint {
        let current = derive_remove_plan(cfg, &catalog, intake.clone()).await?;
        let actual = current.fingerprint();
        if actual != expected {
            return Err(CmdInputError::TargetDrifted {
                intake_id,
                detail: format!(
                    "The dry-run pinned fingerprint {expected}; the target now hashes to {actual}."
                ),
            }
            .into());
        }
    }

    let partition = PartitionIdx::new(intake_id);
    let paper_root_id: i64 = partition.root().get();
    let envelope_path = intake.stored_path.clone();
    let source_pdf_path = intake.source_pdf_path.clone();

    let deleted = catalog
        .delete_book_derived(intake_id, paper_root_id)
        .context("delete cascaded catalog rows")?;

    let mut corpus = Corpus::open(&cfg.papers_corpus_db()).context("open papers corpus")?;
    corpus
        .drop_partition(partition)
        .context("drop corpus partition")?;
    drop(corpus);

    if let Some(dim) = corpus_vector_dim(cfg)? {
        let store = ChunkStore::open(&cfg.papers_lancedb_dir(), dim)
            .await
            .context("open papers vector store")?;
        store
            .delete_partition(partition)
            .await
            .context("delete vector partition")?;
    }

    if let Some(path) = envelope_path.as_deref() {
        let p = std::path::Path::new(path);
        if p.exists() {
            std::fs::remove_file(p)
                .with_context(|| format!("remove envelope file {}", p.display()))?;
        }
    }

    if let Some(path) = source_pdf_path.as_deref() {
        let p = std::path::Path::new(path);
        if p.exists() {
            std::fs::remove_file(p)
                .with_context(|| format!("remove source PDF {}", p.display()))?;
        }
    }

    let existed = catalog
        .delete_intake(intake_id)
        .context("delete intake row")?;

    Ok(RemovePaperOutcome {
        intake_id,
        source_sha256: intake.source_sha256,
        catalog_deleted: deleted,
        intake_row_existed: existed,
    })
}

/// What the execute step would delete: returned by [`plan_remove`]
/// and consumed by the control-plane dry-run leg.
#[derive(Debug, Clone)]
pub struct RemovePaperPlan {
    pub intake: Intake,
    pub counts: ItemRemovalCounts,
    pub vector_rows: Option<usize>,
    pub corpus_nodes: u64,
    pub envelope_path: Option<String>,
    pub envelope_exists: bool,
    pub source_pdf_path: Option<String>,
    pub source_pdf_exists: bool,
}

impl RemovePaperPlan {
    /// Stable hex SHA-256 over the fields the operator confirmed in
    /// the dry-run output. Mirrors [`crate::cmd::remove::RemovePlan::fingerprint`]
    /// and additionally folds the source-PDF path/presence in, since
    /// `papers.remove` deletes that file too.
    pub fn fingerprint(&self) -> String {
        let mut h = Sha256::new();
        h.update(b"papers.remove\x00");
        h.update(self.intake.intake_id.to_be_bytes());
        h.update(b"\x00");
        h.update(self.intake.source_sha256.as_bytes());
        h.update(b"\x00");
        h.update(self.intake.status.as_str().as_bytes());
        h.update(b"\x00");
        h.update(self.corpus_nodes.to_be_bytes());
        h.update(b"\x00");
        match self.vector_rows {
            Some(n) => {
                h.update(b"S");
                h.update((n as u64).to_be_bytes());
            }
            None => h.update(b"N"),
        }
        h.update(b"\x00");
        h.update(self.envelope_path.as_deref().unwrap_or("").as_bytes());
        h.update(b"\x00");
        h.update([u8::from(self.envelope_exists)]);
        h.update(b"\x00");
        h.update(self.source_pdf_path.as_deref().unwrap_or("").as_bytes());
        h.update(b"\x00");
        h.update([u8::from(self.source_pdf_exists)]);
        h.update(b"\x00");
        for v in [
            self.counts.book_state,
            self.counts.node_publication_attrs,
            self.counts.node_overrides,
            self.counts.node_contributors,
            self.counts.node_categories,
            self.counts.node_reviews,
            self.counts.node_role_takeovers,
            self.counts.node_paper_audit,
            self.counts.toc_edits,
        ] {
            h.update(v.to_be_bytes());
        }
        format!("{:x}", h.finalize())
    }
}

/// Aggregate outcome of [`execute_remove_from_plan`].
#[derive(Debug, Clone, Default)]
pub struct RemovePaperOutcome {
    pub intake_id: i64,
    pub source_sha256: String,
    pub catalog_deleted: ItemRemovalCounts,
    pub intake_row_existed: bool,
}

fn resolve_intake(catalog: &Catalog, args: &RemovePaperArgs) -> Result<Intake> {
    if let Some(id) = args.intake_id {
        catalog
            .intake_by_id(id)
            .context("look up intake")?
            .ok_or_else(|| CmdInputError::UnknownIntake { intake_id: id }.into())
    } else {
        let sha = args.sha.as_deref().expect("checked by plan_remove");
        catalog
            .intake_by_sha(sha)
            .context("look up intake by sha")?
            .ok_or_else(|| {
                CmdInputError::UnknownSha {
                    sha: sha.to_string(),
                }
                .into()
            })
    }
}

/// Open `papers_corpus.db` read-only, or report it absent. An intake
/// row can exist before the corpus does, so a caller that only reads
/// treats a missing file as an empty corpus instead of creating one.
fn open_corpus_read_only(path: &std::path::Path) -> Result<Option<Corpus>> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(
        Corpus::open_read_only(path).context("open papers corpus")?,
    ))
}

fn corpus_vector_dim(cfg: &Config) -> Result<Option<usize>> {
    let Some(corpus) = open_corpus_read_only(&cfg.papers_corpus_db())? else {
        return Ok(None);
    };
    let dim = corpus
        .meta_get(bookrack_corpus::VECTOR_DIM_KEY)
        .context("read vector_dim stamp")?;
    Ok(dim.and_then(|s| s.parse::<usize>().ok()))
}

async fn count_vector_rows(
    store: &ChunkStore,
    partition: PartitionIdx,
) -> std::result::Result<usize, bookrack_vectors::VectorsError> {
    Ok(store.scan_partition(partition).await?.len())
}

fn read_corpus_node_count(cfg: &Config, paper_root_id: NodeId) -> Result<u64> {
    let Some(corpus) = open_corpus_read_only(&cfg.papers_corpus_db())? else {
        return Ok(0);
    };
    corpus
        .count_book_nodes(paper_root_id)
        .context("count corpus nodes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_catalog::NewIntake;
    use bookrack_core::{ItemKind, NodeType};
    use bookrack_corpus::NewNode;

    /// Seed a minimal paper through the library APIs the production
    /// paths use: intake row, envelope file on disk, corpus partition
    /// with a root node.
    fn seed_paper(cfg: &Config, catalog: &mut Catalog, corpus: &mut Corpus, sha: &str) -> i64 {
        let intake_id = catalog
            .register_intake(ItemKind::Paper, &NewIntake::new(sha).format("pdf"))
            .expect("register")
            .into_intake()
            .intake_id;
        let papers_dir = cfg.papers_dir();
        std::fs::create_dir_all(&papers_dir).expect("papers_dir");
        let envelope_path = papers_dir.join(bookrack_extract::envelope::envelope_filename(
            ItemKind::Paper,
            intake_id,
        ));
        std::fs::write(&envelope_path, b"{\"schema_version\":2}").expect("seed envelope");
        catalog
            .set_stored_path(
                ItemKind::Paper,
                intake_id,
                envelope_path.to_string_lossy().as_ref(),
            )
            .expect("stored_path");
        let partition = corpus.allocate_partition(intake_id).expect("partition");
        let root_node =
            NewNode::root(partition.book_root_id, NodeType::Work).title(format!("Paper {sha}"));
        corpus.insert_node(&root_node).expect("root node");
        intake_id
    }

    fn temp_cfg() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = Config::new(
            dir.path().to_path_buf(),
            "http://localhost:11434".to_string(),
        );
        (dir, cfg)
    }

    /// Names of the `.db` files sitting directly under `dir`, sorted.
    /// A read-only connection to an existing WAL database still
    /// creates `-shm` / `-wal` sidecars, so a byte-identical directory
    /// is deliberately not required.
    fn database_files(dir: &std::path::Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".db"))
            .collect();
        names.sort();
        names
    }

    /// Every entry directly under `dir`, sorted — catalog snapshots
    /// land as `.bak`, so the extension filter above would miss them.
    fn dir_entries(dir: &std::path::Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn user_version(path: &std::path::Path) -> i64 {
        let conn = bookrack_dbkit::open_production(path).expect("open for pragma read");
        conn.pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version")
    }

    fn set_user_version(path: &std::path::Path, version: i64) {
        let conn = bookrack_dbkit::open_production(path).expect("open for pragma write");
        conn.pragma_update(None, "user_version", version)
            .expect("write user_version");
    }

    /// One `node_paper_audit` row for the paper's own scope, filled
    /// with values the projection writer would produce.
    fn paper_audit_row(intake_id: i64) -> bookrack_catalog::NewNodePaperAudit {
        use bookrack_catalog::{FLAG_COLUMNS, GRADE_COLUMNS};
        let mut grades: [String; GRADE_COLUMNS.len()] = Default::default();
        for g in grades.iter_mut() {
            *g = "medium".to_string();
        }
        bookrack_catalog::NewNodePaperAudit {
            intake_id,
            scope: ItemKind::Paper.as_scope_str().to_string(),
            profile_name: "default".to_string(),
            verdict: "clean".to_string(),
            confidence: "medium".to_string(),
            csl_type: Some("article-journal".to_string()),
            audited_at: "2026-06-04T00:00:00Z".to_string(),
            extractor_version: "0.0.0-test".to_string(),
            grades,
            flags: [0; FLAG_COLUMNS.len()],
            pipeline_run_id: None,
            profile_fingerprint: None,
            profile_toggle_summary: None,
        }
    }

    fn plan_args(intake_id: i64) -> RemovePaperArgs {
        RemovePaperArgs {
            intake_id: Some(intake_id),
            sha: None,
            dry_run: true,
            yes: true,
        }
    }

    #[tokio::test]
    async fn plan_reports_counts_and_writes_nothing() {
        let (_tmp, cfg) = temp_cfg();
        let intake_id = {
            let mut catalog = Catalog::open(&cfg.papers_catalog_db()).expect("catalog");
            let mut corpus = Corpus::open(&cfg.papers_corpus_db()).expect("corpus");
            seed_paper(&cfg, &mut catalog, &mut corpus, "sha-dry")
        };

        let plan = plan_remove(&cfg, &plan_args(intake_id))
            .await
            .expect("plan succeeds");
        assert_eq!(plan.corpus_nodes, 1);

        let catalog = Catalog::open_read_only(&cfg.papers_catalog_db()).expect("reopen");
        assert!(
            catalog.intake_by_id(intake_id).expect("lookup").is_some(),
            "plan-only must not delete the intake row",
        );
        assert_eq!(
            database_files(cfg.data_dir()),
            vec![
                "papers_catalog.db".to_string(),
                "papers_corpus.db".to_string()
            ],
            "plan-only must not create a database the seed did not",
        );
        assert!(
            !cfg.papers_lancedb_dir().exists(),
            "plan-only must not materialize the vector store",
        );
        assert!(
            dir_entries(&cfg.backup_dir()).is_empty(),
            "plan-only must not write a catalog backup",
        );
    }

    /// A count that reaches the dry-run output but not the fingerprint
    /// is a table the drift check cannot see between two RPCs.
    #[tokio::test]
    async fn the_plan_fingerprint_covers_the_paper_audit_projection() {
        let (_tmp, cfg) = temp_cfg();
        let intake_id = {
            let mut catalog = Catalog::open(&cfg.papers_catalog_db()).expect("catalog");
            let mut corpus = Corpus::open(&cfg.papers_corpus_db()).expect("corpus");
            seed_paper(&cfg, &mut catalog, &mut corpus, "sha-fingerprint")
        };

        let before = plan_remove(&cfg, &plan_args(intake_id))
            .await
            .expect("plan without a projection row")
            .fingerprint();

        {
            let catalog = Catalog::open(&cfg.papers_catalog_db()).expect("catalog");
            catalog
                .upsert_node_paper_audit(&paper_audit_row(intake_id))
                .expect("projection row");
        }

        let after = plan_remove(&cfg, &plan_args(intake_id))
            .await
            .expect("plan with a projection row")
            .fingerprint();

        assert_ne!(
            before, after,
            "the fingerprint did not move when the audit projection gained a row",
        );
    }

    #[tokio::test]
    async fn plan_on_an_empty_data_root_creates_no_database() {
        let (tmp, cfg) = temp_cfg();

        let _err = plan_remove(&cfg, &plan_args(1))
            .await
            .expect_err("a data root with no catalog cannot resolve an intake");

        assert!(
            database_files(tmp.path()).is_empty(),
            "plan-only against an empty data root created {:?}",
            database_files(tmp.path()),
        );
    }

    #[tokio::test]
    async fn plan_does_not_materialize_the_vector_store() {
        let (_tmp, cfg) = temp_cfg();
        let intake_id = {
            let mut catalog = Catalog::open(&cfg.papers_catalog_db()).expect("catalog");
            let mut corpus = Corpus::open(&cfg.papers_corpus_db()).expect("corpus");
            let id = seed_paper(&cfg, &mut catalog, &mut corpus, "sha-no-lance");
            // A cluster that has been embedded once carries the stamp,
            // even after its lancedb directory is deleted.
            corpus
                .meta_set(bookrack_corpus::VECTOR_DIM_KEY, "8")
                .expect("stamp vector_dim");
            id
        };
        assert!(!cfg.papers_lancedb_dir().exists());

        let plan = plan_remove(&cfg, &plan_args(intake_id))
            .await
            .expect("plan succeeds without a vector store");

        assert!(
            !cfg.papers_lancedb_dir().exists(),
            "plan-only must not materialize the lancedb layout",
        );
        assert_eq!(
            plan.vector_rows, None,
            "a plan that found no vector store must report no row count",
        );
    }

    #[tokio::test]
    async fn plan_neither_migrates_nor_backs_up_the_catalog() {
        let (_tmp, cfg) = temp_cfg();
        let intake_id = {
            let mut catalog = Catalog::open(&cfg.papers_catalog_db()).expect("catalog");
            let mut corpus = Corpus::open(&cfg.papers_corpus_db()).expect("corpus");
            seed_paper(&cfg, &mut catalog, &mut corpus, "sha-stale")
        };
        // Present the catalog as one revision behind: a read-write open
        // would back it up and migrate it, a read-only open must not.
        let stale = user_version(&cfg.papers_catalog_db()) - 1;
        set_user_version(&cfg.papers_catalog_db(), stale);

        let planned = plan_remove(&cfg, &plan_args(intake_id)).await;

        assert!(
            dir_entries(&cfg.backup_dir()).is_empty(),
            "plan-only wrote a catalog backup: {:?}",
            dir_entries(&cfg.backup_dir()),
        );
        assert_eq!(
            user_version(&cfg.papers_catalog_db()),
            stale,
            "plan-only migrated the catalog",
        );
        assert!(
            planned.is_ok(),
            "a schema-conformant catalog must still plan: {:?}",
            planned.err(),
        );
    }
}
