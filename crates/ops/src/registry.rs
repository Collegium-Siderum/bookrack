// SPDX-License-Identifier: Apache-2.0

//! Library registry — the in-process scheduler entry point.
//!
//! A [`LibraryRegistry`] holds one [`LibraryHandle`] per configured
//! library and a single mutable pointer to the default. Every caller —
//! the interactive REPL, the MCP server, future tray UI clients —
//! routes through this type to obtain an [`Ops`] handle; no caller opens
//! an [`Ops`] (or the databases behind it) on its own.
//!
//! Routing in this phase is one method deep: [`get`] returns the
//! handle, the caller drives [`Ops`] directly. Future work (priority
//! queues, per-library quotas, query-vs-ingest preemption) layers on
//! top of the same routing point without touching call sites.
//!
//! [`get`]: LibraryRegistry::get

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use bookrack_catalog::Catalog;
use bookrack_corpus::Corpus;
use bookrack_embed::Embedder;
use bookrack_glean::{GleanParams, GleanReport};
use bookrack_ingest::ocr::{OcrIngestParams, OcrIngestReport, ingest_ocr_intake};
use bookrack_ingest::{IngestParams, IngestReport};
use eyre::WrapErr;
use tokio::sync::Mutex as AsyncMutex;

use crate::Ops;

/// Why a registry operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// The named library is not in the registry. `available` lists the
    /// names the registry does carry, sorted, so the caller can pick
    /// the right one without a separate listing call.
    #[error(
        "no library named {name:?} (available: {})",
        if available.is_empty() { "<none>".to_string() } else { available.join(", ") }
    )]
    LibraryUnknown {
        /// The name the caller asked for.
        name: String,
        /// Library names the registry currently carries, sorted.
        available: Vec<String>,
    },

    /// Build was attempted from an empty handle iterator: the registry
    /// must carry at least one library to be usable.
    #[error("the library registry is empty")]
    Empty,

    /// The internal `RwLock` guarding the default-library name was
    /// poisoned. Indicates a bug — a panic while a writer held the lock
    /// — rather than a misuse path the caller can recover from.
    #[error("internal: default-library lock poisoned")]
    DefaultLockPoisoned,
}

/// A fallible registry operation.
pub type Result<T> = std::result::Result<T, RegistryError>;

/// One named library bound to the scheduler — its short name and the
/// warm [`Ops`] that drives its catalog, corpus, and vector store.
///
/// Always held behind an `Arc`: the scheduler hands clones to whatever
/// task needs to run an op, so the underlying [`Ops`] is never moved or
/// reopened during the registry's lifetime.
///
/// The handle also owns a per-library [`AsyncMutex`] used to serialise
/// ingest writes. Read ops keep going through the shared `Arc<Ops>`
/// without contention; only [`LibraryHandle::ingest_book`] takes the
/// lock, so two queue workers — current and hypothetical — never run
/// the catalog/corpus write path in parallel against the same library.
pub struct LibraryHandle<E: Embedder> {
    name: String,
    ops: Arc<Ops<E>>,
    ingest_lock: AsyncMutex<()>,
    glean_lock: AsyncMutex<()>,
}

impl<E: Embedder> LibraryHandle<E> {
    /// Wrap an already-warm [`Ops`] under the given short name.
    pub fn new(name: impl Into<String>, ops: Ops<E>) -> Arc<LibraryHandle<E>> {
        LibraryHandle::from_arc(name, Arc::new(ops))
    }

    /// Wrap a pre-shared [`Arc<Ops>`] under the given short name.
    /// Useful when the caller already holds a shared handle and wants to
    /// register it without bumping the strong-count more than necessary.
    pub fn from_arc(name: impl Into<String>, ops: Arc<Ops<E>>) -> Arc<LibraryHandle<E>> {
        Arc::new(LibraryHandle {
            name: name.into(),
            ops,
            ingest_lock: AsyncMutex::new(()),
            glean_lock: AsyncMutex::new(()),
        })
    }

    /// The short name this library is registered under.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The warm [`Ops`] driving this library's stores.
    pub fn ops(&self) -> &Ops<E> {
        &self.ops
    }

    /// A new strong reference to the shared [`Ops`]. Callers that need
    /// to embed the ops handle inside their own [`Arc`]-shaped state
    /// (the MCP server, the queue worker) clone through here so the
    /// underlying ops is never moved or reopened.
    pub fn ops_arc(&self) -> Arc<Ops<E>> {
        Arc::clone(&self.ops)
    }
}

