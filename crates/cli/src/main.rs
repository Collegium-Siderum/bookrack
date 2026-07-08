// SPDX-License-Identifier: Apache-2.0

//! bookrack command-line entry point.
//!
//! A thin shell over the library pipeline: parse arguments, resolve
//! [`Config`], install the tracing subscriber, open the stores, and call
//! the graduated `ingest_book` / `search` entry points. All orchestration
//! lives in those library functions; this binary only wires inputs to them
//! and renders their reports. Operational tuning comes entirely from the
//! environment via `Config` and the `*Config::from_env` resolvers — the
//! command surface carries no tuning flags, so there is a single source of
//! truth for every default.

mod cmd;
mod exec;
mod init;
mod preflight;
mod run;
mod util;

use std::path::{Path, PathBuf};

use bookrack_cli_grammar::{
    CorpusAction, DistillAction, DryrunArgs, IngestArgs, IntakeAction, LogsArgs, PapersAction,
    QueueAction, RemoveArgs, StampsAction, WriteMetadataAction, WriteVectorsAction,
};
use bookrack_config::{Config, ConfigError, LibrarySelection};
use bookrack_runtime::cmd::audit_profile::AuditProfileAction;
use bookrack_runtime::cmd::index_profile::IndexProfileAction;
use bookrack_runtime::cmd::libraries::CopyMode;
use eyre::{Context, Result};

/// Trailing block shown by `bookrack --help`. Names the environment
/// variables that select the library and the embed backend, and the
/// runtime prerequisite a fresh install most often trips over: Ollama
/// must be reachable for any command that embeds text.
const TOP_AFTER_HELP: &str = "\
Environment:
  BOOKRACK_DATA_DIR     library data root (overridden by --data-dir)
  BOOKRACK_REGISTRY     TOML file mapping --library names to roots
  BOOKRACK_OLLAMA_URL   Ollama endpoint (default http://localhost:11434)
  BOOKRACK_EMBED_MODEL  embedding model tag (default qwen3-embedding:0.6b)
  BOOKRACK_LOG          tracing filter directive (default info; debug for verbose)

Library reads (search, browse, metadata, status) live behind `bookrack exec
library.<tool>`. Run `bookrack run` to start a session, then enumerate the
live control-plane surface with `bookrack exec tools`.

Prerequisites:
  Start Ollama and pull the embed model before running the session, e.g.:
      ollama pull qwen3-embedding:0.6b";

#[derive(clap::Parser)]
#[command(
    name = "bookrack",
    version,
    about = "Search a local library of books.",
    after_help = TOP_AFTER_HELP,
)]
struct Cli {
    /// Select the library at this data root, overriding the
    /// environment. On local commands (`run`, `init`, `doctor`,
    /// `audit-profile`, `distill`, `runs`) this switches the data
    /// root for the invocation. On commands that route through a
    /// running daemon, the daemon must already be serving this root;
    /// a mismatch aborts the command without acting. Mutually
    /// exclusive with `--library`.
    #[arg(
        long,
        global = true,
        conflicts_with = "library",
        help_heading = "Common Options"
    )]
    data_dir: Option<PathBuf>,
    /// Select the named library from the registry (see
    /// BOOKRACK_REGISTRY). Behaves like `--data-dir`: a switch on
    /// local commands, an assertion against the running daemon on
    /// routed commands. Mutually exclusive with `--data-dir`.
    #[arg(long, global = true, help_heading = "Common Options")]
    library: Option<String>,
    /// Select an audit profile by name. Built-in names are
    /// `default`, `trust-source`, and `strict`. Without this flag the
    /// `<data_root>/audit-rules/audit_profile.local.toml` overlay is
    /// merged onto the shipped default; with it the overlay is
    /// bypassed and the named preset wins. Applies to `ingest`,
    /// `intake ocr`, `dryrun`, `metadata reaudit`, `metadata advance`,
    /// and `papers metadata reaudit`; passing the flag on any other
    /// subcommand aborts before any RPC is sent.
    #[arg(
        long,
        global = true,
        value_name = "NAME",
        help_heading = "Common Options"
    )]
    audit_profile: Option<String>,
    /// Emit machine-parseable JSON instead of the human renderer.
    /// Mutually exclusive with `--quiet`.
    #[arg(
        long,
        global = true,
        conflicts_with = "quiet",
        help_heading = "Output Options"
    )]
    json: bool,
    /// Suppress non-essential stdout on success. Errors still
    /// surface through the reporter.
    #[arg(long, global = true, help_heading = "Output Options")]
    quiet: bool,
    /// Strip ANSI styling from output even when stderr is a TTY.
    #[arg(long, global = true, help_heading = "Output Options")]
    no_color: bool,
    #[command(subcommand)]
    command: Command,
}

impl Cli {
    /// The library selection these top-level flags express.
    fn selection(&self) -> LibrarySelection {
        LibrarySelection {
            data_dir: self.data_dir.clone(),
            library: self.library.clone(),
        }
    }
}

