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

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Environment variable naming the data root (an absolute directory).
pub const DATA_DIR_ENV: &str = "BOOKRACK_DATA_DIR";

/// Environment variable overriding the Ollama endpoint.
pub const OLLAMA_URL_ENV: &str = "BOOKRACK_OLLAMA_URL";

/// Ollama endpoint used when [`OLLAMA_URL_ENV`] is unset.
pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Environment variable naming the directory that holds the PDFium
/// dynamic library. When unset, the directory of the running executable
/// is used — the layout of a shipped bookrack build, which places the
/// library beside the binary.
pub const PDFIUM_LIB_ENV: &str = "BOOKRACK_PDFIUM_LIB";

/// Resolved configuration. Construct with [`Config::load`] (from the
/// environment) or [`Config::new`] (from an explicit data root, e.g. a
/// CLI override).
#[derive(Debug, Clone)]
pub struct Config {
    data_dir: PathBuf,
    ollama_url: String,
}

/// Why configuration resolution failed.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The data-root variable is unset or empty.
    #[error("{DATA_DIR_ENV} is not set (copy .env.example to .env and fill it in)")]
    MissingDataDir,
    /// The data-root variable points at something that is not a
    /// directory — usually a typo in the configured path.
    #[error(
        "{DATA_DIR_ENV} points to {}, which is not an existing directory",
        .0.display()
    )]
    DataDirNotFound(PathBuf),
}

impl Config {
    /// Resolve configuration from the environment, loading a `.env`
    /// file first if one is present.
    ///
    /// Fails if the data root is unset or does not name an existing
    /// directory. The Ollama endpoint falls back to
    /// [`DEFAULT_OLLAMA_URL`] when unset.
    pub fn load() -> Result<Config, ConfigError> {
        // A missing .env is fine: the variables may be set directly.
        dotenvy::dotenv().ok();
        resolve(
            std::env::var(DATA_DIR_ENV).ok(),
            std::env::var(OLLAMA_URL_ENV).ok(),
        )
    }

    /// Construct from an explicit data root, for callers that resolve
    /// the root themselves (e.g. a CLI flag). Performs no filesystem
    /// check — the caller vouches for the path.
    pub fn new(data_dir: PathBuf, ollama_url: String) -> Config {
        Config {
            data_dir,
            ollama_url,
        }
    }

    /// The data root that every other path is derived from.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The Ollama HTTP endpoint for embeddings.
    pub fn ollama_url(&self) -> &str {
        &self.ollama_url
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

    /// Directory for log files and crash reports. Kept under the data
    /// root — like every other path — so diagnostics never land inside
    /// the project workspace.
    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
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

/// Environment variable overriding the log filter directive.
pub const LOG_ENV: &str = "BOOKRACK_LOG";

/// Filter directive used when [`LOG_ENV`] is unset.
///
/// bookrack's own crates log at `info`; the vector-store dependencies
/// (`lance*`, `datafusion`) are pinned to `warn` because they emit a high
/// volume of `info` events — manifest loads, plan runs, file audits — that
/// would otherwise bury the pipeline's own diagnostics. Override the whole
/// directive with [`LOG_ENV`] to see them.
pub const DEFAULT_LOG: &str =
    "info,lance=warn,lance_namespace_impls=warn,lance_table=warn,datafusion=warn";

/// Number of nearest passages a query returns when [`SearchConfig`] is
/// left at its default.
pub const DEFAULT_SEARCH_TOP_K: usize = 5;

/// Environment variable overriding the search result count.
pub const SEARCH_TOP_K_ENV: &str = "BOOKRACK_SEARCH_TOP_K";

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
        }
    }
}

/// Retrieval knobs. Separate from [`EmbedConfig`] so the query side reads
/// only what it needs.
#[derive(Debug, Clone)]
pub struct SearchConfig {
    /// How many nearest passages a query returns.
    pub top_k: usize,
}