impl<E: Embedder + Send + Sync + 'static> LibraryHandle<E> {
    /// Ingest one book into this library through the registry-mediated
    /// write path.
    ///
    /// Holds the per-library ingest mutex for the duration of the call,
    /// opens fresh [`Catalog`] and [`Corpus`] handles from the library's
    /// stored paths, then forwards to [`bookrack_ingest::ingest_book`]
    /// with the library's warm embedder. The caller — currently the
    /// `bookrack run` REPL's queue worker — never opens a database on
    /// its own.
    ///
    /// Returns an error when this handle was built catalog-only, since
    /// ingest requires the embedder that lives on the
    /// [`bookrack_query::Library`] half.
    pub async fn ingest_book(
        &self,
        path: &Path,
        params: &IngestParams,
    ) -> eyre::Result<IngestReport> {
        let embedder = self
            .ops
            .embedder()
            .ok_or_else(|| eyre::eyre!("ingest requires a library handle with an embedder"))?;
        let _guard = self.ingest_lock.lock().await;
        let mut corpus =
            Corpus::open(self.ops.corpus_db()).context("open corpus for ingest write")?;
        let mut catalog = Catalog::open_with_backup(self.ops.catalog_db(), self.ops.backup_dir())
            .context("open catalog for ingest write")?;
        let report = bookrack_ingest::ingest_book(
            path,
            &mut corpus,
            &mut catalog,
            self.ops.lancedb_dir(),
            self.ops.books_dir(),
            embedder,
            params,
        )
        .await
        .context("registry-mediated ingest")?;
        // The first ingest into a previously empty data dir creates the
        // lance dir mid-process; the warm `Library` cached its store at
        // startup, when the dir did not yet exist, and would otherwise
        // serve every subsequent search from `store = None`. Force the
        // library to rebind its store from the on-disk lance dir before
        // the ingest_book lock drops.
        if let Some(library) = self.ops.library() {
            library
                .refresh_store()
                .await
                .context("refresh library store after ingest")?;
        }
        Ok(report)
    }

    /// Register an OCR markdown product as a derived-source intake of
    /// an already-recorded scan PDF, then run the book pipeline
    /// (STRUCTURE → CHUNK → EMBED) over its text layer.
    ///
    /// Mirrors [`Self::ingest_book`]: holds the per-library ingest
    /// mutex, opens fresh [`Catalog`] and [`Corpus`] handles from the
    /// library's stored paths, then forwards to
    /// [`bookrack_ingest::ocr::ingest_ocr_intake`] with the library's
    /// warm embedder. Refreshes the book-side [`Library`] store so the
    /// first OCR intake into a previously empty data dir lights up the
    /// read path on the same lock.
    ///
    /// Returns an error when this handle was built catalog-only, since
    /// the OCR ingest path requires the embedder that lives on the
    /// [`bookrack_query::Library`] half.
    pub async fn ingest_ocr(
        &self,
        ocr_md: &Path,
        from_pdf: &Path,
        ocr_params: &OcrIngestParams,
        params: &IngestParams,
    ) -> eyre::Result<OcrIngestReport> {
        let embedder = self
            .ops
            .embedder()
            .ok_or_else(|| eyre::eyre!("OCR intake requires a library handle with an embedder"))?;
        let _guard = self.ingest_lock.lock().await;
        let mut corpus =
            Corpus::open(self.ops.corpus_db()).context("open corpus for OCR intake write")?;
        let mut catalog = Catalog::open_with_backup(self.ops.catalog_db(), self.ops.backup_dir())
            .context("open catalog for OCR intake write")?;
        let report = ingest_ocr_intake(
            &mut corpus,
            &mut catalog,
            self.ops.lancedb_dir(),
            self.ops.books_dir(),
            ocr_md,
            from_pdf,
            embedder,
            params,
            ocr_params,
        )
        .await
        .context("registry-mediated OCR intake")?;
        if let Some(library) = self.ops.library() {
            library
                .refresh_store()
                .await
                .context("refresh library store after OCR intake")?;
        }
        Ok(report)
    }

    /// Glean one paper into this library through the registry-mediated
    /// write path.
    ///
    /// Mirrors [`Self::ingest_book`] for the paper pipeline: holds the
    /// per-library glean mutex for the duration of the call, opens
    /// fresh [`Catalog`] and [`Corpus`] handles from the library's
    /// configured paper-side paths, then forwards to
    /// [`bookrack_glean::glean_paper`] with the paper-side warm
    /// embedder. After a successful run the paper-side [`Library`]
    /// rebinds its store from disk so the first paper into a previously
    /// empty data dir lights up the read path.
    ///
    /// Returns an error when this handle has no papers backend attached
    /// (`Ops::with_papers` was never called), since glean has no way to
    /// open a corpus or run an embedder without one.
    pub async fn glean_paper(
        &self,
        path: &Path,
        params: &GleanParams,
    ) -> eyre::Result<GleanReport> {
        let embedder = self
            .ops
            .papers_embedder()
            .ok_or_else(|| eyre::eyre!("library handle has no papers backend"))?;
        let corpus_db = self
            .ops
            .papers_corpus_db()
            .ok_or_else(|| eyre::eyre!("library handle has no papers backend"))?;
        let catalog_db = self
            .ops
            .papers_catalog_db()
            .ok_or_else(|| eyre::eyre!("library handle has no papers backend"))?;
        let lancedb_dir = self
            .ops
            .papers_lancedb_dir()
            .ok_or_else(|| eyre::eyre!("library handle has no papers backend"))?;
        let papers_dir = self
            .ops
            .papers_dir()
            .ok_or_else(|| eyre::eyre!("library handle has no papers backend"))?;
        let _guard = self.glean_lock.lock().await;
        let mut corpus = Corpus::open(corpus_db).context("open papers corpus for glean write")?;
        let mut catalog = Catalog::open_with_backup(catalog_db, self.ops.backup_dir())
            .context("open papers catalog for glean write")?;
        let report = bookrack_glean::glean_paper(
            path,
            &mut corpus,
            &mut catalog,
            lancedb_dir,
            papers_dir,
            embedder,
            params,
        )
        .await
        .context("registry-mediated glean")?;
        if let Some(library) = self.ops.papers_library() {
            library
                .refresh_store()
                .await
                .context("refresh papers library store after glean")?;
        }
        Ok(report)
    }

    /// Open the paper catalog for synchronous read/write. Returns an
    /// error when this handle has no papers backend attached.
    pub fn open_paper_catalog(&self) -> eyre::Result<Catalog> {
        let catalog_db = self
            .ops
            .papers_catalog_db()
            .ok_or_else(|| eyre::eyre!("library handle has no papers backend"))?;
        Catalog::open_with_backup(catalog_db, self.ops.backup_dir()).context("open papers catalog")
    }

    /// Re-run the paper-side metadata audit on an existing intake's
    /// cached extraction envelope and write only the `confidence` /
    /// `audit_verdict` rollup. Returns the new and previous verdict /
    /// confidence pair.
    pub async fn reaudit_paper(
        &self,
        intake_id: i64,
        profile: &bookrack_glean::audit::PaperAuditProfile,
        data: &bookrack_glean::audit::PaperAuditData,
    ) -> eyre::Result<bookrack_glean::reaudit::ReauditOutcome> {
        let catalog_db = self
            .ops
            .papers_catalog_db()
            .ok_or_else(|| eyre::eyre!("library handle has no papers backend"))?;
        let _guard = self.glean_lock.lock().await;
        let catalog = Catalog::open_with_backup(catalog_db, self.ops.backup_dir())
            .context("open papers catalog for reaudit")?;
        bookrack_glean::reaudit::reaudit_paper(&catalog, intake_id, profile, data)
            .map_err(|e| eyre::Report::from(e).wrap_err("registry-mediated paper reaudit"))
    }
}

