// SPDX-License-Identifier: Apache-2.0

//! Terminal implementation of [`WizardDriver`].
//!
//! Reads stdin for prompts, writes progress to stdout, errors to
//! stderr. Owns every operator-facing string of the wizard; the
//! runner hands over structured reports only.
//!
//! Every one of those strings goes through [`Console`], so what the
//! operator reads is what a test reads. Writing to the process streams
//! directly from a step would put that claim back out of reach.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use bookrack_embed::ProbeReport as EmbedProbeReport;
use eyre::{Context, ContextCompat, Result};

use super::runner::validate_unused_or_force;
use super::{
    DataRootHint, FinalizeSummary, OllamaStep, PdfiumChoice, PdfiumInstallOutcome, PdfiumReport,
    SmokeOutcome, WizardDriver,
};

/// Terminal I/O the driver needs, injected so the operator-facing
/// strings can be asserted without a TTY.
pub(super) trait Console: Send + Sync {
    /// Progress the operator reads as the run proceeds.
    fn line(&self, text: &str);
    /// A warning or a failure: the same text, on the error stream.
    fn warn(&self, text: &str);
    /// Render a prompt and read one trimmed answer.
    fn prompt(&self, prompt: &str) -> Result<String>;
}

/// The real terminal: stdout for progress, stderr for warnings, stdin
/// for answers.
struct TerminalConsole;

impl Console for TerminalConsole {
    fn line(&self, text: &str) {
        println!("{text}");
    }

    fn warn(&self, text: &str) {
        eprintln!("{text}");
    }

    fn prompt(&self, prompt: &str) -> Result<String> {
        print!("{prompt}");
        std::io::stdout().flush().context("flush stdout")?;
        let stdin = std::io::stdin();
        let mut buf = String::new();
        stdin.lock().read_line(&mut buf).context("read line")?;
        Ok(buf.trim().to_string())
    }
}

pub struct CliWizardDriver {
    /// Mirrors `WizardOpts::non_interactive`: suppresses every prompt
    /// this driver would otherwise issue after step 1.
    non_interactive: bool,
    console: Box<dyn Console>,
}

impl CliWizardDriver {
    /// Drive the wizard against the process's own terminal.
    pub fn terminal(non_interactive: bool) -> Self {
        Self {
            non_interactive,
            console: Box::new(TerminalConsole),
        }
    }
}

#[async_trait::async_trait]
impl WizardDriver for CliWizardDriver {
    /// Step 1: pick the data root.
    ///
    /// In interactive mode this is the only prompt the wizard issues.
    /// It carries a Press-Enter default: an existing portable layout
    /// beside the running binary when there is one, otherwise the
    /// platform's suggested root. Only a host whose data directory
    /// cannot be located leaves the prompt without one, and there an
    /// empty answer is the one remaining way to fail. Validation runs
    /// before the choice is echoed, so a refused root never renders a
    /// `Using` line.
    async fn step_data_root(&self, hint: DataRootHint) -> Result<PathBuf> {
        let console = &*self.console;
        print_intro(console);
        console.line("[1/5] Data root");
        if let Some(path) = &hint.data_dir {
            let abs = absolutise(path)?;
            validate_unused_or_force(&abs, hint.force)?;
            console.line(&format!("      Using {}", abs.display()));
            return Ok(abs);
        }
        if hint.non_interactive {
            eyre::bail!("--data-dir is required in --non-interactive mode");
        }
        // A discovered layout outranks a suggested one: something is
        // already there, and defaulting past it would strand it.
        let offered = hint.portable.as_ref().or(hint.default_root.as_ref());
        let question = match (&hint.portable, &hint.default_root) {
            (Some(portable), _) => {
                console.line(&format!(
                    "      Portable layout detected at {}.",
                    portable.display()
                ));
                "      Press Enter to use it, or type another path: ".to_string()
            }
            (None, Some(default_root)) => format!(
                "      Press Enter to use {}, or type another path: ",
                default_root.display()
            ),
            (None, None) => "      Where should books, indexes, and logs live? Path: ".to_string(),
        };
        let typed = console.prompt(&question)?;
        let chosen = if typed.is_empty() {
            offered
                .cloned()
                .context("a data root path is required (this host has no portable layout and no platform data directory to default to)")?
        } else {
            PathBuf::from(typed)
        };
        let abs = absolutise(&chosen)?;
        validate_unused_or_force(&abs, hint.force)?;
        console.line(&format!("      Using {}", abs.display()));
        Ok(abs)
    }

