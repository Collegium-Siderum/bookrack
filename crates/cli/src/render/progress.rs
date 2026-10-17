// SPDX-License-Identifier: Apache-2.0

//! Status spinner used by the CLI's async-wait renderers.
//!
//! Wraps [`indicatif::ProgressBar`] behind a small, opinionated API:
//! one status line drawn next to the spinner, lines printed above it,
//! and a clear on finish. The bar stays hidden until a reveal delay
//! has elapsed, so a wait that ends quickly never draws a frame. The
//! wrapper centralises the formatting so individual subcommand modules
//! do not write `\r` rewrites by hand.

use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

use super::ctx;

/// Redraw interval of the spinner glyph.
// setting: internal -- the animation rate of a glyph that carries no
// information
const TICK: Duration = Duration::from_millis(120);

/// Status spinner that draws on stderr.
///
/// The spinner is inert when the installed [`RenderCtx`] is in
/// `Quiet` or `Json` mode; in those modes every method becomes a
/// no-op so call sites can render unconditionally.
///
/// [`RenderCtx`]: super::RenderCtx
pub struct SpinnerWithStage {
    inner: Option<ProgressBar>,
}

impl SpinnerWithStage {
    /// Builds a spinner that starts drawing on stderr once
    /// `reveal_after` has elapsed, unless it was finished first.
    pub fn new(reveal_after: Duration) -> Self {
        if ctx().is_quiet() || ctx().is_json() {
            return Self { inner: None };
        }
        let pb = ProgressBar::with_draw_target(None, ProgressDrawTarget::hidden());
        pb.set_style(spinner_style());
        pb.enable_steady_tick(TICK);
        let revealer = pb.clone();
        std::thread::spawn(move || {
            std::thread::sleep(reveal_after);
            if !revealer.is_finished() {
                revealer.set_draw_target(ProgressDrawTarget::stderr());
            }
        });
        Self { inner: Some(pb) }
    }

    /// Replaces the status line drawn next to the spinner.
    pub fn set_line(&self, line: &str) {
        if let Some(pb) = &self.inner {
            pb.set_message(line.to_string());
        }
    }

    /// Prints `line` on stderr above the spinner. Goes through
    /// [`ProgressBar::suspend`] because [`ProgressBar::println`]
    /// prints nothing while the bar is hidden.
    pub fn print_above(&self, line: &str) {
        if let Some(pb) = &self.inner {
            pb.suspend(|| eprintln!("{line}"));
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
    ProgressStyle::with_template("{spinner} {wide_msg}")
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", ""])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_mode_builds_no_progress_bar() {
        super::super::init(super::super::RenderCtx::new(
            super::super::OutputMode::Quiet,
            super::super::ColorMode::Never,
        ));
        // The process-wide ctx is first-write-wins; require that this
        // test's init actually took effect before reading the spinner.
        assert!(super::super::ctx().is_quiet());
        let s = SpinnerWithStage::new(Duration::ZERO);
        assert!(s.inner.is_none(), "quiet mode must not build a bar");
        s.set_line("1/2 job [processing ----]");
        s.print_above("1/2 job done");
        s.finish_and_clear();
    }
}
