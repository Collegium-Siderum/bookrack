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
mod run;
mod util;

use std::path::PathBuf;

use anyhow::{Context, Result};
use bookrack_config::{Config, ConfigError, LibrarySelection};
use bookrack_repl_grammar::{
    CorpusAction, DryrunArgs, IngestArgs, IntakeAction, PapersAction, QueueAction, RemoveArgs,
    ReplCli, ReplCommand, StampsAction, WriteMetadataAction, WriteVectorsAction,
};
use bookrack_runtime::cmd::audit_profile::AuditProfileAction;
use bookrack_runtime::cmd::libraries::CopyMode;

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
library.<tool>`. Run `bookrack run` to start a session, then ask the live
MCP server for its tool surface with `bookrack exec tools`.

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
    /// Operate on the library at this data root, overriding the
    /// environment. Mutually exclusive with `--library`.
    #[arg(long, global = true, conflicts_with = "library")]
    data_dir: Option<PathBuf>,
    /// Operate on the named library from the registry (see
    /// BOOKRACK_REGISTRY). Mutually exclusive with `--data-dir`.
    #[arg(long, global = true)]
    library: Option<String>,
    /// Select an audit profile by name. Built-in names are
    /// `default`, `trust-source`, and `strict`. Without this flag the
    /// `<data_root>/audit-rules/audit_profile.local.toml` overlay is
    /// merged onto the shipped default; with it the overlay is
    /// bypassed and the named preset wins.
    #[arg(long, global = true, value_name = "NAME")]
    audit_profile: Option<String>,
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
        /// One-release transition: re-enable the in-process reedline
        /// REPL. Default behaviour is the silent daemon; this flag
        /// exists only to give CI scripts that fed REPL via stdin a
        /// window to migrate to `bookrack repl`.
        #[arg(long, hide = true)]
        legacy_repl: bool,
    },
    /// Open an interactive REPL against the running daemon.
    ///
    /// Connects to the daemon over its control socket and dispatches each
    /// command as a control-plane RPC; the REPL process does not hold the
    /// session lock and does not read the data root directly. In non-TTY
    /// mode (stdin piped) every line is parsed and dispatched in sequence,
    /// and the process exits non-zero on the first failure.
    Repl {
        /// Override the runtime directory used to discover the
        /// daemon's control socket. Falls back to
        /// `BOOKRACK_RUNTIME_DIR` or the platform default.
        #[arg(long, value_name = "PATH")]
        runtime_dir: Option<PathBuf>,
    },
    /// Call MCP tools against the running session.
    ///
    /// Subcommands:
    ///   `info` (default)          — print the session pid and MCP
    ///                               address. Pure file read of the
    ///                               session lock; never makes an HTTP
    ///                               call.
    ///   `tools`                   — open an MCP client and run
    ///                               `tools/list` against the live
    ///                               server.
    ///   `library.<tool> [<json>]` — call the named MCP tool, with the
    ///                               second positional token forwarded
    ///                               verbatim as JSON arguments.
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
        /// Emit machine-readable JSON instead of the human listing.
        #[arg(long)]
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
        /// With `--rename-envelopes`, list the rename plan without
        /// touching the disk.
        #[arg(long, requires = "rename_envelopes")]
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
}

#[derive(clap::Subcommand, Debug)]
pub(crate) enum LibrariesAction {
    /// List every entry in the registry.
    ///
    /// Marks the `default = "..."` fallback when one is set.
    List {
        /// Emit machine-readable JSON instead of the human listing.
        #[arg(long)]
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
    /// Move the registry's default-library pointer to `name`.
    ///
    /// The change lives in the daemon's in-memory registry only; the
    /// on-disk library registry stays as written.
    Default {
        /// Library short name to set as the daemon's default.
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
        return Err(anyhow::Error::new(err));
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
        return Err(anyhow::Error::new(err));
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
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = parse_cli_with_natural_name_hints();

    // `doctor` resolves on its own — it has a daemon-running path
    // (control plane) and a daemon-not-running fallback that probes
    // the data root directly without going through `Config::resolve`,
    // so an unconfigured install surfaces as a row instead of an
    // opaque resolver bail.
    if let Command::Doctor {
        json,
        install_pdfium,
        rename_envelopes,
        dry_run,
    } = &cli.command
    {
        return cmd::cli_client::doctor::run(
            &cli.selection(),
            *json,
            *install_pdfium,
            *rename_envelopes,
            *dry_run,
            None,
        )
        .await;
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
        legacy_repl,
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
                other => return Err(anyhow::Error::new(other).context("resolve configuration")),
            }
        }
        return run::run_daemon(run::RunOpts {
            selection,
            mcp_addr: *mcp_addr,
            no_mcp: *no_mcp,
            runtime_dir: runtime_dir.clone(),
            legacy_repl: *legacy_repl,
        })
        .await;
    }

    // `repl` is the standalone control-socket client. It needs the
    // session-lock directory to discover the daemon address but does
    // not open any local database, so it dispatches before
    // `Config::resolve` like `run` and `exec`.
    if let Command::Repl { runtime_dir } = &cli.command {
        return cmd::repl_client::run(runtime_dir.clone()).await;
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
    let _profile_name = cli.audit_profile.clone();
    match cli.command {
        Command::AuditProfile { action } => bookrack_runtime::cmd::audit_profile::run(action),
        Command::Verify => cmd::cli_client::verify::run(None).await,
        Command::Libraries { action } => cmd::cli_client::libraries::run(action, None).await,
        Command::Diagnose {
            out,
            days,
            no_scrub,
        } => cmd::cli_client::diagnose::run(out, days, no_scrub, None).await,
        Command::Ingest(args) => cmd::cli_client::ingest::run(args, None).await,
        Command::Intake { action } => cmd::cli_client::intake::run(action, None).await,
        Command::Queue { action } => cmd::cli_client::queue::run(action, None).await,
        Command::Metadata { action } => cmd::cli_client::metadata::run(action, None).await,
        Command::Vectors { action } => cmd::cli_client::vectors::run(action, None).await,
        Command::Corpus { action } => cmd::cli_client::corpus::run(action, None).await,
        Command::Stamps { action } => cmd::cli_client::stamps::run(action, None).await,
        Command::Remove(args) => cmd::cli_client::remove::run(args, None).await,
        Command::Papers { action } => cmd::cli_client::papers::run(action, None).await,
        Command::Dryrun(args) => cmd::cli_client::dryrun::run(args, None).await,
        Command::Quit => cmd::cli_client::quit::run(None).await,
        Command::Doctor { .. } => unreachable!("Doctor is dispatched above"),
        Command::Init { .. } => unreachable!("Init is dispatched above"),
        Command::Run { .. } => unreachable!("Run is dispatched above"),
        Command::Repl { .. } => unreachable!("Repl is dispatched above"),
        Command::Exec { .. } => unreachable!("Exec is dispatched above"),
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
    fn metadata_write_subcommands_parse_through_repl() {
        // The write-side metadata surface lives inside `bookrack run`
        // and is parsed against the REPL grammar with no binary prefix.
        for argv in [
            vec!["metadata", "set", "1", "title", "A New Title"],
            vec!["metadata", "set", "1", "pub_place", "New York"],
            vec!["metadata", "set", "1", "original_year", "1949"],
            vec!["metadata", "clear", "1", "title"],
            vec!["metadata", "ack", "1", "--reason", "test"],
            vec!["metadata", "approve", "1"],
            vec!["metadata", "approve", "1", "--reason", "verified"],
            vec!["metadata", "reject", "1", "--reason", "wrong file"],
            vec!["metadata", "advance", "1"],
        ] {
            ReplCli::try_parse_from(argv.iter().copied())
                .unwrap_or_else(|_| panic!("argv must parse via ReplCli: {argv:?}"));
        }
    }

    #[test]
    fn ingest_accepts_hold_for_metadata_flag_in_repl() {
        ReplCli::try_parse_from(["ingest", "/x/book.epub", "--hold-for-metadata"])
            .expect("the flag parses via ReplCli");
    }

    #[test]
    fn dryrun_subcommand_parses_through_repl() {
        for argv in [
            vec!["dryrun", "/x"],
            vec!["dryrun", "/x", "--stdout"],
            vec!["dryrun", "/x", "--no-chunk"],
            vec!["dryrun", "/x", "--out", "/tmp/r.jsonl"],
        ] {
            ReplCli::try_parse_from(argv.iter().copied())
                .unwrap_or_else(|_| panic!("argv must parse via ReplCli: {argv:?}"));
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
    fn remove_subcommand_parses_through_repl() {
        // `remove` is REPL-only after C3a; positional intake id, --sha
        // alternative, and the destructive toggles must all parse
        // against the REPL grammar.
        for argv in [
            vec!["remove", "42"],
            vec!["remove", "42", "--dry-run"],
            vec!["remove", "42", "--yes"],
            vec!["remove", "--sha", "deadbeef"],
            vec!["remove", "--sha", "deadbeef", "--dry-run"],
        ] {
            ReplCli::try_parse_from(argv.iter().copied())
                .unwrap_or_else(|_| panic!("argv must parse via ReplCli: {argv:?}"));
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
    fn remove_rejects_both_intake_id_and_sha_together_in_repl() {
        // The `--sha` and positional id select the same target two
        // different ways; supplying both is a user error.
        let Err(err) = ReplCli::try_parse_from(["remove", "42", "--sha", "abc"]) else {
            panic!("the two selectors must not be combined");
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
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
