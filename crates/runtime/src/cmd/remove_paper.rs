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
use eyre::{Context, ContextCompat, Result};
use sha2::{Digest, Sha256};

use crate::cmd::remove::ExpectedFingerprint;

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
pub async fn plan_remove(cfg: &Config, args: &RemovePaperArgs) -> Result<RemovePaperPlan> {
    if args.intake_id.is_none() && args.sha.is_none() {
        eyre::bail!("pass an intake id (positional) or --sha <hex>");
    }
    let catalog = Catalog::open_with_backup(&cfg.papers_catalog_db(), &cfg.backup_dir())
        .context("open papers catalog")?;
    let intake = resolve_intake(&catalog, args)?;
    drop(catalog);
    derive_remove_plan(cfg, intake).await
}

/// Compute the plan body for an already-resolved paper intake.
/// Shared by [`plan_remove`] and the drift-check inside
/// [`execute_remove_from_plan`] so the second derivation does not
/// rewrite the catalog backup that [`Catalog::open_with_backup`]
/// stamps on first open.
async fn derive_remove_plan(cfg: &Config, intake: Intake) -> Result<RemovePaperPlan> {
    let intake_id = intake.intake_id;
    let partition = PartitionIdx::new(intake_id);
    let paper_root_node_id: NodeId = partition.root();
    let paper_root_id = paper_root_node_id.get();

    let counts = {
        let catalog = Catalog::open(&cfg.papers_catalog_db()).context("open papers catalog")?;
        catalog
            .count_book_derived(intake_id, paper_root_id)
            .context("count catalog rows")?
    };

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

    let vector_dim = corpus_vector_dim(cfg)?;
    let vector_rows = if let Some(dim) = vector_dim {
        let store = ChunkStore::open(&cfg.papers_lancedb_dir(), dim)
            .await
            .context("open papers vector store")?;
        count_vector_rows(&store, partition).await.ok()
    } else {
        None
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
        .with_context(|| {
            format!(
                "plan referenced paper intake {intake_id}, which no longer exists in the catalog"
            )
        })?;

    if let ExpectedFingerprint::Required(expected) = expected_fingerprint {
        let current = derive_remove_plan(cfg, intake.clone()).await?;
        let actual = current.fingerprint();
        if actual != expected {
            eyre::bail!(
                "papers.remove plan stale: target state for paper intake {intake_id} drifted \
                 since dry-run (expected fingerprint {expected}, current {actual}). Re-run \
                 dry_run=true and confirm again."
            );
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
            .with_context(|| format!("no paper intake registered for id {id}"))
    } else {
        let sha = args.sha.as_deref().expect("checked by plan_remove");
        catalog
            .intake_by_sha(sha)
            .context("look up intake by sha")?
            .with_context(|| format!("no paper intake registered for source_sha256 {sha}"))
    }
}

fn corpus_vector_dim(cfg: &Config) -> Result<Option<usize>> {
    let corpus = Corpus::open(&cfg.papers_corpus_db()).context("open papers corpus")?;
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
    let corpus = Corpus::open(&cfg.papers_corpus_db()).context("open papers corpus")?;
    corpus
        .count_book_nodes(paper_root_id)
        .context("count corpus nodes")
}
