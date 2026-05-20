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
}