#[derive(clap::Subcommand)]
enum Command {
    /// Inspect and compare the built-in audit profiles.
    ///
    /// Pure reflection over the profiles compiled into the binary — no
    /// library, no MCP session.
    AuditProfile {
        #[command(subcommand)]
        action: AuditProfileAction,
    },
    /// List, show, and validate index profiles.
    ///
    /// An index profile couples the embedding model, the ANN index
    /// shape, and the reranker stage into one named, statically-checked
    /// atom. Pure reflection over the built-in profiles compiled into
    /// the binary plus any user profiles under the per-user profile
    /// directory — no library, no MCP session.
    IndexProfile {
        #[command(subcommand)]
        action: IndexProfileAction,
    },
    /// Verify schemas and cross-store counts against the live data root.
    ///
    /// Compare the catalog and corpus schemas against the binary's
    /// TableSpecs and tally the cross-store counts: catalog intakes,
    /// vectors-meta chunk count, and intake-file existence on disk.
    Verify,
    /// Inspect the library registry.
    ///
    /// The registry is the file named by `BOOKRACK_REGISTRY` that maps
    /// short names to data roots.
    Libraries {
        #[command(subcommand)]
        action: LibrariesAction,
    },
    /// Bundle crash reports and logs into a scrubbed `.tar.gz`.
    ///
    /// Collects the data root's crash reports, recent logs, and a small
    /// catalog snapshot. The bundle is suitable for attaching to a bug
    /// report.
    Diagnose {
        /// Output path for the bundle. Defaults to
        /// `<data_dir>/diagnostics/diagnose-<unix_ms>.tar.gz`.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
        /// Time window for recent logs and audit rows, in days.
        #[arg(long, default_value_t = bookrack_diagnose::DEFAULT_DAYS)]
        days: u32,
        /// Skip the scrubber so paths and book titles ride through
        /// verbatim. Appropriate only for bundles kept locally.
        #[arg(long)]
        no_scrub: bool,
    },
    /// Start the daemon session and serve MCP for the terminal's lifetime.
    ///
    /// Warm the library registry, acquire the machine-wide session lock,
    /// and serve MCP over streamable HTTP. The foreground task idles
    /// until a shutdown signal arrives (Ctrl-C, SIGTERM, SIGHUP, or the
    /// control-plane `daemon.shutdown` RPC).
    Run {
        /// Override the MCP listener address. Defaults to the value
        /// from `BOOKRACK_MCP_ADDR` (and falls back to the built-in
        /// loopback address from there).
        #[arg(long, value_name = "ADDR")]
        mcp_addr: Option<std::net::SocketAddr>,
        /// Skip binding the MCP listener. The session lock is still
        /// taken and the registry is still opened; useful when another
        /// tool already owns the MCP port.
        #[arg(long)]
        no_mcp: bool,
        /// Override the runtime directory. Falls back to
        /// `BOOKRACK_RUNTIME_DIR` or the platform default.
        #[arg(long, value_name = "PATH")]
        runtime_dir: Option<PathBuf>,
    },
    /// Call control-plane RPCs against the running session.
    ///
    /// Subcommands:
    ///   `info` (default)          — print the session pid, MCP
    ///                               address, and control socket path.
    ///                               Pure file read of the session
    ///                               lock; never opens the control
    ///                               socket.
    ///   `tools`                   — list the control-plane methods
    ///                               the daemon answers, alongside the
    ///                               daemon's MCP endpoint tools for
    ///                               visibility. Only the control-plane
    ///                               methods are reachable from `exec`.
    ///   `<method> [<json>]`       — call the named control-plane
    ///                               method (e.g. `library.show_book`),
    ///                               with the second positional token
    ///                               forwarded verbatim as JSON params.
    ///
    /// Reads `${BOOKRACK_RUNTIME_DIR}/bookrack.tty.lock` to discover
    /// the session; never opens a catalog, corpus, or vector store.
    Exec {
        /// Subcommand and its positional arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run a one-screen health check.
    ///
    /// Reports data root resolution, schema versions, PDFium library
    /// presence, Ollama daemon reachability, and whether the configured
    /// embed model is pulled. Exits with a non-zero status when any row
    /// fails, so a script can branch on the result.
    Doctor {
        /// Deprecated: use the top-level `--json` instead.
        #[arg(long, hide = true)]
        json: bool,
        /// Download the pinned PDFium build into the per-user managed
        /// directory before gathering the report. PDF ingest needs the
        /// library; the other formats do not.
        #[arg(long)]
        install_pdfium: bool,
        /// Rename legacy-named envelope files under the books and
        /// papers opaque stores to the kind-prefixed shape produced by
        /// `envelope_filename`. Idempotent; already-prefixed files are
        /// skipped.
        #[arg(long)]
        rename_envelopes: bool,
        /// Recover the `derived_from_sha256` edge on OCR product intakes
        /// that predate the column, reading each parent scan PDF's hash
        /// from the intake's envelope provenance. Idempotent; rows whose
        /// edge is already set are skipped. Run this once after
        /// upgrading a library that holds OCR intakes, so
        /// `intake list-ocr-pending` does not list their already-
        /// processed sources. An offline repair: it writes the catalog
        /// directly, so it is refused while a daemon is serving the
        /// library — stop it with `bookrack quit` first.
        #[arg(long)]
        backfill_ocr_derivation: bool,
        /// With `--rename-envelopes` or `--backfill-ocr-derivation`,
        /// compute the plan without touching the disk or database.
        #[arg(long)]
        dry_run: bool,
    },
    /// Submit one or more files for ingest.
    ///
    /// Requires a running bookrack daemon; the command exits with code
    /// 2 if no daemon is found.
    Ingest(IngestArgs),
    /// Drive an intake from a derived source manifestation (OCR-only).
    ///
    /// The job is enqueued onto the persistent ingest queue and
    /// dispatched by the worker as a book ingest job whose source is
    /// the OCR markdown product paired with the original scan PDF.
    Intake {
        #[command(subcommand)]
        action: IntakeAction,
    },
    /// Inspect or mutate the persistent ingest queue.
    ///
    /// Covers `list`, `pause`, `resume`, `clear`, and
    /// `cancel <job-id-prefix>`.
    Queue {
        #[command(subcommand)]
        action: QueueAction,
    },
    /// Edit one book's metadata.
    Metadata {
        #[command(subcommand)]
        action: WriteMetadataAction,
    },
    /// Vector-store writes: rebuild, reembed, reset, or drop.
    ///
    /// Prefer `bookrack index-profile apply`; this namespace is the
    /// low-level escape hatch.
    Vectors {
        #[command(subcommand)]
        action: WriteVectorsAction,
    },
    /// Rebuild the corpus tree from the opaque envelope store.
    Corpus {
        #[command(subcommand)]
        action: CorpusAction,
    },
    /// Reconcile corpus index stamps.
    ///
    /// Prefer `bookrack index-profile apply`; this namespace is the
    /// low-level escape hatch.
    Stamps {
        #[command(subcommand)]
        action: StampsAction,
    },
    /// Drop a book from every store.
    Remove(RemoveArgs),
    /// Paper-side surface: ingest, browse, and export papers.
    ///
    /// Ingest a paper file, browse the paper catalog, or export one
    /// paper's bibliographic record as CSL-JSON. The book-side
    /// counterparts are `ingest`, `metadata`, `corpus`, `vectors`,
    /// and `stamps`.
    Papers {
        #[command(subcommand)]
        action: PapersAction,
    },
    /// Simulate an ingest without writing the live stores.
    Dryrun(DryrunArgs),
    /// Ask the running bookrack daemon to shut down.
    ///
    /// Exits with code 0 whether or not a daemon was found.
    Quit,
    /// Run the interactive install wizard.
    ///
    /// Walks the operator through a five-step install: pick a data
    /// root, check the PDFium library, probe Ollama, smoke-test the
    /// ingest → embed → query pipeline end-to-end in a tempdir, and
    /// finally write `<data_root>/config.toml` plus a pointer in the
    /// platform-default registry. Run after a fresh tarball install.
    Init {
        /// Where the library's data root should live. Required in
        /// `--non-interactive` mode; otherwise the wizard prompts.
        #[arg(long, value_name = "PATH")]
        data_dir: Option<PathBuf>,
        /// Skip every prompt. Requires `--data-dir`.
        #[arg(long)]
        non_interactive: bool,
        /// Accept an existing data root that already holds a
        /// `catalog.db`. Without this flag the wizard refuses, so a
        /// misconfigured run cannot silently graft itself onto a
        /// populated library.
        #[arg(long)]
        force: bool,
        /// Skip the end-to-end smoke step. Useful when developing the
        /// wizard itself or when Ollama is intentionally offline.
        #[arg(long)]
        no_smoke: bool,
    },
    /// Build, verify, or list distilled reference-book entries.
    ///
    /// Operates directly on `<data>/reference.db` and the per-book
    /// `<data>/reference/<slug>/book.toml` directories. Does not go
    /// through the daemon's control plane.
    Distill {
        #[command(subcommand)]
        action: DistillAction,
    },
    /// Inspect `pipeline_runs` — the registry of every top-level
    /// operator invocation, with its `pipeline_run_summary` rollup.
    Runs {
        #[command(subcommand)]
        action: bookrack_cli_grammar::RunsAction,
    },
    /// Inspect `retrieval_calls` — the sidecar recording every
    /// single-store search invocation with the corpus fingerprint that
    /// served it and its per-hit detail.
    Retrieval {
        #[command(subcommand)]
        action: bookrack_cli_grammar::RetrievalAction,
    },
    /// Stream or snapshot the running daemon's logs.
    ///
    /// `--follow` (the default with no other flags) subscribes to the
    /// broadcast for as long as the command runs. `--tail N` snapshots
    /// the last N events from the daemon's in-memory ring through the
    /// `logs.tail` RPC and exits. Combine both for `tail | follow`
    /// semantics. Human mode renders each event as
    /// `HH:MM:SS LEVEL target | message`; `--json` emits the
    /// underlying `LogEvent` payload as newline-delimited JSON.
    Logs(LogsArgs),
}

#[derive(clap::Subcommand, Debug)]
pub(crate) enum LibrariesAction {
    /// List every entry in the registry.
    ///
    /// Marks the `default = "..."` fallback when one is set.
    List {
        /// Deprecated: use the top-level `--json` instead.
        #[arg(long, hide = true)]
        json: bool,
    },
    /// Print the per-library status card.
    ///
    /// The card is what the daemon serves over `library.info`:
    /// configured paths, embed model, vector-store shape, and catalog
    /// counts.
    Info {
        /// Library short name. When omitted, the daemon picks the
        /// registry's current default.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
    /// Set the registry's default-library pointer to `name`.
    ///
    /// Writes straight to the on-disk registry, so it needs no running
    /// daemon and the change persists across restarts. Errors if the
    /// registry does not define `name`.
    Default {
        /// Library short name to record as the registry default.
        name: String,
    },
    /// Clone the current library into a sibling at a new data root.
    ///
    /// Shares `books/` (the envelope store) via hardlinks by default,
    /// and registers the new library so `--library <name>` resolves it.
    /// The new library has no vector store; run `vectors reset` against
    /// it to rebuild under whatever model the env points at.
    Fork {
        /// Short name to register in the library registry.
        new_name: String,
        /// Absolute path where the new data root lives. Must not
        /// already contain a library.
        #[arg(long)]
        data_dir: std::path::PathBuf,
        /// How the envelope store is shared. `hardlink` (default)
        /// keeps disk usage flat; `copy` duplicates bytes outright.
        #[arg(long, value_enum, default_value_t = CopyMode::Hardlink)]
        copy_mode: CopyMode,
        /// Skip the destructive-action confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Probe whether a path looks like a bookrack data root (read-only).
    ///
    /// Resolves locally with no daemon. Exits 0 when the path is a
    /// confirmed or probable data root, 1 when it is not (or its
    /// manifest is unreadable), and 2 for a missing or non-directory
    /// path.
    Detect {
        /// Path to probe.
        path: std::path::PathBuf,
    },
    /// Walk a parent directory (or mounted volumes) and list the
    /// bookrack data roots found (read-only).
    ///
    /// Resolves locally with no daemon and always exits 0. Give exactly
    /// one of a parent directory or `--volumes`.
    #[command(group(clap::ArgGroup::new("scan_target").required(true).args(["parent", "volumes"])))]
    Scan {
        /// Parent directory whose immediate subdirectories are probed.
        parent: Option<std::path::PathBuf>,
        /// Scan mounted volumes instead of a parent directory.
        #[arg(long)]
        volumes: bool,
        /// Register every confirmed data root found. Probable roots are
        /// listed but skipped — bring those in with an explicit `add`.
        #[arg(long)]
        register: bool,
        /// Kind recorded for roots registered with `--register`,
        /// overriding what each manifest declares.
        #[arg(long, value_enum)]
        kind: Option<KindArg>,
    },
    /// Register an existing data root under an explicit name.
    ///
    /// Resolves locally with no daemon. Writes an identity manifest to
    /// the root when it has none (previewed and confirmed first, unless
    /// `--yes`), then records the registry entry.
    Add {
        /// Registry name to record the root under. Wins over the
        /// manifest's birth name; a mismatch is a legal alias.
        name: String,
        /// Data root to register.
        path: std::path::PathBuf,
        /// Library kind for the entry, overriding the manifest's.
        #[arg(long, value_enum)]
        kind: Option<KindArg>,
        /// Free-form description for the entry.
        #[arg(long)]
        description: Option<String>,
        /// Re-mint the root's identity uuid before registering, so a
        /// copied data root registers as a distinct library.
        #[arg(long)]
        new_uuid: bool,
        /// Skip the manifest-write confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Register an existing data root; the name is taken from its
    /// manifest (or directory name when it has no manifest).
    ///
    /// Resolves locally with no daemon. Give `--name` to register under
    /// an alias when the derived name is already taken.
    Register {
        /// Data root to register.
        path: std::path::PathBuf,
        /// Alias to register under, when the derived name collides.
        #[arg(long)]
        name: Option<String>,
        /// Library kind for the entry, overriding the manifest's.
        #[arg(long, value_enum)]
        kind: Option<KindArg>,
        /// Free-form description for the entry.
        #[arg(long)]
        description: Option<String>,
        /// Re-mint the root's identity uuid before registering.
        #[arg(long)]
        new_uuid: bool,
        /// Skip the manifest-write confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Drop a registry entry. The data root stays on disk unless
    /// `--purge` is given.
    ///
    /// Resolves locally with no daemon. `--purge` additionally deletes
    /// the data root, gated on a confirmed/probable detect verdict and a
    /// typed confirmation.
    Remove {
        /// Registry name to forget.
        name: String,
        /// Also delete the data root from disk. Irreversible.
        #[arg(long)]
        purge: bool,
        /// Skip the confirmation prompt (`--purge` still requires the
        /// detect gate to pass).
        #[arg(long)]
        yes: bool,
    },
    /// Read or edit a registered library's `config.toml`.
    ///
    /// Resolves the library's data root from the registry with no
    /// daemon, then reads or edits its `config.toml` in place. With no
    /// `KEY=VALUE` arguments (and no `--unset`), prints the file. The
    /// change does not reach a running daemon until it restarts.
    Config {
        /// Registry name whose config is read or edited.
        name: String,
        /// `KEY=VALUE` pairs to set. Accepted keys: `ollama_url`,
        /// `embed_model`, `mcp_addr`, `log_directive`.
        #[arg(value_parser = parse_key_val, value_name = "KEY=VALUE")]
        sets: Vec<(String, String)>,
        /// Key to remove from the file. Repeatable.
        #[arg(long, value_name = "KEY")]
        unset: Vec<String>,
    },
}

/// Split a `KEY=VALUE` argument on its first `=`. Rejects an empty key or
/// an argument with no `=`, so clap reports the fault and exits 2 before
/// any dispatch.
fn parse_key_val(raw: &str) -> Result<(String, String), String> {
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=VALUE, got '{raw}'"))?;
    if key.is_empty() {
        return Err(format!("empty key in '{raw}'"));
    }
    if value.is_empty() {
        return Err(format!(
            "empty value in '{raw}'; use --unset {key} to clear a key"
        ));
    }
    Ok((key.to_string(), value.to_string()))
}

/// CLI mirror of [`bookrack_config::LibraryKind`], deriving clap's
/// `ValueEnum` so `--kind` lists its choices and completes without the
/// foundational `config` crate taking a clap dependency.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum KindArg {
    /// A primary, long-lived library.
    Prod,
    /// A library used for testing or evaluation.
    Test,
    /// A disposable scratch library.
    Scratch,
}

impl From<KindArg> for bookrack_config::LibraryKind {
    fn from(kind: KindArg) -> Self {
        match kind {
            KindArg::Prod => bookrack_config::LibraryKind::Prod,
            KindArg::Test => bookrack_config::LibraryKind::Test,
            KindArg::Scratch => bookrack_config::LibraryKind::Scratch,
        }
    }
}

/// clap's default "did you mean" tip only sees top-level subcommand
/// names, so a user typing `bookrack list` lands on a suggestion of
/// `bookrack ingest`. This wrapper parses normally, then on a
/// `InvalidSubcommand` error checks the offending token against a
/// hand-maintained map of natural-name aliases and prints a friendlier
/// tip before exiting through clap's own renderer.
fn parse_cli_with_natural_name_hints() -> Cli {
    match <Cli as clap::Parser>::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            if err.kind() == clap::error::ErrorKind::InvalidSubcommand
                && let Some(typed) = invalid_subcommand_token(&err)
                && let Some(hint) = natural_name_hint(&typed)
            {
                eprintln!("tip: did you mean {hint}?");
            }
            err.exit();
        }
    }
}

/// Pull the offending token out of clap's `InvalidSubcommand` error
/// context, or `None` if the context shape is unexpected.
fn invalid_subcommand_token(err: &clap::Error) -> Option<String> {
    err.context().find_map(|(kind, value)| {
        if matches!(kind, clap::error::ContextKind::InvalidSubcommand)
            && let clap::error::ContextValue::String(s) = value
        {
            Some(s.clone())
        } else {
            None
        }
    })
}

/// Map a natural-language guess at a command name to the real
/// invocation. Returns the hint string already shaped for the user
/// (multiple options joined with ` or `), or `None` for tokens not in
/// the table — those fall through to clap's own similarity tip.
///
/// Library reads moved off the external CLI surface: the hints below
/// point at the `bookrack exec library.<tool>` proxy that talks to
/// the running daemon session.
fn natural_name_hint(typed: &str) -> Option<String> {
    let suggestions: &[&str] = match typed {
        "list" | "ls" => &["`bookrack exec library.list_books`"],
        "find" => &["`bookrack exec library.find_books`"],
        "show" => &["`bookrack exec library.show_book`"],
        "stats" => &["`bookrack exec library.stats`"],
        "status" => &[
            "`bookrack exec library.info`",
            "`bookrack exec library.stats`",
        ],
        "search" => &["`bookrack exec library.search`"],
        _ => return None,
    };
    Some(suggestions.join(" or "))
}

/// Handle an unconfigured-install case when the operator typed
/// `bookrack run`. On an interactive TTY, offer to launch the setup
/// wizard inline; on a non-TTY, point at `bookrack init` and propagate
/// the resolver error so the exit code is non-zero.
async fn offer_init_or_exit(err: ConfigError) -> Result<()> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        eprintln!("No library configured.");
        eprintln!("Run `bookrack init` from an interactive terminal first.");
        return Err(eyre::Report::new(err));
    }
    eprintln!("No library configured.");
    print!("Launch the setup wizard now? [Y/n]: ");
    std::io::stdout().flush().context("flush stdout")?;
    let mut buf = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut buf)
        .context("read line")?;
    let answer = buf.trim();
    if !(answer.is_empty()
        || answer.eq_ignore_ascii_case("y")
        || answer.eq_ignore_ascii_case("yes"))
    {
        eprintln!("Aborted. Run `bookrack init` to configure, then `bookrack run`.");
        return Err(eyre::Report::new(err));
    }
    init::run(init::Args {
        data_dir: None,
        non_interactive: false,
        force: false,
        no_smoke: false,
    })
    .await
    .context("setup wizard")
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // Install the color-eyre report and panic hooks. The hooks render
    // `eyre::Report` cause chains and panics with rustc-style colored
    // prefixes when stderr is a TTY, and as plain text when it is not.
    //
    // Two default sections are suppressed because they are noise on a
    // CLI tool's predictable user-input failures (missing file, bad
    // arg, unreachable network endpoint): the `Location:` source line
    // and the `EnvSection` (which carries the `Backtrace omitted. Run
    // with RUST_BACKTRACE=1 ...` hint). These remain available on
    // panics through the panic hook itself.
    //
    // A failure to install is fatal only for the reporter — the
    // program still runs, just with the default `Debug` formatting.
    if let Err(e) = color_eyre::config::HookBuilder::default()
        .display_location_section(false)
        .display_env_section(false)
        .install()
    {
        eprintln!("bookrack: failed to install error reporter: {e}");
    }
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            // Typed user-error variants render as a single
            // operator-facing line and pick their own exit code, so
            // predictable failures stay quiet. `classify_eyre` walks
            // the eyre chain so `.context("...")` wrappers around an
            // RPC call still surface their JSON-RPC error code as the
            // matching CLI exit code. Anything not classifiable falls
            // through to color-eyre's full `Debug` cause chain so
            // unexpected errors stay debuggable.
            if let Some(cause) = bookrack_cli::error::classify_eyre(&err) {
                let cli_err = cause.as_cli();
                if !cli_err.is_self_reported() {
                    eprintln!("bookrack: {cli_err}");
                }
                std::process::ExitCode::from(cli_err.exit_code())
            } else {
                eprintln!("{err:?}");
                std::process::ExitCode::FAILURE
            }
        }
    }
}

