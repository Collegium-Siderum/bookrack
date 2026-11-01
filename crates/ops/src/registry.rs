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

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use bookrack_catalog::{Catalog, RunHandle, RunLock};
use bookrack_config::Config;
use bookrack_corpus::Corpus;
use bookrack_embed::Embedder;
use bookrack_glean::{GleanParams, GleanReport};
use bookrack_ingest::ocr::{OcrIngestParams, OcrIngestReport, ingest_ocr_intake_with_progress};
use bookrack_ingest::{IngestParams, IngestReport, OnEmbedBatch};
use eyre::WrapErr;
use tokio::sync::Mutex as AsyncMutex;

use bookrack_core::{Explain, Problem};

use crate::Ops;
use crate::OpsError;

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

    /// A mount was attempted under a name the registry already carries.
    #[error("a library named {name:?} is already mounted")]
    AlreadyMounted {
        /// The name the caller asked to mount.
        name: String,
    },

    /// A call naming no library has no single library to go to: the
    /// registry default is not mounted, or none is declared and more
    /// than one library is. `mounted` lists the names an explicit call
    /// would reach, sorted; `default` is the registry's pointer as
    /// declared, `None` when it declares none.
    #[error(
        "cannot tell which library a call naming none is for (mounted: {}; {})",
        mounted.join(", "),
        match default {
            Some(name) => format!("the registry default {name:?} is not mounted"),
            None => "the registry names no default".to_string(),
        }
    )]
    UnnamedAmbiguous {
        /// Names the registry currently carries, sorted.
        mounted: Vec<String>,
        /// The registry's `default` pointer as declared.
        default: Option<String>,
    },

    /// The named library is registered but the host decided not to
    /// serve it; `reason` is why, as recorded at that decision.
    #[error("library {name:?} is registered but not mounted: {}", .reason.summary)]
    LibraryUnmounted {
        /// The name the caller asked for.
        name: String,
        /// Why the host does not serve it. Boxed to keep the error
        /// small on every `Result` that carries it.
        reason: Box<Problem>,
    },

    /// The internal `RwLock` guarding the default-library name was
    /// poisoned. Indicates a bug — a panic while a writer held the lock
    /// — rather than a misuse path the caller can recover from.
    #[error("internal: default-library lock poisoned")]
    DefaultLockPoisoned,

    /// The internal `RwLock` guarding the mounted-library map was
    /// poisoned. Indicates a bug — a panic while a writer held the lock
    /// — rather than a misuse path the caller can recover from.
    #[error("internal: mounted-library lock poisoned")]
    LibsLockPoisoned,
}

impl Explain for RegistryError {
    fn explain(&self) -> Problem {
        match self {
            // The summary names the name that did not resolve; the
            // names that would are the caller's next step, so they go
            // in the hint rather than beside the failure.
            Self::LibraryUnknown { name, available } => {
                let problem = Problem::new(format!("no library named {name:?}"));
                if available.is_empty() {
                    problem.hint("Register one with `bookrack libraries register`.")
                } else {
                    problem.hint(format!("Use one of: {}.", available.join(", ")))
                }
            }
            Self::Empty => Problem::new(self.to_string())
                .hint("Register a library with `bookrack libraries register`."),
            Self::AlreadyMounted { .. } => Problem::new(self.to_string())
                .hint("It is already being served, so there is nothing to mount."),
            // The summary states the fact; which libraries are mounted
            // and why none of them is the route are the evidence, and
            // naming one is the next step.
            // The record's own detail and hint travel with it. The
            // record already closes its hint with the step that clears
            // it; one without a hint gets that step alone.
            Self::LibraryUnmounted { name, reason } => {
                Problem::new(format!("library {name:?} is registered but not mounted"))
                    .detail(match &reason.data.detail {
                        Some(detail) => {
                            format!("Bring-up skipped it: {}. {detail}", reason.summary)
                        }
                        None => format!("Bring-up skipped it: {}.", reason.summary),
                    })
                    .hint(match &reason.data.hint {
                        Some(hint) => hint.clone(),
                        None => format!("Run `bookrack libraries mount {name}`."),
                    })
            }
            Self::UnnamedAmbiguous { mounted, default } => {
                let pointer = match default {
                    Some(name) => format!("The registry default {name:?} is not mounted."),
                    None => "The registry names no default.".to_string(),
                };
                Problem::new("cannot tell which library a call naming none is for")
                    .detail(format!(
                        "Mounted libraries: {}. {pointer}",
                        mounted.join(", ")
                    ))
                    .hint(
                        "Name one with `--library <name>` on the command line, or pass the \
                         `library` argument over MCP.",
                    )
            }
            // A poisoned lock is a bug: there is no next step to write.
            Self::DefaultLockPoisoned | Self::LibsLockPoisoned => Problem::from_error_chain(self),
        }
    }
}

/// A fallible registry operation.
pub type Result<T> = std::result::Result<T, RegistryError>;

/// An opaque token whose drop marks a mounted library as no longer
/// held. The registry never inspects it; a host that guards a data root
/// while it serves the library puts that guard here, so releasing it is
/// tied to the lifetime of the handle rather than to the moment the
/// name leaves the map.
pub type MountGuard = Arc<dyn Send + Sync>;

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
///
/// The handle also carries an opaque [`MountGuard`]. The runtime puts
/// the data root's exclusive lock there, so the lock outlives every
/// in-flight caller: a handle resolved before an unmount keeps the root
/// locked until the last clone of that handle is dropped.
pub struct LibraryHandle<E: Embedder> {
    name: String,
    cfg: Arc<Config>,
    templates: JobTemplates,
    ops: Arc<Ops<E>>,
    ingest_lock: AsyncMutex<()>,
    glean_lock: AsyncMutex<()>,
    _mount_guard: Option<MountGuard>,
}

/// The pipeline parameters a queue job for this library starts from.
///
/// Resolved once per library, from that library's own configuration and
/// data root: the audit profile and data overlays under
/// `<data_root>/audit-rules/`, the heading patterns, and the embedding
/// model its index profile declares. A job then patches only what the
/// job itself carries (`force`, `hold_for_metadata`, a named
/// `audit_profile`).
///
/// They live on the handle because the alternative is a process-wide
/// template, and a process serving several libraries has no single
/// right answer for one: whichever library the daemon was started
/// under would decide how every other library's books are ingested.
#[derive(Debug, Clone, Default)]
pub struct JobTemplates {
    /// Book-side ingest parameters.
    pub ingest: IngestParams,
    /// Paper-side glean parameters.
    pub glean: GleanParams,
}

