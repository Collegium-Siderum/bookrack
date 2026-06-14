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

use anyhow::{Context, Result};
use bookrack_catalog::{Catalog, Intake, ItemRemovalCounts};
use bookrack_config::Config;
use bookrack_core::{NodeId, PartitionIdx};
use bookrack_corpus::Corpus;
use bookrack_vectors::ChunkStore;

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

pub async fn run(cfg: &Config, args: RemovePaperArgs) -> Result<()> {
    if args.intake_id.is_none() && args.sha.is_none() {
        anyhow::bail!("pass an intake id (positional) or --sha <hex>");
    }
    let mut catalog = Catalog::open_with_backup(&cfg.papers_catalog_db(), &cfg.backup_dir())
        .context("open papers catalog")?;
    let intake = resolve_intake(&catalog, &args)?;
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

    print_plan(
        &intake,
        &counts,
        vector_rows,
        corpus_nodes,
        envelope_path.as_deref(),
        envelope_exists,
        source_pdf_path.as_deref(),
        source_pdf_exists,
    );

    if args.dry_run {
        return Ok(());
    }

    if !args.yes && !confirm()? {
        println!("aborted; no changes written");
        return Ok(());
    }

    let deleted = catalog
        .delete_book_derived(intake_id, paper_root_id)
        .context("delete cascaded catalog rows")?;

    let mut corpus = Corpus::open(&cfg.papers_corpus_db()).context("open papers corpus")?;
    corpus
        .drop_partition(partition)
        .context("drop corpus partition")?;
    drop(corpus);

    if let Some(dim) = vector_dim {
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

    println!(
        "removed: intake_id={intake_id}, source_sha256={}",
        intake.source_sha256
    );
    println!(
        "  catalog rows: {} (item_state={}, publication_attrs={}, overrides={}, \
         contributors={}, categories={}, reviews={}, role_takeovers={}, toc_edits={})",
        deleted.total(),
        deleted.book_state,
        deleted.node_publication_attrs,
        deleted.node_overrides,
        deleted.node_contributors,
        deleted.node_categories,
        deleted.node_reviews,
        deleted.node_role_takeovers,
        deleted.toc_edits,
    );
    if !existed {
        println!(
            "  note: intake row was already absent — likely a resumed removal cleaned the rest."
        );
    }
    println!(
        "  audit rows preserved (metadata_audit, item_pipeline_audit). \
         Vector tombstones will compact on the next glean's optimize pass."
    );
    Ok(())
}

fn resolve_intake(catalog: &Catalog, args: &RemovePaperArgs) -> Result<Intake> {
    if let Some(id) = args.intake_id {
        catalog
            .intake_by_id(id)
            .context("look up intake")?
            .with_context(|| format!("no paper intake registered for id {id}"))
    } else {
        let sha = args.sha.as_deref().expect("checked by run()");
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

#[allow(clippy::too_many_arguments)]
fn print_plan(
    intake: &Intake,
    counts: &ItemRemovalCounts,
    vector_rows: Option<usize>,
    corpus_nodes: u64,
    envelope_path: Option<&str>,
    envelope_exists: bool,
    source_pdf_path: Option<&str>,
    source_pdf_exists: bool,
) {
    println!(
        "remove plan for paper intake {} (source_sha256={}, status={}):",
        intake.intake_id,
        intake.source_sha256,
        intake.status.as_str(),
    );
    println!("  corpus nodes:    {corpus_nodes}");
    match vector_rows {
        Some(n) => println!("  vector rows:     {n}"),
        None => println!("  vector rows:     n/a (no embed stamp)"),
    }
    println!(
        "  catalog rows:    {} (item_state={}, publication_attrs={}, overrides={}, \
         contributors={}, categories={}, reviews={}, role_takeovers={}, toc_edits={})",
        counts.total(),
        counts.book_state,
        counts.node_publication_attrs,
        counts.node_overrides,
        counts.node_contributors,
        counts.node_categories,
        counts.node_reviews,
        counts.node_role_takeovers,
        counts.toc_edits,
    );
    match envelope_path {
        Some(p) if envelope_exists => println!("  envelope file:   {p}"),
        Some(p) => println!("  envelope file:   {p} (missing on disk; will be skipped)"),
        None => println!("  envelope file:   (none recorded)"),
    }
    match source_pdf_path {
        Some(p) if source_pdf_exists => println!("  source PDF:      {p}"),
        Some(p) => println!("  source PDF:      {p} (missing on disk; will be skipped)"),
        None => println!("  source PDF:      (none recorded)"),
    }
    println!("  audit trail:     metadata_audit and item_pipeline_audit rows are preserved.");
}

fn confirm() -> Result<bool> {
    use std::io::{Write, stdin, stdout};
    let prompt = "About to delete this paper from every store. This is\n\
                  irreversible (vector tombstones are not recoverable).\n\
                  Audit rows are preserved. Type 'yes' to continue: ";
    print!("{prompt}");
    stdout().flush().context("flush stdout")?;
    let mut buf = String::new();
    stdin().read_line(&mut buf).context("read confirmation")?;
    Ok(buf.trim().eq_ignore_ascii_case("yes"))
}