    /// Step 2: report the PDFium search. Warn-only: ingest of EPUB and
    /// TXT works without PDFium; only the PDF adapter needs it. A miss
    /// lists every directory the loader would check; when a pinned
    /// binary exists for this platform, an interactive run offers to
    /// download it on the spot.
    async fn step_pdfium(&self, report: &PdfiumReport) -> Result<PdfiumChoice> {
        let console = &*self.console;
        console.line("[2/5] PDFium native library");
        if let Some(path) = &report.found {
            console.line(&format!("      Found {}", path.display()));
            return Ok(PdfiumChoice::Continue);
        }
        let filename = report.filename;
        console.line(&format!("      WARN: {filename} not found. Searched:"));
        for dir in &report.probed {
            console.line(&format!("            {}", dir.display()));
        }
        if report.installable && !self.non_interactive {
            let answer = console.prompt("      Download the pinned PDFium build now? [Y/n]: ")?;
            if answer.is_empty() || answer.eq_ignore_ascii_case("y") {
                console.line("      Downloading ...");
                return Ok(PdfiumChoice::Install);
            }
        }
        console.line(
            "            Run `bookrack doctor --install-pdfium` later, or set \
             BOOKRACK_PDFIUM_LIB. PDF ingest will fail until the library is \
             present; EPUB and TXT still work.",
        );
        Ok(PdfiumChoice::Continue)
    }

    /// Step 2b: report the download outcome. Warn-only either way; a
    /// failed install degrades PDF ingest, nothing else.
    async fn step_pdfium_install(&self, outcome: &PdfiumInstallOutcome) -> Result<()> {
        let console = &*self.console;
        match outcome {
            PdfiumInstallOutcome::Installed(path) => {
                console.line(&format!("      Installed {}", path.display()));
            }
            PdfiumInstallOutcome::Failed(reason) => {
                console.warn(&format!("      WARN: PDFium install failed: {reason}"));
                console.warn(
                    "            Run `bookrack doctor --install-pdfium` to retry. \
                     PDF ingest will fail until the library is present; EPUB \
                     and TXT still work.",
                );
            }
        }
        Ok(())
    }

    /// Step 3: report the Ollama probe. Unreachable daemon or a
    /// missing embed model aborts the wizard with a remediation hint.
    async fn step_ollama(&self, step: &OllamaStep<'_>) -> Result<()> {
        let console = &*self.console;
        let url = step.url;
        let embed_model = step.embed_model;
        console.line("[3/5] Ollama daemon");
        console.line(&format!("      Probing {url} ..."));
        if !step.report.reachable {
            console.warn(&format!("      FAIL: Ollama is not reachable at {url}."));
            console.warn("            Install it from https://ollama.com, run `ollama serve`,");
            console.warn("            pull the model:");
            console.warn(&format!("              ollama pull {embed_model}"));
            console.warn("            then rerun `bookrack init`.");
            eyre::bail!("Ollama unreachable");
        }
        if !report_has_model(step.report, embed_model) {
            console.warn(&format!(
                "      FAIL: Ollama is up but {embed_model} is not pulled."
            ));
            console.warn(&format!("            Run:  ollama pull {embed_model}"));
            console.warn("            then rerun `bookrack init`.");
            eyre::bail!("embed model not pulled");
        }
        console.line(&format!(
            "      OK ({} model(s) pulled, {embed_model} present)",
            step.report.models.len(),
        ));
        Ok(())
    }

    /// Step 4: report the smoke outcome. A zero-hit search aborts —
    /// the embed or search pipeline is broken end-to-end.
    async fn step_smoke(&self, outcome: &SmokeOutcome) -> Result<()> {
        let console = &*self.console;
        console.line("[4/5] End-to-end probe");
        match outcome {
            SmokeOutcome::Skipped => {
                console.line("      Skipped (--no-smoke).");
            }
            SmokeOutcome::Ran(report) => {
                console.line("      Ingesting a synthetic fixture through Ollama -> LanceDB ...");
                console.line(&format!(
                    "      Ingested {} chunk(s); querying for marker ...",
                    report.chunks_written,
                ));
                if report.hits == 0 {
                    let marker = report.marker_query;
                    eyre::bail!(
                        "smoke search returned no hits for `{marker}` -- the embed or search pipeline is broken"
                    );
                }
                console.line(&format!(
                    "      OK ({} hit(s) on the marker token)",
                    report.hits
                ));
            }
        }
        Ok(())
    }

