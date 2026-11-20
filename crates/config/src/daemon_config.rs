// SPDX-License-Identifier: Apache-2.0

//! The values that belong to the process and the machine it runs on.

use std::path::{Path, PathBuf};

use bookrack_core::knob::KnobOrigin;

use crate::{
    ConfigError, DEFAULT_OLLAMA_URL, EmbedConfig, EnvSnapshot, LogConfig, McpConfig,
    RerankerConfig, RetiredVariable,
};

/// What the console log layer falls back to when its own variable is
/// unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConsoleDefault {
    /// The compiled-in quiet level: a foreground session stays free of
    /// daemon telemetry.
    #[default]
    Quiet,
    /// Whatever the file layer resolved: a headless daemon whose stderr
    /// is its primary log surface.
    MirrorFile,
}

/// What the command line contributes to a [`DaemonConfig`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonFlags {
    /// `run --mcp-addr`: outranks the variable.
    pub mcp_addr: Option<String>,
    /// `run --no-mcp`: no MCP listener, so no address resolves.
    pub no_mcp: bool,
    /// The console log layer's fallback.
    pub console: ConsoleDefault,
}

/// How this machine runs bookrack: where its backends are, how a run is
/// paced, where the process listens and logs, where daemon state lives.
///
/// Resolved once, from the environment and the command line.
/// [`DaemonConfig::resolve`] takes no library file: nothing a library
/// carries can move these values. The name follows the long-lived
/// process, but a one-shot command resolves one too, for the same
/// values.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Ollama HTTP endpoint for embeddings.
    pub ollama_url: String,
    /// Reranker backend deployment.
    pub reranker: RerankerConfig,
    /// The MCP listener, `None` when the process serves none.
    pub mcp: Option<McpConfig>,
    /// Log filter directives.
    pub log: LogConfig,
    daemon_state_dir: Option<PathBuf>,
    embed_pacing: EmbedConfig,
    origins: Vec<KnobOrigin>,
    retired: Vec<RetiredVariable>,
    env: EnvSnapshot,
}

impl DaemonConfig {
    /// Resolve every daemon value from `env` and `flags`. Never fails:
    /// a state directory that cannot be resolved is reported by
    /// [`DaemonConfig::require_daemon_state_dir`], so a command that
    /// needs none is not stopped by it.
    pub fn resolve(env: &EnvSnapshot, flags: &DaemonFlags) -> DaemonConfig {
        let get = |name: &str| env.get(name);
        let dotenv = env.dotenv();
        let ollama_url = crate::ollama_url_knob(get(crate::OLLAMA_URL_ENV), dotenv);
        let state_dir = crate::daemon_state_dir_knob(get(crate::DAEMON_STATE_DIR_ENV), dotenv);
        let (mut embed_pacing, embed_rows) =
            EmbedConfig::resolve_with_origins_from(get, dotenv, None);
        let ollama_url_value = ollama_url
            .value
            .clone()
            .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string());
        embed_pacing.ollama_url = ollama_url_value.clone();
        let (reranker, reranker_rows) = RerankerConfig::resolve_with_origins_from(get, dotenv);
        let (mcp, mcp_rows) = McpConfig::resolve_with_flags_from(get, dotenv, Some(flags));
        let (log, log_rows) = LogConfig::resolve_with_origins_from(get, dotenv, flags.console);

        let mut origins = vec![ollama_url.clone(), state_dir.clone()];
        origins.extend(
            embed_rows
                .into_iter()
                .filter(|row| row.key != "embed.model"),
        );
        origins.extend(reranker_rows);
        origins.extend(mcp_rows);
        origins.extend(log_rows);

        DaemonConfig {
            ollama_url: ollama_url_value,
            reranker,
            mcp: (!flags.no_mcp).then_some(mcp),
            log,
            daemon_state_dir: state_dir.value.map(PathBuf::from),
            embed_pacing,
            origins,
            retired: env.retired_seen(),
            env: env.clone(),
        }
    }

    /// The embedding configuration for a library whose vectors are
    /// built with `model`: the model from the library, the endpoint and
    /// the pacing from this machine.
    pub fn embed_config(&self, model: &str) -> EmbedConfig {
        EmbedConfig {
            model: model.to_string(),
            ..self.embed_pacing.clone()
        }
    }

    /// The daemon state directory, when one resolved.
    pub fn daemon_state_dir(&self) -> Option<&Path> {
        self.daemon_state_dir.as_deref()
    }

    /// The daemon state directory, or the error that says none could be
    /// resolved.
    pub fn require_daemon_state_dir(&self) -> Result<&Path, ConfigError> {
        self.daemon_state_dir
            .as_deref()
            .ok_or(ConfigError::DaemonStateDirUnavailable)
    }

    /// One row per value, in a fixed order: where it came from and
    /// every layer that could have supplied it.
    pub fn origins(&self) -> &[KnobOrigin] {
        &self.origins
    }

    /// The environment this configuration was resolved from, for the
    /// rows that are resolved later against the same one.
    pub fn env(&self) -> &EnvSnapshot {
        &self.env
    }

    /// The retired variables the environment carried, each ignored.
    pub fn retired_seen(&self) -> &[RetiredVariable] {
        &self.retired
    }
}

