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

mod audit_helpers;
mod cmd;
mod doctor;
mod embed_helpers;
mod exec;
mod init;
mod ops_helpers;
mod render;
mod run;
mod util;

use std::path::PathBuf;

use anyhow::{Context, Result};
use bookrack_config::{Config, LibrarySelection, LogConfig};

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
    /// Inspect and compare audit profiles. Pure reflection over the
    /// profiles compiled into the binary — no library, no MCP session.
    AuditProfile {
        #[command(subcommand)]
        action: AuditProfileAction,
    },
    /// Verify the catalog and corpus schemas against the binary's
    /// TableSpecs and tally the cross-store counts (catalog intakes,
    /// vectors-meta chunk count, intake-file existence on disk).
    Verify,
    /// Inspect the library registry — the file named by
    /// `BOOKRACK_REGISTRY` that maps short names to data roots.
    Libraries {
        #[command(subcommand)]
        action: LibrariesAction,
    },
    /// Bundle the data root's crash reports, recent logs, and a small
    /// catalog snapshot into a scrubbed `.tar.gz` for a bug report.
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
    /// Start the daemon-REPL session: warm the library registry,
    /// acquire the machine-wide session lock, and serve MCP over
    /// streamable HTTP for the lifetime of the terminal. The
    /// foreground task idles until a shutdown signal arrives (Ctrl-C,
    /// SIGTERM, SIGHUP, or — in a later phase — the REPL's `exit`).
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
    /// Reach the running bookrack session over MCP. Subcommands:
    ///   `info` (default)        — print the session pid + MCP address.
    ///                             Pure file read of the session lock;
    ///                             never makes an HTTP call.
    ///   `tools`                  — open an MCP client and run
    ///                             `tools/list` against the live server.
    ///   `library.<tool> [<json>]` — call the named MCP tool, with the
    ///                             second positional token forwarded
    ///                             verbatim as JSON arguments.
    /// Reads `${BOOKRACK_RUNTIME_DIR}/bookrack.tty.lock` to discover
    /// the session; never opens a catalog, corpus, or vector store.
    Exec {
        /// Subcommand and its positional arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run a one-screen health check: data root resolution, schema
    /// versions, PDFium library presence, Ollama daemon reachability,
    /// and whether the configured embed model is pulled. Exits with a
    /// non-zero status when any row fails, so a script can branch on the
    /// result.
    Doctor {
        /// Emit machine-readable JSON instead of the human listing.
        #[arg(long)]
        json: bool,
    },
    /// Walk the operator through a five-step install: pick a data root,
    /// check the PDFium library, probe Ollama, smoke-test the
    /// ingest -> embed -> query pipeline end-to-end in a tempdir, and
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
pub(crate) enum IntakeAction {
    /// Bring an OCR product into the library as a derived source
    /// manifestation. The scan PDF named by `--from-pdf` is registered
    /// as the durable source anchor (status `needs_ocr`); the OCR
    /// markdown is registered as its own intake whose `Provenance`
    /// forensically references the PDF's hash and flows through
    /// STRUCTURE / CHUNK / EMBED. The expected page count comes from
    /// the source PDF's `/Pages`; pass `--expected-pages` to override
    /// it when PDFium cannot read the source, and `--allow-partial`
    /// to accept an OCR product whose sheets do not cover every page.
    Ocr {
        /// Path to the polyocr single-file Markdown product, with
        /// page markers `<!-- page <label> (sheet <n>) -->`.
        ocr_md: PathBuf,
        /// Path to the scan PDF the OCR product was produced from.
        #[arg(long, value_name = "PDF")]
        from_pdf: PathBuf,
        /// Override the expected page count rather than reading it
        /// from the source PDF's `/Pages`.
        #[arg(long, value_name = "N")]
        expected_pages: Option<u32>,
        /// Accept a partial OCR product. The present sheets are
        /// recorded into `Provenance.partial_pages`; missing pages
        /// surface in the OCR intake's `partial_pages` field rather
        /// than being silently treated as blank.
        #[arg(long)]
        allow_partial: bool,
    },
}

#[derive(clap::Subcommand, Debug)]
pub(crate) enum LibrariesAction {
    /// List every entry in the registry, marking the `default = "..."`
    /// fallback when one is set.
    List {
        /// Emit machine-readable JSON instead of the human listing.
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Subcommand, Debug)]
pub(crate) enum StampsAction {
    /// Probe the embedder for its vector dimension and write the
    /// resulting stamps into `corpus.db`'s `index_meta` when the table
    /// is unstamped. Fails on a stamp mismatch — the operator can then
    /// decide whether to rebuild.
    Reconcile,
}

#[derive(clap::Subcommand, Debug)]
pub(crate) enum WriteVectorsAction {
    /// Build or rebuild the ANN index from explicit parameters. Without
    /// any flag, reads the persisted config from `vectors_meta.json` and
    /// rebuilds from that — useful after corpus growth has exceeded the
    /// L2 churn threshold.
    Rebuild {
        /// IVF family — `ivf-flat`, `ivf-sq`, `ivf-pq`, `ivf-hnsw-flat`,
        /// `ivf-hnsw-sq`, `ivf-hnsw-pq`. Defaults to whatever the meta
        /// holds, or `ivf-flat` for a fresh library.
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        num_partitions: Option<u32>,
        #[arg(long)]
        num_sub_vectors: Option<u32>,
        #[arg(long)]
        num_bits: Option<u32>,
        #[arg(long)]
        nprobes: Option<u32>,
        #[arg(long)]
        refine_factor: Option<u32>,
    },
    /// Drop the ANN index and mark the meta as brute-force. Search
    /// falls back to a full scan on the next query.
    Drop,
    /// Re-embed every (or a single) book's chunks in place: read the
    /// existing chunk rows back from LanceDB, drop their vectors, run
    /// them back through the active embedder, and write them as the
    /// new vectors. Use after switching `embed_model` or `embed_dim`.
    Reembed {
        /// Restrict the reembed to one intake id. Without this flag,
        /// every intake currently in the `Embedded` state is reembedded.
        #[arg(long, value_name = "INTAKE_ID")]
        book: Option<i64>,
        /// Restrict the reembed to intakes whose stored
        /// `extractor_version` does not equal this binary's
        /// `bookrack_extract::EXTRACTOR_VERSION`. Combines with `--book`
        /// by intersection.
        #[arg(long)]
        stale_only: bool,
        /// Print the plan (per-book chunk counts) and exit without
        /// writing.
        #[arg(long)]
        dry_run: bool,
        /// Skip the destructive-action confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(clap::Subcommand, Debug)]
pub(crate) enum CorpusAction {
    /// Rebuild `corpus.db` from the v1 extraction envelopes recorded in
    /// the opaque store. Intakes whose envelope is missing, mismatched,
    /// or corrupt are reported but skipped.
    Rebuild {
        /// After the corpus tree is rebuilt, also re-embed every
        /// reembedded book's chunks. Without this flag the LanceDB
        /// chunks table is left as-is — search still works because
        /// node ids are deterministic, but the vectors are unchanged.
        #[arg(long)]
        include_vectors: bool,
        /// Restrict the rebuild to one intake id. Without this flag,
        /// every intake whose lifecycle is past `Extracted` is rebuilt.
        #[arg(long, value_name = "INTAKE_ID")]
        book: Option<i64>,
        /// Restrict the rebuild to intakes whose stored
        /// `extractor_version` does not equal this binary's
        /// `bookrack_extract::EXTRACTOR_VERSION`. Combines with `--book`
        /// by intersection.
        #[arg(long)]
        stale_only: bool,
        /// Print the per-intake classification and exit without writing.
        #[arg(long)]
        dry_run: bool,
        /// Skip the destructive-action confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(clap::Subcommand, Debug)]
pub(crate) enum WriteMetadataAction {
    /// Set (or change) one metadata field's value.
    Set {
        /// The intake id of the book.
        book: i64,
        /// The field column on `node_publication_attrs` to write
        /// (e.g. `title`, `publisher`, `year`, `language`).
        field: String,
        /// The new value.
        value: String,
    },
    /// Clear an override, falling back to the extracted base value.
    Clear {
        /// The intake id of the book.
        book: i64,
        /// The field whose override is removed.
        field: String,
    },
    /// Acknowledge a metadata gap and let the book through, signing
    /// the override with a reason for the audit trail.
    Ack {
        /// The intake id of the book.
        book: i64,
        /// Why the gap was accepted.
        #[arg(long)]
        reason: String,
    },
    /// Mark the record reviewed and correct. A human or LLM uses this
    /// after confirming the metadata; the pipeline never writes this
    /// status itself.
    Approve {
        /// The intake id of the book.
        book: i64,
        /// Optional note for the audit trail.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Reject the book outright (e.g. wrong source file, irrecoverable
    /// metadata). The book stays ingested but downstream consumers can
    /// filter on the rejected status.
    Reject {
        /// The intake id of the book.
        book: i64,
        /// Why the book was rejected.
        #[arg(long)]
        reason: String,
    },
    /// Resume CHUNK→EMBED for a book held at the metadata gate.
    Advance {
        /// The intake id of the book.
        book: i64,
    },
}

#[derive(clap::Subcommand, Debug)]
pub(crate) enum AuditProfileAction {
    /// Print every built-in profile name, one per line.
    List {
        /// Emit machine-readable JSON instead of the plain listing.
        #[arg(long)]
        json: bool,
    },
    /// Pretty-print the effective toggle settings for a named profile.
    Show {
        /// Built-in profile name (`default`, `trust-source`, `strict`).
        name: String,
    },
    /// List the sub-section names that differ between two named profiles
    /// and pretty-print each side's settings for those sections.
    Diff {
        /// First profile name.
        a: String,
        /// Second profile name.
        b: String,
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

    // `doctor` runs before `Config::resolve` so an unconfigured install
    // surfaces as a row in its report instead of a hard error from the
    // resolver -- the very diagnosis the operator needs. `init` runs
    // before resolve for the same reason: it is the wizard that turns
    // an unconfigured install into a configured one.
    if let Command::Doctor { json } = &cli.command {
        return doctor::run(&cli.selection(), *json).await;
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
        return run::run_daemon(run::RunOpts {
            selection: cli.selection(),
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

    let cfg = Config::resolve(&cli.selection()).context("resolve configuration")?;
    let (_guard, _log_stream) = bookrack_obs::init(&cfg, &LogConfig::from_env());

    let _profile_name = cli.audit_profile.clone();
    match cli.command {
        Command::AuditProfile { action } => cmd::audit_profile::run(action),
        Command::Verify => cmd::verify::run(&cfg),
        Command::Libraries { action } => match action {
            LibrariesAction::List { json } => cmd::libraries::list(json),
        },
        Command::Diagnose {
            out,
            days,
            no_scrub,
        } => cmd::diagnose::run(&cfg, out, days, no_scrub),
        Command::Doctor { .. } => unreachable!("Doctor is dispatched before Config::resolve"),
        Command::Init { .. } => unreachable!("Init is dispatched before Config::resolve"),
        Command::Run { .. } => unreachable!("Run is dispatched before Config::resolve"),
        Command::Exec { .. } => unreachable!("Exec is dispatched before Config::resolve"),
    }
}

/// Inside-`bookrack run` command grammar. Hosts every write command
/// that was removed from the external [`Cli`]. The REPL fallback in
/// `crate::run` parses tokens through this grammar; dispatch maps each
/// variant to the matching `cmd::*` runner.
#[derive(clap::Parser, Debug)]
#[command(name = "", no_binary_name = true)]
pub(crate) struct ReplCli {
    #[command(subcommand)]
    pub(crate) command: ReplCommand,
}

#[derive(clap::Subcommand, Debug)]
pub(crate) enum ReplCommand {
    /// Ingest and embed a single file (or, with `--recursive`, every
    /// supported file under a directory) into the library. Inside the
    /// REPL this runs synchronously; queue an entire directory through
    /// the queue worker with `queue add <path>` instead.
    Ingest {
        path: PathBuf,
        #[arg(long)]
        recursive: bool,
        #[arg(long)]
        hold_for_metadata: bool,
        #[arg(long)]
        force: bool,
    },
    /// Drive an intake from a derived source manifestation.
    Intake {
        #[command(subcommand)]
        action: IntakeAction,
    },
    /// Edit one book's metadata: set / clear / ack / approve / reject
    /// / advance.
    Metadata {
        #[command(subcommand)]
        action: WriteMetadataAction,
    },
    /// Vector-store writes: ANN rebuild, brute-force drop, re-embed.
    Vectors {
        #[command(subcommand)]
        action: WriteVectorsAction,
    },
    /// Corpus rebuild from the opaque envelopes.
    Corpus {
        #[command(subcommand)]
        action: CorpusAction,
    },
    /// Reconcile `corpus.db` index_meta stamps.
    Stamps {
        #[command(subcommand)]
        action: StampsAction,
    },
    /// Drop a book from every store.
    Remove {
        intake_id: Option<i64>,
        #[arg(long, conflicts_with = "intake_id", value_name = "HEX")]
        sha: Option<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Simulate an ingest up to (but not including) embedding, and write
    /// a JSON report of what the metadata audit would have produced. The
    /// real catalog, corpus, and vector store are not touched.
    Dryrun {
        /// Source file, or a directory the dryrun walks recursively.
        path: PathBuf,
        /// Write the per-book report to this path instead of the default
        /// `<data_root>/dryruns/...` location. Implies the summary is
        /// written alongside with a `.summary.json` suffix.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Write JSONL to stdout instead of to a file. The summary still
        /// lands on stderr at the end of the run.
        #[arg(long)]
        stdout: bool,
        /// Skip the CHUNK step. Saves seconds per large book when only
        /// the audit verdict is wanted.
        #[arg(long)]
        no_chunk: bool,
    },
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
        let report = crate::cmd::verify::build_verify_report(&cfg);
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
        crate::cmd::diagnose::run(&cfg, Some(out.clone()), 7, false).expect("collect");
        assert!(out.exists());
    }
}