/// One row of [`LibraryRegistry::list`] — the registered name and the
/// vector dimension the library's store was opened at (when search is
/// available on the handle).
#[derive(Debug, Clone)]
pub struct LibrarySummary {
    /// The library's registered short name.
    pub name: String,
    /// The embedder dimension the vector store was opened at; `None`
    /// for catalog-only handles.
    pub dimension: Option<usize>,
    /// Whether this is the registry's current default library.
    pub is_default: bool,
}

/// The in-process scheduler: a map of registered libraries keyed by
/// short name, plus a mutable pointer to the default library used when
/// a caller does not name one.
///
/// Held behind an `Arc` and shared across the REPL task, the MCP
/// server, and the queue worker. Routing in this phase is "look up
/// handle, hand it back"; future scheduling logic — priority,
/// throttling, preemption — lands here without touching callers.
pub struct LibraryRegistry<E: Embedder> {
    libs: HashMap<String, Arc<LibraryHandle<E>>>,
    default: RwLock<String>,
}

impl<E: Embedder> LibraryRegistry<E> {
    /// Build a registry from an iterator of [`LibraryHandle`]s and a
    /// chosen default name. Returns [`RegistryError::Empty`] if the
    /// iterator yields no handle, and [`RegistryError::LibraryUnknown`]
    /// if the default name is not among the handles' names.
    pub fn from_handles(
        handles: impl IntoIterator<Item = Arc<LibraryHandle<E>>>,
        default: impl Into<String>,
    ) -> Result<Arc<LibraryRegistry<E>>> {
        let mut libs: HashMap<String, Arc<LibraryHandle<E>>> = HashMap::new();
        for handle in handles {
            libs.insert(handle.name().to_string(), handle);
        }
        if libs.is_empty() {
            return Err(RegistryError::Empty);
        }
        let default = default.into();
        if !libs.contains_key(&default) {
            return Err(RegistryError::LibraryUnknown {
                name: default,
                available: sorted_names(&libs),
            });
        }
        Ok(Arc::new(LibraryRegistry {
            libs,
            default: RwLock::new(default),
        }))
    }