impl<E: Embedder> LibraryHandle<E> {
    /// Wrap an already-warm [`Ops`] under the given short name, paired
    /// with the configuration that library resolves to.
    ///
    /// Job templates default; a host that runs pipeline jobs builds
    /// them from the library's own configuration and passes them to
    /// [`LibraryHandle::with_templates`] instead.
    pub fn new(name: impl Into<String>, cfg: Arc<Config>, ops: Ops<E>) -> Arc<LibraryHandle<E>> {
        LibraryHandle::from_arc(name, cfg, Arc::new(ops))
    }

    /// [`LibraryHandle::new`] with the pipeline parameters queue jobs
    /// for this library start from, and the opaque token that marks
    /// this library as held for as long as the handle lives.
    pub fn with_templates(
        name: impl Into<String>,
        cfg: Arc<Config>,
        ops: Ops<E>,
        templates: JobTemplates,
        guard: Option<MountGuard>,
    ) -> Arc<LibraryHandle<E>> {
        let handle = LibraryHandle::from_arc(name, cfg, Arc::new(ops));
        // `from_arc` owns the construction guard; rebuilding the value
        // here would duplicate it, so take the checked handle apart
        // through its own fields.
        Arc::new(LibraryHandle {
            name: handle.name.clone(),
            cfg: Arc::clone(&handle.cfg),
            templates,
            ops: Arc::clone(&handle.ops),
            ingest_lock: AsyncMutex::new(()),
            glean_lock: AsyncMutex::new(()),
            _mount_guard: guard,
        })
    }

    /// Wrap a pre-shared [`Arc<Ops>`] under the given short name.
    /// Useful when the caller already holds a shared handle and wants to
    /// register it without bumping the strong-count more than necessary.
    ///
    /// `cfg` must be the configuration `ops` was opened from: the two
    /// travel together from here on, and a handler that resolves a
    /// handle takes both from it. Pairing a library's stores with
    /// another library's configuration is the defect this pairing
    /// exists to make impossible, so a mismatched pair trips a
    /// debug assertion at the construction site rather than surfacing
    /// as a library served under the wrong declaration.
    pub fn from_arc(
        name: impl Into<String>,
        cfg: Arc<Config>,
        ops: Arc<Ops<E>>,
    ) -> Arc<LibraryHandle<E>> {
        debug_assert_eq!(
            cfg.catalog_db(),
            ops.catalog_db(),
            "a library handle's configuration and stores must be the same library's",
        );
        Arc::new(LibraryHandle {
            name: name.into(),
            cfg,
            templates: JobTemplates::default(),
            ops,
            ingest_lock: AsyncMutex::new(()),
            glean_lock: AsyncMutex::new(()),
            _mount_guard: None,
        })
    }

    /// The short name this library is registered under.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The configuration this library resolves to.
    ///
    /// The one way a caller holding a handle reaches that library's
    /// configuration: a `Config` taken from anywhere else is some
    /// other library's under an eager multi-mount daemon.
    pub fn cfg(&self) -> &Config {
        &self.cfg
    }

    /// A new strong reference to this library's configuration, for a
    /// caller that has to move it into a task or a template.
    pub fn cfg_arc(&self) -> Arc<Config> {
        Arc::clone(&self.cfg)
    }

