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
pub mod llama_server_pin;
mod manifest;
mod registry;
pub mod reranker_model_pin;

pub use detect::{
    DetectError, DetectVerdict, ScanOutcome, Signal, detect_library, mounted_volumes,
    scan_for_libraries,
};
pub use manifest::{
    LibraryManifest, MANIFEST_FILENAME, MANIFEST_FORMAT, MANIFEST_SCHEMA_VERSION, ManifestError,
    load_manifest, new_manifest, render_manifest_toml, write_manifest,
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

/// Environment variable naming the `llama-server` executable the
/// reranker backend spawns. Authoritative when set; see
/// [`llama_server_pin::locate_llama_server`].
pub const LLAMA_SERVER_BIN_ENV: &str = "BOOKRACK_LLAMA_SERVER_BIN";

/// Environment variable naming the reranker GGUF model file.
/// Authoritative when set; see
/// [`reranker_model_pin::locate_reranker_model`].
pub const RERANKER_MODEL_ENV: &str = "BOOKRACK_RERANKER_MODEL";

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
    shadowed_default: Option<ShadowedDefault>,
    library_identification: Option<LibraryIdentification>,
}

/// A registry `default` library that a path-class resolution silently
/// overrides.
///
/// When the data root is won by a path — the `--data-dir` flag,
/// [`DATA_DIR_ENV`], or the portable exe-neighbour layout — a registry
/// `default = "<name>"` set by the operator has no effect on which root
/// is served, yet stays on disk. This records the eclipsed default so
/// `bookrack info` and `bookrack doctor` can surface it, instead of the
/// operator having to infer the eclipse from [`ResolutionSource`] alone.
/// The precedence itself is unchanged: the path source still wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowedDefault {
    /// The registry name the eclipsed `default` points at.
    pub name: String,
    /// The data root that name maps to — the root that would have been
    /// served had no path source pre-empted it.
    pub data_dir: PathBuf,
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

/// How a resolved [`Config`]'s library name was determined — a second
/// axis orthogonal to [`ResolutionSource`], which records how the *root*
/// was chosen rather than who the root is.
///
/// A registry-class source (`--library` or a registry `default`) carries
/// its name straight from the selection, reported as
/// [`LibraryIdentification::Selected`]. A path-class root — the
/// `--data-dir` flag, [`DATA_DIR_ENV`], or the portable layout — is
/// anonymous at selection time; [`Config::resolve`] matches it back
/// against the registry, claiming a name by manifest uuid
/// ([`LibraryIdentification::ManifestUuid`]) or, failing that, by path
/// ([`LibraryIdentification::Path`]). A root that matches no entry stays
/// anonymous and has no identification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryIdentification {
    /// The name came directly from the registry selection: `--library`
    /// or a registry `default` entry.
    Selected,
    /// A path-class root was claimed by matching its manifest uuid to a
    /// registry entry.
    ManifestUuid,
    /// A path-class root was claimed by matching its path to a registry
    /// entry — the root has no manifest, or its uuid matched no entry.
    Path,
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
    /// Two configured index-profile facts disagree, so no single
    /// effective retrieval combination exists. Constructed through
    /// [`ConfigError::profile_model_conflict`] or
    /// [`ConfigError::profile_reference_conflict`], which spell out the
    /// two values and the repair paths.
    #[error("index profile conflict: {message}")]
    ProfileConfigConflict { message: String },
}

impl ConfigError {
    /// The explicit `embed_model` in `config.toml` and the model the
    /// referenced index profile declares resolve to different values.
    /// Neither side is silently preferred; the operator removes the
    /// explicit field or repoints the profile reference.
    pub fn profile_model_conflict(
        profile: &str,
        profile_model: &str,
        explicit_model: &str,
    ) -> ConfigError {
        ConfigError::ProfileConfigConflict {
            message: format!(
                "config.toml sets embed_model = {explicit_model:?} but index profile \
                 {profile:?} declares {profile_model:?}; unset embed_model \
                 (`bookrack libraries config <name> --unset embed_model`) or reference \
                 a profile that declares {explicit_model:?}"
            ),
        }
    }

    /// `config.toml` and the registry entry both name an index profile
    /// and the names differ. Neither side is treated as newer; the
    /// operator aligns them explicitly.
    pub fn profile_reference_conflict(config_value: &str, registry_value: &str) -> ConfigError {
        ConfigError::ProfileConfigConflict {
            message: format!(
                "config.toml references index profile {config_value:?} but the registry \
                 entry records {registry_value:?}; set both to the same name \
                 (`bookrack libraries config <name> index_profile=...` edits config.toml)"
            ),
        }
    }
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

    pub fn resolve(selection: &LibrarySelection) -> Result<Config, ConfigError> {
        // A missing .env is fine: the variables may be set directly.
        dotenvy::dotenv().ok();
        let registry = load_registry(std::env::var(REGISTRY_ENV).ok())?;
        let default_registry = load_default_registry()?;
        let portable = portable_data_dir();
        let resolved = select_root(
            selection,
            std::env::var(DATA_DIR_ENV).ok(),
            registry.as_ref(),
            portable,
            default_registry.as_ref(),
        )?;
        let mut config = finish(resolved, std::env::var(OLLAMA_URL_ENV).ok())?;
        // A path-class resolution can leave a registry `default` set but
        // ineffective. Detecting it here — after the root is chosen —
        // enriches the Config without touching which root won.
        config.shadowed_default = detect_shadowed_default(
            config.source,
            &config.data_dir,
            registry.as_ref(),
            default_registry.as_ref(),
        );
        // A path-class root arrives anonymous; claim its registry name by
        // manifest uuid, then by path. Orthogonal to the shadow above and
        // never fails — an unmatched or unreadable root stays anonymous.
        let (identified, identification) = identify_library(
            config.source,
            &config.data_dir,
            registry.as_ref(),
            default_registry.as_ref(),
        );
        if identified.is_some() {
            config.library = identified;
        }
        config.library_identification = identification;
        Ok(config)
    }

    pub fn new(data_dir: PathBuf, ollama_url: String) -> Config {
        Config {
            data_dir,
            ollama_url,
            library: None,
            source: ResolutionSource::Explicit,
            root_config: RootConfig::default(),
            shadowed_default: None,
            library_identification: None,
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

    /// The registry `default` library eclipsed by a path-class
    /// resolution, when one is set but has no effect on the served root.
    /// `None` when the default won, no default is set, or the served
    /// root already matches the default's root. Surfaced by `bookrack
    /// info` and `bookrack doctor` so a silently-overridden default is
    /// visible instead of inferred.
    pub fn shadowed_default(&self) -> Option<&ShadowedDefault> {
        self.shadowed_default.as_ref()
    }

    /// How the resolved library name was determined — `Selected` for a
    /// registry selection, `ManifestUuid` or `Path` when a path-class
    /// root was claimed against the registry after selection. `Some`
    /// exactly when [`Config::library`] is, and `None` for an anonymous
    /// path-class root or [`Config::new`]. The second axis alongside
    /// [`Config::source`]: source says how the root was chosen,
    /// identification says who the root turned out to be.
    pub fn library_identification(&self) -> Option<LibraryIdentification> {
        self.library_identification
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
/// embed model, the MCP listen address, the log filter directive, the
/// index-profile reference, and the search knobs. Each field is `None`
/// when the file does not set it; the matching env var (where one
/// exists) overrides this layer, and the hardcoded default wins when
/// both are absent. Written by `bookrack init`; safe to edit by hand.
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
    /// Name of the index profile this library runs under. The profile
    /// resolves outside this crate; the resolved embed model slots into
    /// the resolution chain below the explicit `embed_model` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_profile: Option<String>,
    /// Retrieval knobs, under a `[search]` table.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<RootSearchConfig>,
}

/// The `[search]` table of `<data_root>/config.toml`: the persistent
/// counterpart of the `BOOKRACK_SEARCH_*` env overrides. Fields are
/// optional so a file can pin one knob without freezing the other.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RootSearchConfig {
    /// How many nearest passages a query returns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<usize>,
    /// Cosine-distance threshold at or above which a hit counts as weak.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weak_threshold: Option<f32>,
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
        ..RootConfig::default()
    };
    let body = toml::to_string(&cfg).expect("RootConfig serialization is infallible");
    format!("# bookrack root config. Written by `bookrack init`; safe to edit.\n{body}")
}

/// The keys `bookrack libraries config` accepts, mirroring the fields of
/// [`RootConfig`]. A `set` or `unset` of any other key is refused so a
/// typo cannot silently leave `deny_unknown_fields` to reject the file on
/// the next load. A test asserts this list stays in lockstep with the
/// struct's serialized field names. A dotted key addresses a field in a
/// nested table (`search.top_k` edits `top_k` under `[search]`).
pub const ROOT_CONFIG_KEYS: &[&str] = &[
    "ollama_url",
    "embed_model",
    "mcp_addr",
    "log_directive",
    "index_profile",
    "search.top_k",
    "search.weak_threshold",
];

/// Name the environment variable that overrides `key` in the resolution
/// chain (`env` > `config.toml` > default), or `None` when the key has no
/// env counterpart. Lets a caller warn that a value it just wrote is
/// currently shadowed by a set variable.
pub fn root_config_env_override(key: &str) -> Option<&'static str> {
    match key {
        "ollama_url" => Some(OLLAMA_URL_ENV),
        "embed_model" => Some(EMBED_MODEL_ENV),
        "mcp_addr" => Some(MCP_ADDR_ENV),
        "log_directive" => Some(LOG_ENV),
        "search.top_k" => Some(SEARCH_TOP_K_ENV),
        "search.weak_threshold" => Some(SEARCH_WEAK_THRESHOLD_ENV),
        _ => None,
    }
}