async fn run() -> Result<()> {
    let cli = parse_cli_with_natural_name_hints();

    // Install the process-wide render context from the global output
    // flags. Subcommand renderers read from `bookrack_cli::render::ctx()`
    // to decide between human and JSON formatting; legacy per-command
    // `--json` flags (kept as hidden aliases on `doctor` and
    // `libraries list`) merge into the global view at their dispatch
    // arms below.
    {
        use bookrack_cli::render::{ColorMode, OutputMode, RenderCtx};
        let output = if cli.json {
            OutputMode::Json
        } else if cli.quiet {
            OutputMode::Quiet
        } else {
            OutputMode::Human
        };
        let color = if cli.no_color {
            ColorMode::Never
        } else {
            ColorMode::Auto
        };
        if cli.no_color {
            // SAFETY: run() executes on the single startup task before
            // any subcommand spawns its own work; `set_var` is sound
            // here and propagates `NO_COLOR` to color-eyre's error
            // formatter for the remainder of the process.
            unsafe { std::env::set_var("NO_COLOR", "1") };
        }
        bookrack_cli::render::init(RenderCtx::new(output, color));
    }
    let json_global = cli.json;

    // Refuse a daemon-routed command when the invoking shell's
    // explicit library selection (`--data-dir` / `--library` /
    // `BOOKRACK_DATA_DIR`) disagrees with the library a running
    // daemon is serving. Skipped for commands that resolve a data
    // root locally (`run`, `init`, `audit-profile`, the `index-profile`
    // verbs other than an executing `apply`,
    // `distill`, `runs`, and the offline `libraries` verbs): the flag is a real
    // switch there, not an assertion. Silent when no daemon is running,
    // when no
    // selection was given, or when the lock predates the identity
    // fields that make the comparison possible.
    let index_profile_is_local = match &cli.command {
        // `apply` executes through the daemon, so only its offline
        // `--dry-run` form keeps the local-resolve exemption.
        Command::IndexProfile { action } => {
            !matches!(action, IndexProfileAction::Apply { dry_run: false, .. })
        }
        _ => false,
    };
    if !(index_profile_is_local
        || matches!(
            cli.command,
            Command::Init { .. }
                | Command::Run { .. }
                | Command::AuditProfile { .. }
                | Command::Distill { .. }
                | Command::Runs { .. }
                | Command::Retrieval { .. }
                | Command::Libraries {
                    action: LibrariesAction::Default { .. }
                        | LibrariesAction::Detect { .. }
                        | LibrariesAction::Scan { .. }
                        | LibrariesAction::Add { .. }
                        | LibrariesAction::Register { .. }
                        | LibrariesAction::Remove { .. }
                        | LibrariesAction::Config { .. }
                }
        ))
    {
        preflight::enforce_selection_mismatch(&cli.selection())?;
    }

    // `doctor` resolves on its own — it has a daemon-running path
    // (control plane) and a daemon-not-running fallback that probes
    // the data root directly without going through `Config::resolve`,
    // so an unconfigured install surfaces as a row instead of an
    // opaque resolver bail.
    if let Command::Doctor {
        json,
        install_pdfium,
        rename_envelopes,
        backfill_ocr_derivation,
        dry_run,
    } = &cli.command
    {
        let healthy = cmd::cli_client::doctor::run(
            &cli.selection(),
            *json || json_global,
            *install_pdfium,
            *rename_envelopes,
            *backfill_ocr_derivation,
            *dry_run,
            None,
        )
        .await?;
        // The doctor renderer already drew the per-check table plus
        // the summary line. Surface the failure through a typed
        // `BookrackCliError::DoctorUnhealthy` so the top-level reporter
        // maps it to exit code 1 without printing a redundant
        // `bookrack: …` prefix on top of the table.
        if !healthy {
            return Err(bookrack_cli::error::BookrackCliError::DoctorUnhealthy.into());
        }
        return Ok(());
    }
    if let Command::Init {
        data_dir,
        non_interactive,
        force,
        no_smoke,
    } = &cli.command
    {
        return init::run(init::Args {
            data_dir: data_dir.clone(),
            non_interactive: *non_interactive,
            force: *force,
            no_smoke: *no_smoke,
        })
        .await;
    }

    // `run` owns its own configuration bootstrap (lock acquisition,
    // obs init, library warm-up). Dispatching it before the shared
    // `Config::resolve` below keeps that ownership clean and lets the
    // daemon emit its lock-conflict message without first paying the
    // resolve cost.
    if let Command::Run {
        mcp_addr,
        no_mcp,
        runtime_dir,
    } = &cli.command
    {
        let selection = cli.selection();
        // Probe the resolver before acquiring the session lock. On an
        // unconfigured install, the operator gets the wizard inline
        // instead of an opaque "no library configured" bail -- the
        // platform launchers count on `bookrack run` to be a
        // self-contained first-run flow.
        if let Err(err) = Config::resolve(&selection) {
            match err {
                ConfigError::MissingDataDir | ConfigError::DataDirNotFound(_) => {
                    offer_init_or_exit(err).await?;
                }
                other => return Err(eyre::Report::new(other).wrap_err("resolve configuration")),
            }
        }
        return run::run_daemon(run::RunOpts {
            selection,
            mcp_addr: *mcp_addr,
            no_mcp: *no_mcp,
            runtime_dir: runtime_dir.clone(),
        })
        .await;
    }

    // `exec` is the discovery surface for an already-running daemon.
    // It must NOT open a database — the "no DB handle outside the
    // scheduler" invariant is what gives the daemon-REPL session its
    // single-writer guarantee — so it dispatches before Config::resolve
    // as well.
    if let Command::Exec { args } = &cli.command {
        return exec::run(args, None).await;
    }

    // Every remaining write/read subcommand reaches the daemon
    // through the control plane and never touches the local data
    // root from this process. The `AuditProfile` reflection runner
    // is the lone exception — it reads compiled-in profiles and
    // needs no config.
    let audit_profile = cli.audit_profile.clone();
    if audit_profile.is_some() && !accepts_audit_profile(&cli.command) {
        eyre::bail!(
            "--audit-profile is only consumed by `ingest`, `intake ocr`, \
             `dryrun`, `metadata reaudit`, `metadata advance`, and \
             `papers metadata reaudit`; remove it for this subcommand"
        );
    }
    let selection = cli.selection();
    match cli.command {
        Command::AuditProfile { action } => bookrack_runtime::cmd::audit_profile::run(action),
        Command::IndexProfile { mut action } => {
            match &mut action {
                IndexProfileAction::Current { library, json } => {
                    // The global `--library` selects the same library the
                    // subcommand-local flag does; the local one wins when
                    // both are given.
                    if library.is_none() {
                        *library = selection.library.clone();
                    }
                    *json = *json || json_global;
                }
                IndexProfileAction::Diff { json, .. } => *json = *json || json_global,
                IndexProfileAction::Apply { library, .. } if library.is_none() => {
                    *library = selection.library.clone();
                }
                _ => {}
            }
            // `apply` orchestrates control-plane calls (planning offline
            // under `--dry-run`), so it dispatches to the daemon client;
            // every other verb resolves locally.
            if let IndexProfileAction::Apply {
                name,
                library,
                pipeline,
                dry_run,
                yes,
            } = action
            {
                cmd::cli_client::index_profile::run(
                    &name,
                    library.as_deref(),
                    pipeline,
                    dry_run,
                    yes,
                    None,
                )
                .await
            } else {
                bookrack_runtime::cmd::index_profile::run(action)
            }
        }
        Command::Verify => cmd::cli_client::verify::run(None).await,
        Command::Libraries { mut action } => {
            if let LibrariesAction::List { json } = &mut action {
                *json = *json || json_global;
            }
            match action {
                LibrariesAction::Default { name } => {
                    // `libraries default` writes the registry directly,
                    // so it works with no daemon and the pointer persists
                    // across restarts. Resolve the registry file the same
                    // way the daemon's fork helper does.
                    let registry_path =
                        bookrack_config::registry_target_path().ok_or_else(|| {
                            eyre::eyre!(
                                "no registry location: set BOOKRACK_REGISTRY=<path> or ensure \
                                 the platform config directory is available"
                            )
                        })?;
                    bookrack_config::set_default_library(&registry_path, &name).map_err(|err| {
                        match err {
                            // An unknown name is operator input, not a
                            // system fault: exit 2, not the exit-1 fallback.
                            ConfigError::UnknownLibrary { .. } => eyre::Report::new(
                                bookrack_cli::error::BookrackCliError::LocalUserError {
                                    message: err.to_string(),
                                },
                            ),
                            other => eyre::Report::new(other),
                        }
                    })?;
                    if bookrack_cli::render::ctx().is_json() {
                        println!(
                            "{}",
                            serde_json::json!({
                                "ok": true,
                                "name": name,
                                "registry": registry_path.display().to_string(),
                            })
                        );
                    } else if !bookrack_cli::render::ctx().is_quiet() {
                        println!(
                            "default library set to '{name}' ({})",
                            registry_path.display()
                        );
                    }
                    // The write persists across restarts, but a set
                    // BOOKRACK_DATA_DIR outranks the registry default on
                    // the next resolve. Warn at the moment the mistaken
                    // expectation forms — CLI-local, no daemon involved.
                    let default_root = bookrack_config::find_library(&registry_path, &name)?
                        .map(|entry| entry.data_dir);
                    if !bookrack_cli::render::ctx().is_quiet()
                        && env_data_dir_shadows_default(
                            std::env::var(bookrack_config::DATA_DIR_ENV).ok(),
                            default_root.as_deref(),
                        )
                    {
                        eprintln!(
                            "note: {} is set and will shadow this default until it is unset",
                            bookrack_config::DATA_DIR_ENV
                        );
                    }
                    Ok(())
                }
                // `detect` / `scan` are read-only and resolve locally,
                // never touching a daemon.
                LibrariesAction::Detect { path } => bookrack_cli::libraries_local::detect(path),
                LibrariesAction::Scan {
                    parent,
                    volumes,
                    register,
                    kind,
                } => bookrack_cli::libraries_local::scan(
                    parent,
                    volumes,
                    register,
                    kind.map(Into::into),
                ),
                LibrariesAction::Add {
                    name,
                    path,
                    kind,
                    description,
                    new_uuid,
                    yes,
                } => bookrack_cli::libraries_local::add(
                    Some(name),
                    path,
                    kind.map(Into::into),
                    description,
                    new_uuid,
                    yes,
                ),
                LibrariesAction::Register {
                    path,
                    name,
                    kind,
                    description,
                    new_uuid,
                    yes,
                } => bookrack_cli::libraries_local::add(
                    name,
                    path,
                    kind.map(Into::into),
                    description,
                    new_uuid,
                    yes,
                ),
                LibrariesAction::Remove { name, purge, yes } => {
                    bookrack_cli::libraries_local::remove(name, purge, yes)
                }
                LibrariesAction::Config { name, sets, unset } => {
                    bookrack_cli::libraries_local::config(name, sets, unset)
                }
                other => cmd::cli_client::libraries::run(other, None).await,
            }
        }
        Command::Diagnose {
            out,
            days,
            no_scrub,
        } => cmd::cli_client::diagnose::run(out, days, no_scrub, None).await,
        Command::Ingest(args) => cmd::cli_client::ingest::run(args, None, audit_profile).await,
        Command::Intake { action } => {
            cmd::cli_client::intake::run(action, None, audit_profile, json_global).await
        }
        Command::Queue { action } => cmd::cli_client::queue::run(action, None).await,
        Command::Metadata { action } => {
            cmd::cli_client::metadata::run(action, None, audit_profile).await
        }
        Command::Vectors { action } => cmd::cli_client::vectors::run(action, None).await,
        Command::Corpus { action } => cmd::cli_client::corpus::run(action, None).await,
        Command::Stamps { action } => cmd::cli_client::stamps::run(action, None).await,
        Command::Remove(args) => cmd::cli_client::remove::run(args, None).await,
        Command::Papers { action } => {
            cmd::cli_client::papers::run(action, None, audit_profile).await
        }
        Command::Dryrun(args) => cmd::cli_client::dryrun::run(args, None, audit_profile).await,
        Command::Distill { action } => bookrack_cli::distill_cmd::run(&selection, action).await,
        Command::Runs { action } => bookrack_cli::runs_cmd::run(&selection, action),
        Command::Retrieval { action } => bookrack_cli::retrieval_cmd::run(&selection, action),
        Command::Logs(args) => cmd::cli_client::logs::run(args, None).await,
        Command::Quit => cmd::cli_client::quit::run(None).await,
        Command::Doctor { .. } => unreachable!("Doctor is dispatched above"),
        Command::Init { .. } => unreachable!("Init is dispatched above"),
        Command::Run { .. } => unreachable!("Run is dispatched above"),
        Command::Exec { .. } => unreachable!("Exec is dispatched above"),
    }
}