impl Default for SearchConfig {
    fn default() -> SearchConfig {
        SearchConfig {
            top_k: DEFAULT_SEARCH_TOP_K,
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
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// Filter directive for `EnvFilter`, e.g. `info`, `debug`, or
    /// `bookrack_ingest=debug,info`.
    pub directive: String,
}

impl Default for LogConfig {
    fn default() -> LogConfig {
        LogConfig {
            directive: DEFAULT_LOG.to_string(),
        }
    }
}

impl LogConfig {
    /// Resolve from the environment, falling back to [`DEFAULT_LOG`] when
    /// the override is unset or blank.
    pub fn from_env() -> LogConfig {
        LogConfig::resolve_from(|key| std::env::var(key).ok())
    }

    /// Pure resolution, factored out so it can be tested without mutating
    /// process-global environment variables.
    fn resolve_from(get: impl Fn(&str) -> Option<String>) -> LogConfig {
        LogConfig {
            directive: env_trimmed(get(LOG_ENV)).unwrap_or_else(|| DEFAULT_LOG.to_string()),
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

/// Pure resolution logic, factored out of [`Config::load`] so it can be
/// tested without mutating process-global environment variables.
fn resolve(data_dir: Option<String>, ollama_url: Option<String>) -> Result<Config, ConfigError> {
    let data_dir = data_dir
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or(ConfigError::MissingDataDir)?;
    let data_dir = PathBuf::from(data_dir);
    if !data_dir.is_dir() {
        return Err(ConfigError::DataDirNotFound(data_dir));
    }
    let ollama_url = ollama_url
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string());
    Ok(Config {
        data_dir,
        ollama_url,
    })
}

/// Resolve the directory to load the PDFium dynamic library from.
///
/// This is a free function, not a [`Config`] method: the PDF adapter
/// needs the directory but takes no `Config`, and threading one through
/// only to reach this would widen its signature for nothing.
pub fn pdfium_lib_dir() -> PathBuf {
    pdfium_lib_dir_from(std::env::var(PDFIUM_LIB_ENV).ok())
}

/// Pure resolution logic for [`pdfium_lib_dir`], factored out so it can
/// be tested without mutating process-global environment variables.
///
/// The override wins when set; otherwise the running executable's own
/// directory is used. A failure to locate the executable falls back to
/// the current directory rather than panicking — a missing PDFium
/// library is then reported by the adapter, where the error has
/// context, not here.
fn pdfium_lib_dir_from(override_dir: Option<String>) -> PathBuf {
    if let Some(dir) = override_dir
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return PathBuf::from(dir);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory guaranteed to exist, for happy-path resolution.
    fn existing_dir() -> String {
        std::env::temp_dir().to_string_lossy().into_owned()
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
    fn paths_derive_from_the_data_root() {
        let root = PathBuf::from("root");
        let cfg = Config::new(root.clone(), DEFAULT_OLLAMA_URL.to_string());
        assert_eq!(cfg.sources_dir(), root.join("sources"));
        assert_eq!(cfg.books_dir(), root.join("books"));
        assert_eq!(cfg.corpus_db(), root.join("corpus.db"));
        assert_eq!(cfg.catalog_db(), root.join("catalog.db"));
        assert_eq!(cfg.lancedb_dir(), root.join("lancedb"));
        assert_eq!(cfg.logs_dir(), root.join("logs"));
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

        let cfg = SearchConfig::resolve_from(|key| match key {
            SEARCH_TOP_K_ENV => Some("10".to_string()),
            _ => None,
        });
        assert_eq!(cfg.top_k, 10);

        // A blank value falls back to the default.
        let blank = SearchConfig::resolve_from(|_| Some("  ".to_string()));
        assert_eq!(blank.top_k, DEFAULT_SEARCH_TOP_K);
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
        assert_eq!(LogConfig::default().directive, DEFAULT_LOG);

        // An unset variable falls back to the default.
        let unset = LogConfig::resolve_from(|_| None);
        assert_eq!(unset.directive, DEFAULT_LOG);

        // A set directive is taken verbatim.
        let set = LogConfig::resolve_from(|key| match key {
            LOG_ENV => Some("bookrack_ingest=debug,info".to_string()),
            _ => None,
        });
        assert_eq!(set.directive, "bookrack_ingest=debug,info");

        // A whitespace-only value counts as unset.
        let blank = LogConfig::resolve_from(|_| Some("   ".to_string()));
        assert_eq!(blank.directive, DEFAULT_LOG);
    }

    #[test]
    fn pdfium_lib_dir_uses_the_override_when_set() {
        let pinned = "a/pinned/pdfium/dir";
        assert_eq!(
            pdfium_lib_dir_from(Some(pinned.to_string())),
            PathBuf::from(pinned),
        );
        // Whitespace-only counts as unset and falls through to the
        // executable-directory default.
        assert_ne!(
            pdfium_lib_dir_from(Some("   ".to_string())),
            PathBuf::from("   "),
        );
    }

    #[test]
    fn pdfium_lib_dir_falls_back_to_a_usable_directory_when_unset() {
        // With no override, resolution must still yield a non-empty
        // directory (the running executable's own folder), never an
        // empty path the loader could not use.
        assert!(!pdfium_lib_dir_from(None).as_os_str().is_empty());
    }
}
