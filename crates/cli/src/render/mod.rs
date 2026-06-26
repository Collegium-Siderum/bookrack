// SPDX-License-Identifier: Apache-2.0

//! Shared rendering layer for the CLI front end.
//!
//! Wraps the three external crates the binary uses for operator-facing
//! output (`indicatif` for progress, `comfy-table` for tables,
//! `anstyle` for terminal style tokens) behind a uniform surface so
//! individual subcommand modules do not learn each crate's API on
//! their own.
//!
//! The [`RenderCtx`] holds the operator-visible output mode and
//! color mode chosen at startup from the global CLI flags. It is
//! installed once via [`init`] in `main` and read from anywhere with
//! [`ctx`]. Subcommands that opt into context-aware behaviour read
//! `ctx().output()` to decide between human and JSON rendering.

use std::sync::OnceLock;

pub mod confirm;
pub mod human;
pub mod job_report;
pub mod progress;
pub mod table;

/// Operator-visible output mode chosen at startup from the global
/// `--json` / `--quiet` flags. The default is `Human`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Human-readable output: tables and short sentences. The legacy
    /// pretty-JSON fallback still applies to commands that have not
    /// yet been migrated to dedicated human renderers.
    Human,
    /// Machine-parseable output: pretty-printed JSON only.
    Json,
    /// Suppress stdout on success. Errors still pass through the
    /// reporter.
    Quiet,
}

impl OutputMode {
    pub fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }

    pub fn is_quiet(self) -> bool {
        matches!(self, Self::Quiet)
    }
}

/// Color preference for ANSI styling. `Auto` consults `NO_COLOR` and
/// stderr's TTY status; `Always` forces styles even on a pipe;
/// `Never` strips every escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

impl ColorMode {
    /// Resolves the policy against the current environment.
    pub fn enabled(self) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => std::env::var_os("NO_COLOR").is_none() && stderr_is_tty(),
        }
    }
}

fn stderr_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

#[derive(Debug, Clone)]
pub struct RenderCtx {
    output: OutputMode,
    color: ColorMode,
}

impl RenderCtx {
    pub fn new(output: OutputMode, color: ColorMode) -> Self {
        Self { output, color }
    }

    pub fn output(&self) -> OutputMode {
        self.output
    }

    pub fn color(&self) -> ColorMode {
        self.color
    }

    pub fn is_json(&self) -> bool {
        self.output.is_json()
    }

    pub fn is_quiet(&self) -> bool {
        self.output.is_quiet()
    }

    pub fn color_enabled(&self) -> bool {
        self.color.enabled()
    }
}

impl Default for RenderCtx {
    fn default() -> Self {
        Self::new(OutputMode::Human, ColorMode::Auto)
    }
}

static GLOBAL_CTX: OnceLock<RenderCtx> = OnceLock::new();

/// Installs the process-wide render context. Subsequent calls are
/// no-ops; the first call wins.
pub fn init(ctx: RenderCtx) {
    let _ = GLOBAL_CTX.set(ctx);
}

/// Returns the installed render context, or a default human/auto
/// context when [`init`] has not run yet.
pub fn ctx() -> &'static RenderCtx {
    GLOBAL_CTX.get_or_init(RenderCtx::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_mode_predicates() {
        assert!(OutputMode::Json.is_json());
        assert!(!OutputMode::Json.is_quiet());
        assert!(OutputMode::Quiet.is_quiet());
        assert!(!OutputMode::Human.is_json());
        assert!(!OutputMode::Human.is_quiet());
    }

    #[test]
    fn color_mode_never_is_off() {
        assert!(!ColorMode::Never.enabled());
        assert!(ColorMode::Always.enabled());
    }

    #[test]
    fn default_ctx_is_human() {
        let ctx = RenderCtx::default();
        assert_eq!(ctx.output(), OutputMode::Human);
        assert_eq!(ctx.color(), ColorMode::Auto);
        assert!(!ctx.is_json());
        assert!(!ctx.is_quiet());
    }
}
