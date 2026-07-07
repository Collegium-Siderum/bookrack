// SPDX-License-Identifier: Apache-2.0

//! Path and environment resolution — the single entry point.
//!
//! Every filesystem path the program uses is derived here, from one
//! environment-configured data root. No path is ever a literal in
//! business code: callers ask a [`Config`] for the path they need.
//!
//! The data root is deliberately kept *outside* the project workspace.
//! It holds all book data — source files, the opaque intake store, the
//! databases, and the vector store — so book content, including real
//! titles, never sits next to the source code.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod detect;
mod manifest;
mod registry;

pub use detect::{
    DetectError, DetectVerdict, ScanOutcome, Signal, detect_library, mounted_volumes,
    scan_for_libraries,
};
pub use manifest::{
    LibraryManifest, MANIFEST_FILENAME, MANIFEST_FORMAT, MANIFEST_SCHEMA_VERSION, ManifestError,
    load_manifest, new_manifest, write_manifest,
};
pub use registry::LibraryKind;
use registry::{Registry, parse_registry};

/// Environment variable naming the data root (an absolute directory).
pub const DATA_DIR_ENV: &str = "BOOKRACK_DATA_DIR";

/// Environment variable naming the library registry file (TOML). Optional;
/// only `--library` selection needs it. See [`registry`].
pub const REGISTRY_ENV: &str = "BOOKRACK_REGISTRY";

/// Environment variable overriding the Ollama endpoint.
pub const OLLAMA_URL_ENV: &str = "BOOKRACK_OLLAMA_URL";

/// Ollama endpoint used when [`OLLAMA_URL_ENV`] is unset.
pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Environment variable naming the directory that holds the PDFium
/// dynamic library. When unset, the directory of the running executable
/// is used — the layout of a shipped bookrack build, which places the
/// library beside the binary.
pub const PDFIUM_LIB_ENV: &str = "BOOKRACK_PDFIUM_LIB";

/// Environment variable naming the directory database backups are written
/// to. When unset, a `backup/` directory under the data root is used.
pub const BACKUP_DIR_ENV: &str = "BOOKRACK_BACKUP_DIR";

/// Resolved configuration. Construct with [`Config::load`] (from the
/// environment) or [`Config::new`] (from an explicit data root, e.g. a
/// CLI override).
#[derive(Debug, Clone)]
pub struct Config {
    data_dir: PathBuf,
    ollama_url: String,
    library: Option<String>,
    source: ResolutionSource,
    root_config: RootConfig,
}

/// How the data root in a resolved [`Config`] was selected.
///
/// Surfaced to operators by `bookrack info` so the precedence ladder
/// inside [`Config::resolve`] is no longer a black box: a wrong root is
/// diagnosed by reading the source, not by guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionSource {
    /// Won by the `--data-dir` CLI flag.
    DataDirFlag,
    /// Won by `--library <name>`, with the path looked up in the registry.
    LibraryFlag,
    /// Won by the [`DATA_DIR_ENV`] environment variable.
    EnvVar,
    /// Won by a `bookrack-data` directory next to the running binary —
    /// the portable layout a self-contained tarball ships with.
    PortableExeNeighbor,
    /// Won by the registry's `default = "<name>"` entry — the registry
    /// named by [`REGISTRY_ENV`].
    RegistryDefault,
    /// Won by the `default` entry of the registry at the platform
    /// config-directory path. Written by `bookrack init`.
    DefaultRegistryDefault,
    /// Constructed directly via [`Config::new`], bypassing resolution.
    Explicit,
}

/// How the data root to operate on was selected on the command line.
/// Both fields default to `None`; resolution then falls back to the
/// data-root variable and finally the registry's default library.
#[derive(Debug, Default, Clone)]
pub struct LibrarySelection {
    /// An explicit data root, from `--data-dir`. Wins over everything.
    pub data_dir: Option<PathBuf>,
    /// A registry library name, from `--library`.
    pub library: Option<String>,
}

/// Why configuration resolution failed.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// No source selected a data root: no flag, no env var, no portable
    /// layout, and no registry default. The wizard creates one.
    #[error(
        "no library configured: run `bookrack init` to set one up, \
         or pass --data-dir, or set {DATA_DIR_ENV}"
    )]
    MissingDataDir,
    /// The selected data root is not an existing directory — usually a
    /// typo in a flag, the registry, or the data-root variable.
    #[error("the data root {} is not an existing directory", .0.display())]
    DataDirNotFound(PathBuf),
    /// `--library` names a library the registry does not define.
    /// `available` lists the names the registry does carry, so the
    /// operator can pick the right one without a separate
    /// `libraries list` call.
    #[error(
        "no library named {name:?} in the registry (available: {})",
        if available.is_empty() { "<none>".to_string() } else { available.join(", ") }
    )]
    UnknownLibrary {
        /// The name passed to `--library`.
        name: String,
        /// Library names the registry currently carries, sorted.
        available: Vec<String>,
    },
    /// `--library` was given but no registry is configured.
    #[error("--library needs a registry; set {REGISTRY_ENV} to a TOML file")]
    RegistryNotConfigured,
    /// The registry file could not be read.
    #[error("cannot read the registry at {}", .path.display())]
    RegistryUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The registry file is not valid TOML or has the wrong shape.
    #[error("the registry at {} is malformed", .path.display())]
    RegistryMalformed {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    /// The registry file parses as TOML but a key the writer needs to
    /// edit is the wrong type — e.g. `libraries` is a string rather
    /// than a table. The writer refuses to overwrite a file it does
    /// not understand; the operator fixes it (or removes it) by hand.
    #[error("the registry at {} cannot be merged: {reason}", .path.display())]
    RegistryShape { path: PathBuf, reason: String },
    /// `<data_root>/config.toml` exists but could not be read.
    #[error("cannot read the root config at {}", .path.display())]
    RootConfigUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// `<data_root>/config.toml` is not valid TOML or has the wrong shape.
    #[error("the root config at {} is malformed", .path.display())]
    RootConfigMalformed {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

impl Config {
    /// Resolve configuration from the environment, loading a `.env`
    /// file first if one is present.
    ///
    /// Equivalent to [`Config::resolve`] with an empty selection: the
    /// data root comes from the data-root variable (or the registry's
    /// default library, if one is set).
    pub fn load() -> Result<Config, ConfigError> {
        Config::resolve(&LibrarySelection::default())
    }

    /// Resolve configuration for the selected library, loading a `.env`
    /// file first if one is present.
    ///
    /// The data root is chosen by precedence: `--data-dir`, then
    /// `--library` (looked up in the registry), then [`DATA_DIR_ENV`],
    /// then a `bookrack-data` directory beside the running binary, then
    /// the registry's default library, then the platform-default
    /// registry's default. Fails if no source yields a root, the chosen
    /// root is not an existing directory, or a `--library` name is not
    /// registered. The Ollama endpoint falls back to
    /// [`DEFAULT_OLLAMA_URL`] when neither the env var nor the data
    /// root's `config.toml` sets it.
    pub fn resolve(selection: &LibrarySelection) -> Result<Config, ConfigError> {
        // A missing .env is fine: the variables may be set directly.
        dotenvy::dotenv().ok();
        let registry = load_registry(std::env::var(REGISTRY_ENV).ok())?;
        let default_registry = load_default_registry()?;
        let portable = portable_data_dir();
        let data_dir = select_root(
            selection,
            std::env::var(DATA_DIR_ENV).ok(),
            registry.as_ref(),
            portable,
            default_registry.as_ref(),
        )?;
        finish(data_dir, std::env::var(OLLAMA_URL_ENV).ok())
    }

    /// Construct from an explicit data root, for callers that resolve
    /// the root themselves (e.g. a CLI flag). Performs no filesystem
    /// check — the caller vouches for the path. The resulting [`Config`]
    /// has no library name and its source is reported as
    /// [`ResolutionSource::Explicit`].
    pub fn new(data_dir: PathBuf, ollama_url: String) -> Config {
        Config {
            data_dir,
            ollama_url,
            library: None,
            source: ResolutionSource::Explicit,
            root_config: RootConfig::default(),
        }
    }

    /// Per-data-root configuration loaded from `<data_root>/config.toml`,
    /// or the default (every field `None`) when the file was absent.
    pub fn root_config(&self) -> &RootConfig {
        &self.root_config
    }

    /// The data root that every other path is derived from.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The Ollama HTTP endpoint for embeddings.
    pub fn ollama_url(&self) -> &str {
        &self.ollama_url
    }

    /// The registry library name the data root was selected from, when
    /// the resolution went through the registry (`--library` or the
    /// registry's `default`). `None` when the root came directly from a
    /// path (the `--data-dir` flag, [`DATA_DIR_ENV`], or
    /// [`Config::new`]).
    pub fn library(&self) -> Option<&str> {
        self.library.as_deref()
    }

    /// How the data root was selected. Surfaced by `bookrack info` so
    /// operators can see which precedence rung of [`Config::resolve`]
    /// won, instead of guessing.
    pub fn source(&self) -> ResolutionSource {
        self.source
    }

    /// Directory of user-provided original book files awaiting intake.
    pub fn sources_dir(&self) -> PathBuf {
        self.data_dir.join("sources")
    }

    /// Opaque intake store; each ingested file lives under
    /// `books/<intake_id>/`.
    pub fn books_dir(&self) -> PathBuf {
        self.data_dir.join("books")
    }

    /// SQLite database for the node tree and body text (rebuildable).
    pub fn corpus_db(&self) -> PathBuf {
        self.data_dir.join("corpus.db")
    }

    /// SQLite database for intake, metadata and audit (source of truth).
    pub fn catalog_db(&self) -> PathBuf {
        self.data_dir.join("catalog.db")
    }

    /// LanceDB directory for the vector store.
    pub fn lancedb_dir(&self) -> PathBuf {
        self.data_dir.join("lancedb")
    }

    /// Opaque intake store; each gleaned paper lives under
    /// `papers/<intake_id>/`.
    pub fn papers_dir(&self) -> PathBuf {
        self.data_dir.join("papers")
    }

    /// SQLite database for the papers node tree and body text
    /// (rebuildable). Parallel to [`Config::corpus_db`], opened by the
    /// glean pipeline rather than ingest.
    pub fn papers_corpus_db(&self) -> PathBuf {
        self.data_dir.join("papers_corpus.db")
    }

    /// SQLite database for paper intake, metadata and audit. Parallel
    /// to [`Config::catalog_db`]; both files share [`Config::backup_dir`]
    /// but are clustered separately by file stem during pruning.
    pub fn papers_catalog_db(&self) -> PathBuf {
        self.data_dir.join("papers_catalog.db")
    }

    /// LanceDB directory for the paper vector store. Parallel to
    /// [`Config::lancedb_dir`]; opened with its own pipeline stamps.
    pub fn papers_lancedb_dir(&self) -> PathBuf {
        self.data_dir.join("lancedb_papers")
    }

    /// Directory for log files and crash reports. Kept under the data
    /// root — like every other path — so diagnostics never land inside
    /// the project workspace.
    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    /// Directory holding the audit's runtime-loaded rule files
    /// (`publishers.toml`, `watermarks.toml`). Missing files yield
    /// empty rules; the engine treats every value as neutral.
    pub fn audit_rules_dir(&self) -> PathBuf {
        self.data_dir.join("audit-rules")
    }

    /// Directory database backups are written to before a schema
    /// migration. The [`BACKUP_DIR_ENV`] override wins when set;
    /// otherwise `<data_dir>/backup`, beside the database files it
    /// snapshots.
    pub fn backup_dir(&self) -> PathBuf {
        backup_dir_from(&self.data_dir, std::env::var(BACKUP_DIR_ENV).ok())
    }
}

/// Pure resolution logic for [`Config::backup_dir`], factored out so it can
/// be tested without mutating process-global environment variables. The
/// override wins when set and non-blank; otherwise `<data_dir>/backup`.
fn backup_dir_from(data_dir: &Path, override_dir: Option<String>) -> PathBuf {
    env_trimmed(override_dir)
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("backup"))
}

/// One entry in the library registry: a name, its data root, and a flag
/// for the registry's `default = "..."` selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryEntry {
    /// Short symbolic name a caller passes to `--library`.
    pub name: String,
    /// Absolute data root the registry maps the name to.
    pub data_dir: PathBuf,
    /// True when the registry's `default = "<this name>"` picks this
    /// entry as the resolution fallback.
    pub is_default: bool,
    /// Library kind. A legacy bare-path entry reports the default
    /// [`LibraryKind::Prod`].
    pub kind: LibraryKind,
    /// Free-form description, when the entry carries one.
    pub description: Option<String>,
    /// Index-profile name the entry records, when set.
    pub index_profile: Option<String>,
    /// Cached RFC 3339 creation timestamp, when set.
    pub created_at: Option<String>,
    /// Cached library uuid, when set.
    pub uuid: Option<String>,
}