    /// Build a one-element registry whose default is the wrapped
    /// handle's name. The phase-1 chokepoint for callers that still
    /// hold a single [`Ops`]: pass it through this constructor and from
    /// then on go through [`get`].
    ///
    /// [`get`]: LibraryRegistry::get
    pub fn single(handle: Arc<LibraryHandle<E>>) -> Arc<LibraryRegistry<E>> {
        let name = handle.name().to_string();
        let mut libs: HashMap<String, Arc<LibraryHandle<E>>> = HashMap::new();
        libs.insert(name.clone(), handle);
        Arc::new(LibraryRegistry {
            libs,
            default: RwLock::new(name),
        })
    }

    /// Look up a handle. `None` means "use the current default".
    pub fn get(&self, name: Option<&str>) -> Result<Arc<LibraryHandle<E>>> {
        let key = match name {
            Some(n) => n.to_string(),
            None => self
                .default
                .read()
                .map_err(|_| RegistryError::DefaultLockPoisoned)?
                .clone(),
        };
        self.libs
            .get(&key)
            .cloned()
            .ok_or_else(|| RegistryError::LibraryUnknown {
                name: key,
                available: sorted_names(&self.libs),
            })
    }

    /// Move the default-library pointer to `name`. Returns
    /// [`RegistryError::LibraryUnknown`] if the name is not registered;
    /// no state is changed in that case.
    pub fn set_default(&self, name: &str) -> Result<()> {
        if !self.libs.contains_key(name) {
            return Err(RegistryError::LibraryUnknown {
                name: name.to_string(),
                available: sorted_names(&self.libs),
            });
        }
        let mut guard = self
            .default
            .write()
            .map_err(|_| RegistryError::DefaultLockPoisoned)?;
        *guard = name.to_string();
        Ok(())
    }

    /// Read the current default-library name.
    pub fn default_name(&self) -> Result<String> {
        self.default
            .read()
            .map(|guard| guard.clone())
            .map_err(|_| RegistryError::DefaultLockPoisoned)
    }