    /// Step 5: report what finalize wrote, then the closing hints.
    async fn step_finalize(&self, summary: &FinalizeSummary) -> Result<()> {
        let console = &*self.console;
        console.line("[5/5] Finalizing");
        console.line(&format!(
            "      Created {} (sources, books, logs, audit-rules)",
            summary.data_root.display()
        ));
        if summary.config_kept {
            console.line(&format!(
                "      Kept existing {}",
                summary.config_path.display()
            ));
        } else {
            console.line(&format!("      Wrote {}", summary.config_path.display()));
        }
        if summary.manifest_kept {
            console.line(&format!(
                "      Kept existing {}",
                summary.manifest_path.display()
            ));
        } else {
            console.line(&format!("      Wrote {}", summary.manifest_path.display()));
        }
        match &summary.registry {
            Some(path) => {
                console.line(&format!(
                    "      Wrote {} (default = \"default\")",
                    path.display()
                ));
            }
            None => {
                console.warn(&format!(
                    "      WARN: could not locate the platform config directory. \
                     Set BOOKRACK_DATA_DIR=\"{}\" so other shells find this library.",
                    summary.data_root.display(),
                ));
            }
        }
        print_success(console, &summary.data_root);
        Ok(())
    }
}

fn print_intro(console: &dyn Console) {
    console.line("bookrack init: a five-step setup wizard.");
    console.line("");
}

fn print_success(console: &dyn Console, data_root: &Path) {
    console.line("");
    console.line("bookrack is ready.");
    console.line("");
    console.line(&format!("Data root: {}", data_root.display()));
    console.line("");
    console.line("Try:");
    console.line("  bookrack ingest /path/to/book.epub");
    console.line("  bookrack query \"your question\"");
    console.line("  bookrack-mcp          # start the MCP server on 127.0.0.1:8765");
}

fn report_has_model(probe: &EmbedProbeReport, name: &str) -> bool {
    probe.models.iter().any(|m| m == name)
}

