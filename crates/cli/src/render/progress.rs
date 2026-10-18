// SPDX-License-Identifier: Apache-2.0

//! Progress drawing shared by the CLI's long-running commands.
//!
//! [`SurfaceKind`] picks where a progress line goes from the output
//! mode and whether stderr is a terminal, and builds the [`Surface`]
//! that draws it: nothing, appended lines, or a [`SpinnerWithStage`].
//!
//! [`SpinnerWithStage`] wraps [`indicatif::ProgressBar`] behind a
//! small, opinionated API: one status line drawn next to the spinner,
//! lines printed above it, and a clear on finish. The bar stays hidden
//! until a reveal delay has elapsed, so a wait that ends quickly never
//! draws a frame. The wrapper centralises the formatting so individual
//! subcommand modules do not write `\r` rewrites by hand.

use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

use super::{OutputMode, ctx, stderr_is_tty};

/// Delay before the spinner draws its first frame, so a wait that
/// ends quickly leaves nothing on the terminal.
// setting: internal -- when a progress line first appears, not what it
// reports; the wait itself is unaffected
const REVEAL_AFTER: Duration = Duration::from_millis(500);

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

/// Where a progress line draws: a redrawn status line, an appended
/// log, or nothing.
pub trait Surface: Send {
    /// Replaces the status line. A surface that cannot redraw ignores
    /// it.
    fn status(&mut self, line: &str);
    /// Records a stage transition. A surface that redraws ignores it;
    /// its status line already shows the stage.
    fn transition(&mut self, line: &str);
    /// Prints a line that stays.
    fn notice(&mut self, line: &str);
    /// Removes whatever the surface drew.
    fn clear(&mut self);
}

/// The kind of [`Surface`] a progress line draws on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceKind {
    /// Draws nothing.
    Hidden,
    /// Appends a line per stage transition and notice; never rewrites.
    Plain,
    /// Redraws one status line next to a spinner.
    Spinner,
}

impl SurfaceKind {
    /// `Quiet` and `Json` draw nothing. Human output redraws one line
    /// on a terminal and appends lines anywhere else.
    pub fn select(mode: OutputMode, stderr_tty: bool) -> Self {
        match mode {
            OutputMode::Quiet | OutputMode::Json => Self::Hidden,
            OutputMode::Human if stderr_tty => Self::Spinner,
            OutputMode::Human => Self::Plain,
        }
    }

    /// The kind the installed render context and stderr call for.
    pub fn from_context() -> Self {
        Self::select(ctx().output(), stderr_is_tty())
    }

    pub fn build(self) -> Box<dyn Surface> {
        match self {
            Self::Hidden => Box::new(HiddenSurface),
            Self::Plain => Box::new(PlainSurface),
            Self::Spinner => Box::new(SpinnerSurface(SpinnerWithStage::new(REVEAL_AFTER))),
        }
    }
}

struct HiddenSurface;

impl Surface for HiddenSurface {
    fn status(&mut self, _line: &str) {}
    fn transition(&mut self, _line: &str) {}
    fn notice(&mut self, _line: &str) {}
    fn clear(&mut self) {}
}

struct PlainSurface;

impl Surface for PlainSurface {
    fn status(&mut self, _line: &str) {}
    fn transition(&mut self, line: &str) {
        eprintln!("{line}");
    }
    fn notice(&mut self, line: &str) {
        eprintln!("{line}");
    }
    fn clear(&mut self) {}
}

struct SpinnerSurface(SpinnerWithStage);

impl Surface for SpinnerSurface {
    fn status(&mut self, line: &str) {
        self.0.set_line(line);
    }
    fn transition(&mut self, _line: &str) {}
    fn notice(&mut self, line: &str) {
        self.0.print_above(line);
    }
    fn clear(&mut self) {
        self.0.finish_and_clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_and_json_draw_nothing_and_human_output_appends_off_a_terminal() {
        assert_eq!(
            SurfaceKind::select(OutputMode::Quiet, true),
            SurfaceKind::Hidden
        );
        assert_eq!(
            SurfaceKind::select(OutputMode::Json, true),
            SurfaceKind::Hidden
        );
        assert_eq!(
            SurfaceKind::select(OutputMode::Human, true),
            SurfaceKind::Spinner
        );
        assert_eq!(
            SurfaceKind::select(OutputMode::Human, false),
            SurfaceKind::Plain
        );
    }

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