/// Read the library registry and return every entry, sorted by name.
///
/// Returns `Ok(None)` when [`REGISTRY_ENV`] is unset or blank — the
/// registry is optional, and a binary that resolves through
/// [`DATA_DIR_ENV`] alone has no registry to list. A configured but
/// malformed or unreadable registry surfaces as the matching error.
pub fn list_libraries() -> Result<Option<Vec<LibraryEntry>>, ConfigError> {
    list_libraries_from(std::env::var(REGISTRY_ENV).ok())
}

/// Pure form of [`list_libraries`], for tests that should not mutate
/// the process environment.
fn list_libraries_from(env: Option<String>) -> Result<Option<Vec<LibraryEntry>>, ConfigError> {
    let Some(registry) = load_registry(env)? else {
        return Ok(None);
    };
    Ok(Some(library_entries(&registry)))
}

/// Project a parsed [`Registry`] into a sorted entry list. Pure so the
/// projection can be tested without touching the filesystem.
fn library_entries(registry: &Registry) -> Vec<LibraryEntry> {
    let mut entries: Vec<LibraryEntry> = registry
        .libraries
        .iter()
        .map(|(name, raw)| LibraryEntry {
            name: name.clone(),
            data_dir: raw.data_dir().to_path_buf(),
            is_default: registry.default.as_deref() == Some(name.as_str()),
            kind: raw.kind(),
            description: raw.description().map(str::to_string),
            index_profile: raw.index_profile().map(str::to_string),
            created_at: raw.created_at().map(str::to_string),
            uuid: raw.uuid().map(str::to_string),
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Filename of the per-data-root configuration TOML, written by
/// `bookrack init` next to the catalog and corpus databases.
pub const ROOT_CONFIG_NAME: &str = "config.toml";

/// Per-data-root configuration loaded from `<data_root>/config.toml`.
///
/// Carries runtime knobs that vary by library: the Ollama endpoint, the
/// embed model, the MCP listen address, the log filter directive. Each
/// field is `None` when the file does not set it; the matching env var
/// (where one exists) overrides this layer, and the hardcoded default
/// wins when both are absent. Written by `bookrack init`; safe to edit
/// by hand.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RootConfig {
    /// Ollama HTTP endpoint for embeddings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ollama_url: Option<String>,
    /// Embedding model tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embed_model: Option<String>,
    /// Address the MCP server binds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_addr: Option<String>,
    /// `EnvFilter` directive for tracing verbosity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_directive: Option<String>,
}

/// Read `<data_root>/config.toml`. A missing file resolves to the
/// default (every field `None`) so a fresh data root is no error; a
/// malformed file surfaces as [`ConfigError::RootConfigMalformed`].
pub fn load_root_config(data_dir: &Path) -> Result<RootConfig, ConfigError> {
    let path = data_dir.join(ROOT_CONFIG_NAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RootConfig::default());
        }
        Err(source) => {
            return Err(ConfigError::RootConfigUnreadable { path, source });
        }
    };
    toml::from_str(&text).map_err(|source| ConfigError::RootConfigMalformed { path, source })
}

/// Render a `<data_root>/config.toml` body for a freshly initialized
/// library, using the canonical TOML serializer so the URL and model
/// strings are escaped correctly regardless of which characters they
/// carry.
///
/// The previous wizard hand-rolled the file with `format!`, which let
/// a `"` or a `\n` inside `ollama_url` break the file: `toml::from_str`
/// then refused the document on the next daemon start. The serializer
/// emits a TOML basic string with the right escapes (`\"`, `\n`, `\\`,
/// `\u{...}` for U+2028 and friends) so the same input round-trips
/// through `load_root_config`.
pub fn render_root_config_toml(ollama_url: &str, embed_model: &str) -> String {
    let cfg = RootConfig {
        ollama_url: Some(ollama_url.to_string()),
        embed_model: Some(embed_model.to_string()),
        mcp_addr: None,
        log_directive: None,
    };
    let body = toml::to_string(&cfg).expect("RootConfig serialization is infallible");
    format!("# bookrack root config. Written by `bookrack init`; safe to edit.\n{body}")
}

/// Embedding model served by the local Ollama daemon, used when
/// [`EmbedConfig`] is left at its default.
pub const DEFAULT_EMBED_MODEL: &str = "qwen3-embedding:0.6b";

/// Environment variable overriding the embedding model tag.
pub const EMBED_MODEL_ENV: &str = "BOOKRACK_EMBED_MODEL";

/// Environment variable overriding the target batch size, in characters.
pub const EMBED_BATCH_CHAR_BUDGET_ENV: &str = "BOOKRACK_EMBED_BATCH_CHAR_BUDGET";

/// Environment variable overriding the hard per-request chunk cap.
pub const EMBED_BATCH_MAX_CHUNKS_ENV: &str = "BOOKRACK_EMBED_BATCH_MAX_CHUNKS";

/// Environment variable overriding the OOM-shrink char-budget floor.
pub const EMBED_BATCH_MIN_CHAR_BUDGET_ENV: &str = "BOOKRACK_EMBED_BATCH_MIN_CHAR_BUDGET";

/// Default interval, in seconds, between EMBED-progress heartbeats on
/// stderr. Calibrated to be visible on a tiny book («47 s» small EPUB)
/// without spamming the log on a fast embedder.
pub const DEFAULT_EMBED_PROGRESS_INTERVAL_SECS: u64 = 5;

/// Environment variable overriding the EMBED-progress heartbeat
/// interval, in seconds. A value of `0` emits a heartbeat after every
/// batch; any non-numeric value falls back to the default.
pub const EMBED_PROGRESS_INTERVAL_ENV: &str = "BOOKRACK_EMBED_PROGRESS_INTERVAL_SECS";

/// Environment variable overriding the log filter directive.
pub const LOG_ENV: &str = "BOOKRACK_LOG";

/// Environment variable overriding the human-readable console layer's
/// filter directive. Lets an operator dial up stderr verbosity for one
/// session without touching [`LOG_ENV`], which governs the structured
/// file layer that persists across runs.
pub const LOG_CONSOLE_ENV: &str = "BOOKRACK_LOG_CONSOLE";

/// Filter directive used when [`LOG_ENV`] is unset.
///
/// bookrack's own crates log at `info`; the vector-store dependencies
/// (`lance*`, `datafusion`) are pinned to `warn` because they emit a high
/// volume of `info` events — manifest loads, plan runs, file audits — that
/// would otherwise bury the pipeline's own diagnostics.
///
/// `lance_index::vector::kmeans` is pinned a step lower (`error`) because
/// its empty-cluster warnings are expected at small chunk counts with
/// `num_partitions > 1`; the misleading "duplicate vectors" hint they
/// carry is a known false positive for the IVF training step and not
/// actionable for an operator.
///
/// Override the whole directive with [`LOG_ENV`] to see the suppressed
/// events.
pub const DEFAULT_LOG: &str = "info,lance=warn,lance_namespace_impls=warn,lance_table=warn,\
     lance_index=error,datafusion=warn,rusqlite_migration=warn";

/// Default console-layer filter when [`LOG_CONSOLE_ENV`] is unset. The
/// stderr layer is silenced down to errors by default so the foreground
/// REPL stays clear of daemon-internal telemetry; the file layer keeps
/// recording at [`DEFAULT_LOG`] verbosity for after-the-fact inspection.
/// Override with `BOOKRACK_LOG_CONSOLE=debug` (or any `EnvFilter`
/// directive) when you want full verbosity on screen for one run.
pub const DEFAULT_LOG_CONSOLE: &str = "error";

/// Number of nearest passages a query returns when [`SearchConfig`] is
/// left at its default.
pub const DEFAULT_SEARCH_TOP_K: usize = 5;

/// Environment variable overriding the search result count.
pub const SEARCH_TOP_K_ENV: &str = "BOOKRACK_SEARCH_TOP_K";

/// Cosine-distance threshold at or above which a hit is treated as a
/// weak match. Calibrated against the EMBED spike's real corpus: real
/// monolingual matches sit around 0.25, cross-language matches around
/// 0.45, and noise / prompt-only embeddings around 0.55 and above.
pub const DEFAULT_SEARCH_WEAK_THRESHOLD: f32 = 0.5;

/// Environment variable overriding the weak-hit distance threshold.
pub const SEARCH_WEAK_THRESHOLD_ENV: &str = "BOOKRACK_SEARCH_WEAK_THRESHOLD";

/// Listen address the MCP server binds when [`McpConfig`] is left at its
/// default. Loopback only: the server is a local query daemon, not a
/// network service.
pub const DEFAULT_MCP_ADDR: &str = "127.0.0.1:8765";

/// Environment variable overriding the MCP server listen address.
pub const MCP_ADDR_ENV: &str = "BOOKRACK_MCP_ADDR";

