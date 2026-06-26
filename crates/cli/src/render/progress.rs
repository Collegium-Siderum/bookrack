// SPDX-License-Identifier: Apache-2.0

//! Stage-aware spinner used by the CLI's async-job renderers.
//!
//! Wraps [`indicatif::ProgressBar`] behind a small, opinionated API:
//! one current stage label, an optional `0..=100` percentage, and a
//! tail message. The wrapper centralises the formatting so individual
//! subcommand modules do not write `\r` rewrites by hand.

use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

use super::ctx;

/// Stage-tagged spinner that draws on stderr.
///
/// The spinner is hidden when the installed [`RenderCtx`] is in
/// `Quiet` or `Json` mode; in those modes every method becomes a
/// no-op so call sites can render unconditionally.
pub struct SpinnerWithStage {
    inner: Option<ProgressBar>,
}

impl SpinnerWithStage {
    /// Builds a new spinner. `label` is shown verbatim before the
    /// stage tag (typically a short job-id prefix).
    pub fn new(label: &str) -> Self {
        if ctx().is_quiet() || ctx().is_json() {
            return Self { inner: None };
        }
        let pb = ProgressBar::new_spinner();
        pb.set_style(spinner_style());
        pb.enable_steady_tick(Duration::from_millis(120));
        pb.set_message(label.to_string());
        Self { inner: Some(pb) }
    }

    /// Updates the stage tag and optional progress percentage.
    pub fn set_stage(&self, stage: &str, percent: Option<u8>) {
        if let Some(pb) = &self.inner {
            let pct = percent
                .map(|p| format!("{p:>3}%"))
                .unwrap_or_else(|| "----".to_string());
            pb.set_message(format!("[{stage} {pct}]"));
        }
    }

    /// Appends a tail message to the current line.
    pub fn set_tail(&self, tail: &str) {
        if let Some(pb) = &self.inner {
            pb.set_message(format!("{} {}", pb.message(), tail));
        }
    }

    /// Clears the spinner from the terminal.
    pub fn finish_and_clear(&self) {
        if let Some(pb) = &self.inner {
            pb.finish_and_clear();
        }
    }
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner} {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", ""])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_mode_is_a_noop() {
        super::super::init(super::super::RenderCtx::new(
            super::super::OutputMode::Quiet,
            super::super::ColorMode::Never,
        ));
        let s = SpinnerWithStage::new("xxxxxxxx");
        s.set_stage("EXTRACT", Some(50));
        s.set_tail("reading file");
        s.finish_and_clear();
    }
}
