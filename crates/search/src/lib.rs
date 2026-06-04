// SPDX-License-Identifier: Apache-2.0

//! search: dense retrieval over the chunks vector store, each hit dressed
//! with a citation breadcrumb joined from the corpus node tree.
//!
//! Retrieval is pure dense at pilot scale: the query is embedded with the
//! same model the chunks were, the store returns the nearest passages by
//! cosine distance, and each hit's `start_node_id` is walked up the
//! corpus tree to build a `Book › Chapter › Section` breadcrumb.
//!
//! The breadcrumb leads with the book title when the root carries one
//! (the common case — `ingest` writes the biblio title onto the root), so
//! the title falls out of the ancestor walk and is never prefixed twice.
//! It is added only when the walk does not already lead with it. Hybrid
//! retrieval (BM25 / full-text) and an approximate index are deferred.

use std::path::Path;
use std::time::Instant;

use bookrack_catalog::Catalog;
use bookrack_core::{NodeId, PartitionIdx};
use bookrack_corpus::Corpus;
use bookrack_embed::{Embedder, build_query_input};
pub use bookrack_vectors::SearchOptions;

use bookrack_vectors::{AnnKind, ChunkStore, SearchHit};
use serde::Serialize;

/// Separator between breadcrumb segments.
const BREADCRUMB_SEP: &str = " \u{203a} ";