/// Read `<data_root>/config.toml` verbatim, preserving comments and
/// layout for a human-readable dump. A missing file resolves to an empty
/// string so a caller can present "no config yet" without a separate
/// branch; an unreadable file surfaces as
/// [`ConfigError::RootConfigUnreadable`].
pub fn read_root_config_text(data_dir: &Path) -> Result<String, ConfigError> {
    let path = data_dir.join(ROOT_CONFIG_NAME);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(source) => Err(ConfigError::RootConfigUnreadable { path, source }),
    }
}

/// Why an edit of `<data_root>/config.toml` could not proceed. Split from
/// [`ConfigError`] so the CLI maps an operator-input fault (an unknown
/// key, an unparseable value, a hand-corrupted file) to its user-error
/// exit code, while a genuine I/O failure keeps the internal-error one.
#[derive(Debug, thiserror::Error)]
pub enum RootConfigSetError {
    /// A `key=value` set (or an `unset`) named a key outside
    /// [`ROOT_CONFIG_KEYS`].
    #[error("unknown key '{key}'; accepted keys are {}", ROOT_CONFIG_KEYS.join(", "))]
    UnknownKey {
        /// The rejected key.
        key: String,
    },
    /// A value failed the light shape check for its key.
    #[error("invalid value for '{key}': {reason}")]
    InvalidValue {
        /// The key whose value was rejected.
        key: String,
        /// A short account of what the value should look like.
        reason: String,
    },
    /// The existing file is not parseable as TOML, so an edit that would
    /// preserve its comments cannot begin.
    #[error("the root config at {} is malformed: {reason}", .path.display())]
    Malformed {
        /// The config file path.
        path: PathBuf,
        /// The parse failure, formatted.
        reason: String,
    },
    /// Reading the existing file failed.
    #[error(transparent)]
    Io(#[from] ConfigError),
    /// Writing the edited file back failed.
    #[error("cannot write the root config at {}", .path.display())]
    Write {
        /// The config file path.
        path: PathBuf,
        /// The write failure.
        #[source]
        source: std::io::Error,
    },
}

/// Apply `sets` and `unsets` to `<data_root>/config.toml`, editing the
/// document in place so hand-written comments and key ordering survive.
///
/// Every key is checked against [`ROOT_CONFIG_KEYS`] and every set value
/// against a light per-key shape check before anything is written, so a
/// rejected batch leaves the file untouched. A missing file starts from
/// an empty document, so a data root that predates `config.toml` can
/// still be configured. The write is atomic.
pub fn set_root_config_values(
    data_dir: &Path,
    sets: &[(String, String)],
    unsets: &[String],
) -> Result<(), RootConfigSetError> {
    for (key, value) in sets {
        validate_root_config_key(key)?;
        validate_root_config_value(key, value)?;
    }
    for key in unsets {
        validate_root_config_key(key)?;
    }

    let path = data_dir.join(ROOT_CONFIG_NAME);
    let text = read_root_config_text(data_dir)?;
    let mut doc =
        text.parse::<toml_edit::DocumentMut>()
            .map_err(|source| RootConfigSetError::Malformed {
                path: path.clone(),
                reason: source.to_string(),
            })?;

    /// Render a validated value as the TOML type its key requires, so a
    /// numeric knob round-trips as a number rather than a quoted string.
    fn root_config_value_item(key: &str, value: &str) -> toml_edit::Item {
        match key {
            "search.top_k" => toml_edit::value(value.parse::<i64>().expect("validated integer")),
            "search.weak_threshold" => {
                toml_edit::value(value.parse::<f64>().expect("validated number"))
            }
            _ => toml_edit::value(value),
        }
    }

    for (key, value) in sets {
        let item = root_config_value_item(key, value);
        match key.split_once('.') {
            Some((table, field)) => {
                let entry = doc.entry(table).or_insert(toml_edit::table());
                if entry.as_table_like().is_none() {
                    *entry = toml_edit::table();
                }
                entry[field] = item;
            }
            None => doc[key.as_str()] = item,
        }
    }
    for key in unsets {
        match key.split_once('.') {
            Some((table, field)) => {
                if let Some(t) = doc.get_mut(table).and_then(toml_edit::Item::as_table_mut) {
                    t.remove(field);
                    if t.is_empty() {
                        doc.remove(table);
                    }
                }
            }
            None => {
                doc.remove(key.as_str());
            }
        }
    }

    write_atomically(&path, &doc.to_string())
        .map_err(|source| RootConfigSetError::Write { path, source })
}

/// Reject a key that is not one bookrack recognizes.
fn validate_root_config_key(key: &str) -> Result<(), RootConfigSetError> {
    if ROOT_CONFIG_KEYS.contains(&key) {
        Ok(())
    } else {
        Err(RootConfigSetError::UnknownKey {
            key: key.to_string(),
        })
    }
}

/// Light per-key value check: `ollama_url` must carry a scheme and an
/// authority, `mcp_addr` must have a `host:port` shape with a numeric
/// port, `search.top_k` must be a positive integer, and
/// `search.weak_threshold` a finite number. `embed_model`,
/// `log_directive`, and `index_profile` are free-form — a model tag has
/// no fixed grammar, a tracing directive is validated by the runtime
/// that consumes it, and a profile reference is checked against the
/// profile store by the caller that can resolve it.
fn validate_root_config_value(key: &str, value: &str) -> Result<(), RootConfigSetError> {
    let invalid = |reason: &str| {
        Err(RootConfigSetError::InvalidValue {
            key: key.to_string(),
            reason: reason.to_string(),
        })
    };
    match key {
        "ollama_url" => {
            let Some((scheme, rest)) = value.split_once("://") else {
                return invalid("expected a URL like 'http://host:port'");
            };
            if scheme.is_empty() || rest.is_empty() {
                return invalid("expected a URL like 'http://host:port'");
            }
            Ok(())
        }
        "mcp_addr" => {
            let Some((host, port)) = value.rsplit_once(':') else {
                return invalid("expected a 'host:port' address");
            };
            if host.is_empty() || port.parse::<u16>().is_err() {
                return invalid("expected a 'host:port' address with a numeric port");
            }
            Ok(())
        }
        "search.top_k" => match value.parse::<i64>() {
            Ok(n) if n > 0 => Ok(()),
            _ => invalid("expected a positive integer"),
        },
        "search.weak_threshold" => match value.parse::<f32>() {
            Ok(v) if v.is_finite() => Ok(()),
            _ => invalid("expected a finite number"),
        },
        _ => Ok(()),
    }
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
    /// Resolve from the environment alone, ignoring the per-root config
    /// layer. Callers that hold a loaded [`RootConfig`] (and possibly a
    /// resolved index profile) use [`EmbedConfig::resolve`] instead so
    /// the file and profile layers participate.
    ///
    /// Only operational knobs are read here: the model tag and the
    /// batching budgets. Content-identity parameters (chunk length,
    /// overlap, grouping) are not configurable — they are frozen with
    /// `CHUNK_VERSION` so a change forces a re-derivation. A malformed or
    /// empty value falls back to the default rather than failing.
    pub fn from_env() -> EmbedConfig {
        EmbedConfig::resolve_from(|key| std::env::var(key).ok(), &RootConfig::default(), None)
    }

    /// Resolve the model by the full precedence chain: env var > explicit
    /// `config.toml` field > profile-derived value > hardcoded default.
    /// The index profile resolves outside this crate; `profile_model` is
    /// the embed model it declares, when a profile is in effect. The
    /// batching budgets remain env-only knobs.
    pub fn resolve(root: &RootConfig, profile_model: Option<&str>) -> EmbedConfig {
        EmbedConfig::resolve_from(|key| std::env::var(key).ok(), root, profile_model)
    }

    /// Pure resolution, factored out so it can be tested without mutating
    /// process-global environment variables.
    fn resolve_from(
        get: impl Fn(&str) -> Option<String>,
        root: &RootConfig,
        profile_model: Option<&str>,
    ) -> EmbedConfig {
        let d = EmbedConfig::default();
        EmbedConfig {
            model: env_trimmed(get(EMBED_MODEL_ENV))
                .or_else(|| env_trimmed(root.embed_model.clone()))
                .or_else(|| profile_model.map(str::to_string))
                .unwrap_or(d.model),
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
    /// Resolve from the environment alone, ignoring the per-root config
    /// layer. Callers that hold a loaded [`RootConfig`] use
    /// [`SearchConfig::resolve`] instead so the `[search]` table
    /// participates.
    pub fn from_env() -> SearchConfig {
        SearchConfig::resolve_from(|key| std::env::var(key).ok(), &RootConfig::default())
    }

    /// Resolve each knob by precedence `env var > config.toml [search] >
    /// hardcoded default`. A malformed or empty env value falls through
    /// to the file layer rather than failing.
    pub fn resolve(root: &RootConfig) -> SearchConfig {
        SearchConfig::resolve_from(|key| std::env::var(key).ok(), root)
    }

    /// Pure resolution, factored out so it can be tested without mutating
    /// process-global environment variables.
    fn resolve_from(get: impl Fn(&str) -> Option<String>, root: &RootConfig) -> SearchConfig {
        let file = root.search.clone().unwrap_or_default();
        SearchConfig {
            top_k: env_usize(
                get(SEARCH_TOP_K_ENV),
                file.top_k.unwrap_or(DEFAULT_SEARCH_TOP_K),
            ),
            weak_distance_threshold: env_f32(
                get(SEARCH_WEAK_THRESHOLD_ENV),
                file.weak_threshold.unwrap_or(DEFAULT_SEARCH_WEAK_THRESHOLD),
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

/// Detect a registry `default` library eclipsed by a path-class
/// resolution. Pure over its inputs — a bypass enrichment run after
/// [`select_root`] chose the root, so it never influences which root
/// wins and never fails.
///
/// A shadow exists only when the root was won by a path source
/// ([`ResolutionSource::DataDirFlag`], [`ResolutionSource::EnvVar`], or
/// [`ResolutionSource::PortableExeNeighbor`]) *and* a registry sets a
/// `default` that resolves to a root other than `resolved_dir`. A
/// `--library` selection and the two registry-default rungs are the
/// operator's intent or the default already winning, so neither shadows.
/// The default is looked up registry-first, then the platform-default
/// registry, matching [`select_root`]'s 5th and 6th rungs.
fn detect_shadowed_default(
    source: ResolutionSource,
    resolved_dir: &Path,
    registry: Option<&Registry>,
    default_registry: Option<&Registry>,
) -> Option<ShadowedDefault> {
    match source {
        ResolutionSource::DataDirFlag
        | ResolutionSource::EnvVar
        | ResolutionSource::PortableExeNeighbor => {}
        ResolutionSource::LibraryFlag
        | ResolutionSource::RegistryDefault
        | ResolutionSource::DefaultRegistryDefault
        | ResolutionSource::Explicit => return None,
    }
    let (name, data_dir) = registry
        .and_then(registry_default_root)
        .or_else(|| default_registry.and_then(registry_default_root))?;
    if data_dir == resolved_dir {
        None
    } else {
        Some(ShadowedDefault { name, data_dir })
    }
}

/// The registry's `default` library name paired with its resolved root,
/// or `None` when no default is set or it names an entry the registry
/// does not carry. Pure over the registry.
fn registry_default_root(registry: &Registry) -> Option<(String, PathBuf)> {
    let name = registry.default.clone()?;
    let data_dir = lookup_library(registry, &name).ok()?;
    Some((name, data_dir))
}

/// Determine the resolved root's library name and how it was identified,
/// run after [`select_root`] and [`finish`]. Pure over its inputs and,
/// like [`detect_shadowed_default`], a bypass enrichment that never fails
/// and never changes which root won.
///
/// A registry-class source (`--library`, either registry `default`) has
/// its name already in hand from selection, so this reports
/// [`LibraryIdentification::Selected`] without re-deriving it (the name
/// component is `None`, leaving the selected name untouched). A
/// path-class source is anonymous, so the effective root's manifest is
/// matched back against the registry: an `Ok(Some(_))` manifest is tried
/// by uuid first ([`LibraryIdentification::ManifestUuid`]); an absent
/// manifest, or a uuid that matches no entry, falls back to a path match
/// ([`LibraryIdentification::Path`]). A manifest that fails to read
/// yields no claim at all — the root stays anonymous rather than being
/// claimed on shaky identity. The registry is consulted before the
/// platform-default registry, matching [`select_root`]'s precedence.
fn identify_library(
    source: ResolutionSource,
    resolved_dir: &Path,
    registry: Option<&Registry>,
    default_registry: Option<&Registry>,
) -> (Option<String>, Option<LibraryIdentification>) {
    match source {
        ResolutionSource::LibraryFlag
        | ResolutionSource::RegistryDefault
        | ResolutionSource::DefaultRegistryDefault => {
            return (None, Some(LibraryIdentification::Selected));
        }
        ResolutionSource::Explicit => return (None, None),
        ResolutionSource::DataDirFlag
        | ResolutionSource::EnvVar
        | ResolutionSource::PortableExeNeighbor => {}
    }
    let registries: Vec<&Registry> = [registry, default_registry].into_iter().flatten().collect();
    match load_manifest(resolved_dir) {
        Ok(Some(manifest)) => {
            for reg in &registries {
                if let Some(entry) = find_library_by_uuid(reg, &manifest.uuid) {
                    return (Some(entry.name), Some(LibraryIdentification::ManifestUuid));
                }
            }
            claim_root_by_path(&registries, resolved_dir)
        }
        Ok(None) => claim_root_by_path(&registries, resolved_dir),
        Err(_) => (None, None),
    }
}

/// Claim a registry name for `resolved_dir` by matching an entry's data
/// root to it, registry before platform-default registry. Both sides are
/// canonicalized best-effort before comparison — a failure to
/// canonicalize (a nonexistent or unreadable path) falls back to a raw
/// comparison — so a mount alias or symlink still matches.
fn claim_root_by_path(
    registries: &[&Registry],
    resolved_dir: &Path,
) -> (Option<String>, Option<LibraryIdentification>) {
    for reg in registries {
        if let Some(entry) = library_entries(reg)
            .into_iter()
            .find(|entry| same_root(&entry.data_dir, resolved_dir))
        {
            return (Some(entry.name), Some(LibraryIdentification::Path));
        }
    }
    (None, None)
}

/// Whether two paths name the same root, comparing canonicalized forms
/// and falling back to a raw comparison when canonicalization fails.
fn same_root(a: &Path, b: &Path) -> bool {
    let ca = a.canonicalize();
    let cb = b.canonicalize();
    match (ca, cb) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
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
        shadowed_default: None,
        library_identification: None,
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

/// Remove the library named `name` from the registry, discarding the
/// [`RemoveReport`]. The unit-returning form of [`remove_library`] for
/// callers that need only the side effect.
pub fn remove_library_from_registry(path: &Path, name: &str) -> Result<(), ConfigError> {
    remove_library(path, name).map(|_| ())
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

/// Why a high-level registration operation ([`add_library`]) could not
/// proceed. Kept distinct from [`ConfigError`] so the CLI maps an
/// operator-input fault (a bad target, an unreadable identity) to its
/// user-error exit code while a genuine registry or manifest I/O failure
/// keeps the internal-error one.
#[derive(Debug, thiserror::Error)]
pub enum LibraryOpError {
    /// The target path does not exist or is not a directory.
    #[error("{0}")]
    BadTarget(String),
    /// The target directory carries an identity manifest that cannot be
    /// read (foreign magic or a future schema version). Registration
    /// refuses to guess; the operator repairs the manifest or re-mints
    /// it with `--new-uuid`.
    #[error("{path} has an unreadable identity manifest: {reason}", path = .path.display())]
    UnreadableTarget {
        /// The target data root.
        path: PathBuf,
        /// The manifest read failure, formatted.
        reason: String,
    },
    /// Reading the manifest-write confirmation response failed.
    #[error("failed to read confirmation: {0}")]
    Confirm(#[source] std::io::Error),
    /// A registry read or write failed.
    #[error(transparent)]
    Registry(#[from] ConfigError),
    /// Writing or re-minting the target's identity manifest failed for a
    /// reason other than a read-only volume.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

/// Options for [`add_library`] beyond the entry fields.
#[derive(Debug, Default, Clone, Copy)]
pub struct AddOptions {
    /// Re-mint a fresh uuid for the target root before registering,
    /// rewriting its identity manifest. Resolves a uuid clash by turning
    /// the new root into a genuine copy with its own identity.
    pub new_uuid: bool,
}

/// The result of an [`add_library`] call: either a completed
/// registration, or the ambiguity the CLI must resolve interactively
/// before anything is written.
#[derive(Debug)]
pub enum AddOutcome {
    /// The library was registered.
    Registered(AddReport),
    /// The manifest-write confirmation was declined; nothing was written.
    Aborted,
    /// A key derived from the manifest or directory name already belongs
    /// to a different library. The operator must pick an explicit alias.
    KeyTaken {
        /// The contested key.
        key: String,
        /// The data root the key already maps to.
        existing_path: PathBuf,
    },
    /// The target root's identity uuid already belongs to a registered
    /// library. The operator chooses between repointing the existing
    /// entry (a move) and re-minting the uuid (a copy, `--new-uuid`).
    UuidClash {
        /// The shared uuid.
        uuid: String,
        /// The name of the entry that already carries it.
        existing_key: String,
        /// The data root that entry maps to.
        existing_path: PathBuf,
    },
}

/// What [`add_library`] did, for the CLI to render.
#[derive(Debug)]
pub struct AddReport {
    /// The key the entry was recorded under.
    pub key: String,
    /// The data root registered.
    pub data_dir: PathBuf,
    /// The uuid cached into the entry, or `None` when the manifest could
    /// not be written (a read-only root).
    pub uuid: Option<String>,
    /// True when a fresh identity manifest was written to the root.
    pub wrote_manifest: bool,
    /// True when the manifest write was skipped because the root is
    /// read-only, so the entry carries no uuid cache.
    pub read_only_degraded: bool,
    /// True when this entry became the registry default (none was
    /// recorded before).
    pub became_default: bool,
}

/// Register a data root under the registry at `registry_path`, writing an
/// identity manifest to the root first when it has none.
///
/// `key` is the explicit registry name (`add <name>`, or `register
/// --name <alias>`); `None` derives the key from the root's manifest
/// birth name, or its directory basename when there is no manifest.
///
/// The operation stops without writing when it needs an operator
/// decision: a derived key that already names a different library
/// ([`AddOutcome::KeyTaken`]), or a manifest uuid already registered
/// under another name ([`AddOutcome::UuidClash`]). When the root has no
/// manifest, `confirm_manifest` is called with the manifest that would
/// be written; returning `Ok(false)` yields [`AddOutcome::Aborted`]. A
/// manifest write that fails because the root is read-only degrades to a
/// uuid-less registration rather than an error, so a snapshot or optical
/// volume can still be registered.
pub fn add_library<C>(
    registry_path: &Path,
    key: Option<&str>,
    data_dir: &Path,
    kind: Option<LibraryKind>,
    description: Option<String>,
    opts: AddOptions,
    confirm_manifest: C,
) -> Result<AddOutcome, LibraryOpError>
where
    C: FnOnce(&LibraryManifest) -> std::io::Result<bool>,
{
    let verdict = detect_library(data_dir).map_err(|e| LibraryOpError::BadTarget(e.to_string()))?;
    let existing = match verdict {
        DetectVerdict::Confirmed(manifest) => Some(manifest),
        DetectVerdict::Probable { .. } | DetectVerdict::NotALibrary { .. } => None,
        DetectVerdict::Unreadable { reason } => {
            return Err(LibraryOpError::UnreadableTarget {
                path: data_dir.to_path_buf(),
                reason,
            });
        }
    };

    let explicit_key = key.is_some();
    let key = resolve_add_key(key, existing.as_ref(), data_dir);
    let registry = load_registry_at(registry_path)?;
    let entries = library_entries(&registry);
    let became_default = registry.default.is_none();

    // A derived key that already names a root at a different path is a
    // collision the operator must break with an explicit alias. An
    // explicit key always wins — it overwrites its own entry by design.
    if !explicit_key
        && let Some(other) = entries
            .iter()
            .find(|e| e.name == key && e.data_dir != *data_dir)
    {
        return Ok(AddOutcome::KeyTaken {
            key,
            existing_path: other.data_dir.clone(),
        });
    }

    // Entry kind and description: an explicit flag wins over the manifest.
    let kind = kind
        .or_else(|| existing.as_ref().map(|m| m.kind))
        .unwrap_or_default();
    let description = description.or_else(|| existing.as_ref().and_then(|m| m.description.clone()));

    let mut wrote_manifest = false;
    let mut read_only_degraded = false;
    let (uuid, created_at) = if let Some(manifest) = &existing {
        if opts.new_uuid {
            let fresh = regenerate_manifest_uuid(data_dir)?;
            wrote_manifest = true;
            (Some(fresh.uuid), fresh.created_at)
        } else {
            if let Some(clash) =
                find_library_by_uuid(&registry, &manifest.uuid).filter(|e| e.name != key)
            {
                return Ok(AddOutcome::UuidClash {
                    uuid: manifest.uuid.clone(),
                    existing_key: clash.name.clone(),
                    existing_path: clash.data_dir.clone(),
                });
            }
            (Some(manifest.uuid.clone()), manifest.created_at.clone())
        }
    } else {
        let manifest = new_manifest(key.clone(), kind, description.clone());
        match confirm_manifest(&manifest) {
            Ok(true) => {}
            Ok(false) => return Ok(AddOutcome::Aborted),
            Err(e) => return Err(LibraryOpError::Confirm(e)),
        }
        match write_manifest(data_dir, &manifest) {
            Ok(()) => {
                wrote_manifest = true;
                (Some(manifest.uuid), manifest.created_at)
            }
            Err(ManifestError::Io { error, .. }) if is_read_only(&error) => {
                read_only_degraded = true;
                (None, None)
            }
            Err(e) => return Err(LibraryOpError::Manifest(e)),
        }
    };

    let fields = LibraryEntryFields {
        data_dir: data_dir.to_path_buf(),
        kind,
        description,
        index_profile: None,
        created_at,
        uuid: uuid.clone(),
    };
    upsert_library_entry(registry_path, &key, &fields)?;

    Ok(AddOutcome::Registered(AddReport {
        key,
        data_dir: data_dir.to_path_buf(),
        uuid,
        wrote_manifest,
        read_only_degraded,
        became_default,
    }))
}

/// The registry key an [`add_library`] call records under: an explicit
/// name wins, else the manifest birth name, else the directory basename.
fn resolve_add_key(
    explicit: Option<&str>,
    manifest: Option<&LibraryManifest>,
    data_dir: &Path,
) -> String {
    if let Some(key) = explicit {
        return key.to_string();
    }
    if let Some(manifest) = manifest {
        return manifest.name.clone();
    }
    data_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "library".to_string())
}

/// Whether an I/O error means the target filesystem rejects writes —
/// a read-only volume or a directory the caller cannot write to. Such a
/// manifest write degrades to a uuid-less registration instead of
/// failing outright.
fn is_read_only(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
    )
}

/// Regenerate the identity uuid of the manifest at `data_dir`, writing
/// the rewritten manifest back atomically and returning it. The root
/// must already carry a readable manifest; the birth name, kind,
/// description, and creation timestamp are preserved.
pub fn regenerate_manifest_uuid(data_dir: &Path) -> Result<LibraryManifest, ManifestError> {
    let mut manifest = load_manifest(data_dir)?.ok_or_else(|| ManifestError::NotALibrary {
        path: data_dir.join(MANIFEST_FILENAME),
    })?;
    manifest.uuid = uuid::Uuid::now_v7().to_string();
    write_manifest(data_dir, &manifest)?;
    Ok(manifest)
}

/// Point an existing registry entry at a new data root, leaving its other
/// metadata untouched. The move branch of a uuid clash: a library that
/// reappeared at a new path keeps its single entry, which follows it,
/// rather than gaining a duplicate.
pub fn repoint_library(
    registry_path: &Path,
    name: &str,
    new_data_dir: &Path,
) -> Result<(), ConfigError> {
    let mut doc = read_registry_table(registry_path)?;
    let upgraded = normalize_registry_entries(&mut doc, registry_path)?;
    if !registry_has_library(&doc, name) {
        return Err(ConfigError::UnknownLibrary {
            name: name.to_string(),
            available: registry_library_names(&doc),
        });
    }
    let entry = doc
        .get_mut("libraries")
        .and_then(toml::Value::as_table_mut)
        .and_then(|libraries| libraries.get_mut(name))
        .and_then(toml::Value::as_table_mut)
        .expect("registry_has_library confirmed a table entry");
    entry.insert(
        "data_dir".to_string(),
        toml::Value::String(new_data_dir.display().to_string()),
    );
    write_registry_table(registry_path, &doc)?;
    if upgraded {
        emit_registry_upgrade_notice();
    }
    Ok(())
}

/// What [`remove_library`] removed, for the CLI to act on.
#[derive(Debug)]
pub struct RemoveReport {
    /// The data root the forgotten entry mapped to. `--purge` deletes it
    /// after the entry is dropped.
    pub data_dir: PathBuf,
    /// True when the removed entry was the registry default, so the
    /// `default` pointer was cleared to avoid a dangling reference.
    pub default_cleared: bool,
}

/// Remove the library named `name` from the registry, returning the data
/// root it mapped to and whether the default pointer was cleared. The
/// data root itself is never touched. Errors with
/// [`ConfigError::UnknownLibrary`] when no such entry exists.
pub fn remove_library(registry_path: &Path, name: &str) -> Result<RemoveReport, ConfigError> {
    let mut doc = read_registry_table(registry_path)?;
    let upgraded = normalize_registry_entries(&mut doc, registry_path)?;
    let removed = doc
        .get_mut("libraries")
        .and_then(toml::Value::as_table_mut)
        .and_then(|libraries| libraries.remove(name));
    let Some(removed) = removed else {
        return Err(ConfigError::UnknownLibrary {
            name: name.to_string(),
            available: registry_library_names(&doc),
        });
    };
    let data_dir = removed
        .as_table()
        .and_then(|entry| entry.get("data_dir"))
        .and_then(toml::Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_default();
    let default_cleared = doc.get("default").and_then(toml::Value::as_str) == Some(name);
    if default_cleared {
        doc.remove("default");
    }
    write_registry_table(registry_path, &doc)?;
    if upgraded {
        emit_registry_upgrade_notice();
    }
    Ok(RemoveReport {
        data_dir,
        default_cleared,
    })
}

/// Look up one registry entry by name at `path`, returning `None` when
/// the registry defines no such entry (or no registry file exists). The
/// read-side counterpart the CLI uses to inspect an entry — its data
/// root, its detect verdict — before a destructive `remove --purge`.
pub fn find_library(path: &Path, name: &str) -> Result<Option<LibraryEntry>, ConfigError> {
    let registry = load_registry_at(path)?;
    Ok(library_entries(&registry)
        .into_iter()
        .find(|entry| entry.name == name))
}

/// Look up one registry entry by manifest uuid, returning `None` when no
/// entry carries a matching uuid. The uuid-keyed counterpart to
/// [`find_library`]: [`add_library`] uses it to detect a uuid already
/// registered under another name, and [`Config::resolve`] to claim a
/// path-class root's registry name.
pub fn find_library_by_uuid(registry: &Registry, uuid: &str) -> Option<LibraryEntry> {
    library_entries(registry)
        .into_iter()
        .find(|entry| entry.uuid.as_deref() == Some(uuid))
}

/// Load the registry at `path` as a strongly typed [`Registry`]. A
/// missing file resolves to an empty registry so the caller can check a
/// fresh location without a separate branch.
fn load_registry_at(path: &Path) -> Result<Registry, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(ConfigError::RegistryUnreadable {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    parse_registry(&text).map_err(|source| ConfigError::RegistryMalformed {
        path: path.to_path_buf(),
        source,
    })
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
    fn env_path_shadows_a_default_that_points_elsewhere() {
        // The env variable won the root, but the registry still names
        // `prod` (/roots/prod) as default: an eclipse the operator
        // should be able to see.
        let shadow = detect_shadowed_default(
            ResolutionSource::EnvVar,
            Path::new("/env/root"),
            Some(&sample_registry()),
            None,
        )
        .expect("an eclipsed default is detected");
        assert_eq!(shadow.name, "prod");
        assert_eq!(shadow.data_dir, PathBuf::from("/roots/prod"));
    }

    #[test]
    fn data_dir_flag_shadows_like_the_env_variable() {
        let shadow = detect_shadowed_default(
            ResolutionSource::DataDirFlag,
            Path::new("/explicit/root"),
            Some(&sample_registry()),
            None,
        )
        .expect("the flag eclipses the default too");
        assert_eq!(shadow.name, "prod");
    }

    #[test]
    fn portable_layout_shadows_like_the_env_variable() {
        let shadow = detect_shadowed_default(
            ResolutionSource::PortableExeNeighbor,
            Path::new("/portable/bookrack-data"),
            Some(&sample_registry()),
            None,
        )
        .expect("the portable layout eclipses the default too");
        assert_eq!(shadow.name, "prod");
        assert_eq!(shadow.data_dir, PathBuf::from("/roots/prod"));
    }

    #[test]
    fn no_shadow_when_the_default_root_equals_the_resolved_root() {
        // The env variable points at the very root the default names:
        // no conflict, so nothing is eclipsed.
        assert_eq!(
            detect_shadowed_default(
                ResolutionSource::EnvVar,
                Path::new("/roots/prod"),
                Some(&sample_registry()),
                None,
            ),
            None
        );
    }

    #[test]
    fn no_shadow_for_an_explicit_library_selection() {
        // `--library` is the operator naming a root outright; it never
        // reads as an eclipse.
        assert_eq!(
            detect_shadowed_default(
                ResolutionSource::LibraryFlag,
                Path::new("/roots/test"),
                Some(&sample_registry()),
                None,
            ),
            None
        );
    }

    #[test]
    fn no_shadow_when_no_default_is_set() {
        let registry = parse_registry("[libraries]\nprod = \"/roots/prod\"\n")
            .expect("registry without a default parses");
        assert_eq!(
            detect_shadowed_default(
                ResolutionSource::EnvVar,
                Path::new("/env/root"),
                Some(&registry),
                None,
            ),
            None
        );
    }

    #[test]
    fn shadow_takes_the_registry_default_over_the_platform_default() {
        // Both registries set a default; the primary registry wins, in
        // step with select_root's 5th-before-6th precedence.
        let default_registry = parse_registry(
            "default = \"platform\"\n\
             [libraries]\n\
             platform = \"/roots/platform\"\n",
        )
        .expect("platform-default registry parses");
        let shadow = detect_shadowed_default(
            ResolutionSource::EnvVar,
            Path::new("/env/root"),
            Some(&sample_registry()),
            Some(&default_registry),
        )
        .expect("a shadow is detected");
        assert_eq!(shadow.name, "prod");
        assert_eq!(shadow.data_dir, PathBuf::from("/roots/prod"));
    }

    #[test]
    fn shadow_falls_to_the_platform_default_when_the_registry_has_none() {
        let registry = parse_registry("[libraries]\nprod = \"/roots/prod\"\n")
            .expect("registry without a default parses");
        let default_registry = parse_registry(
            "default = \"platform\"\n\
             [libraries]\n\
             platform = \"/roots/platform\"\n",
        )
        .expect("platform-default registry parses");
        let shadow = detect_shadowed_default(
            ResolutionSource::EnvVar,
            Path::new("/env/root"),
            Some(&registry),
            Some(&default_registry),
        )
        .expect("the platform default is eclipsed");
        assert_eq!(shadow.name, "platform");
        assert_eq!(shadow.data_dir, PathBuf::from("/roots/platform"));
    }

    #[test]
    fn identify_claims_a_path_class_root_by_manifest_uuid() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let manifest = new_manifest("birth-name", LibraryKind::Prod, None);
        write_manifest(root, &manifest).expect("write manifest");
        // The registry entry points elsewhere but shares the uuid: the
        // uuid match claims it regardless of the recorded path.
        let registry = parse_registry(&format!(
            "[libraries.hammer]\ndata_dir = \"/roots/elsewhere\"\nuuid = \"{}\"\n",
            manifest.uuid
        ))
        .expect("registry parses");
        let (name, id) = identify_library(ResolutionSource::EnvVar, root, Some(&registry), None);
        assert_eq!(name.as_deref(), Some("hammer"));
        assert_eq!(id, Some(LibraryIdentification::ManifestUuid));
    }

    #[test]
    fn identify_falls_back_to_a_path_match_when_no_manifest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let registry = parse_registry(&format!(
            "[libraries.hammer]\ndata_dir = \"{}\"\n",
            root.display()
        ))
        .expect("registry parses");
        let (name, id) =
            identify_library(ResolutionSource::DataDirFlag, root, Some(&registry), None);
        assert_eq!(name.as_deref(), Some("hammer"));
        assert_eq!(id, Some(LibraryIdentification::Path));
    }

    #[test]
    fn identify_yields_nothing_when_neither_uuid_nor_path_matches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let registry = parse_registry(
            "[libraries.other]\ndata_dir = \"/roots/other\"\nuuid = \"unrelated-uuid\"\n",
        )
        .expect("registry parses");
        let (name, id) = identify_library(
            ResolutionSource::PortableExeNeighbor,
            root,
            Some(&registry),
            None,
        );
        assert_eq!(name, None);
        assert_eq!(id, None);
    }

    #[test]
    fn identify_makes_no_claim_when_the_manifest_is_unreadable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        // A file whose magic does not match makes load_manifest return
        // Err; even a path entry pointing at this root does not rescue it.
        std::fs::write(root.join(MANIFEST_FILENAME), "format = \"not-bookrack\"\n")
            .expect("write foreign manifest");
        let registry = parse_registry(&format!(
            "[libraries.hammer]\ndata_dir = \"{}\"\n",
            root.display()
        ))
        .expect("registry parses");
        let (name, id) = identify_library(ResolutionSource::EnvVar, root, Some(&registry), None);
        assert_eq!(name, None);
        assert_eq!(id, None);
    }

    #[test]
    fn identify_reports_selected_for_a_registry_source_without_reading_disk() {
        // A registry-class source keeps its selected name; passing a
        // nonexistent root and no registries proves no manifest is read.
        let (name, id) = identify_library(
            ResolutionSource::LibraryFlag,
            Path::new("/does/not/exist"),
            None,
            None,
        );
        assert_eq!(name, None);
        assert_eq!(id, Some(LibraryIdentification::Selected));
    }

    #[test]
    fn identify_reports_nothing_for_an_explicit_config() {
        let (name, id) = identify_library(
            ResolutionSource::Explicit,
            Path::new("/anywhere"),
            None,
            None,
        );
        assert_eq!(name, None);
        assert_eq!(id, None);
    }

    #[test]
    fn identify_prefers_the_registry_over_the_platform_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let manifest = new_manifest("birth-name", LibraryKind::Prod, None);
        write_manifest(root, &manifest).expect("write manifest");
        // Both registries carry the uuid under different names; the
        // primary registry wins, in step with select_root's precedence.
        let registry = parse_registry(&format!(
            "[libraries.primary]\ndata_dir = \"/roots/primary\"\nuuid = \"{}\"\n",
            manifest.uuid
        ))
        .expect("registry parses");
        let default_registry = parse_registry(&format!(
            "[libraries.platform]\ndata_dir = \"/roots/platform\"\nuuid = \"{}\"\n",
            manifest.uuid
        ))
        .expect("platform-default registry parses");
        let (name, id) = identify_library(
            ResolutionSource::EnvVar,
            root,
            Some(&registry),
            Some(&default_registry),
        );
        assert_eq!(name.as_deref(), Some("primary"));
        assert_eq!(id, Some(LibraryIdentification::ManifestUuid));
    }

    #[test]
    fn find_library_by_uuid_matches_the_entry_carrying_the_uuid() {
        let registry = parse_registry(
            "[libraries.a]\ndata_dir = \"/roots/a\"\nuuid = \"uuid-a\"\n\
             [libraries.b]\ndata_dir = \"/roots/b\"\nuuid = \"uuid-b\"\n",
        )
        .expect("registry parses");
        assert_eq!(
            find_library_by_uuid(&registry, "uuid-b").map(|e| e.name),
            Some("b".to_string())
        );
        assert_eq!(find_library_by_uuid(&registry, "uuid-z"), None);
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
        let cfg = EmbedConfig::resolve_from(
            |key| match key {
                EMBED_MODEL_ENV => Some("custom-model".to_string()),
                EMBED_BATCH_CHAR_BUDGET_ENV => Some("4000".to_string()),
                EMBED_BATCH_MAX_CHUNKS_ENV => Some("32".to_string()),
                EMBED_BATCH_MIN_CHAR_BUDGET_ENV => Some("250".to_string()),
                _ => None,
            },
            &RootConfig::default(),
            None,
        );
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
    fn embed_model_resolution_orders_env_file_profile_default() {
        let file = RootConfig {
            embed_model: Some("file-model".to_string()),
            ..RootConfig::default()
        };
        let env = |key: &str| match key {
            EMBED_MODEL_ENV => Some("env-model".to_string()),
            _ => None,
        };

        // Every layer set: the env var wins.
        let all = EmbedConfig::resolve_from(env, &file, Some("profile-model"));
        assert_eq!(all.model, "env-model");

        // No env: the explicit config.toml field wins over the profile.
        let no_env = EmbedConfig::resolve_from(|_| None, &file, Some("profile-model"));
        assert_eq!(no_env.model, "file-model");

        // No env, no explicit field: the profile-derived value wins.
        let profile_only =
            EmbedConfig::resolve_from(|_| None, &RootConfig::default(), Some("profile-model"));
        assert_eq!(profile_only.model, "profile-model");

        // Nothing set anywhere: the hardcoded default.
        let bare = EmbedConfig::resolve_from(|_| None, &RootConfig::default(), None);
        assert_eq!(bare.model, DEFAULT_EMBED_MODEL);
    }

    #[test]
    fn search_config_file_layer_sits_between_env_and_default() {
        let file = RootConfig {
            search: Some(RootSearchConfig {
                top_k: Some(7),
                weak_threshold: Some(0.25),
            }),
            ..RootConfig::default()
        };

        // No env: the [search] table supplies both knobs.
        let from_file = SearchConfig::resolve_from(|_| None, &file);
        assert_eq!(from_file.top_k, 7);
        assert!((from_file.weak_distance_threshold - 0.25).abs() < 1e-6);

        // Env set: it wins over the file.
        let from_env = SearchConfig::resolve_from(
            |key| match key {
                SEARCH_TOP_K_ENV => Some("3".to_string()),
                _ => None,
            },
            &file,
        );
        assert_eq!(from_env.top_k, 3);
        assert!((from_env.weak_distance_threshold - 0.25).abs() < 1e-6);

        // A malformed env value falls through to the file, not the
        // hardcoded default.
        let bad_env = SearchConfig::resolve_from(
            |key| match key {
                SEARCH_TOP_K_ENV => Some("not-a-number".to_string()),
                _ => None,
            },
            &file,
        );
        assert_eq!(bad_env.top_k, 7);

        // A file that pins one knob leaves the other at its default.
        let partial = RootConfig {
            search: Some(RootSearchConfig {
                top_k: Some(9),
                weak_threshold: None,
            }),
            ..RootConfig::default()
        };
        let half = SearchConfig::resolve_from(|_| None, &partial);
        assert_eq!(half.top_k, 9);
        assert_eq!(half.weak_distance_threshold, DEFAULT_SEARCH_WEAK_THRESHOLD);
    }

    #[test]
    fn profile_conflict_messages_carry_both_values_and_a_repair_path() {
        let model = ConfigError::profile_model_conflict("prof", "profile-model", "file-model");
        let text = model.to_string();
        assert!(text.contains("\"file-model\""));
        assert!(text.contains("\"profile-model\""));
        assert!(text.contains("--unset embed_model"));

        let reference = ConfigError::profile_reference_conflict("a-prof", "b-prof");
        let text = reference.to_string();
        assert!(text.contains("\"a-prof\""));
        assert!(text.contains("\"b-prof\""));
        assert!(text.contains("libraries config"));
    }

    #[test]
    fn embed_config_progress_interval_default_and_override() {
        let d = EmbedConfig::default();
        assert_eq!(
            d.progress_interval,
            Duration::from_secs(DEFAULT_EMBED_PROGRESS_INTERVAL_SECS),
        );

        let cfg = EmbedConfig::resolve_from(
            |key| match key {
                EMBED_PROGRESS_INTERVAL_ENV => Some("12".to_string()),
                _ => None,
            },
            &RootConfig::default(),
            None,
        );
        assert_eq!(cfg.progress_interval, Duration::from_secs(12));

        // A zero-second override opts into a heartbeat after every
        // batch — that is the documented contract of the env var.
        let burst = EmbedConfig::resolve_from(
            |key| match key {
                EMBED_PROGRESS_INTERVAL_ENV => Some("0".to_string()),
                _ => None,
            },
            &RootConfig::default(),
            None,
        );
        assert_eq!(burst.progress_interval, Duration::ZERO);

        // Non-numeric falls back, never panics.
        let bad = EmbedConfig::resolve_from(
            |key| match key {
                EMBED_PROGRESS_INTERVAL_ENV => Some("not-a-number".to_string()),
                _ => None,
            },
            &RootConfig::default(),
            None,
        );
        assert_eq!(
            bad.progress_interval,
            Duration::from_secs(DEFAULT_EMBED_PROGRESS_INTERVAL_SECS),
        );
    }

    #[test]
    fn embed_config_from_env_falls_back_on_blank_or_malformed() {
        let d = EmbedConfig::default();
        let cfg = EmbedConfig::resolve_from(
            |key| match key {
                // Whitespace-only counts as unset.
                EMBED_MODEL_ENV => Some("   ".to_string()),
                // Non-numeric falls back rather than failing.
                EMBED_BATCH_CHAR_BUDGET_ENV => Some("not-a-number".to_string()),
                _ => None,
            },
            &RootConfig::default(),
            None,
        );
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

        let cfg = SearchConfig::resolve_from(
            |key| match key {
                SEARCH_TOP_K_ENV => Some("10".to_string()),
                SEARCH_WEAK_THRESHOLD_ENV => Some("0.42".to_string()),
                _ => None,
            },
            &RootConfig::default(),
        );
        assert_eq!(cfg.top_k, 10);
        assert!((cfg.weak_distance_threshold - 0.42).abs() < 1e-6);

        // A blank value falls back to the default.
        let blank = SearchConfig::resolve_from(|_| Some("  ".to_string()), &RootConfig::default());
        assert_eq!(blank.top_k, DEFAULT_SEARCH_TOP_K);
        assert_eq!(blank.weak_distance_threshold, DEFAULT_SEARCH_WEAK_THRESHOLD,);

        // Non-finite values fall back rather than poisoning the
        // threshold comparison downstream.
        let bad = SearchConfig::resolve_from(
            |key| match key {
                SEARCH_WEAK_THRESHOLD_ENV => Some("nan".to_string()),
                _ => None,
            },
            &RootConfig::default(),
        );
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
    fn set_root_config_writes_typed_nested_search_keys() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join(ROOT_CONFIG_NAME),
            "# hand-written note\nembed_model = \"m\"\n",
        )
        .expect("seed config");
        set_root_config_values(
            tmp.path(),
            &[
                ("index_profile".to_string(), "some-profile".to_string()),
                ("search.top_k".to_string(), "7".to_string()),
                ("search.weak_threshold".to_string(), "0.4".to_string()),
            ],
            &[],
        )
        .expect("set nested keys");

        // Numeric knobs land as TOML numbers, not quoted strings, and
        // the dotted keys land under a `[search]` table.
        let text = read_root_config_text(tmp.path()).expect("read config");
        assert!(text.contains("# hand-written note"));
        assert!(text.contains("[search]"));
        assert!(text.contains("top_k = 7"));
        assert!(!text.contains("top_k = \"7\""));

        // The written file round-trips through the typed loader.
        let cfg = load_root_config(tmp.path()).expect("load config");
        assert_eq!(cfg.index_profile.as_deref(), Some("some-profile"));
        let search = cfg.search.expect("search table present");
        assert_eq!(search.top_k, Some(7));
        assert!((search.weak_threshold.expect("threshold") - 0.4).abs() < 1e-6);

        // Unsetting both search keys removes the now-empty table.
        set_root_config_values(
            tmp.path(),
            &[],
            &[
                "search.top_k".to_string(),
                "search.weak_threshold".to_string(),
            ],
        )
        .expect("unset nested keys");
        let text = read_root_config_text(tmp.path()).expect("read config");
        assert!(!text.contains("[search]"));
        let cfg = load_root_config(tmp.path()).expect("load config");
        assert!(cfg.search.is_none());
        assert_eq!(cfg.index_profile.as_deref(), Some("some-profile"));
    }

    #[test]
    fn set_root_config_rejects_bad_search_values() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for (key, value) in [
            ("search.top_k", "0"),
            ("search.top_k", "not-a-number"),
            ("search.top_k", "-3"),
            ("search.weak_threshold", "nan"),
            ("search.weak_threshold", "inf"),
            ("search.weak_threshold", "abc"),
        ] {
            let err =
                set_root_config_values(tmp.path(), &[(key.to_string(), value.to_string())], &[])
                    .expect_err("bad value refused");
            assert!(matches!(err, RootConfigSetError::InvalidValue { .. }));
        }
        // Nothing was written by the rejected batches.
        assert_eq!(read_root_config_text(tmp.path()).expect("read"), "");
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
    fn root_config_keys_match_struct_fields() {
        // Every serialized field of a fully-populated `RootConfig` must
        // appear in `ROOT_CONFIG_KEYS`, so adding a field without
        // extending the write whitelist fails here rather than silently
        // rejecting the key at runtime. A nested table flattens to
        // dotted keys, matching the whitelist's addressing scheme.
        let full = RootConfig {
            ollama_url: Some("http://x:1".to_string()),
            embed_model: Some("m".to_string()),
            mcp_addr: Some("127.0.0.1:1".to_string()),
            log_directive: Some("info".to_string()),
            index_profile: Some("p".to_string()),
            search: Some(RootSearchConfig {
                top_k: Some(5),
                weak_threshold: Some(0.5),
            }),
        };
        let value = toml::Value::try_from(&full).expect("RootConfig serializes to a table");
        let mut fields: Vec<String> = Vec::new();
        for (key, item) in value.as_table().expect("RootConfig is a table") {
            match item.as_table() {
                Some(nested) => {
                    fields.extend(nested.keys().map(|inner| format!("{key}.{inner}")));
                }
                None => fields.push(key.clone()),
            }
        }
        fields.sort();
        let mut keys: Vec<String> = ROOT_CONFIG_KEYS.iter().map(|k| k.to_string()).collect();
        keys.sort();
        assert_eq!(fields, keys);
    }

    #[test]
    fn set_root_config_preserves_comments_and_edits_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join(ROOT_CONFIG_NAME),
            "# hand-written note\nembed_model = \"old-model\"\n",
        )
        .expect("seed config");
        set_root_config_values(
            tmp.path(),
            &[("embed_model".to_string(), "new-model".to_string())],
            &[],
        )
        .expect("set applies");
        let text = read_root_config_text(tmp.path()).expect("read back");
        assert!(text.contains("# hand-written note"));
        assert!(text.contains("new-model"));
        assert!(!text.contains("old-model"));
    }

    #[test]
    fn set_root_config_rejects_unknown_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = set_root_config_values(
            tmp.path(),
            &[("not_a_key".to_string(), "x".to_string())],
            &[],
        )
        .expect_err("unknown key rejected");
        assert!(matches!(err, RootConfigSetError::UnknownKey { .. }));
        // The batch is rejected before any write.
        assert!(!tmp.path().join(ROOT_CONFIG_NAME).exists());
    }

    #[test]
    fn set_root_config_validates_values() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            set_root_config_values(
                tmp.path(),
                &[("ollama_url".to_string(), "not-a-url".to_string())],
                &[],
            ),
            Err(RootConfigSetError::InvalidValue { .. })
        ));
        assert!(matches!(
            set_root_config_values(
                tmp.path(),
                &[("mcp_addr".to_string(), "host-without-port".to_string())],
                &[],
            ),
            Err(RootConfigSetError::InvalidValue { .. })
        ));
        // A well-formed URL and address pass.
        set_root_config_values(
            tmp.path(),
            &[
                ("ollama_url".to_string(), "http://host:11434".to_string()),
                ("mcp_addr".to_string(), "127.0.0.1:8765".to_string()),
            ],
            &[],
        )
        .expect("valid values accepted");
    }

    #[test]
    fn set_root_config_starts_from_empty_when_file_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        set_root_config_values(
            tmp.path(),
            &[("embed_model".to_string(), "m".to_string())],
            &[],
        )
        .expect("write from empty");
        let cfg = load_root_config(tmp.path()).expect("reloads");
        assert_eq!(cfg.embed_model.as_deref(), Some("m"));
    }