/// Tunable parameters for the embedding subsystem.
///
/// A single source of truth for the knobs the `embed` client and the
/// ingest-side batching read, rather than scattering them as literals.
/// The defaults are the values the EMBED spike calibrated against real
/// books: deliberately conservative, and meant to be retuned from real
/// run data rather than treated as fixed.
///
/// Scheduler-side knobs (throttle mode, sleep curve, AIMD coefficients)
/// join this struct when the ingest EMBED stage lands; for now it
/// carries the client and batching parameters, which are stable.
#[derive(Debug, Clone)]
pub struct EmbedConfig {
    /// Ollama model tag to embed with.
    pub model: String,
    /// Per-request HTTP timeout.
    pub request_timeout: Duration,
    /// How many times a transient transport failure is retried.
    pub max_retries: u32,
    /// Base delay for exponential backoff between retries.
    pub backoff_base: Duration,
    /// Target batch size, in characters of chunk text — a proxy for
    /// token count that needs no client-side tokenizer.
    pub batch_char_budget: usize,
    /// Hard cap on chunks per embed request, regardless of the budget.
    pub batch_max_chunks: usize,
    /// Floor the char budget cannot shrink below under OOM feedback.
    pub batch_min_char_budget: usize,
    /// Capacity of the producer-to-consumer channel, in chunks.
    pub channel_capacity: usize,
    /// Lower bound on the gap between EMBED-progress heartbeats. The
    /// loop emits a heartbeat on the first batch and then at most once
    /// per `progress_interval`; `Duration::ZERO` opts into a heartbeat
    /// after every batch.
    pub progress_interval: Duration,
}

impl Default for EmbedConfig {
    fn default() -> EmbedConfig {
        EmbedConfig {
            model: DEFAULT_EMBED_MODEL.to_string(),
            request_timeout: Duration::from_secs(120),
            max_retries: 4,
            backoff_base: Duration::from_secs(1),
            batch_char_budget: 8_000,
            batch_max_chunks: 64,
            batch_min_char_budget: 500,
            channel_capacity: 2_000,
            progress_interval: Duration::from_secs(DEFAULT_EMBED_PROGRESS_INTERVAL_SECS),
        }
    }
}

impl EmbedConfig {
    /// Resolve from the environment, overriding the operational knobs the
    /// EMBED stage exposes and leaving every other field at its default.
    ///
    /// Only operational knobs are read here: the model tag and the
    /// batching budgets. Content-identity parameters (chunk length,
    /// overlap, grouping) are not configurable — they are frozen with
    /// `CHUNK_VERSION` so a change forces a re-derivation. A malformed or
    /// empty value falls back to the default rather than failing.
    pub fn from_env() -> EmbedConfig {
        EmbedConfig::resolve_from(|key| std::env::var(key).ok())
    }

    /// Pure resolution, factored out of [`EmbedConfig::from_env`] so it can
    /// be tested without mutating process-global environment variables.
    fn resolve_from(get: impl Fn(&str) -> Option<String>) -> EmbedConfig {
        let d = EmbedConfig::default();
        EmbedConfig {
            model: env_trimmed(get(EMBED_MODEL_ENV)).unwrap_or(d.model),
            request_timeout: d.request_timeout,
            max_retries: d.max_retries,
            backoff_base: d.backoff_base,
            batch_char_budget: env_usize(get(EMBED_BATCH_CHAR_BUDGET_ENV), d.batch_char_budget),
            batch_max_chunks: env_usize(get(EMBED_BATCH_MAX_CHUNKS_ENV), d.batch_max_chunks),
            batch_min_char_budget: env_usize(
                get(EMBED_BATCH_MIN_CHAR_BUDGET_ENV),
                d.batch_min_char_budget,
            ),
            channel_capacity: d.channel_capacity,
            progress_interval: Duration::from_secs(env_usize(
                get(EMBED_PROGRESS_INTERVAL_ENV),
                DEFAULT_EMBED_PROGRESS_INTERVAL_SECS as usize,
            ) as u64),
        }
    }
}

/// Retrieval knobs. Separate from [`EmbedConfig`] so the query side reads
/// only what it needs.
#[derive(Debug, Clone)]
pub struct SearchConfig {
    /// How many nearest passages a query returns.
    pub top_k: usize,
    /// Cosine-distance threshold at or above which a hit is treated as
    /// a weak match. When every top-`top_k` hit lands at or above this
    /// value, the CLI prints an advisory line so the operator knows
    /// the recall set is probably noise.
    pub weak_distance_threshold: f32,
}

impl Default for SearchConfig {
    fn default() -> SearchConfig {
        SearchConfig {
            top_k: DEFAULT_SEARCH_TOP_K,
            weak_distance_threshold: DEFAULT_SEARCH_WEAK_THRESHOLD,
        }
    }
}

impl SearchConfig {
    /// Resolve from the environment, falling back to the default when the
    /// override is unset or malformed.
    pub fn from_env() -> SearchConfig {
        SearchConfig::resolve_from(|key| std::env::var(key).ok())
    }

    /// Pure resolution, factored out so it can be tested without mutating
    /// process-global environment variables.
    fn resolve_from(get: impl Fn(&str) -> Option<String>) -> SearchConfig {
        SearchConfig {
            top_k: env_usize(get(SEARCH_TOP_K_ENV), DEFAULT_SEARCH_TOP_K),
            weak_distance_threshold: env_f32(
                get(SEARCH_WEAK_THRESHOLD_ENV),
                DEFAULT_SEARCH_WEAK_THRESHOLD,
            ),
        }
    }
}

/// MCP server knobs. Separate from the data-path config so the daemon
/// entry point reads only what it needs.
#[derive(Debug, Clone)]
pub struct McpConfig {
    /// Address the streamable-HTTP server binds, e.g. `127.0.0.1:8765`.
    pub addr: String,
}

impl Default for McpConfig {
    fn default() -> McpConfig {
        McpConfig {
            addr: DEFAULT_MCP_ADDR.to_string(),
        }
    }
}

impl McpConfig {
    /// Resolve from the environment, falling back to [`DEFAULT_MCP_ADDR`]
    /// when the override is unset or blank.
    pub fn from_env() -> McpConfig {
        McpConfig::resolve_from(|key| std::env::var(key).ok())
    }

    /// Pure resolution, factored out so it can be tested without mutating
    /// process-global environment variables.
    fn resolve_from(get: impl Fn(&str) -> Option<String>) -> McpConfig {
        McpConfig {
            addr: env_trimmed(get(MCP_ADDR_ENV)).unwrap_or_else(|| DEFAULT_MCP_ADDR.to_string()),
        }
    }
}

/// Logging verbosity, resolved separately from the data-path config so an
/// entry point can install its subscriber before touching anything else.
///
/// The two filter directives drive independent layers in the subscriber:
/// [`directive`](Self::directive) governs the structured JSON file layer
/// (the persistent operational log), while
/// [`console_level`](Self::console_level) governs the human-readable
/// stderr layer (the foreground REPL or attached terminal). Splitting
/// them lets a daemon run silently on screen while still recording at
/// full verbosity on disk.
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// Filter directive for the file layer's `EnvFilter`, e.g. `info`,
    /// `debug`, or `bookrack_ingest=debug,info`. Overridden by
    /// [`LOG_ENV`].
    pub directive: String,
    /// Filter directive for the stderr console layer's `EnvFilter`.
    /// Defaults to [`DEFAULT_LOG_CONSOLE`] (`error`) so REPL sessions
    /// stay free of daemon-internal telemetry; overridden by
    /// [`LOG_CONSOLE_ENV`].
    pub console_level: String,
}

impl Default for LogConfig {
    fn default() -> LogConfig {
        LogConfig {
            directive: DEFAULT_LOG.to_string(),
            console_level: DEFAULT_LOG_CONSOLE.to_string(),
        }
    }
}

impl LogConfig {
    /// Resolve from the environment, falling back to [`DEFAULT_LOG`]
    /// and [`DEFAULT_LOG_CONSOLE`] when the respective overrides are
    /// unset or blank.
    pub fn from_env() -> LogConfig {
        LogConfig::resolve_from(|key| std::env::var(key).ok())
    }

    /// Pure resolution, factored out so it can be tested without mutating
    /// process-global environment variables.
    fn resolve_from(get: impl Fn(&str) -> Option<String>) -> LogConfig {
        LogConfig {
            directive: env_trimmed(get(LOG_ENV)).unwrap_or_else(|| DEFAULT_LOG.to_string()),
            console_level: env_trimmed(get(LOG_CONSOLE_ENV))
                .unwrap_or_else(|| DEFAULT_LOG_CONSOLE.to_string()),
        }
    }

    /// Resolve a [`LogConfig`] suitable for a headless daemon binary
    /// whose stderr is the operator's primary log surface
    /// (systemd / journalctl / docker logs).
    ///
    /// Behaves like [`from_env`](Self::from_env) for the file directive
    /// and any explicit [`LOG_CONSOLE_ENV`] override, but when the
    /// console override is unset it mirrors the file directive instead
    /// of falling to [`DEFAULT_LOG_CONSOLE`] — so the daemon's full
    /// telemetry reaches stderr by default.
    pub fn for_headless_daemon() -> LogConfig {
        LogConfig::resolve_headless_from(|key| std::env::var(key).ok())
    }

    /// Pure resolution for [`for_headless_daemon`](Self::for_headless_daemon),
    /// factored out for testing.
    fn resolve_headless_from(get: impl Fn(&str) -> Option<String>) -> LogConfig {
        let directive = env_trimmed(get(LOG_ENV)).unwrap_or_else(|| DEFAULT_LOG.to_string());
        let console_level = env_trimmed(get(LOG_CONSOLE_ENV)).unwrap_or_else(|| directive.clone());
        LogConfig {
            directive,
            console_level,
        }
    }
}