/// Closed white-list of the subcommands that consume the global
/// `--audit-profile` flag. Every other variant is rejected up front in
/// `main` so the flag cannot silently drop on a path that does not
/// thread it into the RPC params.
///
/// The match is exhaustive on purpose: when a new command joins the
/// audit-profile-aware set, the new variant fails to compile here
/// until its arm is added.
fn accepts_audit_profile(command: &Command) -> bool {
    use bookrack_cli_grammar::{
        IntakeAction, PapersAction, PapersMetadataAction, WriteMetadataAction,
    };
    match command {
        Command::Ingest(_) => true,
        Command::Intake { action } => matches!(action, IntakeAction::Ocr { .. }),
        Command::Dryrun(_) => true,
        Command::Metadata { action } => matches!(
            action,
            WriteMetadataAction::Reaudit { .. } | WriteMetadataAction::Advance { .. }
        ),
        Command::Papers { action } => matches!(
            action,
            PapersAction::Metadata {
                action: PapersMetadataAction::Reaudit { .. }
            }
        ),
        Command::AuditProfile { .. }
        | Command::IndexProfile { .. }
        | Command::Verify
        | Command::Libraries { .. }
        | Command::Diagnose { .. }
        | Command::Queue { .. }
        | Command::Vectors { .. }
        | Command::Corpus { .. }
        | Command::Stamps { .. }
        | Command::Remove(_)
        | Command::Distill { .. }
        | Command::Runs { .. }
        | Command::Retrieval { .. }
        | Command::Logs(_)
        | Command::Quit
        | Command::Doctor { .. }
        | Command::Init { .. }
        | Command::Run { .. }
        | Command::Exec { .. } => false,
    }
}