#[cfg(test)]
mod tests {
    use bookrack_core::knob::Layer;

    use super::*;
    use crate::{
        DAEMON_STATE_DIR_ENV, DEFAULT_LOG_CONSOLE, EMBED_BATCH_MAX_CHUNKS_ENV, LOG_CONSOLE_ENV,
        LOG_ENV, LibraryConfig, MCP_ADDR_ENV, OLLAMA_URL_ENV, RERANKER_CTX_ENV,
        RERANKER_THREADS_ENV, RERANKER_URL_ENV, RootConfig, RootSearchConfig,
    };

    fn row<'a>(cfg: &'a DaemonConfig, key: &str) -> &'a KnobOrigin {
        cfg.origins()
            .iter()
            .find(|row| row.key == key)
            .unwrap_or_else(|| panic!("no {key} row"))
    }

    fn busy_env() -> EnvSnapshot {
        EnvSnapshot::from_pairs([
            (OLLAMA_URL_ENV, "http://embed-host:1"),
            (EMBED_BATCH_MAX_CHUNKS_ENV, "7"),
            (RERANKER_URL_ENV, "http://rerank-host:2"),
            (MCP_ADDR_ENV, "127.0.0.1:9"),
            (LOG_ENV, "debug"),
            (DAEMON_STATE_DIR_ENV, "/state"),
            (RERANKER_CTX_ENV, "4096"),
            (RERANKER_THREADS_ENV, "3"),
            ("BOOKRACK_SEARCH_TOP_K", "9"),
        ])
    }

    #[test]
    fn daemon_config_rows_and_fields_agree() {
        for env in [EnvSnapshot::default(), busy_env()] {
            let cfg = DaemonConfig::resolve(&env, &DaemonFlags::default());
            let value = |key: &str| row(&cfg, key).value.clone();
            assert_eq!(
                value("ollama_url").as_deref(),
                Some(cfg.ollama_url.as_str())
            );
            assert_eq!(value("reranker.url"), cfg.reranker.url);
            assert_eq!(
                value("reranker.ctx"),
                cfg.reranker.ctx.map(|v| v.to_string())
            );
            assert_eq!(
                value("reranker.threads"),
                cfg.reranker.threads.map(|v| v.to_string())
            );
            assert_eq!(value("mcp.addr"), cfg.mcp.as_ref().map(|m| m.addr.clone()));
            assert_eq!(
                value("log.directive").as_deref(),
                Some(cfg.log.directive.as_str())
            );
            assert_eq!(
                value("log.console_level").as_deref(),
                Some(cfg.log.console_level.as_str())
            );
            assert_eq!(
                value("daemon_state_dir"),
                cfg.daemon_state_dir().map(|p| p.display().to_string())
            );
            let embed = cfg.embed_config("some-model");
            assert_eq!(embed.model, "some-model");
            assert_eq!(embed.ollama_url, cfg.ollama_url);
            assert_eq!(
                value("embed.batch_char_budget"),
                Some(embed.batch_char_budget.to_string())
            );
            assert_eq!(
                value("embed.batch_max_chunks"),
                Some(embed.batch_max_chunks.to_string())
            );
            assert_eq!(
                value("embed.batch_min_char_budget"),
                Some(embed.batch_min_char_budget.to_string())
            );
            assert_eq!(
                value("embed.progress_interval_secs"),
                Some(embed.progress_interval.as_secs().to_string())
            );
            // Twelve rows, and none of them a library's.
            assert_eq!(cfg.origins().len(), 12);
            assert!(cfg.origins().iter().all(|row| row.key != "embed.model"));
        }
        let busy = DaemonConfig::resolve(&busy_env(), &DaemonFlags::default());
        assert_eq!(busy.ollama_url, "http://embed-host:1");
        assert_eq!(busy.embed_config("m").batch_max_chunks, 7);
        assert_eq!(busy.daemon_state_dir(), Some(Path::new("/state")));
        assert_eq!(busy.reranker.ctx, Some(4096));
        assert_eq!(busy.reranker.threads, Some(3));
    }

    #[test]
    fn the_environment_cannot_reach_a_library_value() {
        let root = RootConfig {
            search: Some(RootSearchConfig {
                top_k: Some(3),
                weak_threshold: None,
            }),
        };
        // Two processes started under different environments resolve
        // different daemon values and the same library values.
        let quiet = DaemonConfig::resolve(&EnvSnapshot::default(), &DaemonFlags::default());
        let busy = DaemonConfig::resolve(&busy_env(), &DaemonFlags::default());
        assert_ne!(quiet.ollama_url, busy.ollama_url);

        let library = LibraryConfig::resolve(&root, None);
        assert_eq!(library.search.top_k, 3);
        let top_k = library
            .origins()
            .iter()
            .find(|row| row.key == "search.top_k")
            .expect("search.top_k row");
        assert_eq!(top_k.layer, Layer::File);
    }

    #[test]
    fn a_flag_outranks_the_variable_for_the_mcp_address() {
        let env = EnvSnapshot::from_pairs([(MCP_ADDR_ENV, "127.0.0.1:9")]);
        let flags = DaemonFlags {
            mcp_addr: Some("127.0.0.1:7".to_string()),
            ..DaemonFlags::default()
        };
        let cfg = DaemonConfig::resolve(&env, &flags);
        assert_eq!(
            cfg.mcp.as_ref().map(|m| m.addr.as_str()),
            Some("127.0.0.1:7")
        );
        let addr = row(&cfg, "mcp.addr");
        assert_eq!(addr.layer, Layer::Flag);
        assert_eq!(addr.site, "run --mcp-addr");
        assert!(
            addr.shadowed
                .iter()
                .any(|s| s.site == MCP_ADDR_ENV && s.value == "127.0.0.1:9"),
            "{addr:?}"
        );
    }

    #[test]
    fn a_process_without_mcp_resolves_no_mcp_address() {
        let env = EnvSnapshot::from_pairs([(MCP_ADDR_ENV, "127.0.0.1:9")]);
        let flags = DaemonFlags {
            no_mcp: true,
            ..DaemonFlags::default()
        };
        let cfg = DaemonConfig::resolve(&env, &flags);
        assert!(cfg.mcp.is_none());
        let addr = row(&cfg, "mcp.addr");
        assert_eq!(addr.value, None);
        assert_eq!(addr.layer, Layer::Flag);
        assert_eq!(addr.site, "run --no-mcp");
    }

    #[test]
    fn the_console_layer_mirrors_the_file_layer_only_when_asked_and_unset() {
        let env = EnvSnapshot::from_pairs([(LOG_ENV, "debug")]);
        let mirror = DaemonFlags {
            console: ConsoleDefault::MirrorFile,
            ..DaemonFlags::default()
        };

        let quiet = DaemonConfig::resolve(&env, &DaemonFlags::default());
        assert_eq!(quiet.log.console_level, DEFAULT_LOG_CONSOLE);

        let mirrored = DaemonConfig::resolve(&env, &mirror);
        assert_eq!(mirrored.log.console_level, "debug");
        let console = row(&mirrored, "log.console_level");
        assert_eq!(console.layer, Layer::Default);
        assert_eq!(console.site, "mirrors log.directive");

        let explicit = EnvSnapshot::from_pairs([(LOG_ENV, "debug"), (LOG_CONSOLE_ENV, "warn")]);
        let cfg = DaemonConfig::resolve(&explicit, &mirror);
        assert_eq!(cfg.log.console_level, "warn");
    }

    #[test]
    fn a_missing_state_directory_does_not_fail_resolution() {
        let mut cfg = DaemonConfig::resolve(&EnvSnapshot::default(), &DaemonFlags::default());
        cfg.daemon_state_dir = None;
        assert!(matches!(
            cfg.require_daemon_state_dir(),
            Err(ConfigError::DaemonStateDirUnavailable)
        ));
    }

    #[test]
    fn the_retired_variables_seen_are_the_snapshots() {
        let env = EnvSnapshot::from_pairs([("BOOKRACK_VECTORS_BYPASS_ANN", "1")]);
        let cfg = DaemonConfig::resolve(&env, &DaemonFlags::default());
        let names: Vec<&str> = cfg.retired_seen().iter().map(|var| var.name).collect();
        assert_eq!(names, ["BOOKRACK_VECTORS_BYPASS_ANN"]);
    }
}