/// Trim an environment value, treating whitespace-only as unset.
fn env_trimmed(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Parse an environment value as `usize`, falling back to `default` when
/// it is unset, blank, or unparseable.
fn env_usize(value: Option<String>, default: usize) -> usize {
    env_trimmed(value)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Parse an environment value as `f32`, falling back to `default` when
/// it is unset, blank, unparseable, or not a finite number.
fn env_f32(value: Option<String>, default: f32) -> f32 {
    env_trimmed(value)
        .and_then(|s| s.parse::<f32>().ok())
        .filter(|n| n.is_finite())
        .unwrap_or(default)
}

/// Outcome of [`select_root`]: the chosen data root paired with the
/// metadata `bookrack info` needs to explain how it was chosen.
#[cfg_attr(test, derive(Debug))]
struct Resolved {
    data_dir: PathBuf,
    source: ResolutionSource,
    library: Option<String>,
}

/// Pick the data root by precedence, without checking that the chosen
/// path exists. Pure over its inputs so the precedence rules can be
/// tested without mutating the environment, writing registry files, or
/// touching the running executable's directory.
///
/// Order, highest first:
/// 1. the `--data-dir` flag,
/// 2. the `--library` flag (looked up in the registry),
/// 3. the data-root environment variable,
/// 4. a `bookrack-data` directory probed beside the running binary,
/// 5. the registry's default library,
/// 6. the platform-default registry's default library.
fn select_root(
    selection: &LibrarySelection,
    env_data_dir: Option<String>,
    registry: Option<&Registry>,
    portable_dir: Option<PathBuf>,
    default_registry: Option<&Registry>,
) -> Result<Resolved, ConfigError> {
    if let Some(dir) = &selection.data_dir {
        return Ok(Resolved {
            data_dir: dir.clone(),
            source: ResolutionSource::DataDirFlag,
            library: None,
        });
    }
    if let Some(name) = &selection.library {
        let registry = registry.ok_or(ConfigError::RegistryNotConfigured)?;
        let data_dir = lookup_library(registry, name)?;
        return Ok(Resolved {
            data_dir,
            source: ResolutionSource::LibraryFlag,
            library: Some(name.clone()),
        });
    }
    if let Some(dir) = env_trimmed(env_data_dir) {
        return Ok(Resolved {
            data_dir: PathBuf::from(dir),
            source: ResolutionSource::EnvVar,
            library: None,
        });
    }
    if let Some(dir) = portable_dir {
        return Ok(Resolved {
            data_dir: dir,
            source: ResolutionSource::PortableExeNeighbor,
            library: None,
        });
    }
    if let Some(registry) = registry
        && let Some(name) = &registry.default
    {
        let data_dir = lookup_library(registry, name)?;
        return Ok(Resolved {
            data_dir,
            source: ResolutionSource::RegistryDefault,
            library: Some(name.clone()),
        });
    }
    if let Some(registry) = default_registry
        && let Some(name) = &registry.default
    {
        let data_dir = lookup_library(registry, name)?;
        return Ok(Resolved {
            data_dir,
            source: ResolutionSource::DefaultRegistryDefault,
            library: Some(name.clone()),
        });
    }
    Err(ConfigError::MissingDataDir)
}

/// Look up a named library's root in the registry.
fn lookup_library(registry: &Registry, name: &str) -> Result<PathBuf, ConfigError> {
    registry
        .libraries
        .get(name)
        .map(|entry| entry.data_dir().to_path_buf())
        .ok_or_else(|| {
            let mut available: Vec<String> = registry.libraries.keys().cloned().collect();
            available.sort();
            ConfigError::UnknownLibrary {
                name: name.to_string(),
                available,
            }
        })
}

/// Validate the chosen root and build a [`Config`]. The root must be an
/// existing directory. The Ollama endpoint is resolved by precedence
/// `env var > <data_root>/config.toml > hardcoded default`, so a
/// per-library override in the TOML still loses to an explicit
/// environment variable.
fn finish(resolved: Resolved, ollama_url_env: Option<String>) -> Result<Config, ConfigError> {
    let Resolved {
        data_dir,
        source,
        library,
    } = resolved;
    if !data_dir.is_dir() {
        return Err(ConfigError::DataDirNotFound(data_dir));
    }
    let root_config = load_root_config(&data_dir)?;
    let ollama_url = env_trimmed(ollama_url_env)
        .or_else(|| env_trimmed(root_config.ollama_url.clone()))
        .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string());
    Ok(Config {
        data_dir,
        ollama_url,
        library,
        source,
        root_config,
    })
}

/// Merge a single entry into the registry at `path`, creating the file
/// (and its parent directories) when absent. Existing entries are
/// preserved; the entry's data root is overwritten when the name
/// matches an existing one. The `default = "..."` field is set to
/// `name` only when no default is currently recorded — an operator who
/// has already chosen a default is never silently overridden.
///
/// `bookrack init` is the only writer; the function is exposed so the
/// platform-default registry under the OS config directory can be
/// produced by the wizard without crates outside `config` rebuilding
/// the file format.
pub fn merge_library_into_registry(
    path: &Path,
    name: &str,
    data_dir: &Path,
) -> Result<(), ConfigError> {
    let mut doc = read_registry_table(path)?;
    let upgraded = upsert_library_in_table(&mut doc, name, data_dir, path)?;
    write_registry_table(path, &doc)?;
    if upgraded {
        emit_registry_upgrade_notice();
    }
    Ok(())
}

/// Point the registry's `default = "..."` selection at `name`, writing
/// the change straight to disk. Unlike [`merge_library_into_registry`],
/// this overwrites any existing default — it is the explicit
/// "make this the default" entry point behind `bookrack libraries
/// default`, and the change persists across daemon restarts. Errors
/// with [`ConfigError::UnknownLibrary`] when the registry does not
/// define `name`, so the default can never point at a missing library.
pub fn set_default_library(path: &Path, name: &str) -> Result<(), ConfigError> {
    let mut doc = read_registry_table(path)?;
    let upgraded = normalize_registry_entries(&mut doc, path)?;
    if !registry_has_library(&doc, name) {
        return Err(ConfigError::UnknownLibrary {
            name: name.to_string(),
            available: registry_library_names(&doc),
        });
    }
    doc.insert("default".to_string(), toml::Value::String(name.to_string()));
    write_registry_table(path, &doc)?;
    if upgraded {
        emit_registry_upgrade_notice();
    }
    Ok(())
}

/// Remove the library named `name` from the registry, writing the
/// change straight to disk. When the removed library was the recorded
/// default, the `default` pointer is dropped so it never dangles.
/// Errors with [`ConfigError::UnknownLibrary`] when no such entry
/// exists. Removing a registry entry never touches the library's data
/// root — it only forgets the name.
pub fn remove_library_from_registry(path: &Path, name: &str) -> Result<(), ConfigError> {
    let mut doc = read_registry_table(path)?;
    let upgraded = normalize_registry_entries(&mut doc, path)?;
    let removed = doc
        .get_mut("libraries")
        .and_then(toml::Value::as_table_mut)
        .map(|libraries| libraries.remove(name).is_some())
        .unwrap_or(false);
    if !removed {
        return Err(ConfigError::UnknownLibrary {
            name: name.to_string(),
            available: registry_library_names(&doc),
        });
    }
    if doc.get("default").and_then(toml::Value::as_str) == Some(name) {
        doc.remove("default");
    }
    write_registry_table(path, &doc)?;
    if upgraded {
        emit_registry_upgrade_notice();
    }
    Ok(())
}

/// Insert or replace a full registry entry, writing every metadata
/// field the caller sets. Like [`merge_library_into_registry`], the
/// `default` pointer is set to `name` only when none is recorded yet.
/// This is the write-side counterpart of the read-side entry table.
pub fn upsert_library_entry(
    path: &Path,
    name: &str,
    entry: &LibraryEntryFields,
) -> Result<(), ConfigError> {
    let mut doc = read_registry_table(path)?;
    let upgraded = normalize_registry_entries(&mut doc, path)?;
    let libraries = doc
        .get_mut("libraries")
        .expect("normalize_registry_entries inserts the libraries table")
        .as_table_mut()
        .expect("normalize_registry_entries guarantees a table");
    libraries.insert(name.to_string(), toml::Value::Table(entry.to_toml_table()));
    if !doc.contains_key("default") {
        doc.insert("default".to_string(), toml::Value::String(name.to_string()));
    }
    write_registry_table(path, &doc)?;
    if upgraded {
        emit_registry_upgrade_notice();
    }
    Ok(())
}

/// The metadata a full registry entry can carry, for the write side of
/// the registry. `data_dir` is required; every other field is written
/// only when set. Mirrors the read-side registry entry table.
#[derive(Debug, Clone)]
pub struct LibraryEntryFields {
    /// Absolute data root the name maps to.
    pub data_dir: PathBuf,
    /// Library kind.
    pub kind: LibraryKind,
    /// Free-form description.
    pub description: Option<String>,
    /// Index-profile name the library is built under.
    pub index_profile: Option<String>,
    /// RFC 3339 creation timestamp.
    pub created_at: Option<String>,
    /// Stable library uuid.
    pub uuid: Option<String>,
}

impl LibraryEntryFields {
    /// Render the entry into a TOML table, emitting only the fields
    /// that are set. `data_dir` and `kind` are always present.
    fn to_toml_table(&self) -> toml::Table {
        let mut table = toml::Table::new();
        table.insert(
            "data_dir".to_string(),
            toml::Value::String(self.data_dir.display().to_string()),
        );
        table.insert(
            "kind".to_string(),
            toml::Value::String(self.kind.as_str().to_string()),
        );
        for (key, value) in [
            ("description", self.description.as_ref()),
            ("index_profile", self.index_profile.as_ref()),
            ("created_at", self.created_at.as_ref()),
            ("uuid", self.uuid.as_ref()),
        ] {
            if let Some(v) = value {
                table.insert(key.to_string(), toml::Value::String(v.clone()));
            }
        }
        table
    }
}

/// Read the registry at `path` as a free-form TOML table. A missing
/// file resolves to an empty table so a writer can create a fresh file
/// without a separate branch.
fn read_registry_table(path: &Path) -> Result<toml::Table, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(toml::Table::new()),
        Err(source) => {
            return Err(ConfigError::RegistryUnreadable {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    text.parse::<toml::Table>()
        .map_err(|source| ConfigError::RegistryMalformed {
            path: path.to_path_buf(),
            source,
        })
}

/// Mutate `doc` to carry the new library entry in table form,
/// preserving every other table field. Returns whether a legacy
/// string entry was upgraded to table form, so the caller can emit the
/// one-time upgrade notice.
fn upsert_library_in_table(
    doc: &mut toml::Table,
    name: &str,
    data_dir: &Path,
    path: &Path,
) -> Result<bool, ConfigError> {
    let upgraded = normalize_registry_entries(doc, path)?;
    let libraries = doc
        .get_mut("libraries")
        .expect("normalize_registry_entries inserts the libraries table")
        .as_table_mut()
        .expect("normalize_registry_entries guarantees a table");
    let mut table = toml::Table::new();
    table.insert(
        "data_dir".to_string(),
        toml::Value::String(data_dir.display().to_string()),
    );
    libraries.insert(name.to_string(), toml::Value::Table(table));
    if !doc.contains_key("default") {
        doc.insert("default".to_string(), toml::Value::String(name.to_string()));
    }
    Ok(upgraded)
}

/// Validate the registry's top-level shape and upgrade any legacy
/// bare-string entries to table form in place, ensuring a `libraries`
/// table exists. Returns true when at least one legacy entry was
/// upgraded. Errors when an existing `libraries` or `default` key has
/// the wrong type — the writer refuses to clobber a file it does not
/// understand.
fn normalize_registry_entries(doc: &mut toml::Table, path: &Path) -> Result<bool, ConfigError> {
    if let Some(existing) = doc.get("libraries")
        && !existing.is_table()
    {
        return Err(ConfigError::RegistryShape {
            path: path.to_path_buf(),
            reason: "`libraries` is not a table".to_string(),
        });
    }
    if let Some(existing) = doc.get("default")
        && !existing.is_str()
    {
        return Err(ConfigError::RegistryShape {
            path: path.to_path_buf(),
            reason: "`default` is not a string".to_string(),
        });
    }
    let libraries = doc
        .entry("libraries".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .expect("ensured to be a table by the check above");
    Ok(upgrade_legacy_entries(libraries))
}

/// Rewrite every bare-string entry in a `libraries` table into the
/// table form `{ data_dir = "<path>" }`, leaving existing table
/// entries untouched. Returns true when at least one entry changed.
fn upgrade_legacy_entries(libraries: &mut toml::Table) -> bool {
    let legacy: Vec<String> = libraries
        .iter()
        .filter_map(|(name, value)| value.as_str().map(|_| name.clone()))
        .collect();
    for name in &legacy {
        let Some(path) = libraries.get(name).and_then(toml::Value::as_str) else {
            continue;
        };
        let mut table = toml::Table::new();
        table.insert(
            "data_dir".to_string(),
            toml::Value::String(path.to_string()),
        );
        libraries.insert(name.clone(), toml::Value::Table(table));
    }
    !legacy.is_empty()
}

/// Whether the registry table defines a library named `name`.
fn registry_has_library(doc: &toml::Table, name: &str) -> bool {
    doc.get("libraries")
        .and_then(toml::Value::as_table)
        .map(|libraries| libraries.contains_key(name))
        .unwrap_or(false)
}

/// The sorted library names the registry table carries, for the
/// "available: ..." hint on an unknown-library error.
fn registry_library_names(doc: &toml::Table) -> Vec<String> {
    let mut names: Vec<String> = doc
        .get("libraries")
        .and_then(toml::Value::as_table)
        .map(|libraries| libraries.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();
    names
}

/// Print the one-time notice that a legacy registry file was rewritten
/// into the entry-table format. Emitted on stderr so it never pollutes
/// a `--json` stdout stream.
fn emit_registry_upgrade_notice() {
    eprintln!("info: registry upgraded to entry-table format");
}

/// Serialize `doc` and write it to `path` atomically, creating the
/// parent directory as needed.
fn write_registry_table(path: &Path, doc: &toml::Table) -> Result<(), ConfigError> {
    let serialised = toml::to_string_pretty(doc).expect("toml::Table is always serialisable");
    write_atomically(path, &serialised).map_err(|source| ConfigError::RegistryUnreadable {
        path: path.to_path_buf(),
        source,
    })
}

/// Write `contents` to `path` atomically: stage a temporary file in the
/// same directory, flush it, then rename it over the target. A reader
/// racing the write sees either the old file or the new one, never a
/// truncated one. The single I/O error is returned raw so each caller
/// wraps it in its own error type.
pub(crate) fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(&parent)?;
    tmp.write_all(contents.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Load the registry from the file named by [`REGISTRY_ENV`]. Returns
/// `Ok(None)` when the variable is unset or blank — the registry is
/// optional, and only `--library` selection requires it.
fn load_registry(path: Option<String>) -> Result<Option<Registry>, ConfigError> {
    let Some(path) = env_trimmed(path) else {
        return Ok(None);
    };
    let path = PathBuf::from(path);
    let text =
        std::fs::read_to_string(&path).map_err(|source| ConfigError::RegistryUnreadable {
            path: path.clone(),
            source,
        })?;
    let registry =
        parse_registry(&text).map_err(|source| ConfigError::RegistryMalformed { path, source })?;
    Ok(Some(registry))
}

/// Directory name probed beside the running binary as a portable-mode
/// data root. A self-contained tarball that ships with a
/// `bookrack-data/` subdirectory next to the binary is then movable
/// (and the directory remains writable) without any environment
/// configuration.
pub const PORTABLE_DATA_NAME: &str = "bookrack-data";

/// Probe for a portable-mode data root beside the running binary.
///
/// Returns `Some(<exe_dir>/bookrack-data)` when that directory exists,
/// and `None` otherwise. The directory must already exist — a missing
/// one is treated as the user opting out of portable mode; the resolver
/// then falls through to the next precedence rung. A failure to locate
/// the executable returns `None` too, so the resolver continues rather
/// than treating it as an error.
pub fn portable_data_dir() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    portable_data_dir_from(exe_dir)
}

/// Pure resolution logic for [`portable_data_dir`], factored out so it
/// can be tested without locating the running executable.
fn portable_data_dir_from(exe_dir: Option<PathBuf>) -> Option<PathBuf> {
    let candidate = exe_dir?.join(PORTABLE_DATA_NAME);
    candidate.is_dir().then_some(candidate)
}

/// Filename of the platform-default registry written by `bookrack init`.
pub const DEFAULT_REGISTRY_NAME: &str = "registry.toml";

/// Path of the platform-default registry: the file `bookrack init`
/// writes so a freshly installed binary can find its data root without
/// an exported environment variable.
///
/// Returns `None` only when the platform config directory itself cannot
/// be located, which is unusual.
pub fn default_registry_path() -> Option<PathBuf> {
    default_registry_path_from(dirs::config_dir())
}

/// Pure form of [`default_registry_path`], factored out so the join
/// shape can be tested without depending on the host's HOME.
fn default_registry_path_from(config_dir: Option<PathBuf>) -> Option<PathBuf> {
    config_dir.map(|d| d.join("bookrack").join(DEFAULT_REGISTRY_NAME))
}

/// Resolve the registry file the write-side commands edit.
///
/// [`REGISTRY_ENV`] wins when set; otherwise the platform-default
/// registry path. Returns `None` when neither is available — the
/// caller then has no registry to record the change in. Used by both
/// the offline CLI write verbs and the daemon's `fork` helper so the
/// two agree on which file is the registry.
pub fn registry_target_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(REGISTRY_ENV) {
        return Some(PathBuf::from(path));
    }
    default_registry_path()
}

/// Load the platform-default registry, if present. A missing file is
/// not an error: the resolver simply falls through to
/// [`ConfigError::MissingDataDir`].
fn load_default_registry() -> Result<Option<Registry>, ConfigError> {
    let Some(path) = default_registry_path() else {
        return Ok(None);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(ConfigError::RegistryUnreadable { path, source }),
    };
    let registry =
        parse_registry(&text).map_err(|source| ConfigError::RegistryMalformed { path, source })?;
    Ok(Some(registry))
}

/// Filename of the PDFium dynamic library on this platform.
pub fn pdfium_library_filename() -> &'static str {
    if cfg!(target_os = "windows") {
        "pdfium.dll"
    } else if cfg!(target_os = "macos") {
        "libpdfium.dylib"
    } else {
        "libpdfium.so"
    }
}

/// Per-user directory where an operator-initiated install places the
/// pinned PDFium library; the last stop in the [`locate_pdfium`] search
/// chain. `None` when the platform data directory cannot be located.
pub fn pdfium_managed_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("bookrack").join("pdfium"))
}

/// Outcome of the PDFium library search.
#[derive(Debug)]
pub struct PdfiumLocation {
    /// First searched directory holding the platform library, if any.
    pub dir: Option<PathBuf>,
    /// Every directory that was checked, in search order.
    pub probed: Vec<PathBuf>,
}

/// Locate the directory to load the PDFium dynamic library from.
///
/// `BOOKRACK_PDFIUM_LIB`, when set, is authoritative: only that
/// directory is checked, so a typo in the override surfaces as a miss
/// instead of being papered over by a fallback. When unset, the running
/// executable's own directory (the release-archive layout) is checked
/// first, then the per-user managed directory installs land in.
///
/// This is a free function, not a [`Config`] method: the PDF adapter
/// needs the directory but takes no `Config`, and threading one through
/// only to reach this would widen its signature for nothing.
pub fn locate_pdfium() -> PdfiumLocation {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    locate_pdfium_from(
        std::env::var(PDFIUM_LIB_ENV).ok(),
        exe_dir,
        pdfium_managed_dir(),
        &|dir| dir.join(pdfium_library_filename()).is_file(),
    )
}

/// Pure search logic for [`locate_pdfium`], factored out so the chain
/// can be tested without mutating process-global environment variables
/// or touching the filesystem.
fn locate_pdfium_from(
    override_dir: Option<String>,
    exe_dir: Option<PathBuf>,
    managed_dir: Option<PathBuf>,
    has_library: &dyn Fn(&Path) -> bool,
) -> PdfiumLocation {
    let candidates: Vec<PathBuf> = match override_dir
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(dir) => vec![PathBuf::from(dir)],
        None => [exe_dir, managed_dir].into_iter().flatten().collect(),
    };
    let dir = candidates.iter().find(|d| has_library(d)).cloned();
    PdfiumLocation {
        dir,
        probed: candidates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::RawRegistryEntry;

    /// A directory guaranteed to exist, for happy-path resolution.
    fn existing_dir() -> String {
        std::env::temp_dir().to_string_lossy().into_owned()
    }

    /// The env-only resolution path (empty selection, no registry, no
    /// portable layout, no platform-default registry), exercising
    /// [`select_root`] + [`finish`] without touching the process
    /// environment.
    fn resolve(
        data_dir: Option<String>,
        ollama_url: Option<String>,
    ) -> Result<Config, ConfigError> {
        let root = select_root(&LibrarySelection::default(), data_dir, None, None, None)?;
        finish(root, ollama_url)
    }

    /// A two-entry registry with `prod` the default, for selection tests.
    fn sample_registry() -> Registry {
        parse_registry(
            "default = \"prod\"\n\
             [libraries]\n\
             prod = \"/roots/prod\"\n\
             test = \"/roots/test\"\n",
        )
        .expect("sample registry parses")
    }

    #[test]
    fn missing_data_dir_is_an_error() {
        assert!(matches!(
            resolve(None, None),
            Err(ConfigError::MissingDataDir)
        ));
        // Whitespace-only counts as unset.
        assert!(matches!(
            resolve(Some("   ".to_string()), None),
            Err(ConfigError::MissingDataDir)
        ));
    }

    #[test]
    fn nonexistent_data_dir_is_an_error() {
        let bogus = "this_path_does_not_exist_zzz/bookrack".to_string();
        assert!(matches!(
            resolve(Some(bogus), None),
            Err(ConfigError::DataDirNotFound(_))
        ));
    }

    #[test]
    fn data_dir_flag_wins_over_everything() {
        let selection = LibrarySelection {
            data_dir: Some(PathBuf::from("/explicit/root")),
            library: Some("test".to_string()),
        };
        let resolved = select_root(
            &selection,
            Some("/env/root".to_string()),
            Some(&sample_registry()),
            None,
            None,
        )
        .expect("the flag resolves");
        assert_eq!(resolved.data_dir, PathBuf::from("/explicit/root"));
        assert_eq!(resolved.source, ResolutionSource::DataDirFlag);
        assert_eq!(resolved.library, None);
    }

    #[test]
    fn library_flag_looks_up_the_registry() {
        let selection = LibrarySelection {
            data_dir: None,
            library: Some("test".to_string()),
        };
        // Wins over the data-root variable.
        let resolved = select_root(
            &selection,
            Some("/env/root".to_string()),
            Some(&sample_registry()),
            None,
            None,
        )
        .expect("the library resolves");
        assert_eq!(resolved.data_dir, PathBuf::from("/roots/test"));
        assert_eq!(resolved.source, ResolutionSource::LibraryFlag);
        assert_eq!(resolved.library.as_deref(), Some("test"));
    }

    #[test]
    fn unknown_library_is_an_error() {
        let selection = LibrarySelection {
            data_dir: None,
            library: Some("staging".to_string()),
        };
        match select_root(&selection, None, Some(&sample_registry()), None, None) {
            Err(ConfigError::UnknownLibrary { name, available }) => {
                assert_eq!(name, "staging");
                // sample_registry holds `prod` and `test`, sorted.
                assert_eq!(available, vec!["prod".to_string(), "test".to_string()]);
            }
            other => panic!("expected UnknownLibrary, got {other:?}"),
        }
    }

    #[test]
    fn unknown_library_error_message_lists_available_names() {
        let err = ConfigError::UnknownLibrary {
            name: "ghost".to_string(),
            available: vec!["alpha".to_string(), "beta".to_string()],
        };
        let rendered = format!("{err}");
        assert!(
            rendered.contains("no library named \"ghost\""),
            "missing name: {rendered}"
        );
        assert!(
            rendered.contains("available: alpha, beta"),
            "missing available list: {rendered}"
        );
    }

    #[test]
    fn unknown_library_error_renders_none_marker_when_registry_is_empty() {
        let err = ConfigError::UnknownLibrary {
            name: "ghost".to_string(),
            available: Vec::new(),
        };
        let rendered = format!("{err}");
        assert!(
            rendered.contains("available: <none>"),
            "expected <none> marker: {rendered}"
        );
    }

    #[test]
    fn library_without_a_registry_is_an_error() {
        let selection = LibrarySelection {
            data_dir: None,
            library: Some("prod".to_string()),
        };
        assert!(matches!(
            select_root(&selection, Some("/env/root".to_string()), None, None, None),
            Err(ConfigError::RegistryNotConfigured)
        ));
    }

    #[test]
    fn data_root_variable_wins_over_the_registry_default() {
        let selection = LibrarySelection::default();
        let resolved = select_root(
            &selection,
            Some("/env/root".to_string()),
            Some(&sample_registry()),
            None,
            None,
        )
        .expect("the variable resolves");
        assert_eq!(resolved.data_dir, PathBuf::from("/env/root"));
        assert_eq!(resolved.source, ResolutionSource::EnvVar);
        assert_eq!(resolved.library, None);
    }

    #[test]
    fn registry_default_is_the_last_resort() {
        let selection = LibrarySelection::default();
        // No flag, no data-root variable: fall to the registry default.
        let resolved = select_root(&selection, None, Some(&sample_registry()), None, None)
            .expect("the default resolves");
        assert_eq!(resolved.data_dir, PathBuf::from("/roots/prod"));
        assert_eq!(resolved.source, ResolutionSource::RegistryDefault);
        assert_eq!(resolved.library.as_deref(), Some("prod"));
    }

    #[test]
    fn no_source_at_all_is_missing_data_dir() {
        assert!(matches!(
            select_root(&LibrarySelection::default(), None, None, None, None),
            Err(ConfigError::MissingDataDir)
        ));
    }

    #[test]
    fn registry_without_a_default_falls_through() {
        let registry = parse_registry("[libraries]\nprod = \"/roots/prod\"\n")
            .expect("registry without a default parses");
        assert!(registry.default.is_none());
        assert!(matches!(
            select_root(
                &LibrarySelection::default(),
                None,
                Some(&registry),
                None,
                None,
            ),
            Err(ConfigError::MissingDataDir)
        ));
    }

    #[test]
    fn malformed_registry_is_rejected() {
        assert!(parse_registry("this is not = valid = toml").is_err());
    }

    #[test]
    fn ollama_url_defaults_when_unset() {
        let cfg = resolve(Some(existing_dir()), None).expect("valid data dir");
        assert_eq!(cfg.ollama_url(), DEFAULT_OLLAMA_URL);
    }

    #[test]
    fn ollama_url_is_taken_when_set() {
        let cfg = resolve(Some(existing_dir()), Some("http://host:9999".to_string()))
            .expect("valid data dir");
        assert_eq!(cfg.ollama_url(), "http://host:9999");
    }

    #[test]
    fn explicit_construction_reports_no_library_and_explicit_source() {
        let cfg = Config::new(PathBuf::from("root"), DEFAULT_OLLAMA_URL.to_string());
        assert_eq!(cfg.library(), None);
        assert_eq!(cfg.source(), ResolutionSource::Explicit);
    }

    #[test]
    fn paths_derive_from_the_data_root() {
        let root = PathBuf::from("root");
        let cfg = Config::new(root.clone(), DEFAULT_OLLAMA_URL.to_string());
        assert_eq!(cfg.sources_dir(), root.join("sources"));
        assert_eq!(cfg.books_dir(), root.join("books"));
        assert_eq!(cfg.corpus_db(), root.join("corpus.db"));
        assert_eq!(cfg.catalog_db(), root.join("catalog.db"));
        assert_eq!(cfg.lancedb_dir(), root.join("lancedb"));
        assert_eq!(cfg.papers_dir(), root.join("papers"));
        assert_eq!(cfg.papers_corpus_db(), root.join("papers_corpus.db"));
        assert_eq!(cfg.papers_catalog_db(), root.join("papers_catalog.db"));
        assert_eq!(cfg.papers_lancedb_dir(), root.join("lancedb_papers"));
        assert_eq!(cfg.logs_dir(), root.join("logs"));
    }

    #[test]
    fn backup_dir_defaults_under_the_data_root_and_honours_the_override() {
        let root = PathBuf::from("root");
        // Unset falls back to `backup/` under the data root.
        assert_eq!(backup_dir_from(&root, None), root.join("backup"));
        // Whitespace-only counts as unset.
        assert_eq!(
            backup_dir_from(&root, Some("   ".to_string())),
            root.join("backup")
        );
        // A set value wins, taken verbatim.
        assert_eq!(
            backup_dir_from(&root, Some("/elsewhere/backups".to_string())),
            PathBuf::from("/elsewhere/backups")
        );
    }

    #[test]
    fn embed_config_default_carries_the_calibrated_budget() {
        let cfg = EmbedConfig::default();
        assert_eq!(cfg.model, DEFAULT_EMBED_MODEL);
        // The spike's calibration knee.
        assert_eq!(cfg.batch_char_budget, 8_000);
        // The OOM-shrink floor must sit below the steady-state budget.
        assert!(cfg.batch_min_char_budget < cfg.batch_char_budget);
    }

    #[test]
    fn embed_config_from_env_overrides_operational_knobs() {
        let cfg = EmbedConfig::resolve_from(|key| match key {
            EMBED_MODEL_ENV => Some("custom-model".to_string()),
            EMBED_BATCH_CHAR_BUDGET_ENV => Some("4000".to_string()),
            EMBED_BATCH_MAX_CHUNKS_ENV => Some("32".to_string()),
            EMBED_BATCH_MIN_CHAR_BUDGET_ENV => Some("250".to_string()),
            _ => None,
        });
        assert_eq!(cfg.model, "custom-model");
        assert_eq!(cfg.batch_char_budget, 4_000);
        assert_eq!(cfg.batch_max_chunks, 32);
        assert_eq!(cfg.batch_min_char_budget, 250);
        // Untouched fields keep their calibrated defaults.
        let d = EmbedConfig::default();
        assert_eq!(cfg.request_timeout, d.request_timeout);
        assert_eq!(cfg.max_retries, d.max_retries);
        assert_eq!(cfg.channel_capacity, d.channel_capacity);
    }

    #[test]
    fn embed_config_progress_interval_default_and_override() {
        let d = EmbedConfig::default();
        assert_eq!(
            d.progress_interval,
            Duration::from_secs(DEFAULT_EMBED_PROGRESS_INTERVAL_SECS),
        );

        let cfg = EmbedConfig::resolve_from(|key| match key {
            EMBED_PROGRESS_INTERVAL_ENV => Some("12".to_string()),
            _ => None,
        });
        assert_eq!(cfg.progress_interval, Duration::from_secs(12));

        // A zero-second override opts into a heartbeat after every
        // batch — that is the documented contract of the env var.
        let burst = EmbedConfig::resolve_from(|key| match key {
            EMBED_PROGRESS_INTERVAL_ENV => Some("0".to_string()),
            _ => None,
        });
        assert_eq!(burst.progress_interval, Duration::ZERO);

        // Non-numeric falls back, never panics.
        let bad = EmbedConfig::resolve_from(|key| match key {
            EMBED_PROGRESS_INTERVAL_ENV => Some("not-a-number".to_string()),
            _ => None,
        });
        assert_eq!(
            bad.progress_interval,
            Duration::from_secs(DEFAULT_EMBED_PROGRESS_INTERVAL_SECS),
        );
    }

    #[test]
    fn embed_config_from_env_falls_back_on_blank_or_malformed() {
        let d = EmbedConfig::default();
        let cfg = EmbedConfig::resolve_from(|key| match key {
            // Whitespace-only counts as unset.
            EMBED_MODEL_ENV => Some("   ".to_string()),
            // Non-numeric falls back rather than failing.
            EMBED_BATCH_CHAR_BUDGET_ENV => Some("not-a-number".to_string()),
            _ => None,
        });
        assert_eq!(cfg.model, d.model);
        assert_eq!(cfg.batch_char_budget, d.batch_char_budget);
    }

    #[test]
    fn search_config_default_and_env_override() {
        assert_eq!(SearchConfig::default().top_k, DEFAULT_SEARCH_TOP_K);
        assert_eq!(
            SearchConfig::default().weak_distance_threshold,
            DEFAULT_SEARCH_WEAK_THRESHOLD,
        );

        let cfg = SearchConfig::resolve_from(|key| match key {
            SEARCH_TOP_K_ENV => Some("10".to_string()),
            SEARCH_WEAK_THRESHOLD_ENV => Some("0.42".to_string()),
            _ => None,
        });
        assert_eq!(cfg.top_k, 10);
        assert!((cfg.weak_distance_threshold - 0.42).abs() < 1e-6);

        // A blank value falls back to the default.
        let blank = SearchConfig::resolve_from(|_| Some("  ".to_string()));
        assert_eq!(blank.top_k, DEFAULT_SEARCH_TOP_K);
        assert_eq!(blank.weak_distance_threshold, DEFAULT_SEARCH_WEAK_THRESHOLD,);

        // Non-finite values fall back rather than poisoning the
        // threshold comparison downstream.
        let bad = SearchConfig::resolve_from(|key| match key {
            SEARCH_WEAK_THRESHOLD_ENV => Some("nan".to_string()),
            _ => None,
        });
        assert_eq!(bad.weak_distance_threshold, DEFAULT_SEARCH_WEAK_THRESHOLD,);
    }

    #[test]
    fn mcp_config_default_and_env_override() {
        assert_eq!(McpConfig::default().addr, DEFAULT_MCP_ADDR);

        let cfg = McpConfig::resolve_from(|key| match key {
            MCP_ADDR_ENV => Some("0.0.0.0:9000".to_string()),
            _ => None,
        });
        assert_eq!(cfg.addr, "0.0.0.0:9000");

        // A blank value falls back to the default.
        let blank = McpConfig::resolve_from(|_| Some("  ".to_string()));
        assert_eq!(blank.addr, DEFAULT_MCP_ADDR);
    }

    #[test]
    fn log_config_default_and_env_override() {
        let defaults = LogConfig::default();
        assert_eq!(defaults.directive, DEFAULT_LOG);
        assert_eq!(defaults.console_level, DEFAULT_LOG_CONSOLE);

        // An unset variable falls back to the default for each field.
        let unset = LogConfig::resolve_from(|_| None);
        assert_eq!(unset.directive, DEFAULT_LOG);
        assert_eq!(unset.console_level, DEFAULT_LOG_CONSOLE);

        // A set directive is taken verbatim.
        let set = LogConfig::resolve_from(|key| match key {
            LOG_ENV => Some("bookrack_ingest=debug,info".to_string()),
            _ => None,
        });
        assert_eq!(set.directive, "bookrack_ingest=debug,info");
        assert_eq!(set.console_level, DEFAULT_LOG_CONSOLE);

        // A whitespace-only value counts as unset.
        let blank = LogConfig::resolve_from(|_| Some("   ".to_string()));
        assert_eq!(blank.directive, DEFAULT_LOG);
        assert_eq!(blank.console_level, DEFAULT_LOG_CONSOLE);
    }

    #[test]
    fn log_config_console_level_env_override() {
        // A set console directive is taken verbatim and leaves the file
        // directive untouched.
        let console = LogConfig::resolve_from(|key| match key {
            LOG_CONSOLE_ENV => Some("debug".to_string()),
            _ => None,
        });
        assert_eq!(console.console_level, "debug");
        assert_eq!(console.directive, DEFAULT_LOG);

        // Both overrides apply independently.
        let both = LogConfig::resolve_from(|key| match key {
            LOG_ENV => Some("bookrack_ingest=debug,info".to_string()),
            LOG_CONSOLE_ENV => Some("bookrack=trace".to_string()),
            _ => None,
        });
        assert_eq!(both.directive, "bookrack_ingest=debug,info");
        assert_eq!(both.console_level, "bookrack=trace");

        // Whitespace-only console value counts as unset.
        let blank = LogConfig::resolve_from(|key| match key {
            LOG_CONSOLE_ENV => Some("   ".to_string()),
            _ => None,
        });
        assert_eq!(blank.console_level, DEFAULT_LOG_CONSOLE);
    }

    #[test]
    fn log_config_headless_daemon_mirrors_file_directive() {
        // With both overrides unset, the console mirrors the file
        // default so journalctl gets full daemon telemetry.
        let unset = LogConfig::resolve_headless_from(|_| None);
        assert_eq!(unset.directive, DEFAULT_LOG);
        assert_eq!(unset.console_level, DEFAULT_LOG);

        // BOOKRACK_LOG carries through to console as well when the
        // console override is unset.
        let file_only = LogConfig::resolve_headless_from(|key| match key {
            LOG_ENV => Some("bookrack=trace".to_string()),
            _ => None,
        });
        assert_eq!(file_only.directive, "bookrack=trace");
        assert_eq!(file_only.console_level, "bookrack=trace");

        // An explicit BOOKRACK_LOG_CONSOLE wins, leaving the file
        // directive untouched.
        let console_override = LogConfig::resolve_headless_from(|key| match key {
            LOG_CONSOLE_ENV => Some("warn".to_string()),
            _ => None,
        });
        assert_eq!(console_override.directive, DEFAULT_LOG);
        assert_eq!(console_override.console_level, "warn");
    }

    #[test]
    fn locate_pdfium_only_checks_the_override_when_set() {
        let pinned = PathBuf::from("a/pinned/pdfium/dir");
        let exe = PathBuf::from("exe/dir");
        let managed = PathBuf::from("managed/dir");

        let found = locate_pdfium_from(
            Some(pinned.display().to_string()),
            Some(exe.clone()),
            Some(managed.clone()),
            &|_| true,
        );
        assert_eq!(found.dir, Some(pinned.clone()));
        assert_eq!(found.probed, vec![pinned.clone()]);

        // An override without the library is a miss, not a fallthrough:
        // the other candidates must not even be probed.
        let missing = locate_pdfium_from(
            Some(pinned.display().to_string()),
            Some(exe),
            Some(managed),
            &|_| false,
        );
        assert_eq!(missing.dir, None);
        assert_eq!(missing.probed, vec![pinned]);
    }

    #[test]
    fn locate_pdfium_checks_exe_then_managed_dir_without_an_override() {
        let exe = PathBuf::from("exe/dir");
        let managed = PathBuf::from("managed/dir");

        // Whitespace-only override counts as unset.
        let hit = locate_pdfium_from(
            Some("   ".to_string()),
            Some(exe.clone()),
            Some(managed.clone()),
            &|d| d == managed.as_path(),
        );
        assert_eq!(hit.dir, Some(managed.clone()));
        assert_eq!(hit.probed, vec![exe.clone(), managed.clone()]);

        let miss = locate_pdfium_from(None, Some(exe.clone()), Some(managed.clone()), &|_| false);
        assert_eq!(miss.dir, None);
        assert_eq!(miss.probed, vec![exe, managed]);
    }

    #[test]
    fn library_entries_lists_every_library_and_marks_the_default() {
        let entries = library_entries(&sample_registry());
        assert_eq!(entries.len(), 2);
        // Sorted by name.
        assert_eq!(entries[0].name, "prod");
        assert_eq!(entries[1].name, "test");
        assert!(entries[0].is_default);
        assert!(!entries[1].is_default);
        assert_eq!(entries[0].data_dir, PathBuf::from("/roots/prod"));
        assert_eq!(entries[1].data_dir, PathBuf::from("/roots/test"));
    }

    #[test]
    fn library_entries_marks_no_default_when_registry_has_none() {
        let registry =
            parse_registry("[libraries]\nprod = \"/roots/prod\"\n").expect("registry parses");
        let entries = library_entries(&registry);
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].is_default);
    }

    #[test]
    fn list_libraries_from_returns_none_when_unset_or_blank() {
        assert!(matches!(list_libraries_from(None), Ok(None)));
        assert!(matches!(
            list_libraries_from(Some("   ".to_string())),
            Ok(None)
        ));
    }

    #[test]
    fn pdfium_library_filename_is_platform_specific() {
        let name = pdfium_library_filename();
        if cfg!(target_os = "windows") {
            assert_eq!(name, "pdfium.dll");
        } else if cfg!(target_os = "macos") {
            assert_eq!(name, "libpdfium.dylib");
        } else {
            assert_eq!(name, "libpdfium.so");
        }
    }

    #[test]
    fn locate_pdfium_probes_a_real_directory_in_a_live_process() {
        // The impure entry point must always have at least the running
        // executable's own directory to probe; an empty candidate list
        // would make the not-found report useless.
        assert!(!locate_pdfium().probed.is_empty());
    }

    #[test]
    fn missing_data_dir_error_points_at_init_wizard() {
        let rendered = format!("{}", ConfigError::MissingDataDir);
        assert!(
            rendered.contains("bookrack init"),
            "missing init pointer: {rendered}"
        );
        assert!(
            rendered.contains("--data-dir"),
            "missing data-dir option: {rendered}"
        );
        assert!(
            rendered.contains(DATA_DIR_ENV),
            "missing env var name: {rendered}"
        );
    }

    #[test]
    fn portable_data_dir_detects_a_neighbouring_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let exe_dir = tmp.path().to_path_buf();
        // Without the sibling directory, portable mode is off.
        assert!(portable_data_dir_from(Some(exe_dir.clone())).is_none());
        // Create the marker directory; portable mode is now on.
        let marker = exe_dir.join(PORTABLE_DATA_NAME);
        std::fs::create_dir(&marker).expect("create marker");
        assert_eq!(portable_data_dir_from(Some(exe_dir)), Some(marker));
    }

    #[test]
    fn portable_data_dir_returns_none_when_exe_dir_is_unknown() {
        assert!(portable_data_dir_from(None).is_none());
    }

    #[test]
    fn portable_data_dir_ignores_a_neighbouring_file() {
        // A file (not a directory) at the marker name must not be picked
        // up as a data root, even though it exists.
        let tmp = tempfile::tempdir().expect("tempdir");
        let exe_dir = tmp.path().to_path_buf();
        std::fs::write(exe_dir.join(PORTABLE_DATA_NAME), b"").expect("write marker");
        assert!(portable_data_dir_from(Some(exe_dir)).is_none());
    }

    #[test]
    fn default_registry_path_joins_under_the_platform_config_dir() {
        let parent = PathBuf::from("/abs/config");
        assert_eq!(
            default_registry_path_from(Some(parent.clone())),
            Some(parent.join("bookrack").join(DEFAULT_REGISTRY_NAME)),
        );
        assert!(default_registry_path_from(None).is_none());
    }

    #[test]
    fn portable_data_dir_beats_registry_default_but_loses_to_env_var() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let portable = tmp.path().to_path_buf();
        // With no env var, the portable layout wins over the registry.
        let resolved = select_root(
            &LibrarySelection::default(),
            None,
            Some(&sample_registry()),
            Some(portable.clone()),
            None,
        )
        .expect("portable resolves");
        assert_eq!(resolved.data_dir, portable);
        assert_eq!(resolved.source, ResolutionSource::PortableExeNeighbor);
        assert_eq!(resolved.library, None);
        // With the env var set, the env var still wins.
        let resolved = select_root(
            &LibrarySelection::default(),
            Some("/env/root".to_string()),
            Some(&sample_registry()),
            Some(portable),
            None,
        )
        .expect("env wins");
        assert_eq!(resolved.data_dir, PathBuf::from("/env/root"));
        assert_eq!(resolved.source, ResolutionSource::EnvVar);
    }

    #[test]
    fn default_registry_is_the_last_resort_before_missing_data_dir() {
        // No flag, no env var, no portable layout, no explicit registry:
        // the platform-default registry takes over.
        let resolved = select_root(
            &LibrarySelection::default(),
            None,
            None,
            None,
            Some(&sample_registry()),
        )
        .expect("default registry resolves");
        assert_eq!(resolved.data_dir, PathBuf::from("/roots/prod"));
        assert_eq!(resolved.source, ResolutionSource::DefaultRegistryDefault);
        assert_eq!(resolved.library.as_deref(), Some("prod"));
    }

    #[test]
    fn registry_default_beats_default_registry_default() {
        // The explicit registry (named by BOOKRACK_REGISTRY) wins over
        // the platform-default registry, so a user who opts in to a
        // named registry never has their choice silently overridden.
        let explicit = parse_registry(
            "default = \"alpha\"\n\
             [libraries]\n\
             alpha = \"/roots/alpha\"\n",
        )
        .expect("explicit registry parses");
        let resolved = select_root(
            &LibrarySelection::default(),
            None,
            Some(&explicit),
            None,
            Some(&sample_registry()),
        )
        .expect("explicit wins");
        assert_eq!(resolved.data_dir, PathBuf::from("/roots/alpha"));
        assert_eq!(resolved.source, ResolutionSource::RegistryDefault);
    }

    #[test]
    fn root_config_missing_file_is_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = load_root_config(tmp.path()).expect("missing file resolves to default");
        assert!(cfg.ollama_url.is_none());
        assert!(cfg.embed_model.is_none());
        assert!(cfg.mcp_addr.is_none());
        assert!(cfg.log_directive.is_none());
    }

    #[test]
    fn root_config_parses_known_fields() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join(ROOT_CONFIG_NAME),
            "ollama_url = \"http://elsewhere:1234\"\n\
             embed_model = \"alt-model\"\n",
        )
        .expect("write root config");
        let cfg = load_root_config(tmp.path()).expect("root config parses");
        assert_eq!(cfg.ollama_url.as_deref(), Some("http://elsewhere:1234"));
        assert_eq!(cfg.embed_model.as_deref(), Some("alt-model"));
        assert!(cfg.mcp_addr.is_none());
    }

    #[test]
    fn root_config_rejects_unknown_fields() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join(ROOT_CONFIG_NAME),
            "ollama_url = \"http://x\"\n\
             not_a_known_field = \"oops\"\n",
        )
        .expect("write root config");
        assert!(matches!(
            load_root_config(tmp.path()),
            Err(ConfigError::RootConfigMalformed { .. })
        ));
    }

    #[test]
    fn ollama_url_env_var_beats_root_config_beats_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        std::fs::write(
            root.join(ROOT_CONFIG_NAME),
            "ollama_url = \"http://from-file:5555\"\n",
        )
        .expect("write root config");
        let env_value = root.to_string_lossy().into_owned();

        // No env var: root config wins over the hardcoded default.
        let cfg = resolve(Some(env_value.clone()), None).expect("resolve with root config");
        assert_eq!(cfg.ollama_url(), "http://from-file:5555");
        // With env var set: env var wins over the root config.
        let cfg = resolve(Some(env_value), Some("http://from-env:7777".to_string()))
            .expect("resolve with env override");
        assert_eq!(cfg.ollama_url(), "http://from-env:7777");
    }

    #[test]
    fn merge_library_creates_fresh_registry_with_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("registry.toml");
        let data_root = tmp.path().join("a");
        std::fs::create_dir_all(&data_root).expect("create data root");
        merge_library_into_registry(&path, "default", &data_root).expect("merge");
        let registry = parse_registry(&std::fs::read_to_string(&path).expect("read"))
            .expect("registry parses");
        assert_eq!(registry.default.as_deref(), Some("default"));
        assert_eq!(
            registry.libraries.get("default").map(|e| e.data_dir()),
            Some(data_root.as_path())
        );
    }

    #[test]
    fn merge_library_creates_parent_directories() {
        // The platform default registry sits two directories deep
        // (`<config>/bookrack/registry.toml`); the writer creates them
        // so a fresh user does not have to mkdir first.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp
            .path()
            .join("nested")
            .join("under")
            .join("registry.toml");
        let data_root = tmp.path().join("a");
        std::fs::create_dir_all(&data_root).expect("create data root");
        merge_library_into_registry(&path, "default", &data_root).expect("merge");
        assert!(path.is_file());
    }

    #[test]
    fn merge_library_preserves_existing_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("registry.toml");
        // Existing registry already names `alpha` as default.
        std::fs::write(
            &path,
            "default = \"alpha\"\n\
             [libraries]\n\
             alpha = \"/roots/alpha\"\n",
        )
        .expect("seed");
        let beta_root = tmp.path().join("beta");
        std::fs::create_dir_all(&beta_root).expect("create beta");
        merge_library_into_registry(&path, "beta", &beta_root).expect("merge");
        let registry = parse_registry(&std::fs::read_to_string(&path).expect("read"))
            .expect("registry parses");
        // The default is untouched; both libraries are present.
        assert_eq!(registry.default.as_deref(), Some("alpha"));
        assert_eq!(
            registry.libraries.get("alpha").map(|e| e.data_dir()),
            Some(Path::new("/roots/alpha"))
        );
        assert_eq!(
            registry.libraries.get("beta").map(|e| e.data_dir()),
            Some(beta_root.as_path())
        );
    }

    #[test]
    fn merge_library_overwrites_an_existing_entry_with_the_same_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("registry.toml");
        std::fs::write(
            &path,
            "default = \"default\"\n\
             [libraries]\n\
             default = \"/old/root\"\n",
        )
        .expect("seed");
        let new_root = tmp.path().join("new");
        std::fs::create_dir_all(&new_root).expect("create new");
        merge_library_into_registry(&path, "default", &new_root).expect("merge");
        let registry = parse_registry(&std::fs::read_to_string(&path).expect("read"))
            .expect("registry parses");
        assert_eq!(
            registry.libraries.get("default").map(|e| e.data_dir()),
            Some(new_root.as_path())
        );
    }

    #[test]
    fn merge_library_rejects_a_libraries_key_of_the_wrong_type() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("registry.toml");
        std::fs::write(&path, "libraries = \"not-a-table\"\n").expect("seed");
        let err =
            merge_library_into_registry(&path, "default", tmp.path()).expect_err("should refuse");
        assert!(
            matches!(err, ConfigError::RegistryShape { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn set_default_upgrades_a_legacy_file_and_repoints_the_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("registry.toml");
        std::fs::write(
            &path,
            "default = \"alpha\"\n\
             [libraries]\n\
             alpha = \"/roots/alpha\"\n\
             beta = \"/roots/beta\"\n",
        )
        .expect("seed");
        set_default_library(&path, "beta").expect("set default");
        let text = std::fs::read_to_string(&path).expect("read");
        let registry = parse_registry(&text).expect("registry parses");
        assert_eq!(registry.default.as_deref(), Some("beta"));
        // The whole file is rewritten in table form: both legacy
        // entries are now tables, and both data roots survive.
        assert!(matches!(
            registry.libraries["alpha"],
            RawRegistryEntry::Table(_)
        ));
        assert!(matches!(
            registry.libraries["beta"],
            RawRegistryEntry::Table(_)
        ));
        assert_eq!(
            registry.libraries["alpha"].data_dir(),
            Path::new("/roots/alpha")
        );
    }

    #[test]
    fn set_default_rejects_an_unknown_library() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("registry.toml");
        std::fs::write(&path, "[libraries]\nalpha = \"/roots/alpha\"\n").expect("seed");
        let err = set_default_library(&path, "ghost").expect_err("unknown library");
        match err {
            ConfigError::UnknownLibrary { name, available } => {
                assert_eq!(name, "ghost");
                assert_eq!(available, vec!["alpha".to_string()]);
            }
            other => panic!("expected UnknownLibrary, got {other:?}"),
        }
    }

    #[test]
    fn a_write_leaves_a_complete_reparsable_file() {
        // The atomic writer must never leave a truncated file: after a
        // write the registry reparses cleanly and reflects the change.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("registry.toml");
        std::fs::write(&path, "[libraries]\nalpha = \"/roots/alpha\"\n").expect("seed");
        set_default_library(&path, "alpha").expect("set default");
        let text = std::fs::read_to_string(&path).expect("read");
        let registry = parse_registry(&text).expect("file reparses after write");
        assert_eq!(registry.default.as_deref(), Some("alpha"));
    }

    #[test]
    fn a_write_preserves_unknown_top_level_keys() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("registry.toml");
        std::fs::write(
            &path,
            "future_knob = \"keep me\"\n\
             [libraries]\n\
             alpha = \"/roots/alpha\"\n",
        )
        .expect("seed");
        set_default_library(&path, "alpha").expect("set default");
        let doc = std::fs::read_to_string(&path)
            .expect("read")
            .parse::<toml::Table>()
            .expect("parses");
        assert_eq!(
            doc.get("future_knob").and_then(toml::Value::as_str),
            Some("keep me"),
            "an unknown top-level key must survive a write"
        );
    }

    #[test]
    fn remove_library_forgets_the_entry_and_drops_a_dangling_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("registry.toml");
        std::fs::write(
            &path,
            "default = \"alpha\"\n\
             [libraries]\n\
             alpha = \"/roots/alpha\"\n\
             beta = \"/roots/beta\"\n",
        )
        .expect("seed");
        remove_library_from_registry(&path, "alpha").expect("remove");
        let registry = parse_registry(&std::fs::read_to_string(&path).expect("read"))
            .expect("registry parses");
        assert!(!registry.libraries.contains_key("alpha"));
        assert!(registry.libraries.contains_key("beta"));
        // `default` pointed at the removed library, so it is dropped
        // rather than left dangling.
        assert_eq!(registry.default, None);
    }

    #[test]
    fn remove_library_rejects_an_unknown_library() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("registry.toml");
        std::fs::write(&path, "[libraries]\nalpha = \"/roots/alpha\"\n").expect("seed");
        let err = remove_library_from_registry(&path, "ghost").expect_err("unknown library");
        assert!(
            matches!(err, ConfigError::UnknownLibrary { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn upsert_library_entry_writes_every_set_metadata_field() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("registry.toml");
        let entry = LibraryEntryFields {
            data_dir: PathBuf::from("/roots/prod"),
            kind: LibraryKind::Test,
            description: Some("primary".to_string()),
            index_profile: Some("qwen3-0.6b-default".to_string()),
            created_at: Some("2026-06-30T12:00:00Z".to_string()),
            uuid: Some("01890a5d-0000-7000-8000-000000000000".to_string()),
        };
        upsert_library_entry(&path, "prod", &entry).expect("upsert");
        let registry = parse_registry(&std::fs::read_to_string(&path).expect("read"))
            .expect("registry parses");
        // First entry becomes the default when none was recorded.
        assert_eq!(registry.default.as_deref(), Some("prod"));
        let raw = &registry.libraries["prod"];
        assert_eq!(raw.data_dir(), Path::new("/roots/prod"));
        assert_eq!(raw.kind(), LibraryKind::Test);
        assert_eq!(raw.description(), Some("primary"));
        assert_eq!(raw.index_profile(), Some("qwen3-0.6b-default"));
        assert_eq!(raw.created_at(), Some("2026-06-30T12:00:00Z"));
        assert_eq!(raw.uuid(), Some("01890a5d-0000-7000-8000-000000000000"));
    }

    #[test]
    fn merge_library_rejects_a_malformed_existing_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("registry.toml");
        std::fs::write(&path, "this is not = valid = toml").expect("seed");
        let err =
            merge_library_into_registry(&path, "default", tmp.path()).expect_err("should refuse");
        assert!(
            matches!(err, ConfigError::RegistryMalformed { .. }),
            "got {err:?}"
        );
    }
}