/// Whether a set `BOOKRACK_DATA_DIR` will shadow a freshly written
/// registry default on the next resolve. True when the variable is set
/// to a non-blank path other than `default_root` — the case where the
/// env root outranks the registry default and points somewhere else.
/// Pure over its inputs so the decision is tested without mutating the
/// process environment.
fn env_data_dir_shadows_default(env: Option<String>, default_root: Option<&Path>) -> bool {
    let Some(dir) = env.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
        return false;
    };
    match default_root {
        Some(root) => Path::new(&dir) != root,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn data_dir_and_library_are_mutually_exclusive() {
        let parsed = Cli::try_parse_from([
            "bookrack",
            "--data-dir",
            "/x",
            "--library",
            "test",
            "verify",
        ]);
        assert!(parsed.is_err(), "the two selectors must not be combined");
    }

    #[test]
    fn selection_carries_the_flags_through() {
        let cli = Cli::try_parse_from(["bookrack", "--library", "test", "verify"])
            .expect("a lone --library parses");
        let selection = cli.selection();
        assert_eq!(selection.library.as_deref(), Some("test"));
        assert!(selection.data_dir.is_none());
    }

    #[test]
    fn libraries_default_parses_the_name_argument() {
        let cli = Cli::try_parse_from(["bookrack", "libraries", "default", "prod"])
            .expect("`libraries default <name>` parses");
        match cli.command {
            Command::Libraries {
                action: LibrariesAction::Default { name },
            } => assert_eq!(name, "prod"),
            _ => panic!("expected `libraries default`"),
        }
    }

    #[test]
    fn libraries_default_requires_a_name() {
        let parsed = Cli::try_parse_from(["bookrack", "libraries", "default"]);
        assert!(parsed.is_err(), "the name argument is required");
    }

    #[test]
    fn env_data_dir_shadows_a_default_pointing_elsewhere() {
        assert!(env_data_dir_shadows_default(
            Some("/env/root".to_string()),
            Some(Path::new("/roots/prod")),
        ));
    }

    #[test]
    fn env_data_dir_does_not_shadow_when_unset() {
        assert!(!env_data_dir_shadows_default(
            None,
            Some(Path::new("/roots/prod"))
        ));
        // Whitespace-only counts as unset.
        assert!(!env_data_dir_shadows_default(
            Some("   ".to_string()),
            Some(Path::new("/roots/prod")),
        ));
    }

    #[test]
    fn env_data_dir_does_not_shadow_when_it_points_at_the_default_root() {
        assert!(!env_data_dir_shadows_default(
            Some("/roots/prod".to_string()),
            Some(Path::new("/roots/prod")),
        ));
    }

    #[test]
    fn libraries_add_parses_name_path_and_flags() {
        let cli = Cli::try_parse_from([
            "bookrack",
            "libraries",
            "add",
            "prod",
            "/roots/prod",
            "--kind",
            "test",
            "--new-uuid",
            "--yes",
        ])
        .expect("`libraries add` parses");
        match cli.command {
            Command::Libraries {
                action:
                    LibrariesAction::Add {
                        name,
                        path,
                        kind,
                        new_uuid,
                        yes,
                        ..
                    },
            } => {
                assert_eq!(name, "prod");
                assert_eq!(path, std::path::PathBuf::from("/roots/prod"));
                assert!(matches!(kind, Some(KindArg::Test)));
                assert!(new_uuid);
                assert!(yes);
            }
            _ => panic!("expected `libraries add`"),
        }
    }

    #[test]
    fn libraries_add_requires_name_and_path() {
        assert!(
            Cli::try_parse_from(["bookrack", "libraries", "add", "prod"]).is_err(),
            "add needs both a name and a path"
        );
    }

    #[test]
    fn libraries_register_takes_an_optional_alias() {
        let cli = Cli::try_parse_from([
            "bookrack",
            "libraries",
            "register",
            "/roots/prod",
            "--name",
            "alias",
        ])
        .expect("`libraries register` parses");
        match cli.command {
            Command::Libraries {
                action: LibrariesAction::Register { path, name, .. },
            } => {
                assert_eq!(path, std::path::PathBuf::from("/roots/prod"));
                assert_eq!(name.as_deref(), Some("alias"));
            }
            _ => panic!("expected `libraries register`"),
        }
    }

    #[test]
    fn libraries_remove_parses_purge_and_yes() {
        let cli = Cli::try_parse_from(["bookrack", "libraries", "remove", "prod", "--purge"])
            .expect("`libraries remove` parses");
        match cli.command {
            Command::Libraries {
                action: LibrariesAction::Remove { name, purge, yes },
            } => {
                assert_eq!(name, "prod");
                assert!(purge);
                assert!(!yes);
            }
            _ => panic!("expected `libraries remove`"),
        }
    }

    #[test]
    fn libraries_config_parses_sets_and_unsets() {
        let cli = Cli::try_parse_from([
            "bookrack",
            "libraries",
            "config",
            "prod",
            "embed_model=alt-model",
            "ollama_url=http://host:11434",
            "--unset",
            "mcp_addr",
            "--unset",
            "log_directive",
        ])
        .expect("`libraries config` parses");
        match cli.command {
            Command::Libraries {
                action: LibrariesAction::Config { name, sets, unset },
            } => {
                assert_eq!(name, "prod");
                assert_eq!(
                    sets,
                    vec![
                        ("embed_model".to_string(), "alt-model".to_string()),
                        ("ollama_url".to_string(), "http://host:11434".to_string()),
                    ]
                );
                assert_eq!(
                    unset,
                    vec!["mcp_addr".to_string(), "log_directive".to_string()]
                );
            }
            _ => panic!("expected `libraries config`"),
        }
    }

    #[test]
    fn libraries_config_prints_file_with_no_pairs() {
        let cli = Cli::try_parse_from(["bookrack", "libraries", "config", "prod"])
            .expect("`libraries config <name>` parses");
        match cli.command {
            Command::Libraries {
                action: LibrariesAction::Config { name, sets, unset },
            } => {
                assert_eq!(name, "prod");
                assert!(sets.is_empty());
                assert!(unset.is_empty());
            }
            _ => panic!("expected `libraries config`"),
        }
    }

    #[test]
    fn libraries_config_rejects_a_pair_without_equals() {
        for argv in [
            vec!["bookrack", "libraries", "config", "prod", "embed_model"],
            vec!["bookrack", "libraries", "config", "prod", "=value"],
            vec!["bookrack", "libraries", "config", "prod", "embed_model="],
        ] {
            let Err(err) = Cli::try_parse_from(argv.iter().copied()) else {
                panic!("a malformed pair must error: {argv:?}");
            };
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ValueValidation,
                "argv: {argv:?}"
            );
        }
    }

    #[test]
    fn index_profile_verbs_parse() {
        for argv in [
            vec!["bookrack", "index-profile", "list"],
            vec!["bookrack", "index-profile", "list", "--json"],
            vec!["bookrack", "index-profile", "show", "qwen3-0.6b-default"],
            vec![
                "bookrack",
                "index-profile",
                "validate",
                "qwen3-0.6b-default",
            ],
            vec![
                "bookrack",
                "index-profile",
                "validate",
                "x",
                "--allow-unknown-model",
            ],
        ] {
            Cli::try_parse_from(argv.iter().copied())
                .unwrap_or_else(|_| panic!("argv must parse: {argv:?}"));
        }
    }

    #[test]
    fn index_profile_allow_unknown_model_is_only_on_validate() {
        let Err(err) =
            Cli::try_parse_from(["bookrack", "index-profile", "list", "--allow-unknown-model"])
        else {
            panic!("--allow-unknown-model must not attach to list");
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn index_profile_current_parses_with_and_without_library() {
        // Without --library the handler falls back to the registry
        // default; the grammar must accept the bare form.
        for argv in [
            vec!["bookrack", "index-profile", "current"],
            vec!["bookrack", "index-profile", "current", "--json"],
            vec!["bookrack", "index-profile", "current", "--library", "x"],
        ] {
            let cli = Cli::try_parse_from(argv.iter().copied())
                .unwrap_or_else(|_| panic!("argv must parse: {argv:?}"));
            let Command::IndexProfile { action } = cli.command else {
                panic!("must parse as index-profile");
            };
            assert!(matches!(action, IndexProfileAction::Current { .. }));
        }
    }

    #[test]
    fn index_profile_diff_requires_two_names() {
        let cli = Cli::try_parse_from([
            "bookrack",
            "index-profile",
            "diff",
            "qwen3-0.6b-default",
            "qwen3-4b-quality",
        ])
        .expect("two names parse");
        let Command::IndexProfile {
            action: IndexProfileAction::Diff { a, b, json },
        } = cli.command
        else {
            panic!("must parse as diff");
        };
        assert_eq!(a, "qwen3-0.6b-default");
        assert_eq!(b, "qwen3-4b-quality");
        assert!(!json);

        let Err(err) = Cli::try_parse_from(["bookrack", "index-profile", "diff", "only-one"])
        else {
            panic!("diff must require both names");
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn index_profile_apply_grammar_covers_defaults_and_pipeline_enum() {
        use bookrack_runtime::cmd::index_profile::PipelineFilter;

        // Bare form: every flag at its default.
        let cli = Cli::try_parse_from(["bookrack", "index-profile", "apply", "some-profile"])
            .expect("bare apply parses");
        let Command::IndexProfile {
            action:
                IndexProfileAction::Apply {
                    name,
                    library,
                    pipeline,
                    dry_run,
                    yes,
                },
        } = cli.command
        else {
            panic!("must parse as apply");
        };
        assert_eq!(name, "some-profile");
        assert!(library.is_none());
        assert_eq!(pipeline, PipelineFilter::All);
        assert!(!dry_run);
        assert!(!yes);

        // Full form with every flag set.
        let cli = Cli::try_parse_from([
            "bookrack",
            "index-profile",
            "apply",
            "some-profile",
            "--library",
            "x",
            "--pipeline",
            "papers",
            "--dry-run",
            "--yes",
        ])
        .expect("full apply parses");
        let Command::IndexProfile {
            action:
                IndexProfileAction::Apply {
                    library,
                    pipeline,
                    dry_run,
                    yes,
                    ..
                },
        } = cli.command
        else {
            panic!("must parse as apply");
        };
        assert_eq!(library.as_deref(), Some("x"));
        assert_eq!(pipeline, PipelineFilter::Papers);
        assert!(dry_run);
        assert!(yes);

        // The pipeline filter is a closed enum and the name is required.
        assert!(
            Cli::try_parse_from([
                "bookrack",
                "index-profile",
                "apply",
                "p",
                "--pipeline",
                "everything"
            ])
            .is_err()
        );
        let Err(err) = Cli::try_parse_from(["bookrack", "index-profile", "apply"]) else {
            panic!("apply must require a profile name");
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn libraries_scan_accepts_register_and_kind() {
        let cli = Cli::try_parse_from([
            "bookrack",
            "libraries",
            "scan",
            "/parent",
            "--register",
            "--kind",
            "prod",
        ])
        .expect("`libraries scan --register` parses");
        match cli.command {
            Command::Libraries {
                action: LibrariesAction::Scan { register, kind, .. },
            } => {
                assert!(register);
                assert!(matches!(kind, Some(KindArg::Prod)));
            }
            _ => panic!("expected `libraries scan`"),
        }
    }

    #[test]
    fn accepts_audit_profile_white_list_matches_consumers() {
        let consumers = [
            vec!["bookrack", "ingest", "/tmp/x.epub"],
            vec![
                "bookrack",
                "intake",
                "ocr",
                "/tmp/x.md",
                "--from-pdf",
                "/tmp/x.pdf",
            ],
            vec!["bookrack", "dryrun", "/tmp/x.epub"],
            vec!["bookrack", "metadata", "reaudit", "1"],
            vec!["bookrack", "metadata", "advance", "1"],
            vec!["bookrack", "papers", "metadata", "reaudit", "1"],
        ];
        for argv in consumers {
            let cli = Cli::try_parse_from(argv.clone()).expect("argv parses");
            assert!(
                accepts_audit_profile(&cli.command),
                "expected {argv:?} to consume --audit-profile",
            );
        }
    }

    #[test]
    fn accepts_audit_profile_rejects_unrelated_commands() {
        // Non-audit subcommands must be rejected up front when the
        // global flag is set, so the value cannot silently drop.
        let outsiders = [
            vec!["bookrack", "verify"],
            vec!["bookrack", "metadata", "set", "1", "title", "x"],
            vec!["bookrack", "metadata", "approve", "1"],
            vec!["bookrack", "queue", "list"],
            vec!["bookrack", "vectors", "rebuild"],
            vec!["bookrack", "papers", "list"],
            vec!["bookrack", "logs", "--tail", "5"],
        ];
        for argv in outsiders {
            let cli = Cli::try_parse_from(argv.clone()).expect("argv parses");
            assert!(
                !accepts_audit_profile(&cli.command),
                "did not expect {argv:?} to consume --audit-profile",
            );
        }
    }

    #[test]
    fn metadata_write_subcommands_parse_through_cli() {
        for argv in [
            vec!["bookrack", "metadata", "set", "1", "title", "A New Title"],
            vec!["bookrack", "metadata", "set", "1", "pub_place", "New York"],
            vec!["bookrack", "metadata", "set", "1", "original_year", "1949"],
            vec!["bookrack", "metadata", "clear", "1", "title"],
            vec!["bookrack", "metadata", "ack", "1", "--reason", "test"],
            vec!["bookrack", "metadata", "approve", "1"],
            vec![
                "bookrack", "metadata", "approve", "1", "--reason", "verified",
            ],
            vec![
                "bookrack",
                "metadata",
                "reject",
                "1",
                "--reason",
                "wrong file",
            ],
            vec!["bookrack", "metadata", "advance", "1"],
        ] {
            Cli::try_parse_from(argv.iter().copied())
                .unwrap_or_else(|_| panic!("argv must parse: {argv:?}"));
        }
    }

    #[test]
    fn ingest_accepts_hold_for_metadata_flag() {
        Cli::try_parse_from(["bookrack", "ingest", "/x/book.epub", "--hold-for-metadata"])
            .expect("the flag parses");
    }

    #[test]
    fn ingest_accepts_priority_flag() {
        Cli::try_parse_from(["bookrack", "ingest", "/x/book.epub", "--priority", "high"])
            .expect("the flag parses");
    }

    #[test]
    fn intake_ocr_accepts_priority_force_hold_flags() {
        Cli::try_parse_from([
            "bookrack",
            "intake",
            "ocr",
            "/x/out.md",
            "--from-pdf",
            "/x/scan.pdf",
            "--force",
            "--hold-for-metadata",
            "--priority",
            "normal",
        ])
        .expect("the flags parse");
    }

    #[test]
    fn dryrun_subcommand_parses() {
        for argv in [
            vec!["bookrack", "dryrun", "/x"],
            vec!["bookrack", "dryrun", "/x", "--stdout"],
            vec!["bookrack", "dryrun", "/x", "--no-chunk"],
            vec!["bookrack", "dryrun", "/x", "--out", "/tmp/r.jsonl"],
        ] {
            Cli::try_parse_from(argv.iter().copied())
                .unwrap_or_else(|_| panic!("argv must parse: {argv:?}"));
        }
    }

    // The behavioural coverage of `run_metadata_*` lives in
    // `crates/ops/tests/metadata_writes.rs`, where the logic itself sits.
    // The CLI handlers here are thin shells that hand the request off to
    // `bookrack_ops::writes::metadata::*`.

    #[test]
    fn natural_name_hints_cover_the_common_typos_from_the_test_report() {
        for (typed, expected) in [
            ("list", "`bookrack exec library.list_books`"),
            ("ls", "`bookrack exec library.list_books`"),
            ("find", "`bookrack exec library.find_books`"),
            ("show", "`bookrack exec library.show_book`"),
            ("stats", "`bookrack exec library.stats`"),
            ("search", "`bookrack exec library.search`"),
        ] {
            assert_eq!(natural_name_hint(typed).as_deref(), Some(expected));
        }

        // `status` is ambiguous between library-level and per-book; the
        // hint surfaces both so the user picks.
        let status = natural_name_hint("status").expect("status maps");
        assert!(status.contains("`bookrack exec library.info`"));
        assert!(status.contains("`bookrack exec library.stats`"));
        assert!(status.contains(" or "));

        // Tokens not in the table fall through to clap's similarity tip;
        // returning None is how we signal that.
        assert_eq!(natural_name_hint("nope"), None);
        assert_eq!(natural_name_hint(""), None);
    }

    #[test]
    fn remove_subcommand_parses() {
        for argv in [
            vec!["bookrack", "remove", "42"],
            vec!["bookrack", "remove", "42", "--dry-run"],
            vec!["bookrack", "remove", "42", "--yes"],
            vec!["bookrack", "remove", "--sha", "deadbeef"],
            vec!["bookrack", "remove", "--sha", "deadbeef", "--dry-run"],
            vec!["bookrack", "papers", "remove", "42"],
            vec!["bookrack", "papers", "remove", "--sha", "deadbeef", "--yes"],
        ] {
            Cli::try_parse_from(argv.iter().copied())
                .unwrap_or_else(|_| panic!("argv must parse: {argv:?}"));
        }
    }

    #[test]
    fn remove_requires_a_locator() {
        for argv in [
            vec!["bookrack", "remove"],
            vec!["bookrack", "remove", "--dry-run"],
            vec!["bookrack", "papers", "remove"],
            vec!["bookrack", "papers", "remove", "--dry-run"],
        ] {
            let Err(err) = Cli::try_parse_from(argv.iter().copied()) else {
                panic!("an empty selector must error: {argv:?}");
            };
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "argv: {argv:?}"
            );
        }
    }

    #[test]
    fn vectors_drop_accepts_yes_flag() {
        for argv in [
            vec!["bookrack", "vectors", "drop"],
            vec!["bookrack", "vectors", "drop", "--yes"],
            vec!["bookrack", "papers", "vectors", "drop"],
            vec!["bookrack", "papers", "vectors", "drop", "--yes"],
        ] {
            Cli::try_parse_from(argv.iter().copied())
                .unwrap_or_else(|_| panic!("argv must parse: {argv:?}"));
        }
    }

    #[test]
    fn verify_short_circuits_on_an_uninitialised_data_dir() {
        // A freshly mkdir'd data directory has no catalog.db on disk
        // yet. Verify must NOT try to open one (that would create an
        // empty file and then fail schema verification), and must NOT
        // open corpus.db either (that would write tables into a store
        // verify is supposed to only read).
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = Config::new(tmp.path().to_path_buf(), "http://localhost:0".to_string());
        let report = bookrack_runtime::cmd::verify::build_verify_report(&cfg);
        assert!(report.not_initialised);
        assert!(!report.catalog_schema_ok);
        assert!(report.catalog_schema_error.is_none());
        assert!(!report.corpus_schema_ok);
        assert!(report.corpus_schema_error.is_none());
        assert!(report.vectors_built_at_chunk_count.is_none());
        // The data dir is unchanged — no stores were created as a
        // side effect of the verify call.
        assert!(!cfg.catalog_db().exists());
        assert!(!cfg.corpus_db().exists());
    }

    #[test]
    fn remove_rejects_both_intake_id_and_sha_together() {
        // The `--sha` and positional id select the same target two
        // different ways; supplying both is a user error.
        for argv in [
            vec!["bookrack", "remove", "42", "--sha", "abc"],
            vec!["bookrack", "papers", "remove", "42", "--sha", "abc"],
        ] {
            let Err(err) = Cli::try_parse_from(argv.iter().copied()) else {
                panic!("the two selectors must not be combined: {argv:?}");
            };
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "argv: {argv:?}"
            );
        }
    }

    #[test]
    fn invalid_subcommand_token_extracts_the_offending_string() {
        let Err(err) = Cli::try_parse_from(["bookrack", "list"]) else {
            panic!("`list` is not a valid subcommand and must error");
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
        assert_eq!(invalid_subcommand_token(&err).as_deref(), Some("list"));
    }

    #[test]
    fn diagnose_subcommand_parses() {
        for argv in [
            vec!["bookrack", "diagnose"],
            vec!["bookrack", "diagnose", "--days", "14"],
            vec!["bookrack", "diagnose", "--no-scrub"],
            vec!["bookrack", "diagnose", "--out", "/tmp/d.tar.gz"],
            vec![
                "bookrack",
                "diagnose",
                "--out",
                "/tmp/d.tar.gz",
                "--days",
                "3",
                "--no-scrub",
            ],
        ] {
            Cli::try_parse_from(argv.iter().copied())
                .unwrap_or_else(|_| panic!("argv must parse: {argv:?}"));
        }
    }

    #[test]
    fn diagnose_default_days_is_seven() {
        let cli = Cli::try_parse_from(["bookrack", "diagnose"]).expect("parse");
        let Command::Diagnose { days, no_scrub, .. } = cli.command else {
            panic!("expected the Diagnose variant");
        };
        assert_eq!(days, 7);
        assert!(!no_scrub);
    }

    #[test]
    fn diagnose_emits_a_bundle_at_the_explicit_out_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path().to_path_buf();
        std::fs::create_dir_all(data_dir.join("logs")).unwrap();
        let cfg = Config::new(data_dir, "http://localhost:0/".to_string());
        let out = tmp.path().join("custom.tar.gz");
        bookrack_runtime::cmd::diagnose::run(&cfg, Some(out.clone()), 7, false).expect("collect");
        assert!(out.exists());
    }
}