    /// The pipeline parameters a job for this library starts from.
    pub fn templates(&self) -> &JobTemplates {
        &self.templates
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
    /// stored paths, then forwards to
    /// [`bookrack_ingest::ingest_book_with_progress`] with the library's
    /// warm embedder, passing `on_embed_batch` through. The caller —
    /// currently the `bookrack run` REPL's queue worker — never opens a
    /// database on its own.
    ///
    /// Returns an error when this handle was built catalog-only, since
    /// ingest requires the embedder that lives on the
    /// [`bookrack_query::Library`] half.
    pub async fn ingest_book(
        &self,
        path: &Path,
        params: &IngestParams,
        on_embed_batch: &OnEmbedBatch<'_>,
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
        let report = bookrack_ingest::ingest_book_with_progress(
            path,
            &mut corpus,
            &mut catalog,
            self.ops.lancedb_dir(),
            self.ops.books_dir(),
            embedder,
            params,
            on_embed_batch,
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
    /// [`bookrack_ingest::ocr::ingest_ocr_intake_with_progress`] with
    /// the library's warm embedder, reporting each EMBED batch to
    /// `on_embed_batch` as [`Self::ingest_book`] does. Refreshes the
    /// book-side [`Library`] store so the first OCR intake into a
    /// previously empty data dir lights up the read path on the same
    /// lock.
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
        on_embed_batch: &OnEmbedBatch<'_>,
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
        let report = ingest_ocr_intake_with_progress(
            &mut corpus,
            &mut catalog,
            self.ops.lancedb_dir(),
            self.ops.books_dir(),
            ocr_md,
            from_pdf,
            embedder,
            params,
            ocr_params,
            on_embed_batch,
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
        // A caller that already opened a `pipeline_runs` row keeps
        // ownership of its lifecycle; otherwise this handle opens one
        // on the paper catalog so the `node_paper_audit` row lands
        // with a run id instead of NULL.
        let owned_run = if params.pipeline_run_id.is_none() {
            open_glean_pipeline_run(&catalog, catalog_db)
        } else {
            None
        };
        let mut effective = params.clone();
        if let Some(run) = owned_run.as_ref() {
            effective.pipeline_run_id = Some(run.id().to_string());
        }
        let result = bookrack_glean::glean_paper(
            path,
            &mut corpus,
            &mut catalog,
            lancedb_dir,
            papers_dir,
            embedder,
            &effective,
        )
        .await
        .context("registry-mediated glean");
        if let Some(run) = owned_run {
            finalize_glean_pipeline_run(&catalog, run, result.is_ok());
        }
        let report = result?;
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
    ///
    /// The audit itself is
    /// [`crate::writes::papers_metadata::reaudit_paper_metadata`];
    /// what this wrapper adds is the glean lock, which serializes the
    /// re-audit against an ingest writing the same catalog.
    pub async fn reaudit_paper(
        &self,
        intake_id: i64,
        profile: &bookrack_glean::audit::PaperAuditProfile,
        data: &bookrack_glean::audit::PaperAuditData,
    ) -> std::result::Result<crate::dto::writes::ReauditOutcome, OpsError> {
        let _guard = self.glean_lock.lock().await;
        crate::writes::papers_metadata::reaudit_paper_metadata(
            &self.ops,
            crate::dto::writes::PaperReauditRequest { intake_id },
            data,
            profile,
        )
    }
}

/// One row of [`LibraryRegistry::list`] — the registered name, the root
/// the library was opened at, and the vector dimension its store was
/// opened at (when search is available on the handle).
#[derive(Debug, Clone)]
pub struct LibrarySummary {
    /// The library's registered short name.
    pub name: String,
    /// The data root this library's handle is open on, taken from the
    /// handle's own configuration rather than re-resolved.
    pub data_dir: PathBuf,
    /// The embedder dimension the vector store was opened at; `None`
    /// for catalog-only handles.
    pub dimension: Option<usize>,
    /// Whether this is the registry's current default library.
    pub is_default: bool,
}

/// A registered library the host decided not to serve, and why.
///
/// Built only through [`UnmountedLibrary::new`], which runs every part
/// of the reason through [`redact_source_paths`]: the reason reaches
/// status cards and queue rows as plain text, so it is cleaned where
/// it is made rather than at each surface that shows it.
///
/// The host closes the reason's hint, when it has one, with the step
/// that serves the library again before it records the reason; a call
/// naming the library carries that hint as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmountedLibrary {
    name: String,
    data_dir: PathBuf,
    reason: Problem,
}

impl UnmountedLibrary {
    /// Record that `name`, registered at `data_dir`, is not served
    /// because of `reason`.
    pub fn new(name: impl Into<String>, data_dir: impl Into<PathBuf>, reason: Problem) -> Self {
        UnmountedLibrary {
            name: name.into(),
            data_dir: data_dir.into(),
            reason: reason.redacted(),
        }
    }

    /// The library's registered short name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The data root the registry records for the library.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Why the library is not served.
    pub fn reason(&self) -> &Problem {
        &self.reason
    }
}

/// Both of a registry's tables, read under one lock: the libraries it
/// serves and the ones it recorded as not served.
#[derive(Debug, Clone)]
pub struct RegistrySnapshot {
    /// The served libraries, sorted by name, with the unnamed route
    /// marked as [`LibraryRegistry::list`] marks it.
    pub mounted: Vec<LibrarySummary>,
    /// The libraries recorded as not served, sorted by name.
    pub unmounted: Vec<UnmountedLibrary>,
}

/// The two tables behind one lock: a name is in at most one of them,
/// and a reader sees both at the same instant.
struct Sets<E: Embedder> {
    mounted: HashMap<String, Arc<LibraryHandle<E>>>,
    unmounted: BTreeMap<String, UnmountedLibrary>,
}

/// The in-process scheduler: a map of registered libraries keyed by
/// short name, plus a mutable pointer to the default library a caller
/// that names none is routed to.
///
/// The default pointer is a cache of the on-disk registry's `default`
/// selection, not a second source of truth: it is seeded from the
/// registry at bring-up and re-synced whenever `library.set_default`
/// writes the registry, so the two agree after every write the daemon
/// performs. An external write to the registry does not push into this
/// cache; a restart re-seeds it. The pointer may name a library that is
/// not mounted, or nothing at all; where an unnamed call then goes is
/// [`get`]'s rule.
///
/// Beside the mounted set the registry keeps the libraries the host
/// decided not to serve, each with its reason
/// ([`record_unmounted`]). A call naming one of them is refused with
/// that reason, and [`mount`] clears the record when the library comes
/// up after all.
///
/// Held behind an `Arc` and shared across the REPL task, the MCP
/// server, and the queue worker. Routing in this phase is "look up
/// handle, hand it back"; future scheduling logic — priority,
/// throttling, preemption — lands here without touching callers.
///
/// The mounted set is itself mutable: [`mount`] and [`unmount`] add and
/// remove libraries while the host runs. Every read hands back a cloned
/// `Arc`, so a caller that resolved a handle keeps serving from it —
/// and keeps that handle's [`MountGuard`] alive — after the name has
/// left the map.
///
/// [`get`]: LibraryRegistry::get
/// [`mount`]: LibraryRegistry::mount
/// [`unmount`]: LibraryRegistry::unmount
/// [`record_unmounted`]: LibraryRegistry::record_unmounted
pub struct LibraryRegistry<E: Embedder> {
    sets: RwLock<Sets<E>>,
    default: RwLock<Option<String>>,
}

impl<E: Embedder> LibraryRegistry<E> {
    /// Build a registry from an iterator of [`LibraryHandle`]s and the
    /// registry's `default` pointer as declared, `None` when it declares
    /// none. The pointer need not name a handle: a default the mounted
    /// set lacks is kept, and [`get`] refuses the unnamed call for as
    /// long as that is so. Returns [`RegistryError::Empty`] if the
    /// iterator yields no handle.
    ///
    /// [`get`]: LibraryRegistry::get
    pub fn from_handles(
        handles: impl IntoIterator<Item = Arc<LibraryHandle<E>>>,
        default: Option<String>,
    ) -> Result<Arc<LibraryRegistry<E>>> {
        let mut mounted: HashMap<String, Arc<LibraryHandle<E>>> = HashMap::new();
        for handle in handles {
            mounted.insert(handle.name().to_string(), handle);
        }
        if mounted.is_empty() {
            return Err(RegistryError::Empty);
        }
        Ok(Arc::new(LibraryRegistry {
            sets: RwLock::new(Sets {
                mounted,
                unmounted: BTreeMap::new(),
            }),
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
        let mut mounted: HashMap<String, Arc<LibraryHandle<E>>> = HashMap::new();
        mounted.insert(name.clone(), handle);
        Arc::new(LibraryRegistry {
            sets: RwLock::new(Sets {
                mounted,
                unmounted: BTreeMap::new(),
            }),
            default: RwLock::new(Some(name)),
        })
    }

    fn read_sets(&self) -> Result<std::sync::RwLockReadGuard<'_, Sets<E>>> {
        self.sets
            .read()
            .map_err(|_| RegistryError::LibsLockPoisoned)
    }

    fn write_sets(&self) -> Result<std::sync::RwLockWriteGuard<'_, Sets<E>>> {
        self.sets
            .write()
            .map_err(|_| RegistryError::LibsLockPoisoned)
    }

    fn read_default(&self) -> Result<Option<String>> {
        self.default
            .read()
            .map(|guard| guard.clone())
            .map_err(|_| RegistryError::DefaultLockPoisoned)
    }

    /// Look up a handle. `None` is a call that names no library, routed
    /// by `unnamed_route`: the default pointer when it is mounted, the
    /// only mounted library when no default is declared, and otherwise
    /// [`RegistryError::UnnamedAmbiguous`] — a declared default that is
    /// not mounted refuses rather than falling back to whatever is.
    ///
    /// A name recorded as not served is
    /// [`RegistryError::LibraryUnmounted`], carrying the reason; a name
    /// in neither table is [`RegistryError::LibraryUnknown`].
    pub fn get(&self, name: Option<&str>) -> Result<Arc<LibraryHandle<E>>> {
        let default = self.read_default()?;
        let sets = self.read_sets()?;
        match name {
            Some(key) => match sets.mounted.get(key) {
                Some(handle) => Ok(Arc::clone(handle)),
                None => match sets.unmounted.get(key) {
                    Some(record) => Err(RegistryError::LibraryUnmounted {
                        name: key.to_string(),
                        reason: Box::new(record.reason.clone()),
                    }),
                    None => Err(RegistryError::LibraryUnknown {
                        name: key.to_string(),
                        available: sorted_names(&sets.mounted),
                    }),
                },
            },
            None => unnamed_route(&sets.mounted, default.as_deref()).cloned(),
        }
    }

    /// Move the default-library pointer to `name`. Returns
    /// [`RegistryError::LibraryUnknown`] if the name is not mounted;
    /// no state is changed in that case.
    pub fn set_default(&self, name: &str) -> Result<()> {
        {
            let sets = self.read_sets()?;
            if !sets.mounted.contains_key(name) {
                return Err(RegistryError::LibraryUnknown {
                    name: name.to_string(),
                    available: sorted_names(&sets.mounted),
                });
            }
        }
        let mut guard = self
            .default
            .write()
            .map_err(|_| RegistryError::DefaultLockPoisoned)?;
        *guard = Some(name.to_string());
        Ok(())
    }

    /// Add a library to the mounted set, clearing any record of it as
    /// not served. Returns [`RegistryError::AlreadyMounted`] when the
    /// name is already served; the caller's handle is dropped in that
    /// case and nothing changes.
    pub fn mount(&self, handle: Arc<LibraryHandle<E>>) -> Result<()> {
        let mut sets = self.write_sets()?;
        if sets.mounted.contains_key(handle.name()) {
            return Err(RegistryError::AlreadyMounted {
                name: handle.name().to_string(),
            });
        }
        sets.unmounted.remove(handle.name());
        sets.mounted.insert(handle.name().to_string(), handle);
        Ok(())
    }

    /// Record a library the host decided not to serve. Returns
    /// [`RegistryError::AlreadyMounted`] when the name is served, since
    /// a library cannot be both; a record under the same name is
    /// replaced.
    pub fn record_unmounted(&self, library: UnmountedLibrary) -> Result<()> {
        let mut sets = self.write_sets()?;
        if sets.mounted.contains_key(&library.name) {
            return Err(RegistryError::AlreadyMounted { name: library.name });
        }
        sets.unmounted.insert(library.name.clone(), library);
        Ok(())
    }

    /// Remove a library from the mounted set and hand its handle back.
    /// Returns [`RegistryError::LibraryUnknown`] when the name is not
    /// mounted.
    ///
    /// Removal takes the name out of routing immediately, but the
    /// library's [`MountGuard`] is released only when the last clone of
    /// the returned handle is dropped — including clones a caller
    /// resolved before this call. Dropping the return value is
    /// therefore a request to release, not a guarantee that the release
    /// has happened.
    pub fn unmount(&self, name: &str) -> Result<Arc<LibraryHandle<E>>> {
        let mut sets = self.write_sets()?;
        match sets.mounted.remove(name) {
            Some(handle) => Ok(handle),
            None => Err(RegistryError::LibraryUnknown {
                name: name.to_string(),
                available: sorted_names(&sets.mounted),
            }),
        }
    }

    /// Read the current default-library name; `None` when the registry
    /// declares none.
    pub fn default_name(&self) -> Result<Option<String>> {
        self.read_default()
    }

    /// Project every registered library to a [`LibrarySummary`], sorted
    /// by name. `is_default` marks the library an unnamed call reaches,
    /// by the same rule [`get`] routes with; when that rule refuses, no
    /// row is marked and the listing still carries every row — the
    /// listing is what the shutdown path walks to release each root,
    /// and a route with nowhere to go is not a reason to skip one.
    ///
    /// [`get`]: LibraryRegistry::get
    pub fn list(&self) -> Result<Vec<LibrarySummary>> {
        let default = self.read_default()?;
        let sets = self.read_sets()?;
        Ok(summaries(&sets.mounted, default.as_deref()))
    }

    /// The libraries recorded as not served, sorted by name.
    pub fn unmounted(&self) -> Result<Vec<UnmountedLibrary>> {
        Ok(self.read_sets()?.unmounted.values().cloned().collect())
    }

    /// Both tables at one instant. A surface that reports the served
    /// and the unserved libraries side by side reads them here rather
    /// than through [`list`] and [`unmounted`] in turn: a mount landing
    /// between those two reads would show one name in both lists.
    ///
    /// [`list`]: LibraryRegistry::list
    /// [`unmounted`]: LibraryRegistry::unmounted
    pub fn snapshot(&self) -> Result<RegistrySnapshot> {
        let default = self.read_default()?;
        let sets = self.read_sets()?;
        Ok(RegistrySnapshot {
            mounted: summaries(&sets.mounted, default.as_deref()),
            unmounted: sets.unmounted.values().cloned().collect(),
        })
    }

    /// Number of mounted libraries. A poisoned map reads as zero, the
    /// same shape a caller already handles for an empty registry.
    pub fn len(&self) -> usize {
        self.sets.read().map(|sets| sets.mounted.len()).unwrap_or(0)
    }

    /// Whether the registry holds no libraries. `false` for registries
    /// built through [`from_handles`] or [`single`], which reject empty
    /// inputs, until every library is unmounted.
    ///
    /// [`from_handles`]: LibraryRegistry::from_handles
    /// [`single`]: LibraryRegistry::single
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The mounted set as sorted summaries, with the unnamed route marked.
fn summaries<E: Embedder>(
    mounted: &HashMap<String, Arc<LibraryHandle<E>>>,
    default: Option<&str>,
) -> Vec<LibrarySummary> {
    let route = unnamed_route(mounted, default)
        .ok()
        .map(|h| h.name().to_string());
    let mut out: Vec<LibrarySummary> = mounted
        .values()
        .map(|h| LibrarySummary {
            name: h.name().to_string(),
            data_dir: h.cfg().data_dir().to_path_buf(),
            dimension: h.ops().dimension(),
            is_default: Some(h.name()) == route.as_deref(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}
/// Where a call that names no library goes, from the mounted set and
/// the default pointer as declared:
///
/// 1. the default, when it names a mounted library;
/// 2. the only mounted library, when no default is declared;
/// 3. [`RegistryError::Empty`] when nothing is mounted;
/// 4. [`RegistryError::UnnamedAmbiguous`] otherwise — several
///    libraries and no default, or a declared default that is not
///    mounted, which refuses rather than landing on whatever is.
fn unnamed_route<'a, E: Embedder>(
    libs: &'a HashMap<String, Arc<LibraryHandle<E>>>,
    default: Option<&str>,
) -> Result<&'a Arc<LibraryHandle<E>>> {
    if let Some(name) = default
        && let Some(handle) = libs.get(name)
    {
        return Ok(handle);
    }
    if libs.is_empty() {
        return Err(RegistryError::Empty);
    }
    if default.is_none()
        && libs.len() == 1
        && let Some(only) = libs.values().next()
    {
        return Ok(only);
    }
    Err(RegistryError::UnnamedAmbiguous {
        mounted: sorted_names(libs),
        default: default.map(str::to_string),
    })
}

/// Open a `pipeline_runs` row on the paper catalog for one glean
/// invocation and take the run's liveness record beside `catalog_db`.
/// Run lifecycle is best-effort: an open failure demotes to a warning
/// and the glean proceeds with a NULL `pipeline_run_id`, and a record
/// that cannot be taken leaves the run registered but untracked.
fn open_glean_pipeline_run(catalog: &Catalog, catalog_db: &Path) -> Option<RunHandle> {
    let library_root = catalog_db.parent().and_then(Path::to_str);
    let pipeline_run_id = match catalog.open_pipeline_run("glean", None, library_root) {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(error = %err, "glean: open_pipeline_run failed");
            return None;
        }
    };
    let lock = bookrack_catalog::run_locks_dir(catalog_db).and_then(|dir| {
        RunLock::acquire(&dir, &pipeline_run_id)
            .inspect_err(|err| {
                tracing::warn!(
                    error = %err,
                    pipeline_run_id,
                    "glean: run liveness record could not be taken",
                );
            })
            .ok()
    });
    Some(RunHandle::new(pipeline_run_id, lock))
}

/// Close the run row and refresh its rollup. Best-effort: any error
/// here logs and the glean outcome stays untouched. A row that could
/// not be stamped keeps its liveness record, so a repair can close it
/// later instead of it reading as a live run forever.
fn finalize_glean_pipeline_run(catalog: &Catalog, run: RunHandle, ok: bool) {
    let status = if ok { "ok" } else { "error" };
    let pipeline_run_id = run.id().to_string();
    if let Err(err) = catalog.close_pipeline_run(&pipeline_run_id, status) {
        tracing::warn!(error = %err, pipeline_run_id, "glean: close_pipeline_run failed");
        run.abandon();
        return;
    }
    drop(run);
    if let Err(err) = catalog.compute_run_summary(&pipeline_run_id) {
        tracing::warn!(error = %err, pipeline_run_id, "glean: compute_run_summary failed");
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

    use bookrack_config::Config;

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

    /// A configuration over a root that is never opened. Paths are
    /// derived from it rather than written out, so the stores below and
    /// the handle's configuration name the same library.
    fn fake_cfg(root: &str) -> std::sync::Arc<Config> {
        std::sync::Arc::new(Config::new(
            PathBuf::from(root),
            "http://127.0.0.1:11434".to_string(),
        ))
    }

    fn fake_ops(cfg: &Config) -> Ops<FakeEmbedder> {
        Ops::catalog_only(
            cfg.corpus_db(),
            cfg.catalog_db(),
            &cfg.lancedb_dir(),
            cfg.books_dir(),
            cfg.backup_dir(),
            Caller::cli(),
        )
    }

    fn handle(name: &str) -> std::sync::Arc<LibraryHandle<FakeEmbedder>> {
        let cfg = fake_cfg(&format!("/dev/null/{name}"));
        let ops = fake_ops(&cfg);
        LibraryHandle::new(name, cfg, ops)
    }

    /// The construction guard is the whole point of pairing a handle's
    /// configuration with its stores: a pair naming two different
    /// libraries is the defect this pairing exists to prevent, so it
    /// must not be constructible in a debug build.
    ///
    /// `debug_assert` compiles out under `--release`; this test asserts
    /// the guard, so it is a debug-profile test by nature.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "the same library's")]
    fn a_handle_pairing_two_libraries_trips_the_construction_guard() {
        let cfg = fake_cfg("/dev/null/alpha");
        let ops = fake_ops(&fake_cfg("/dev/null/beta"));
        let _ = LibraryHandle::new("alpha", cfg, ops);
    }

    /// A paper catalog on disk, since the run's liveness record lives
    /// beside the database file.
    fn glean_catalog(dir: &tempfile::TempDir) -> (PathBuf, bookrack_catalog::Catalog) {
        let path = dir.path().join("papers_catalog.db");
        let catalog = bookrack_catalog::Catalog::open(&path).expect("open catalog");
        (path, catalog)
    }

    #[test]
    fn glean_run_lifecycle_opens_closes_and_rolls_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, catalog) = glean_catalog(&dir);
        let locks = bookrack_catalog::run_locks_dir(&path).expect("lock dir");

        let handle = super::open_glean_pipeline_run(&catalog, &path).expect("run id");
        let id = handle.id().to_string();
        let run = catalog.pipeline_run(&id).expect("read").expect("present");
        assert_eq!(run.command, "glean");
        assert_eq!(run.status.as_deref(), Some("running"));
        assert!(run.finished_at.is_none());
        // An in-flight run is not mistakable for an abandoned one.
        assert_eq!(
            bookrack_catalog::run_liveness(&locks, &id),
            bookrack_catalog::RunLiveness::Held,
        );

        super::finalize_glean_pipeline_run(&catalog, handle, true);
        let run = catalog.pipeline_run(&id).expect("read").expect("present");
        assert_eq!(run.status.as_deref(), Some("ok"));
        assert!(run.finished_at.is_some());
        assert_eq!(
            bookrack_catalog::run_liveness(&locks, &id),
            bookrack_catalog::RunLiveness::NoRecord,
        );
        let summary = catalog
            .pipeline_run_summary(&id)
            .expect("read")
            .expect("rollup row present");
        assert_eq!(summary.n_papers, 0);
        assert_eq!(summary.n_books, 0);
    }

    #[test]
    fn glean_run_finalize_records_error_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, catalog) = glean_catalog(&dir);
        let handle = super::open_glean_pipeline_run(&catalog, &path).expect("run id");
        let id = handle.id().to_string();
        super::finalize_glean_pipeline_run(&catalog, handle, false);
        let run = catalog.pipeline_run(&id).expect("read").expect("present");
        assert_eq!(run.status.as_deref(), Some("error"));
    }

    #[test]
    fn single_registers_one_default() {
        let reg = LibraryRegistry::single(handle("prod"));
        assert_eq!(reg.default_name().unwrap().as_deref(), Some("prod"));
        assert!(reg.get(None).is_ok());
        assert_eq!(reg.get(None).unwrap().name(), "prod");
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
    }

    #[test]
    fn get_named_returns_named_handle() {
        let reg =
            LibraryRegistry::from_handles([handle("a"), handle("b")], Some("a".into())).unwrap();
        assert_eq!(reg.get(Some("b")).unwrap().name(), "b");
        assert_eq!(reg.get(None).unwrap().name(), "a");
    }

    #[test]
    fn get_unknown_lists_available_sorted() {
        let reg =
            LibraryRegistry::from_handles([handle("paper"), handle("books")], Some("paper".into()))
                .unwrap();
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
        let reg =
            LibraryRegistry::from_handles([handle("a"), handle("b")], Some("a".into())).unwrap();
        reg.set_default("b").unwrap();
        assert_eq!(reg.default_name().unwrap().as_deref(), Some("b"));
        assert_eq!(reg.get(None).unwrap().name(), "b");
    }

    #[test]
    fn set_default_unknown_rejects_without_mutation() {
        let reg =
            LibraryRegistry::from_handles([handle("a"), handle("b")], Some("a".into())).unwrap();
        assert!(matches!(
            reg.set_default("zzz"),
            Err(RegistryError::LibraryUnknown { .. })
        ));
        assert_eq!(reg.default_name().unwrap().as_deref(), Some("a"));
    }

    #[test]
    fn from_handles_rejects_empty() {
        let empty: [std::sync::Arc<LibraryHandle<FakeEmbedder>>; 0] = [];
        assert!(matches!(
            LibraryRegistry::from_handles(empty, Some("anything".into())),
            Err(RegistryError::Empty)
        ));
    }

    fn expect_unnamed_ambiguous<T>(result: super::Result<T>) -> (Vec<String>, Option<String>) {
        match result {
            Err(RegistryError::UnnamedAmbiguous { mounted, default }) => (mounted, default),
            Err(other) => panic!("expected UnnamedAmbiguous, got {other:?}"),
            Ok(_) => panic!("expected UnnamedAmbiguous, got Ok"),
        }
    }

    /// A default the mounted set lacks is a routing fact, not a build
    /// error: the registry comes up and refuses the unnamed call.
    #[test]
    fn from_handles_keeps_an_unmounted_default() {
        let reg = LibraryRegistry::from_handles([handle("a")], Some("ghost".into()))
            .expect("an unmounted default is kept, not refused");
        let (mounted, default) = expect_unnamed_ambiguous(reg.get(None));
        assert_eq!(mounted, vec!["a".to_string()]);
        assert_eq!(default.as_deref(), Some("ghost"));
    }

    #[test]
    fn get_none_refuses_when_the_declared_default_is_not_mounted() {
        let reg = LibraryRegistry::from_handles(
            [handle("a"), handle("b"), handle("c")],
            Some("a".into()),
        )
        .unwrap();
        reg.unmount("a").unwrap();
        let (mounted, default) = expect_unnamed_ambiguous(reg.get(None));
        assert_eq!(mounted, vec!["b".to_string(), "c".to_string()]);
        assert_eq!(default.as_deref(), Some("a"));
    }

    #[test]
    fn get_none_routes_the_only_library_when_no_default_is_declared() {
        let reg = LibraryRegistry::from_handles([handle("only")], None).unwrap();
        assert_eq!(reg.get(None).unwrap().name(), "only");
        assert_eq!(reg.default_name().unwrap(), None);
    }

    #[test]
    fn get_none_refuses_two_libraries_without_a_default() {
        let reg = LibraryRegistry::from_handles([handle("a"), handle("b")], None).unwrap();
        let (mounted, default) = expect_unnamed_ambiguous(reg.get(None));
        assert_eq!(mounted, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(default, None);
    }

    /// `is_default` is where an unnamed call lands. When it lands
    /// nowhere the listing still succeeds and carries every row, with
    /// no row marked: the listing is read by the shutdown path, which
    /// has to walk every library whether or not one of them is the
    /// route.
    #[test]
    fn list_marks_the_unnamed_route_without_failing() {
        let two_no_default =
            LibraryRegistry::from_handles([handle("a"), handle("b")], None).unwrap();
        let rows = two_no_default.list().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| !row.is_default), "{rows:?}");

        let default_not_mounted =
            LibraryRegistry::from_handles([handle("a"), handle("b")], Some("ghost".into()))
                .unwrap();
        let rows = default_not_mounted.list().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| !row.is_default), "{rows:?}");

