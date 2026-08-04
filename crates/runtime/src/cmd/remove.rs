// SPDX-License-Identifier: Apache-2.0

//! `bookrack remove` — drop one book from every store.
//!
//! Resolves the target intake by id or by `--sha`, fans out a partition
//! delete to the vector store and the corpus, drops the per-book rows
//! from the cascaded `catalog.db` tables, removes the opaque-store
//! envelope file, and last deletes the `intake` row itself. Keeping the
//! `intake` row until every other step has succeeded makes a mid-run
//! failure resumable by feeding the same `intake_id` to a second call.
//!
//! `metadata_audit` and `book_pipeline_audit` rows are preserved by
//! design: both tables are denormalized to outlive their book.
//!
//! The vector-store delete leaves tombstones in LanceDB. Compaction is
//! left to the existing `optimize` path that the ingest and reembed
//! flows run on their next pass — `remove` does not run it inline.
//!
//! [`plan_remove`] computes the dry-run plan and
//! [`execute_remove_from_plan`] runs the destructive step against the
//! pinned intake id. Strict: the intake must still resolve when the
//! execute leg runs, else the call aborts without writing.

use bookrack_catalog::{Catalog, Intake, ItemRemovalCounts};
use bookrack_config::Config;
use bookrack_core::{NodeId, PartitionIdx};
use bookrack_corpus::Corpus;
use bookrack_vectors::ChunkStore;
use eyre::{Context, Result};
use sha2::{Digest, Sha256};

use crate::cmd::input_error::CmdInputError;

/// What produces the layer `remove` needs before it can resolve
/// anything. Shared by the plan leg's guard and its wording.
const INGEST_FIRST: &str = "Ingest a book into this library first.";

/// Inputs `Cli` collects for a `bookrack remove` invocation.
pub struct RemoveArgs {
    /// Positional intake id. `None` means the caller passed `--sha`.
    pub intake_id: Option<i64>,
    /// `--sha <hex>` alternative to the positional id.
    pub sha: Option<String>,
    /// Print the plan and exit without writing.
    pub dry_run: bool,
    /// Skip the destructive-action confirmation prompt.
    pub yes: bool,
}

/// Plan a remove without writing: resolve the intake, count
/// everything the execute step would delete, and report whether the
/// envelope file is on disk. Consumed by the control-plane handler's
/// dry-run leg.
///
/// Every store is opened read-only, so the plan neither creates a
/// database nor migrates one nor materializes the lancedb layout. A
/// missing catalog is an error — there is no intake to resolve; a
/// missing corpus or vector store is reported as empty.
pub async fn plan_remove(cfg: &Config, args: &RemoveArgs) -> Result<RemovePlan> {
    if args.intake_id.is_none() && args.sha.is_none() {
        eyre::bail!("pass an intake id (positional) or --sha <hex>");
    }
    // Only the absent file is caller input: a library nobody has
    // ingested into has no intake to name. Every other way the open
    // can fail is a fault and keeps its own reporting.
    if !cfg.catalog_db().exists() {
        return Err(CmdInputError::NotIngested {
            what: "catalog",
            hint: INGEST_FIRST,
        }
        .into());
    }
    let catalog = Catalog::open_read_only(&cfg.catalog_db()).context("open catalog")?;
    let intake = resolve_intake(&catalog, args)?;
    derive_remove_plan(cfg, &catalog, intake).await
}

/// Compute the plan body for an already-resolved intake against the
/// caller's catalog handle. Both legs pass a handle they already
/// hold, so deriving a plan never opens the catalog a second time —
/// which is what lets the execute leg re-derive the plan for its
/// drift check without churning a second backup file per RPC.
async fn derive_remove_plan(cfg: &Config, catalog: &Catalog, intake: Intake) -> Result<RemovePlan> {
    let intake_id = intake.intake_id;
    let partition = PartitionIdx::new(intake_id);
    let book_root_node_id: NodeId = partition.root();
    let book_root_id = book_root_node_id.get();

    let counts = catalog
        .count_book_derived(intake_id, book_root_id)
        .context("count catalog rows")?;

    let envelope_path = intake.stored_path.clone();
    let envelope_exists = envelope_path
        .as_deref()
        .map(|p| std::path::Path::new(p).exists())
        .unwrap_or(false);

    // `try_open` reads the dimension off the table's own schema and
    // reports a store that is not on disk as absent, so the plan
    // needs neither the corpus stamp nor a creating open.
    let vector_rows = match ChunkStore::try_open(&cfg.lancedb_dir())
        .await
        .context("open vector store")?
    {
        Some(store) => count_vector_rows(&store, partition).await.ok(),
        None => None,
    };

    let corpus_nodes = read_corpus_node_count(cfg, book_root_node_id)?;

    Ok(RemovePlan {
        intake,
        counts,
        vector_rows,
        corpus_nodes,
        envelope_path,
        envelope_exists,
    })
}