    #[test]
    fn set_root_config_unset_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join(ROOT_CONFIG_NAME), "embed_model = \"m\"\n")
            .expect("seed config");
        // Unsetting a key the file never set is a no-op, not an error.
        set_root_config_values(tmp.path(), &[], &["mcp_addr".to_string()]).expect("unset absent");
        let cfg = load_root_config(tmp.path()).expect("reloads");
        assert_eq!(cfg.embed_model.as_deref(), Some("m"));
        // Unsetting a key that is present removes it.
        set_root_config_values(tmp.path(), &[], &["embed_model".to_string()])
            .expect("unset present");
        let cfg = load_root_config(tmp.path()).expect("reloads");
        assert!(cfg.embed_model.is_none());
    }

    #[test]
    fn set_root_config_rejects_malformed_existing_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join(ROOT_CONFIG_NAME), "this is = = not toml\n")
            .expect("seed garbage");
        assert!(matches!(
            set_root_config_values(
                tmp.path(),
                &[("embed_model".to_string(), "m".to_string())],
                &[],
            ),
            Err(RootConfigSetError::Malformed { .. })
        ));
    }

    #[test]
    fn root_config_env_override_maps_each_key() {
        assert_eq!(root_config_env_override("ollama_url"), Some(OLLAMA_URL_ENV));
        assert_eq!(
            root_config_env_override("embed_model"),
            Some(EMBED_MODEL_ENV)
        );
        assert_eq!(root_config_env_override("mcp_addr"), Some(MCP_ADDR_ENV));
        assert_eq!(root_config_env_override("log_directive"), Some(LOG_ENV));
        assert_eq!(
            root_config_env_override("search.top_k"),
            Some(SEARCH_TOP_K_ENV)
        );
        assert_eq!(
            root_config_env_override("search.weak_threshold"),
            Some(SEARCH_WEAK_THRESHOLD_ENV)
        );
        // The profile reference has no env counterpart by design.
        assert_eq!(root_config_env_override("index_profile"), None);
        assert_eq!(root_config_env_override("nope"), None);
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

    #[test]
    fn add_library_registers_a_manifestless_root_and_makes_it_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let registry = tmp.path().join("registry.toml");
        let root = tmp.path().join("alpha");
        std::fs::create_dir_all(&root).expect("root");
        let outcome = add_library(
            &registry,
            Some("alpha"),
            &root,
            None,
            None,
            AddOptions::default(),
            |_m| Ok(true),
        )
        .expect("add");
        let report = match outcome {
            AddOutcome::Registered(report) => report,
            other => panic!("expected Registered, got {other:?}"),
        };
        assert!(report.became_default);
        assert!(report.wrote_manifest);
        assert!(report.uuid.is_some());
        // The identity manifest landed and the entry carries its uuid.
        assert!(load_manifest(&root).expect("load").is_some());
        let parsed =
            parse_registry(&std::fs::read_to_string(&registry).expect("read")).expect("reg");
        assert_eq!(parsed.default.as_deref(), Some("alpha"));
        assert_eq!(parsed.libraries["alpha"].uuid(), report.uuid.as_deref());
    }

    #[test]
    fn add_library_aborts_when_manifest_write_is_declined() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let registry = tmp.path().join("registry.toml");
        let root = tmp.path().join("alpha");
        std::fs::create_dir_all(&root).expect("root");
        let outcome = add_library(
            &registry,
            Some("alpha"),
            &root,
            None,
            None,
            AddOptions::default(),
            |_m| Ok(false),
        )
        .expect("add");
        assert!(matches!(outcome, AddOutcome::Aborted));
        assert!(load_manifest(&root).expect("load").is_none());
        assert!(!registry.exists(), "a declined add writes nothing");
    }

    #[test]
    fn add_library_reports_key_taken_for_a_derived_name_collision() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let registry = tmp.path().join("registry.toml");
        let first = tmp.path().join("a").join("lib");
        let second = tmp.path().join("b").join("lib");
        std::fs::create_dir_all(&first).expect("first");
        std::fs::create_dir_all(&second).expect("second");
        add_library(
            &registry,
            None,
            &first,
            None,
            None,
            AddOptions::default(),
            |_m| Ok(true),
        )
        .expect("first");
        // The second root derives the same key at a different path.
        let outcome = add_library(
            &registry,
            None,
            &second,
            None,
            None,
            AddOptions::default(),
            |_m| Ok(true),
        )
        .expect("second");
        match outcome {
            AddOutcome::KeyTaken { key, existing_path } => {
                assert_eq!(key, "lib");
                assert_eq!(existing_path, first);
            }
            other => panic!("expected KeyTaken, got {other:?}"),
        }
    }

    #[test]
    fn add_library_with_an_explicit_key_overwrites_rather_than_conflicts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let registry = tmp.path().join("registry.toml");
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&first).expect("first");
        std::fs::create_dir_all(&second).expect("second");
        add_library(
            &registry,
            Some("lib"),
            &first,
            None,
            None,
            AddOptions::default(),
            |_m| Ok(true),
        )
        .expect("first");
        let outcome = add_library(
            &registry,
            Some("lib"),
            &second,
            None,
            None,
            AddOptions::default(),
            |_m| Ok(true),
        )
        .expect("second");
        assert!(matches!(outcome, AddOutcome::Registered(_)));
        let parsed =
            parse_registry(&std::fs::read_to_string(&registry).expect("read")).expect("reg");
        assert_eq!(parsed.libraries["lib"].data_dir(), second);
    }

    #[test]
    fn add_library_reports_a_uuid_clash_against_a_registered_identity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let registry = tmp.path().join("registry.toml");
        let orig = tmp.path().join("orig");
        std::fs::create_dir_all(&orig).expect("orig");
        let manifest = new_manifest("orig", LibraryKind::Prod, None);
        write_manifest(&orig, &manifest).expect("manifest");
        add_library(
            &registry,
            Some("orig"),
            &orig,
            None,
            None,
            AddOptions::default(),
            |_m| Ok(true),
        )
        .expect("register orig");
        // A second root carrying the very same manifest (same uuid).
        let copy = tmp.path().join("copy");
        std::fs::create_dir_all(&copy).expect("copy");
        write_manifest(&copy, &manifest).expect("copy manifest");
        let outcome = add_library(
            &registry,
            Some("copy"),
            &copy,
            None,
            None,
            AddOptions::default(),
            |_m| Ok(true),
        )
        .expect("add copy");
        match outcome {
            AddOutcome::UuidClash {
                uuid,
                existing_key,
                existing_path,
            } => {
                assert_eq!(uuid, manifest.uuid);
                assert_eq!(existing_key, "orig");
                assert_eq!(existing_path, orig);
            }
            other => panic!("expected UuidClash, got {other:?}"),
        }
    }

    #[test]
    fn add_library_new_uuid_remints_and_registers_the_copy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let registry = tmp.path().join("registry.toml");
        let orig = tmp.path().join("orig");
        std::fs::create_dir_all(&orig).expect("orig");
        let manifest = new_manifest("orig", LibraryKind::Prod, None);
        write_manifest(&orig, &manifest).expect("manifest");
        add_library(
            &registry,
            Some("orig"),
            &orig,
            None,
            None,
            AddOptions::default(),
            |_m| Ok(true),
        )
        .expect("register orig");
        let copy = tmp.path().join("copy");
        std::fs::create_dir_all(&copy).expect("copy");
        write_manifest(&copy, &manifest).expect("copy manifest");
        let outcome = add_library(
            &registry,
            Some("copy"),
            &copy,
            None,
            None,
            AddOptions { new_uuid: true },
            |_m| Ok(true),
        )
        .expect("add copy");
        let report = match outcome {
            AddOutcome::Registered(report) => report,
            other => panic!("expected Registered, got {other:?}"),
        };
        assert_ne!(report.uuid.as_deref(), Some(manifest.uuid.as_str()));
        // The copy's manifest on disk now carries the fresh uuid.
        let rewritten = load_manifest(&copy).expect("load").expect("present");
        assert_eq!(Some(rewritten.uuid.as_str()), report.uuid.as_deref());
        assert_ne!(rewritten.uuid, manifest.uuid);
    }

    #[cfg(unix)]
    #[test]
    fn add_library_degrades_on_a_read_only_root() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let registry = tmp.path().join("registry.toml");
        let root = tmp.path().join("ro");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        // If the effective user can write anyway (running as root), the
        // degrade branch is unreachable; skip rather than assert a false
        // state.
        if tempfile::NamedTempFile::new_in(&root).is_ok() {
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).ok();
            return;
        }
        let outcome = add_library(
            &registry,
            Some("ro"),
            &root,
            None,
            None,
            AddOptions::default(),
            |_m| Ok(true),
        )
        .expect("add");
        let report = match outcome {
            AddOutcome::Registered(report) => report,
            other => panic!("expected Registered, got {other:?}"),
        };
        assert!(report.read_only_degraded);
        assert!(report.uuid.is_none());
        assert!(!report.wrote_manifest);
        let parsed =
            parse_registry(&std::fs::read_to_string(&registry).expect("read")).expect("reg");
        assert_eq!(parsed.libraries["ro"].uuid(), None);
        // Restore perms so tempdir teardown can remove the directory.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[test]
    fn remove_library_returns_the_data_root_and_clears_a_dangling_default() {
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
        let report = remove_library(&path, "alpha").expect("remove");
        assert_eq!(report.data_dir, PathBuf::from("/roots/alpha"));
        assert!(report.default_cleared);
        let parsed = parse_registry(&std::fs::read_to_string(&path).expect("read")).expect("reg");
        assert_eq!(parsed.default, None);
        assert!(!parsed.libraries.contains_key("alpha"));
    }

    #[test]
    fn regenerate_manifest_uuid_changes_only_the_uuid() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let m = new_manifest("lib", LibraryKind::Test, Some("desc".to_string()));
        write_manifest(tmp.path(), &m).expect("write");
        let fresh = regenerate_manifest_uuid(tmp.path()).expect("regen");
        assert_ne!(fresh.uuid, m.uuid);
        assert_eq!(fresh.name, m.name);
        assert_eq!(fresh.kind, m.kind);
        assert_eq!(fresh.description, m.description);
        let reloaded = load_manifest(tmp.path()).expect("load").expect("present");
        assert_eq!(reloaded.uuid, fresh.uuid);
    }

    #[test]
    fn find_library_returns_the_entry_or_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("registry.toml");
        std::fs::write(&path, "[libraries]\nalpha = \"/roots/alpha\"\n").expect("seed");
        let found = find_library(&path, "alpha")
            .expect("find")
            .expect("present");
        assert_eq!(found.data_dir, PathBuf::from("/roots/alpha"));
        assert!(find_library(&path, "ghost").expect("find").is_none());
        // A missing registry file resolves to `None`, not an error.
        assert!(
            find_library(&tmp.path().join("absent.toml"), "alpha")
                .expect("find")
                .is_none()
        );
    }

    #[test]
    fn repoint_library_moves_an_entry_to_a_new_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("registry.toml");
        std::fs::write(&path, "[libraries]\nalpha = \"/roots/alpha\"\n").expect("seed");
        repoint_library(&path, "alpha", Path::new("/roots/moved")).expect("repoint");
        let parsed = parse_registry(&std::fs::read_to_string(&path).expect("read")).expect("reg");
        assert_eq!(
            parsed.libraries["alpha"].data_dir(),
            Path::new("/roots/moved")
        );
        assert!(matches!(
            repoint_library(&path, "ghost", Path::new("/x")),
            Err(ConfigError::UnknownLibrary { .. })
        ));
    }
}