        let one_no_default = LibraryRegistry::from_handles([handle("a")], None).unwrap();
        let rows = one_no_default.list().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_default, "{rows:?}");
    }

    /// The unnamed route is refused with the mounted set as evidence
    /// and a way to name a library as the next step; the summary
    /// carries neither.
    #[test]
    fn unnamed_ambiguous_explains_the_mounted_set_and_the_pointer() {
        use bookrack_core::Explain;
        let problem = RegistryError::UnnamedAmbiguous {
            mounted: vec!["alpha".into(), "beta".into()],
            default: Some("gamma".into()),
        }
        .explain();
        assert_eq!(
            problem.summary,
            "cannot tell which library a call naming none is for"
        );
        let detail = problem.data.detail.expect("detail");
        assert!(detail.contains("alpha, beta"), "{detail}");
        assert!(detail.contains("\"gamma\" is not mounted"), "{detail}");
        let hint = problem.data.hint.expect("hint");
        assert!(hint.contains("--library <name>"), "{hint}");
        assert!(hint.contains("`library` argument"), "{hint}");

        let undeclared = RegistryError::UnnamedAmbiguous {
            mounted: vec!["alpha".into(), "beta".into()],
            default: None,
        }
        .explain();
        let detail = undeclared.data.detail.expect("detail");
        assert!(detail.contains("names no default"), "{detail}");
    }

    /// Compile-time registration: every variant is named here, so a
    /// variant added to the `#[non_exhaustive]` enum stops its author
    /// at a same-crate `match` and sends them to the wire mappings in
    /// the runtime and MCP crates, where a downstream `match` cannot
    /// be exhaustive and a new variant would otherwise take a default
    /// code.
    #[test]
    fn every_registry_error_variant_is_registered_for_wire_mapping() {
        fn assert_classified(e: &RegistryError) {
            match e {
                RegistryError::LibraryUnknown { .. }
                | RegistryError::Empty
                | RegistryError::AlreadyMounted { .. }
                | RegistryError::UnnamedAmbiguous { .. }
                | RegistryError::LibraryUnmounted { .. }
                | RegistryError::DefaultLockPoisoned
                | RegistryError::LibsLockPoisoned => {}
            }
        }
        assert_classified(&RegistryError::Empty);
    }

    #[tokio::test]
    async fn ingest_book_on_catalog_only_handle_errors_without_running_ingest() {
        let h = handle("books");
        let err = h
            .ingest_book(
                Path::new("/does/not/exist.epub"),
                &IngestParams::default(),
                &|_, _| {},
            )
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

    /// A mount guard that records its own release, so a test can tell
    /// "the name left the map" apart from "the token was dropped".
    struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A guarded handle plus the flag its guard sets when released.
    fn guarded_handle(
        name: &str,
    ) -> (
        std::sync::Arc<LibraryHandle<FakeEmbedder>>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let cfg = fake_cfg(&format!("/dev/null/{name}"));
        let ops = fake_ops(&cfg);
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard: super::MountGuard = std::sync::Arc::new(DropFlag(std::sync::Arc::clone(&flag)));
        let handle = LibraryHandle::with_templates(
            name,
            cfg,
            ops,
            super::JobTemplates::default(),
            Some(guard),
        );
        (handle, flag)
    }

    fn released(flag: &std::sync::atomic::AtomicBool) -> bool {
        flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[test]
    fn mount_adds_a_name_to_the_listing() {
        let reg = LibraryRegistry::from_handles([handle("a")], Some("a".into())).unwrap();
        reg.mount(handle("b")).unwrap();
        let names: Vec<String> = reg.list().unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(reg.get(Some("b")).unwrap().name(), "b");
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn unmount_removes_it_and_drops_the_guard() {
        let (b, flag) = guarded_handle("b");
        let reg = LibraryRegistry::from_handles([handle("a")], Some("a".into())).unwrap();
        reg.mount(b).unwrap();
        assert!(
            !released(&flag),
            "the guard is held while the library is mounted"
        );

        let removed = reg.unmount("b").unwrap();
        assert_eq!(removed.name(), "b");
        assert!(
            !released(&flag),
            "the caller still holds the returned handle, so the guard is still held",
        );
        drop(removed);
        assert!(
            released(&flag),
            "dropping the last handle releases the mount guard",
        );
        assert!(matches!(
            reg.get(Some("b")),
            Err(RegistryError::LibraryUnknown { .. })
        ));
    }

    /// The whole point of hanging the guard on the handle rather than on
    /// the map entry: a caller that resolved a handle before the unmount
    /// keeps the library held until it is done with it.
    #[test]
    fn unmount_keeps_the_guard_alive_while_a_handle_clone_lives() {
        let (b, flag) = guarded_handle("b");
        let reg = LibraryRegistry::from_handles([handle("a")], Some("a".into())).unwrap();
        reg.mount(b).unwrap();

        let in_flight = reg.get(Some("b")).unwrap();
        let removed = reg.unmount("b").unwrap();
        drop(removed);
        assert!(
            !released(&flag),
            "an in-flight caller's clone must keep the guard alive past the unmount",
        );
        drop(in_flight);
        assert!(
            released(&flag),
            "the guard is released when the last clone goes away",
        );
    }

    #[test]
    fn mount_refuses_a_name_already_mounted() {
        let reg = LibraryRegistry::from_handles([handle("a")], Some("a".into())).unwrap();
        match reg.mount(handle("a")) {
            Err(RegistryError::AlreadyMounted { name }) => assert_eq!(name, "a"),
            Err(other) => panic!("expected AlreadyMounted, got {other:?}"),
            Ok(()) => panic!("expected error, got Ok"),
        }
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn unmount_refuses_a_name_that_is_not_mounted() {
        let reg = LibraryRegistry::from_handles([handle("a")], Some("a".into())).unwrap();
        match reg.unmount("ghost") {
            Err(RegistryError::LibraryUnknown { name, available }) => {
                assert_eq!(name, "ghost");
                assert_eq!(available, vec!["a".to_string()]);
            }
            Err(other) => panic!("expected LibraryUnknown, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn list_returns_sorted_entries_with_default_flagged() {
        let reg = LibraryRegistry::from_handles(
            [handle("paper"), handle("books"), handle("journals")],
            Some("books".into()),
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

    /// With no library to offer, the hint names the registering step
    /// instead of an empty list.
    #[test]
    fn unknown_library_against_an_empty_registry_hints_at_registering() {
        use bookrack_core::Explain;
        let problem = RegistryError::LibraryUnknown {
            name: "ghost".into(),
            available: Vec::new(),
        }
        .explain();
        assert_eq!(problem.summary, "no library named \"ghost\"");
        assert!(problem.data.detail.is_none(), "{:?}", problem.data.detail);
        let hint = problem.data.hint.expect("hint");
        assert!(hint.contains("libraries register"), "{hint}");
    }

    /// A poisoned lock is a bug: it has no next step to write.
    #[test]
    fn a_poisoned_lock_explains_without_a_hint() {
        use bookrack_core::Explain;
        for err in [
            RegistryError::DefaultLockPoisoned,
            RegistryError::LibsLockPoisoned,
        ] {
            let problem = err.explain();
            assert_eq!(problem.summary, err.to_string());
            assert!(problem.data.hint.is_none(), "{:?}", problem.data.hint);
        }
    }

    /// A library recorded as not served is refused by name with its
    /// reason, listed as unmounted, and both end when it is mounted.
    #[test]
    fn a_recorded_unmounted_library_is_refused_with_its_reason_until_mounted() {
        let reg = LibraryRegistry::from_handles([handle("a")], Some("a".into())).unwrap();
        reg.record_unmounted(super::UnmountedLibrary::new(
            "b",
            "/disk/library/b",
            bookrack_core::Problem::new("cannot read the catalog").hint("Upgrade bookrack."),
        ))
        .unwrap();
        match reg.get(Some("b")) {
            Err(RegistryError::LibraryUnmounted { name, reason }) => {
                assert_eq!(name, "b");
                assert_eq!(reason.summary, "cannot read the catalog");
            }
            Err(other) => panic!("expected LibraryUnmounted, got {other:?}"),
            Ok(_) => panic!("a recorded library must not resolve to a handle"),
        }
        let unmounted = reg.unmounted().unwrap();
        assert_eq!(unmounted.len(), 1);
        assert_eq!(unmounted[0].name(), "b");
        assert_eq!(unmounted[0].data_dir(), Path::new("/disk/library/b"));
        assert_eq!(reg.len(), 1, "a recorded library is not counted as mounted");

        reg.mount(handle("b")).unwrap();
        assert!(
            reg.unmounted().unwrap().is_empty(),
            "mounting clears the record"
        );
        assert_eq!(reg.get(Some("b")).unwrap().name(), "b");
    }

    /// A call naming a library bring-up skipped carries the record's
    /// hint as it is, since the record already closes it with the mount;
    /// a record without a hint gets the mount alone.
    #[test]
    fn an_unmounted_librarys_hint_is_the_records_own() {
        use bookrack_core::Explain;
        let closed = "Reconnect it. Then run `bookrack libraries mount b`.";
        let err = RegistryError::LibraryUnmounted {
            name: "b".into(),
            reason: Box::new(bookrack_core::Problem::new("cannot serve library 'b'").hint(closed)),
        };
        let hint = err.explain().data.hint.expect("hint");
        assert_eq!(hint, closed, "the record's hint travels unchanged");
        assert_eq!(
            hint.matches("`bookrack libraries mount b`").count(),
            1,
            "{hint}"
        );

        let bare = RegistryError::LibraryUnmounted {
            name: "b".into(),
            reason: Box::new(bookrack_core::Problem::new("b failed: disk on fire")),
        };
        assert_eq!(
            bare.explain().data.hint.as_deref(),
            Some("Run `bookrack libraries mount b`.")
        );
    }

    #[test]
    fn recording_a_mounted_name_as_unmounted_is_refused() {
        let reg = LibraryRegistry::from_handles([handle("a")], Some("a".into())).unwrap();
        let err = reg
            .record_unmounted(super::UnmountedLibrary::new(
                "a",
                "/disk/library/a",
                bookrack_core::Problem::new("root locked"),
            ))
            .expect_err("a served library cannot also be unmounted");
        assert!(matches!(err, RegistryError::AlreadyMounted { name } if name == "a"));
        assert!(reg.unmounted().unwrap().is_empty());
    }

    /// The snapshot reads both tables under one lock and marks the
    /// unnamed route as the listing does.
    #[test]
    fn snapshot_reads_both_tables_at_once() {
        let reg =
            LibraryRegistry::from_handles([handle("a"), handle("b")], Some("b".into())).unwrap();
        reg.record_unmounted(super::UnmountedLibrary::new(
            "c",
            "/disk/library/c",
            bookrack_core::Problem::new("root locked"),
        ))
        .unwrap();
        let snapshot = reg.snapshot().unwrap();
        let mounted: Vec<(&str, bool)> = snapshot
            .mounted
            .iter()
            .map(|row| (row.name.as_str(), row.is_default))
            .collect();
        assert_eq!(mounted, vec![("a", false), ("b", true)]);
        assert_eq!(snapshot.unmounted.len(), 1);
        assert_eq!(snapshot.unmounted[0].name(), "c");
    }

    /// The reason is cleaned where the record is made: a dependency
    /// source location loses its build-machine prefix in every part,
    /// while a data path, which names the operator's own file, stays.
    #[test]
    fn skip_reason_is_redacted_at_construction() {
        let located = "/disk/cache/cargo/registry/src/index.crates.io-0123456789abcdef/dep-1.0.0/src/io.rs:7:3";
        let record = super::UnmountedLibrary::new(
            "beta",
            "/disk/library/beta",
            bookrack_core::Problem::new(format!("cannot open the store, {located}"))
                .detail(format!("read failed at {located} under /disk/library/beta"))
                .hint(format!("See {located}.")),
        );
        let reason = record.reason();
        for text in [
            reason.summary.as_str(),
            reason.data.detail.as_deref().unwrap_or(""),
            reason.data.hint.as_deref().unwrap_or(""),
        ] {
            assert!(!text.contains("/disk/cache/cargo"), "{text}");
            assert!(
                text.contains("io.rs:7:3"),
                "the crate-relative tail stays: {text}"
            );
        }
        assert!(
            reason
                .data
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("/disk/library/beta")),
            "a data path is not a source location: {:?}",
            reason.data.detail
        );
    }
}