/// Drift-check selector for [`execute_remove_from_plan`].
pub enum ExpectedFingerprint<'a> {
    /// In-process callers «plan → execute in the same call frame»
    /// have no drift window and skip the check.
    None,
    /// Two-step control-plane callers: the dry-run leg pins the
    /// plan's fingerprint and the execute leg verifies the current
    /// state still hashes to the same value before any deletion.
    Required(&'a str),
}

/// Execute the remove sequence for an intake pinned by an earlier
/// [`plan_remove`] call. Strict: the intake must still resolve in
/// the catalog (even with the cascade rows already cleaned, e.g. a
/// resumed removal), else the call aborts without writing.
///
/// When `expected_fingerprint` is [`ExpectedFingerprint::Required`],
/// the plan body is re-derived against current state and its
/// fingerprint must match before any deletion runs; this is the
/// drift guard for the two-RPC control-plane path.
pub async fn execute_remove_from_plan(
    cfg: &Config,
    intake_id: i64,
    expected_fingerprint: ExpectedFingerprint<'_>,
) -> Result<RemoveOutcome> {
    let mut catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let intake = catalog
        .intake_by_id(intake_id)
        .context("look up intake")?
        .ok_or_else(|| CmdInputError::TargetDrifted {
            intake_id,
            detail: "The intake the plan was minted against is no longer in the catalog."
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
    let book_root_id: i64 = partition.root().get();
    let envelope_path = intake.stored_path.clone();

    // Order:
    //   1. catalog derived rows (transactional)
    //   2. corpus partition drop (idempotent)
    //   3. vectors partition delete (tombstones; compaction deferred)
    //   4. envelope file
    //   5. intake row (the resume anchor — last)
    let deleted = catalog
        .delete_book_derived(intake_id, book_root_id)
        .context("delete cascaded catalog rows")?;

    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    corpus
        .drop_partition(partition)
        .context("drop corpus partition")?;
    drop(corpus);

    if let Some(dim) = corpus_vector_dim(cfg)? {
        let store = ChunkStore::open(&cfg.lancedb_dir(), dim)
            .await
            .context("open vector store")?;
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

    let existed = catalog
        .delete_intake(intake_id)
        .context("delete intake row")?;

    Ok(RemoveOutcome {
        intake_id,
        source_sha256: intake.source_sha256,
        catalog_deleted: deleted,
        intake_row_existed: existed,
    })
}

/// What the execute step would delete: returned by [`plan_remove`]
/// and consumed by the control-plane dry-run leg.
#[derive(Debug, Clone)]
pub struct RemovePlan {
    pub intake: Intake,
    pub counts: ItemRemovalCounts,
    pub vector_rows: Option<usize>,
    pub corpus_nodes: u64,
    pub envelope_path: Option<String>,
    pub envelope_exists: bool,
}

impl RemovePlan {
    /// Stable hex SHA-256 over the fields the operator confirmed in
    /// the dry-run output. The two-step control-plane path stores
    /// this with the registered plan and the execute leg recomputes
    /// it against current state; a mismatch means the catalog,
    /// corpus, vector store, or envelope file drifted between the
    /// two RPCs and the call must abort instead of deleting under
    /// an unconfirmed target.
    pub fn fingerprint(&self) -> String {
        let mut h = Sha256::new();
        h.update(b"remove\x00");
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

/// Aggregate outcome of [`execute_remove_from_plan`]: what actually
/// got deleted.
#[derive(Debug, Clone, Default)]
pub struct RemoveOutcome {
    pub intake_id: i64,
    pub source_sha256: String,
    pub catalog_deleted: ItemRemovalCounts,
    /// `true` if the `intake` row was deleted by this call;
    /// `false` if a prior partial removal had already cleaned it.
    pub intake_row_existed: bool,
}

fn resolve_intake(catalog: &Catalog, args: &RemoveArgs) -> Result<Intake> {
    if let Some(id) = args.intake_id {
        catalog
            .intake_by_id(id)
            .context("look up intake")?
            .ok_or_else(|| CmdInputError::UnknownIntake { intake_id: id }.into())
    } else {
        let sha = args.sha.as_deref().expect("checked by run()");
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

/// Open `corpus.db` read-only, or report it absent. An intake row can
/// exist before the corpus does, so a caller that only reads treats a
/// missing file as an empty corpus instead of creating one.
fn open_corpus_read_only(path: &std::path::Path) -> Result<Option<Corpus>> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(Corpus::open_read_only(path).context("open corpus")?))
}

/// Read the persisted vector dimension from `corpus.db`. `None` means the
/// library has never been ingested into — there is no vector store on disk.
fn corpus_vector_dim(cfg: &Config) -> Result<Option<usize>> {
    let Some(corpus) = open_corpus_read_only(&cfg.corpus_db())? else {
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

fn read_corpus_node_count(cfg: &Config, book_root_id: NodeId) -> Result<u64> {
    let Some(corpus) = open_corpus_read_only(&cfg.corpus_db())? else {
        return Ok(0);
    };
    corpus
        .count_book_nodes(book_root_id)
        .context("count corpus nodes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_catalog::NewIntake;
    use bookrack_core::{Explain, ItemKind, NodeType};
    use bookrack_corpus::NewNode;

    /// Seed a minimal book directly through the library APIs the
    /// production paths use: intake row, envelope file on disk, corpus
    /// partition with a root node. No embedded vectors, so the vector
    /// store is never opened.
    fn seed_book(
        cfg: &Config,
        catalog: &mut Catalog,
        corpus: &mut Corpus,
        sha: &str,
    ) -> (i64, std::path::PathBuf) {
        let intake_id = catalog
            .register_intake(ItemKind::Book, &NewIntake::new(sha).format("epub"))
            .expect("register")
            .into_intake()
            .intake_id;
        let books_dir = cfg.books_dir();
        std::fs::create_dir_all(&books_dir).expect("books_dir");
        let envelope_path = books_dir.join(bookrack_extract::envelope::envelope_filename(
            ItemKind::Book,
            intake_id,
        ));
        std::fs::write(&envelope_path, b"{\"schema_version\":2}").expect("seed envelope");
        catalog
            .set_stored_path(
                ItemKind::Book,
                intake_id,
                envelope_path.to_string_lossy().as_ref(),
            )
            .expect("stored_path");
        let partition = corpus.allocate_partition(intake_id).expect("partition");
        let root_node =
            NewNode::root(partition.book_root_id, NodeType::Work).title(format!("Book {sha}"));
        corpus.insert_node(&root_node).expect("root node");
        (intake_id, envelope_path)
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
    /// The contract the dry-run leg owes is about databases and the
    /// lancedb directory: a read-only connection to an existing WAL
    /// database still creates `-shm` / `-wal` sidecars, so a
    /// byte-identical directory is deliberately not required.
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

    /// Every entry directly under `dir`, sorted. Used where the
    /// contract is "this directory gains nothing", regardless of the
    /// file's extension — catalog snapshots land as `.bak`.
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

    /// Drive the plan+execute pair the control-plane handler runs,
    /// without the drift check. Mirrors what the dry-run / execute
    /// RPC sequence does end-to-end for an in-process test.
    async fn plan_and_execute(cfg: &Config, args: &RemoveArgs) -> Result<RemoveOutcome> {
        let plan = plan_remove(cfg, args).await?;
        execute_remove_from_plan(cfg, plan.intake.intake_id, ExpectedFingerprint::None).await
    }

    #[tokio::test]
    async fn plan_reports_counts_and_writes_nothing() {
        let (_tmp, cfg) = temp_cfg();
        let intake_id = {
            let mut catalog = Catalog::open(&cfg.catalog_db()).expect("catalog");
            let mut corpus = Corpus::open(&cfg.corpus_db()).expect("corpus");
            seed_book(&cfg, &mut catalog, &mut corpus, "sha-dry").0
        };

        let _plan = plan_remove(
            &cfg,
            &RemoveArgs {
                intake_id: Some(intake_id),
                sha: None,
                dry_run: true,
                yes: true,
            },
        )
        .await
        .expect("plan succeeds");

        let catalog = Catalog::open_read_only(&cfg.catalog_db()).expect("reopen");
        assert!(
            catalog.intake_by_id(intake_id).expect("lookup").is_some(),
            "plan-only must not delete the intake row",
        );
        assert_eq!(
            database_files(cfg.data_dir()),
            vec!["catalog.db".to_string(), "corpus.db".to_string()],
            "plan-only must not create a database the seed did not",
        );
        assert!(
            !cfg.lancedb_dir().exists(),
            "plan-only must not materialize the vector store",
        );
        assert!(
            dir_entries(&cfg.backup_dir()).is_empty(),
            "plan-only must not write a catalog backup",
        );
    }

    #[tokio::test]
    async fn plan_on_an_empty_data_root_creates_no_database() {
        let (tmp, cfg) = temp_cfg();

        let _err = plan_remove(
            &cfg,
            &RemoveArgs {
                intake_id: Some(1),
                sha: None,
                dry_run: true,
                yes: true,
            },
        )
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
            let mut catalog = Catalog::open(&cfg.catalog_db()).expect("catalog");
            let mut corpus = Corpus::open(&cfg.corpus_db()).expect("corpus");
            let id = seed_book(&cfg, &mut catalog, &mut corpus, "sha-no-lance").0;
            // A library that has been embedded once carries the stamp,
            // even after its lancedb directory is deleted.
            corpus
                .meta_set(bookrack_corpus::VECTOR_DIM_KEY, "8")
                .expect("stamp vector_dim");
            id
        };
        assert!(!cfg.lancedb_dir().exists());

        let plan = plan_remove(
            &cfg,
            &RemoveArgs {
                intake_id: Some(intake_id),
                sha: None,
                dry_run: true,
                yes: true,
            },
        )
        .await
        .expect("plan succeeds without a vector store");

        assert!(
            !cfg.lancedb_dir().exists(),
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
            let mut catalog = Catalog::open(&cfg.catalog_db()).expect("catalog");
            let mut corpus = Corpus::open(&cfg.corpus_db()).expect("corpus");
            seed_book(&cfg, &mut catalog, &mut corpus, "sha-stale").0
        };
        // Present the catalog as one revision behind: a read-write open
        // would back it up and migrate it, a read-only open must not.
        let stale = user_version(&cfg.catalog_db()) - 1;
        set_user_version(&cfg.catalog_db(), stale);

        let planned = plan_remove(
            &cfg,
            &RemoveArgs {
                intake_id: Some(intake_id),
                sha: None,
                dry_run: true,
                yes: true,
            },
        )
        .await;

        assert!(
            dir_entries(&cfg.backup_dir()).is_empty(),
            "plan-only wrote a catalog backup: {:?}",
            dir_entries(&cfg.backup_dir()),
        );
        assert_eq!(
            user_version(&cfg.catalog_db()),
            stale,
            "plan-only migrated the catalog",
        );
        assert!(
            planned.is_ok(),
            "a schema-conformant catalog must still plan: {:?}",
            planned.err(),
        );
    }

    #[tokio::test]
    async fn remove_clears_intake_corpus_and_envelope() {
        let (_tmp, cfg) = temp_cfg();
        let (intake_id, envelope_path) = {
            let mut catalog = Catalog::open(&cfg.catalog_db()).expect("catalog");
            let mut corpus = Corpus::open(&cfg.corpus_db()).expect("corpus");
            seed_book(&cfg, &mut catalog, &mut corpus, "sha-rm")
        };
        assert!(envelope_path.exists());

        plan_and_execute(
            &cfg,
            &RemoveArgs {
                intake_id: Some(intake_id),
                sha: None,
                dry_run: false,
                yes: true,
            },
        )
        .await
        .expect("remove succeeds");

        // Intake row gone.
        let catalog = Catalog::open_read_only(&cfg.catalog_db()).expect("reopen catalog");
        assert!(catalog.intake_by_id(intake_id).expect("lookup").is_none(),);
        // Corpus partition gone.
        let corpus = Corpus::open(&cfg.corpus_db()).expect("reopen corpus");
        assert!(
            corpus
                .partition_for_intake(intake_id)
                .expect("lookup")
                .is_none(),
        );
        // Envelope file gone.
        assert!(!envelope_path.exists());
    }

    #[tokio::test]
    async fn remove_is_idempotent_when_rerun_after_a_partial_failure() {
        // Simulate the resumption case: the first remove succeeded
        // catalog-cascade but crashed before the intake row delete.
        // A second run with the same intake id must complete cleanly.
        let (_tmp, cfg) = temp_cfg();
        let intake_id = {
            let mut catalog = Catalog::open(&cfg.catalog_db()).expect("catalog");
            let mut corpus = Corpus::open(&cfg.corpus_db()).expect("corpus");
            let (id, _envelope) = seed_book(&cfg, &mut catalog, &mut corpus, "sha-rerun");
            // Drop everything but the intake row to mimic the partial state.
            let book_root_id = PartitionIdx::new(id).root().get();
            catalog
                .delete_book_derived(id, book_root_id)
                .expect("partial cascade");
            corpus
                .drop_partition(PartitionIdx::new(id))
                .expect("partial corpus drop");
            id
        };

        plan_and_execute(
            &cfg,
            &RemoveArgs {
                intake_id: Some(intake_id),
                sha: None,
                dry_run: false,
                yes: true,
            },
        )
        .await
        .expect("resumed remove succeeds");

        let catalog = Catalog::open_read_only(&cfg.catalog_db()).expect("reopen");
        assert!(catalog.intake_by_id(intake_id).expect("lookup").is_none(),);
    }

    #[tokio::test]
    async fn remove_resolves_by_sha_when_only_sha_supplied() {
        let (_tmp, cfg) = temp_cfg();
        let intake_id = {
            let mut catalog = Catalog::open(&cfg.catalog_db()).expect("catalog");
            let mut corpus = Corpus::open(&cfg.corpus_db()).expect("corpus");
            seed_book(&cfg, &mut catalog, &mut corpus, "sha-by-hash").0
        };
        plan_and_execute(
            &cfg,
            &RemoveArgs {
                intake_id: None,
                sha: Some("sha-by-hash".to_string()),
                dry_run: false,
                yes: true,
            },
        )
        .await
        .expect("remove by sha succeeds");
        let catalog = Catalog::open_read_only(&cfg.catalog_db()).expect("reopen");
        assert!(catalog.intake_by_id(intake_id).expect("lookup").is_none(),);
    }

    #[tokio::test]
    async fn plan_errors_when_neither_id_nor_sha_is_supplied() {
        let (_tmp, cfg) = temp_cfg();
        let err = plan_remove(
            &cfg,
            &RemoveArgs {
                intake_id: None,
                sha: None,
                dry_run: true,
                yes: true,
            },
        )
        .await
        .expect_err("missing selector must error");
        assert!(format!("{err:#}").contains("intake id"));
    }

    #[tokio::test]
    async fn plan_errors_on_unknown_intake_id() {
        let (_tmp, cfg) = temp_cfg();
        // Open and close to materialize empty catalog + corpus.
        {
            Catalog::open(&cfg.catalog_db()).expect("init catalog");
            Corpus::open(&cfg.corpus_db()).expect("init corpus");
        }
        let err = plan_remove(
            &cfg,
            &RemoveArgs {
                intake_id: Some(999),
                sha: None,
                dry_run: true,
                yes: true,
            },
        )
        .await
        .expect_err("unknown id must error");
        // The type is what the control plane classifies on; the string
        // is only what the operator reads. Asserting the message alone
        // would stay green if a later `wrap_err` stripped the type and
        // the refusal reached the wire as a fault again.
        assert!(
            matches!(
                err.root_cause().downcast_ref::<CmdInputError>(),
                Some(CmdInputError::UnknownIntake { intake_id: 999 })
            ),
            "unknown id must reach the caller as caller input: {err:#}"
        );
    }

    #[tokio::test]
    async fn plan_on_a_library_with_no_catalog_reports_the_missing_layer() {
        let (_tmp, cfg) = temp_cfg();
        let err = plan_remove(
            &cfg,
            &RemoveArgs {
                intake_id: Some(1),
                sha: None,
                dry_run: true,
                yes: true,
            },
        )
        .await
        .expect_err("a data root with no catalog cannot resolve an intake");
        assert!(
            matches!(
                err.root_cause().downcast_ref::<CmdInputError>(),
                Some(CmdInputError::NotIngested {
                    what: "catalog",
                    ..
                })
            ),
            "an un-ingested library is caller input, not a fault: {err:#}"
        );
    }

    /// The guard above tests `!exists()` and nothing else. A catalog
    /// that is on disk but cannot be opened is a fault, and must stay
    /// one — without this, widening the guard to `open(..).is_err()`
    /// would turn a real IO failure into "go ingest a book".
    #[tokio::test]
    async fn a_catalog_that_exists_but_will_not_open_stays_a_fault() {
        let (_tmp, cfg) = temp_cfg();
        std::fs::write(cfg.catalog_db(), b"not a database at all").expect("seed junk");

        let err = plan_remove(
            &cfg,
            &RemoveArgs {
                intake_id: Some(1),
                sha: None,
                dry_run: true,
                yes: true,
            },
        )
        .await
        .expect_err("an unreadable catalog must error");
        assert!(
            err.root_cause().downcast_ref::<CmdInputError>().is_none(),
            "an unopenable catalog is not caller input: {err:#}"
        );
    }

    #[tokio::test]
    async fn fingerprint_is_stable_for_unchanged_state() {
        let (_tmp, cfg) = temp_cfg();
        let intake_id = {
            let mut catalog = Catalog::open(&cfg.catalog_db()).expect("catalog");
            let mut corpus = Corpus::open(&cfg.corpus_db()).expect("corpus");
            seed_book(&cfg, &mut catalog, &mut corpus, "sha-fp").0
        };
        let args = RemoveArgs {
            intake_id: Some(intake_id),
            sha: None,
            dry_run: true,
            yes: true,
        };
        let a = plan_remove(&cfg, &args).await.expect("plan a");
        let b = plan_remove(&cfg, &args).await.expect("plan b");
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[tokio::test]
    async fn execute_aborts_when_target_state_drifts_after_plan() {
        let (_tmp, cfg) = temp_cfg();
        let (intake_id, _envelope) = {
            let mut catalog = Catalog::open(&cfg.catalog_db()).expect("catalog");
            let mut corpus = Corpus::open(&cfg.corpus_db()).expect("corpus");
            seed_book(&cfg, &mut catalog, &mut corpus, "sha-drift")
        };
        let plan = plan_remove(
            &cfg,
            &RemoveArgs {
                intake_id: Some(intake_id),
                sha: None,
                dry_run: true,
                yes: true,
            },
        )
        .await
        .expect("plan");
        let pinned = plan.fingerprint();
        // Mutate the catalog cascade out from under the plan: drop
        // the derived rows so corpus_nodes shrinks. The execute leg
        // must notice and refuse to delete.
        {
            let mut catalog = Catalog::open(&cfg.catalog_db()).expect("reopen catalog");
            let book_root_id = PartitionIdx::new(intake_id).root().get();
            catalog
                .delete_book_derived(intake_id, book_root_id)
                .expect("drift cascade");
            let mut corpus = Corpus::open(&cfg.corpus_db()).expect("reopen corpus");
            corpus
                .drop_partition(PartitionIdx::new(intake_id))
                .expect("drift corpus");
        }
        let err = execute_remove_from_plan(&cfg, intake_id, ExpectedFingerprint::Required(&pinned))
            .await
            .expect_err("drift must be rejected");
        let drifted = err.root_cause().downcast_ref::<CmdInputError>();
        assert!(
            matches!(drifted, Some(CmdInputError::TargetDrifted { .. })),
            "expected a typed drift refusal, got: {err:#}",
        );
        assert!(
            drifted
                .map(|e| e.explain())
                .and_then(|p| p.data.detail)
                .is_some_and(|d| d.contains(&pinned)),
            "the fingerprint the operator confirmed is the evidence: {err:#}",
        );
        // Without the guard, the same call proceeds to clean up the
        // remaining intake row idempotently.
        let outcome = execute_remove_from_plan(&cfg, intake_id, ExpectedFingerprint::None)
            .await
            .expect("unguarded execute");
        assert_eq!(outcome.intake_id, intake_id);
    }
}