    /// Project every registered library to a [`LibrarySummary`], sorted
    /// by name. The current default is flagged in `is_default`.
    pub fn list(&self) -> Result<Vec<LibrarySummary>> {
        let default = self
            .default
            .read()
            .map_err(|_| RegistryError::DefaultLockPoisoned)?
            .clone();
        let mut out: Vec<LibrarySummary> = self
            .libs
            .values()
            .map(|h| LibrarySummary {
                name: h.name().to_string(),
                dimension: h.ops().dimension(),
                is_default: h.name() == default,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Number of registered libraries.
    pub fn len(&self) -> usize {
        self.libs.len()
    }

    /// Whether the registry holds no libraries. Always `false` for
    /// registries built through [`from_handles`] or [`single`], which
    /// reject empty inputs; kept for completeness so `clippy` does not
    /// flag a bare `len`.
    ///
    /// [`from_handles`]: LibraryRegistry::from_handles
    /// [`single`]: LibraryRegistry::single
    pub fn is_empty(&self) -> bool {
        self.libs.is_empty()
    }
}

fn sorted_names<E: Embedder>(libs: &HashMap<String, Arc<LibraryHandle<E>>>) -> Vec<String> {
    let mut names: Vec<String> = libs.keys().cloned().collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use bookrack_embed::{EmbedError, Embedder};
    use bookrack_glean::GleanParams;
    use bookrack_ingest::IngestParams;

    use crate::{Caller, Ops};

    use super::{LibraryHandle, LibraryRegistry, RegistryError};

    /// Test stub: implements [`Embedder`] without any network or model.
    /// The registry never invokes embedding; this exists only so the
    /// generic [`Ops`] type parameter resolves.
    struct FakeEmbedder;

    impl Embedder for FakeEmbedder {
        async fn embed_batch(
            &self,
            _texts: &[String],
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(Vec::new())
        }
    }

    fn fake_ops() -> Ops<FakeEmbedder> {
        Ops::catalog_only(
            PathBuf::from("/dev/null/corpus.db"),
            PathBuf::from("/dev/null/catalog.db"),
            Path::new("/dev/null/lancedb"),
            PathBuf::from("/dev/null/books"),
            PathBuf::from("/dev/null/backup"),
            Caller::cli(),
        )
    }

    fn handle(name: &str) -> std::sync::Arc<LibraryHandle<FakeEmbedder>> {
        LibraryHandle::new(name, fake_ops())
    }

    #[test]
    fn single_registers_one_default() {
        let reg = LibraryRegistry::single(handle("prod"));
        assert_eq!(reg.default_name().unwrap(), "prod");
        assert!(reg.get(None).is_ok());
        assert_eq!(reg.get(None).unwrap().name(), "prod");
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
    }

    #[test]
    fn get_named_returns_named_handle() {
        let reg = LibraryRegistry::from_handles([handle("a"), handle("b")], "a").unwrap();
        assert_eq!(reg.get(Some("b")).unwrap().name(), "b");
        assert_eq!(reg.get(None).unwrap().name(), "a");
    }

    #[test]
    fn get_unknown_lists_available_sorted() {
        let reg =
            LibraryRegistry::from_handles([handle("paper"), handle("books")], "paper").unwrap();
        match reg.get(Some("missing")) {
            Err(RegistryError::LibraryUnknown { name, available }) => {
                assert_eq!(name, "missing");
                assert_eq!(available, vec!["books".to_string(), "paper".to_string()]);
            }
            Err(other) => panic!("expected LibraryUnknown, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn set_default_moves_pointer_and_get_none_follows() {
        let reg = LibraryRegistry::from_handles([handle("a"), handle("b")], "a").unwrap();
        reg.set_default("b").unwrap();
        assert_eq!(reg.default_name().unwrap(), "b");
        assert_eq!(reg.get(None).unwrap().name(), "b");
    }

    #[test]
    fn set_default_unknown_rejects_without_mutation() {
        let reg = LibraryRegistry::from_handles([handle("a"), handle("b")], "a").unwrap();
        assert!(matches!(
            reg.set_default("zzz"),
            Err(RegistryError::LibraryUnknown { .. })
        ));
        assert_eq!(reg.default_name().unwrap(), "a");
    }

    #[test]
    fn from_handles_rejects_empty() {
        let empty: [std::sync::Arc<LibraryHandle<FakeEmbedder>>; 0] = [];
        assert!(matches!(
            LibraryRegistry::from_handles(empty, "anything"),
            Err(RegistryError::Empty)
        ));
    }

    #[test]
    fn from_handles_rejects_unknown_default() {
        match LibraryRegistry::from_handles([handle("a")], "ghost") {
            Err(RegistryError::LibraryUnknown { name, available }) => {
                assert_eq!(name, "ghost");
                assert_eq!(available, vec!["a".to_string()]);
            }
            Err(other) => panic!("expected LibraryUnknown, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[tokio::test]
    async fn ingest_book_on_catalog_only_handle_errors_without_running_ingest() {
        let h = handle("books");
        let err = h
            .ingest_book(Path::new("/does/not/exist.epub"), &IngestParams::default())
            .await
            .expect_err("catalog-only handle has no embedder");
        let msg = format!("{err}");
        assert!(
            msg.contains("library handle with an embedder"),
            "expected the catalog-only guard message, got: {msg}",
        );
    }

    #[tokio::test]
    async fn glean_paper_without_papers_backend_errors_without_running_glean() {
        let h = handle("books");
        let err = h
            .glean_paper(Path::new("/does/not/exist.pdf"), &GleanParams::default())
            .await
            .expect_err("handle without papers backend must refuse glean");
        let msg = format!("{err}");
        assert!(
            msg.contains("no papers backend"),
            "expected the no-papers-backend guard message, got: {msg}",
        );
    }

    #[test]
    fn list_returns_sorted_entries_with_default_flagged() {
        let reg = LibraryRegistry::from_handles(
            [handle("paper"), handle("books"), handle("journals")],
            "books",
        )
        .unwrap();
        let summaries = reg.list().unwrap();
        assert_eq!(summaries.len(), 3);
        assert_eq!(summaries[0].name, "books");
        assert!(summaries[0].is_default);
        assert_eq!(summaries[1].name, "journals");
        assert!(!summaries[1].is_default);
        assert_eq!(summaries[2].name, "paper");
        assert!(!summaries[2].is_default);
        for summary in summaries {
            assert!(summary.dimension.is_none());
        }
    }
}