/// Resolve a user-typed path against the current working directory.
/// Relative paths are common in interactive use; the wizard records the
/// absolute form so a later `bookrack` invocation from another
/// directory still finds the same root.
fn absolutise(p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    let cwd = std::env::current_dir().context("read current working directory")?;
    Ok(cwd.join(p))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Answers the scripted console hands back, and everything the
    /// driver rendered, in order.
    #[derive(Default)]
    struct Script {
        answers: Mutex<VecDeque<String>>,
        captured: Mutex<Vec<String>>,
    }

    impl Script {
        fn with_answers<const N: usize>(answers: [&str; N]) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.iter().map(|a| a.to_string()).collect()),
                captured: Mutex::new(Vec::new()),
            })
        }

        fn captured(&self) -> Vec<String> {
            self.captured.lock().expect("captured").clone()
        }

        /// How many rendered lines contain `needle`. Counting rather
        /// than testing presence is what tells a re-ask from a single
        /// question.
        fn times_rendered(&self, needle: &str) -> usize {
            self.captured()
                .iter()
                .filter(|line| line.contains(needle))
                .count()
        }
    }

    /// A console driven by a [`Script`]. Running out of answers is an
    /// error rather than a block: a driver that re-asks without a bound
    /// then fails the test instead of hanging it.
    struct ScriptedConsole(Arc<Script>);

    impl Console for ScriptedConsole {
        fn line(&self, text: &str) {
            self.0.captured.lock().expect("captured").push(text.into());
        }

        fn warn(&self, text: &str) {
            self.0.captured.lock().expect("captured").push(text.into());
        }

        fn prompt(&self, prompt: &str) -> Result<String> {
            self.0
                .captured
                .lock()
                .expect("captured")
                .push(prompt.into());
            self.0
                .answers
                .lock()
                .expect("answers")
                .pop_front()
                .ok_or_else(|| eyre::eyre!("the script ran out of answers"))
        }
    }

    fn scripted_driver(script: &Arc<Script>) -> CliWizardDriver {
        CliWizardDriver {
            non_interactive: false,
            console: Box::new(ScriptedConsole(Arc::clone(script))),
        }
    }

    /// A hint with neither `--data-dir` nor a portable layout: the
    /// shape of a first run on a host that has no bookrack yet.
    fn interactive_hint(default_root: Option<&str>) -> DataRootHint {
        DataRootHint {
            portable: None,
            default_root: default_root.map(PathBuf::from),
            data_dir: None,
            non_interactive: false,
            force: false,
        }
    }

    #[tokio::test]
    async fn first_question_offers_the_platform_default_on_enter() {
        let script = Script::with_answers([""]);
        let driver = scripted_driver(&script);
        let expected = PathBuf::from("/opt/state/bookrack/library");

        let chosen = driver
            .step_data_root(interactive_hint(Some("/opt/state/bookrack/library")))
            .await
            .expect("pressing Enter takes the offered default");

        assert_eq!(chosen, expected);
        assert_eq!(
            script.times_rendered("/opt/state/bookrack/library"),
            2,
            "the prompt must name the default, and the choice must be \
             echoed: {:?}",
            script.captured()
        );
        // The guard is the oracle for the offer: whatever the driver
        // hands back on Enter has to be a root the wizard would accept.
        validate_unused_or_force(&chosen, false)
            .expect("the offered default must pass the data-root guard");
    }

    /// A portable layout already holds data; the suggested root holds
    /// nothing. Defaulting past the former would strand it.
    #[tokio::test]
    async fn a_portable_layout_outranks_the_platform_default() {
        let script = Script::with_answers([""]);
        let driver = scripted_driver(&script);
        let portable = PathBuf::from("/media/stick/bookrack/bookrack-data");
        let hint = DataRootHint {
            portable: Some(portable.clone()),
            ..interactive_hint(Some("/opt/state/bookrack/library"))
        };

        let chosen = driver.step_data_root(hint).await.expect("Enter");

        assert_eq!(chosen, portable);
        assert_eq!(
            script.times_rendered("/opt/state/bookrack/library"),
            0,
            "the suggested root must not be mentioned: {:?}",
            script.captured()
        );
    }

    #[tokio::test]
    async fn a_typed_path_wins_over_the_offered_default() {
        let script = Script::with_answers(["/srv/books"]);
        let driver = scripted_driver(&script);

        let chosen = driver
            .step_data_root(interactive_hint(Some("/opt/state/bookrack/library")))
            .await
            .expect("a typed path");

        assert_eq!(chosen, PathBuf::from("/srv/books"));
    }

    /// The only remaining way for an empty answer to fail: a host whose
    /// platform data directory could not be located at all.
    #[tokio::test]
    async fn an_empty_answer_fails_only_when_there_is_nothing_to_default_to() {
        let script = Script::with_answers([""]);
        let driver = scripted_driver(&script);

        let err = driver
            .step_data_root(interactive_hint(None))
            .await
            .expect_err("nothing to default to");

        let rendered = format!("{err}");
        assert!(
            rendered.contains("a data root path is required"),
            "unexpected reason: {rendered}"
        );
        assert_eq!(
            script.times_rendered("Press Enter"),
            0,
            "a prompt without a default must not offer one: {:?}",
            script.captured()
        );
    }

    /// The guard judges the offered default too. Were the suggested
    /// root ever moved beside the running binary, on macOS that lands
    /// inside `Bookrack.app`, and the wizard must refuse it rather than
    /// quietly write a library that the next upgrade deletes.
    #[tokio::test]
    async fn an_offered_default_inside_a_bundle_is_refused_not_used() {
        let script = Script::with_answers([""]);
        let driver = scripted_driver(&script);
        let inside = "/Applications/Bookrack.app/Contents/Resources/bookrack-data";

        let err = driver
            .step_data_root(interactive_hint(Some(inside)))
            .await
            .expect_err("a bundled default must be refused");

        let rendered = format!("{err}");
        assert!(
            rendered.contains("/Applications/Bookrack.app"),
            "unexpected reason: {rendered}"
        );
        assert_eq!(
            script.times_rendered("      Using "),
            0,
            "a refused root must not be echoed as chosen: {:?}",
            script.captured()
        );
    }
}