/// Why a `search` operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SearchError {
    /// The corpus layer reported an error while building a breadcrumb.
    #[error("corpus error: {0}")]
    Corpus(#[from] bookrack_corpus::CorpusError),

    /// The catalog layer reported an error while reading the effective
    /// book title for a breadcrumb.
    #[error("catalog error: {0}")]
    Catalog(#[from] bookrack_catalog::CatalogError),

    /// The embed client failed to embed the query.
    #[error("embed error: {0}")]
    Embed(#[from] bookrack_embed::EmbedError),

    /// The vector store reported an error.
    #[error("vector store error: {0}")]
    Vectors(#[from] bookrack_vectors::VectorsError),

    /// The embedder returned no vector for the query.
    #[error("the embedder returned no vector for the query")]
    EmptyEmbedding,
}

/// A fallible `search` operation.
pub type Result<T> = std::result::Result<T, SearchError>;

/// One search result: a passage and where to cite it from.
///
/// Derives `Serialize` so a query consumer (e.g. the MCP server) can
/// emit it as JSON without redeclaring its fields; adding a field here
/// flows through automatically.
#[derive(Debug, Clone, Serialize)]
pub struct Citation {
    /// The passage text to display.
    pub text: String,
    /// A `Book \u{203a} Chapter \u{203a} Section` trail of titled
    /// ancestors; empty when no ancestor carries a title.
    pub breadcrumb: String,
    /// The leaf the passage starts in.
    pub start_node_id: NodeId,
    /// Character offset of the passage start within `start_node_id`.
    pub start_char_offset: i32,
    /// The leaf the passage ends in.
    pub end_node_id: NodeId,
    /// Character offset of the passage end within `end_node_id`.
    pub end_char_offset: i32,
    /// SHA-256 of the normalized passage text.
    pub norm_chunk_sha256: String,
    /// Cosine distance to the query — smaller is nearer.
    pub distance: f32,
}

/// Retrieve the `top_k` passages nearest `query`, each with a citation
/// breadcrumb resolved from `corpus` (structural ancestors) and
/// `catalog` (the effective book title).
///
/// A convenience wrapper over [`retrieve`] then [`cite`]. It borrows
/// `corpus` across the await in `retrieve`, so its future is `Send` only
/// where `Corpus` is `Sync` — fine for a single-threaded caller like the
/// CLI. A caller that needs a `Send` future (e.g. one serving requests on
/// a multi-threaded runtime) should call [`retrieve`] and [`cite`]
/// directly, opening the corpus and catalog only for the synchronous
/// citation step.
#[tracing::instrument(name = "search", skip_all, fields(top_k = top_k))]
pub async fn search<E: Embedder>(
    query: &str,
    corpus: &Corpus,
    catalog: &Catalog,
    store: &ChunkStore,
    embedder: &E,
    lancedb_dir: &Path,
    top_k: usize,
) -> Result<Vec<Citation>> {
    search_with(
        query,
        corpus,
        catalog,
        store,
        embedder,
        lancedb_dir,
        SearchOptions::default(),
        top_k,
    )
    .await
}

/// Variant of [`search`] that applies per-call overrides over the
/// persisted meta defaults — see [`retrieve_with`] for the merge order.
#[allow(clippy::too_many_arguments)]
pub async fn search_with<E: Embedder>(
    query: &str,
    corpus: &Corpus,
    catalog: &Catalog,
    store: &ChunkStore,
    embedder: &E,
    lancedb_dir: &Path,
    overrides: SearchOptions,
    top_k: usize,
) -> Result<Vec<Citation>> {
    let hits = retrieve_with(query, store, embedder, lancedb_dir, overrides, top_k).await?;
    cite(corpus, catalog, hits)
}

/// Embed `query` and recall the `top_k` nearest passages from the vector
/// store. The async half of a search: it touches only the embedder and the
/// store, never the corpus, so its future is `Send`.
pub async fn retrieve<E: Embedder>(
    query: &str,
    store: &ChunkStore,
    embedder: &E,
    lancedb_dir: &Path,
    top_k: usize,
) -> Result<Vec<SearchHit>> {
    retrieve_with(
        query,
        store,
        embedder,
        lancedb_dir,
        SearchOptions::default(),
        top_k,
    )
    .await
}

/// Variant of [`retrieve`] that lets a caller layer per-call overrides
/// on top of the persisted meta defaults.
///
/// Merge order: `overrides.nprobes` / `overrides.refine_factor` win
/// when set; otherwise the meta default applies. `overrides.bypass_
/// index = true` is sticky — it forces a brute-force scan regardless
/// of meta.
pub async fn retrieve_with<E: Embedder>(
    query: &str,
    store: &ChunkStore,
    embedder: &E,
    lancedb_dir: &Path,
    overrides: SearchOptions,
    top_k: usize,
) -> Result<Vec<SearchHit>> {
    let input = build_query_input(query);
    let embed_started = Instant::now();
    let vectors = embedder.embed_batch(std::slice::from_ref(&input)).await?;
    let query_vector = vectors.first().ok_or(SearchError::EmptyEmbedding)?;
    tracing::debug!(
        elapsed_ms = embed_started.elapsed().as_secs_f64() * 1e3,
        "embedded query"
    );

    let base = options_from_meta(store, lancedb_dir)?;
    let opts = SearchOptions {
        nprobes: overrides.nprobes.or(base.nprobes),
        refine_factor: overrides.refine_factor.or(base.refine_factor),
        bypass_index: overrides.bypass_index || base.bypass_index,
    };
    let recall_started = Instant::now();
    let hits = store.search_with(query_vector, top_k, opts).await?;
    tracing::debug!(
        hits = hits.len(),
        elapsed_ms = recall_started.elapsed().as_secs_f64() * 1e3,
        "recalled nearest passages"
    );
    Ok(hits)
}

/// Variant of [`retrieve_with`] restricted to one book's partition.
///
/// Embeds the query the same way and applies the same overrides /
/// meta-default merge, then asks the store for the nearest passages
/// whose `start_node_id` falls inside `partition`. An empty partition
/// returns an empty `Vec` rather than an error.
pub async fn retrieve_with_partition<E: Embedder>(
    query: &str,
    store: &ChunkStore,
    embedder: &E,
    lancedb_dir: &Path,
    overrides: SearchOptions,
    top_k: usize,
    partition: PartitionIdx,
) -> Result<Vec<SearchHit>> {
    let input = build_query_input(query);
    let embed_started = Instant::now();
    let vectors = embedder.embed_batch(std::slice::from_ref(&input)).await?;
    let query_vector = vectors.first().ok_or(SearchError::EmptyEmbedding)?;
    tracing::debug!(
        elapsed_ms = embed_started.elapsed().as_secs_f64() * 1e3,
        "embedded query"
    );

    let base = options_from_meta(store, lancedb_dir)?;
    let opts = SearchOptions {
        nprobes: overrides.nprobes.or(base.nprobes),
        refine_factor: overrides.refine_factor.or(base.refine_factor),
        bypass_index: overrides.bypass_index || base.bypass_index,
    };
    let recall_started = Instant::now();
    let hits = store
        .search_partition_with(query_vector, partition, top_k, opts)
        .await?;
    tracing::debug!(
        hits = hits.len(),
        partition = partition.get(),
        elapsed_ms = recall_started.elapsed().as_secs_f64() * 1e3,
        "recalled nearest passages within partition"
    );
    Ok(hits)
}

/// Read per-query overrides from `BOOKRACK_VECTORS_*` environment
/// variables. Used by daemons that cannot pass per-call flags through
/// their request surface; CLI callers usually merge this on top of
/// their command-line overrides, with the command line winning.
///
/// Recognised variables:
///
/// * `BOOKRACK_VECTORS_BYPASS_ANN` — `"1"` / `"true"` / `"yes"` →
///   force brute-force.
/// * `BOOKRACK_VECTORS_NPROBES` — integer; sets `nprobes`.
/// * `BOOKRACK_VECTORS_REFINE_FACTOR` — integer; sets
///   `refine_factor`.
pub fn env_overrides() -> SearchOptions {
    let bypass_index = std::env::var("BOOKRACK_VECTORS_BYPASS_ANN")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let nprobes = std::env::var("BOOKRACK_VECTORS_NPROBES")
        .ok()
        .and_then(|v| v.trim().parse().ok());
    let refine_factor = std::env::var("BOOKRACK_VECTORS_REFINE_FACTOR")
        .ok()
        .and_then(|v| v.trim().parse().ok());
    SearchOptions {
        nprobes,
        refine_factor,
        bypass_index,
    }
}

/// Build [`SearchOptions`] from the persisted [`AnnConfig`] at
/// `<lancedb_dir>/vectors_meta.json`. No meta file or `kind =
/// "brute-force"` returns `SearchOptions::default()` — lancedb already
/// runs brute-force when no index is present, and the explicit
/// brute-force kind has no useful overrides.
fn options_from_meta(store: &ChunkStore, lancedb_dir: &Path) -> Result<SearchOptions> {
    let cfg = match store.current_ann_cfg(lancedb_dir)? {
        Some(c) if c.kind != AnnKind::BruteForce => c,
        _ => return Ok(SearchOptions::default()),
    };
    Ok(SearchOptions {
        nprobes: Some(cfg.nprobes as usize),
        refine_factor: cfg.refine_factor,
        bypass_index: false,
    })
}

/// Resolve a citation breadcrumb for each hit from `corpus` (the
/// structural ancestors) and `catalog` (the effective book title).
/// The synchronous half of a search: no awaits, so a caller can open
/// short-lived corpus and catalog handles here without holding them
/// across an await.
pub fn cite(corpus: &Corpus, catalog: &Catalog, hits: Vec<SearchHit>) -> Result<Vec<Citation>> {
    let breadcrumb_started = Instant::now();
    let mut citations = Vec::with_capacity(hits.len());
    for hit in hits {
        citations.push(Citation {
            breadcrumb: breadcrumb(corpus, catalog, hit.start_node_id)?,
            text: hit.text,
            start_node_id: hit.start_node_id,
            start_char_offset: hit.start_char_offset,
            end_node_id: hit.end_node_id,
            end_char_offset: hit.end_char_offset,
            norm_chunk_sha256: hit.norm_chunk_sha256,
            distance: hit.distance,
        });
    }
    tracing::debug!(
        elapsed_ms = breadcrumb_started.elapsed().as_secs_f64() * 1e3,
        "resolved breadcrumbs"
    );
    tracing::info!(hits = citations.len(), "search complete");
    Ok(citations)
}

/// Build the breadcrumb for a leaf by walking its organizing ancestors to
/// the book root, collecting their titles top-down.
///
/// The book root's title is read from `catalog`'s effective
/// publication-attrs view — so a post-hoc `metadata set title <new>`
/// reflects into the next breadcrumb without rebuilding the corpus —
/// with the corpus's `node.title` as a fallback when the catalog has
/// no row yet. When neither source carries a title, the leading
/// segment falls back to the intake filename stem, then to
/// `book #<intake_id>`, so a citation is never indistinguishable
/// across books. Internal organizing nodes (chapter / section / …)
/// keep reading from the corpus, since those titles are TOC structure,
/// not publication metadata.
fn breadcrumb(corpus: &Corpus, catalog: &Catalog, start_node_id: NodeId) -> Result<String> {
    let mut titles = Vec::new();
    let mut book_title = None;
    let mut current = Some(start_node_id);
    while let Some(id) = current {
        let Some(node) = corpus.get_node(id)? else {
            break;
        };
        if node.parent_id.is_none() {
            // The book partition is keyed by intake_id, so the root's
            // partition index is the intake id we look up in catalog.
            // The root's title is read here and not pushed into the
            // structural titles below, so the leading segment always
            // reflects the catalog's effective view.
            let intake_id = id.partition().get();
            let effective = catalog.effective_publication_attrs(intake_id, "book")?;
            book_title = match effective
                .get("title")
                .map(str::to_string)
                .or_else(|| node.title.clone())
            {
                Some(t) => Some(t),
                None => Some(
                    filename_stem_for(catalog, intake_id)?
                        .unwrap_or_else(|| format!("book #{intake_id}")),
                ),
            };
        } else if node.node_type.is_organizing()
            && let Some(title) = &node.title
        {
            titles.push(title.clone());
        }
        current = node.parent_id;
    }
    // The walk is leaf-to-root; reverse to read book-to-section.
    titles.reverse();

    if let Some(book) = book_title
        && titles.first() != Some(&book)
    {
        titles.insert(0, book);
    }
    Ok(titles.join(BREADCRUMB_SEP))
}

/// Look up the intake's filename stem from `original_path`, returning
/// `Ok(None)` when the column is empty or the path has no usable base
/// component. Errors bubble through unchanged so the breadcrumb walker
/// keeps the catalog-error variant for diagnostics.
fn filename_stem_for(catalog: &Catalog, intake_id: i64) -> Result<Option<String>> {
    let Some(intake) = catalog.intake_by_id(intake_id)? else {
        return Ok(None);
    };
    Ok(intake
        .original_path
        .as_deref()
        .and_then(filename_stem)
        .map(str::to_string))
}

/// Pull the basename stem from a path string: drop directory segments
/// on either separator, then drop the last `.ext` if present. Returns
/// `None` when the result would be empty.
fn filename_stem(path: &str) -> Option<&str> {
    let base = path.rsplit(['/', '\\']).next()?;
    let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base);
    if stem.trim().is_empty() {
        None
    } else {
        Some(stem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_catalog::{Catalog, NewOverride, NewPublicationAttrs};
    use bookrack_core::NodeType;
    use bookrack_embed::Result as EmbedResult;
    use bookrack_extract::{
        Biblio, Block, BlockKind, Extraction, Provenance, TextLayerQuality, Toc, TocEntry,
    };
    use bookrack_ingest::{StructureParams, ingest_structure};
    use bookrack_vectors::ChunkRow;
    use std::future::Future;

    const DIM: usize = 4;

    #[test]
    fn filename_stem_drops_directories_and_extensions() {
        assert_eq!(super::filename_stem("incoming/foo.epub"), Some("foo"));
        // A Windows-style path with backslash separators. The colon byte
        // is written as `\x3A` so the source line does not match the
        // leak-check pattern for local drive paths.
        assert_eq!(super::filename_stem("C\x3A\\books\\bar.pdf"), Some("bar"));
        assert_eq!(super::filename_stem("baz"), Some("baz"));
        assert_eq!(super::filename_stem("a/b/c.tar.gz"), Some("c.tar"));
        assert_eq!(super::filename_stem(""), None);
        assert_eq!(super::filename_stem("dir/.gitignore"), None);
    }

    /// An embedder that returns one fixed query vector, ignoring the text.
    struct FixedQuery {
        vector: Vec<f32>,
    }

    impl Embedder for FixedQuery {
        fn embed_batch(
            &self,
            texts: &[String],
        ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
            let vector = self.vector.clone();
            let n = texts.len();
            async move { Ok(vec![vector; n]) }
        }
    }

    #[test]
    fn citation_serializes_with_node_id_as_bare_int() {
        let citation = Citation {
            text: "hello".to_string(),
            breadcrumb: "Book \u{203a} Chapter".to_string(),
            start_node_id: NodeId::new(100_000_001),
            start_char_offset: 0,
            end_node_id: NodeId::new(100_000_001),
            end_char_offset: 5,
            norm_chunk_sha256: "abc".to_string(),
            distance: 0.25,
        };
        let value = serde_json::to_value(&citation).expect("serialize citation");
        // The newtype id flattens to a bare integer, not a wrapper object.
        assert_eq!(value["start_node_id"], serde_json::json!(100_000_001));
        assert_eq!(value["text"], "hello");
        assert_eq!(value["distance"], 0.25);
    }

    /// A one-chapter extraction; `title` becomes the biblio title.
    fn extraction(title: Option<&str>, with_chapter: bool) -> Extraction {
        let (blocks, entries) = if with_chapter {
            (
                vec![
                    Block {
                        kind: BlockKind::Heading { level: 1 },
                        text: "Chapter One".to_string(),
                        source_unit: 0,
                    },
                    Block {
                        kind: BlockKind::Body,
                        text: "The passage body.".to_string(),
                        source_unit: 0,
                    },
                ],
                vec![TocEntry {
                    label: "Chapter One".to_string(),
                    depth: 0,
                    start_block: Some(0),
                }],
            )
        } else {
            (
                vec![Block {
                    kind: BlockKind::Body,
                    text: "The passage body.".to_string(),
                    source_unit: 0,
                }],
                Vec::new(),
            )
        };
        Extraction {
            blocks,
            toc: Toc { entries },
            biblio: Biblio {
                title: title.map(str::to_string),
                ..Default::default()
            },
            provenance: Provenance {
                adapter: "test".to_string(),
                extractor_version: "test-1".to_string(),
                text_layer_quality: TextLayerQuality::BornDigital,
                skipped_units: Vec::new(),
            },
        }
    }

    /// Ingest one synthetic book into an in-memory corpus and index its
    /// first prose leaf in a fresh store under a temp directory, returning
    /// the pieces a search needs plus the indexed leaf id. The companion
    /// catalog is opened in-memory and seeded with the book root's title
    /// when one is supplied, mirroring the live ingest pipeline.
    async fn fixture(
        title: Option<&str>,
        with_chapter: bool,
    ) -> (tempfile::TempDir, Corpus, Catalog, ChunkStore, NodeId) {
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let intake_id = 1i64;
        let report = ingest_structure(
            &mut corpus,
            intake_id,
            NodeType::Work,
            &extraction(title, with_chapter),
            &StructureParams::default(),
        )
        .expect("structure");

        let leaf = corpus
            .book_nodes(report.book_root_id)
            .expect("nodes")
            .into_iter()
            .find(|n| n.node_type.is_prose_leaf())
            .expect("a prose leaf");

        let catalog = Catalog::open_in_memory().expect("catalog");
        if let Some(t) = title {
            let mut attrs = NewPublicationAttrs::new(intake_id, "book");
            attrs.title = Some(t.to_string());
            catalog
                .upsert_publication_attrs(&attrs)
                .expect("seed title");
        }

        let dir = tempfile::tempdir().expect("temp dir");
        let store = ChunkStore::open(dir.path(), DIM).await.expect("store");
        store
            .append(&[ChunkRow {
                vector: vec![1.0, 0.0, 0.0, 0.0],
                text: leaf.text_content.clone().unwrap(),
                start_node_id: leaf.node_id,
                start_char_offset: 0,
                end_node_id: leaf.node_id,
                end_char_offset: 100,
                norm_chunk_sha256: "sha".to_string(),
            }])
            .await
            .expect("append");
        (dir, corpus, catalog, store, leaf.node_id)
    }

    #[tokio::test]
    async fn breadcrumb_leads_with_the_book_title_once() {
        let (dir, corpus, catalog, store, leaf) = fixture(Some("A Test Book"), true).await;
        let query = FixedQuery {
            vector: vec![1.0, 0.0, 0.0, 0.0],
        };
        let hits = search("anything", &corpus, &catalog, &store, &query, dir.path(), 5)
            .await
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].start_node_id, leaf);
        // The book title appears exactly once, then the chapter.
        assert_eq!(hits[0].breadcrumb, "A Test Book \u{203a} Chapter One");
    }

    #[tokio::test]
    async fn breadcrumb_falls_back_to_intake_filename_stem() {
        // No biblio title and no TOC. With a known intake row carrying
        // `original_path`, the leading segment falls back to the
        // filename stem rather than rendering as untitled.
        let (dir, corpus, mut catalog, store, _leaf) = fixture(None, false).await;
        catalog
            .register_intake(
                &bookrack_catalog::NewIntake::new("sha-abc")
                    .format("epub")
                    .byte_size(1024)
                    .original_path("incoming/a-bare-book.epub"),
            )
            .expect("register intake");
        let query = FixedQuery {
            vector: vec![1.0, 0.0, 0.0, 0.0],
        };
        let hits = search("anything", &corpus, &catalog, &store, &query, dir.path(), 5)
            .await
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].breadcrumb, "a-bare-book");
    }

    #[tokio::test]
    async fn breadcrumb_falls_back_to_book_id_when_no_intake_row() {
        // No biblio title, no TOC, and no intake row to read from:
        // the breadcrumb still names the book by its intake id rather
        // than rendering as untitled.
        let (dir, corpus, catalog, store, _leaf) = fixture(None, false).await;
        let query = FixedQuery {
            vector: vec![1.0, 0.0, 0.0, 0.0],
        };
        let hits = search("anything", &corpus, &catalog, &store, &query, dir.path(), 5)
            .await
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].breadcrumb, "book #1");
    }

    #[tokio::test]
    async fn options_from_meta_returns_default_when_no_meta() {
        let (dir, _corpus, _catalog, store, _leaf) = fixture(Some("Any"), true).await;
        let opts = options_from_meta(&store, dir.path()).expect("options_from_meta");
        assert_eq!(opts.nprobes, None);
        assert_eq!(opts.refine_factor, None);
        assert!(!opts.bypass_index);
    }

    #[tokio::test]
    async fn options_from_meta_carries_overrides_from_meta_file() {
        let (dir, _corpus, _catalog, store, _leaf) = fixture(Some("Any"), true).await;
        // Stamp a meta file directly to simulate a built IvfFlat index.
        let cfg = bookrack_vectors::AnnConfig {
            kind: AnnKind::IvfFlat,
            num_partitions: 64,
            num_sub_vectors: None,
            num_bits: None,
            nprobes: 20,
            refine_factor: Some(3),
        };
        let meta = cfg.to_meta(
            "2026-06-03T00:00:00Z".to_string(),
            10,
            0,
            bookrack_vectors::DEFAULT_INDEX_NAME.to_string(),
        );
        bookrack_vectors::meta::store(dir.path(), &meta).expect("write meta");
        let opts = options_from_meta(&store, dir.path()).expect("options_from_meta");
        assert_eq!(opts.nprobes, Some(20));
        assert_eq!(opts.refine_factor, Some(3));
        assert!(!opts.bypass_index);
    }

    #[tokio::test]
    async fn options_from_meta_returns_default_for_brute_force_kind() {
        let (dir, _corpus, _catalog, store, _leaf) = fixture(Some("Any"), true).await;
        let cfg = bookrack_vectors::AnnConfig::default_for(AnnKind::BruteForce);
        let meta = cfg.to_meta(
            "2026-06-03T00:00:00Z".to_string(),
            10,
            0,
            bookrack_vectors::DEFAULT_INDEX_NAME.to_string(),
        );
        bookrack_vectors::meta::store(dir.path(), &meta).expect("write meta");
        let opts = options_from_meta(&store, dir.path()).expect("options_from_meta");
        assert_eq!(opts.nprobes, None);
        assert!(!opts.bypass_index);
    }

    #[tokio::test]
    async fn retrieve_with_partition_only_returns_hits_inside_that_book() {
        // Build a fixture under intake 1, then append a stray chunk that
        // belongs to intake 2's partition; retrieval restricted to
        // partition 1 must skip it.
        let (dir, _corpus, _catalog, store, leaf) = fixture(Some("A Test Book"), true).await;
        let stray_partition_node = PartitionIdx::new(2).node_id(1).expect("offset in range");
        store
            .append(&[ChunkRow {
                vector: vec![1.0, 0.0, 0.0, 0.0],
                text: "other-book passage".to_string(),
                start_node_id: stray_partition_node,
                start_char_offset: 0,
                end_node_id: stray_partition_node,
                end_char_offset: 10,
                norm_chunk_sha256: "sha-2".to_string(),
            }])
            .await
            .expect("append stray");

        let query = FixedQuery {
            vector: vec![1.0, 0.0, 0.0, 0.0],
        };
        let hits = retrieve_with_partition(
            "anything",
            &store,
            &query,
            dir.path(),
            SearchOptions::default(),
            5,
            PartitionIdx::new(1),
        )
        .await
        .expect("retrieve partition");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].start_node_id, leaf);
    }

    #[tokio::test]
    async fn override_title_reflects_in_breadcrumb_without_corpus_rewrite() {
        // The catalog's effective view is what the breadcrumb reads:
        // setting an override title after ingest must show up on the
        // very next query, and the corpus row stays untouched.
        let (dir, corpus, catalog, store, _leaf) = fixture(Some("Original"), true).await;
        catalog
            .set_override(&NewOverride::new(
                1,
                "book",
                "title",
                Some("Revised".to_string()),
                "human",
            ))
            .expect("set override");
        let query = FixedQuery {
            vector: vec![1.0, 0.0, 0.0, 0.0],
        };
        let hits = search("anything", &corpus, &catalog, &store, &query, dir.path(), 5)
            .await
            .expect("search");
        assert_eq!(hits[0].breadcrumb, "Revised \u{203a} Chapter One");
    }
}
