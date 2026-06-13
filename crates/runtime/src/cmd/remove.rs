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

use anyhow::{Context, Result};
use bookrack_catalog::{Catalog, Intake, ItemRemovalCounts};
use bookrack_config::Config;
use bookrack_core::{NodeId, PartitionIdx};
use bookrack_corpus::Corpus;
use bookrack_vectors::ChunkStore;

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

pub async fn run(cfg: &Config, args: RemoveArgs) -> Result<()> {
    if args.intake_id.is_none() && args.sha.is_none() {
        anyhow::bail!("pass an intake id (positional) or --sha <hex>");
    }
    let mut catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let intake = resolve_intake(&catalog, &args)?;
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

    let vector_dim = corpus_vector_dim(cfg)?;
    let vector_rows = if let Some(dim) = vector_dim {
        let store = ChunkStore::open(&cfg.lancedb_dir(), dim)
            .await
            .context("open vector store")?;
        count_vector_rows(&store, partition).await.ok()
    } else {
        None
    };

    let corpus_nodes = read_corpus_node_count(cfg, book_root_node_id)?;

    print_plan(
        &intake,
        &counts,
        vector_rows,
        corpus_nodes,
        envelope_path.as_deref(),
        envelope_exists,
    );

    if args.dry_run {
        return Ok(());
    }

    if !args.yes && !confirm()? {
        println!("aborted; no changes written");
        return Ok(());
    }

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

    if let Some(dim) = vector_dim {
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

    println!(
        "removed: intake_id={intake_id}, source_sha256={}",
        intake.source_sha256
    );
    println!(
        "  catalog rows: {} (book_state={}, publication_attrs={}, overrides={}, \
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
        "  audit rows preserved (metadata_audit, book_pipeline_audit). \
         Vector tombstones will compact on the next ingest's optimize pass."
    );
    Ok(())
}

fn resolve_intake(catalog: &Catalog, args: &RemoveArgs) -> Result<Intake> {
    if let Some(id) = args.intake_id {
        catalog
            .intake_by_id(id)
            .context("look up intake")?
            .with_context(|| format!("no intake registered for book {id}"))
    } else {
        let sha = args.sha.as_deref().expect("checked by run()");
        catalog
            .intake_by_sha(sha)
            .context("look up intake by sha")?
            .with_context(|| format!("no intake registered for source_sha256 {sha}"))
    }
}

/// Read the persisted vector dimension from `corpus.db`. `None` means the
/// library has never been ingested into — there is no vector store on disk.
fn corpus_vector_dim(cfg: &Config) -> Result<Option<usize>> {
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
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
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    corpus
        .count_book_nodes(book_root_id)
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
) {
    println!(
        "remove plan for intake {} (source_sha256={}, status={}):",
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
        "  catalog rows:    {} (book_state={}, publication_attrs={}, overrides={}, \
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
    println!("  audit trail:     metadata_audit and book_pipeline_audit rows are preserved.");
}

/// Read the destructive-action confirmation. The literal `yes`
/// (case-insensitive, trimmed) passes; anything else aborts.
fn confirm() -> Result<bool> {
    use std::io::{Write, stdin, stdout};
    let prompt = "About to delete this book from every store. This is\n\
                  irreversible (vector tombstones are not recoverable).\n\
                  Audit rows are preserved. Type 'yes' to continue: ";
    print!("{prompt}");
    stdout().flush().context("flush stdout")?;
    let mut buf = String::new();
    stdin().read_line(&mut buf).context("read confirmation")?;
    Ok(buf.trim().eq_ignore_ascii_case("yes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_catalog::NewIntake;
    use bookrack_core::{ItemKind, NodeType};
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
        let envelope_path =
            books_dir.join(bookrack_extract::envelope::envelope_filename(intake_id));
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

    #[tokio::test]
    async fn dry_run_reports_counts_and_writes_nothing() {
        let (_tmp, cfg) = temp_cfg();
        let intake_id = {
            let mut catalog = Catalog::open(&cfg.catalog_db()).expect("catalog");
            let mut corpus = Corpus::open(&cfg.corpus_db()).expect("corpus");
            seed_book(&cfg, &mut catalog, &mut corpus, "sha-dry").0
        };

        run(
            &cfg,
            RemoveArgs {
                intake_id: Some(intake_id),
                sha: None,
                dry_run: true,
                yes: true,
            },
        )
        .await
        .expect("dry-run succeeds");

        let catalog = Catalog::open_read_only(&cfg.catalog_db()).expect("reopen");
        assert!(
            catalog.intake_by_id(intake_id).expect("lookup").is_some(),
            "dry-run must not delete the intake row",
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

        run(
            &cfg,
            RemoveArgs {
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

        run(
            &cfg,
            RemoveArgs {
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
        run(
            &cfg,
            RemoveArgs {
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
    async fn remove_errors_when_neither_id_nor_sha_is_supplied() {
        let (_tmp, cfg) = temp_cfg();
        let err = run(
            &cfg,
            RemoveArgs {
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
    async fn remove_errors_on_unknown_intake_id() {
        let (_tmp, cfg) = temp_cfg();
        // Open and close to materialize empty catalog + corpus.
        {
            Catalog::open(&cfg.catalog_db()).expect("init catalog");
            Corpus::open(&cfg.corpus_db()).expect("init corpus");
        }
        let err = run(
            &cfg,
            RemoveArgs {
                intake_id: Some(999),
                sha: None,
                dry_run: true,
                yes: true,
            },
        )
        .await
        .expect_err("unknown id must error");
        assert!(format!("{err:#}").contains("no intake registered"));
    }
}
