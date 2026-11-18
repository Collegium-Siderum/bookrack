//! `bookrack ingest` — submit a file (or a recursive walk's worth of
//! files), or one rule file, to the daemon's ingest queue, then stay
//! attached until every enqueued job lands in a terminal state. The
//! wait is the one `queue wait` makes, judged the same way once the ids
//! are known.
//!
//! `ingest --rule <RULE> --dry-run` is the one form that reaches no
//! daemon: [`dry_run`] works out what the rule would produce in this
//! process and prints it.

use std::path::{Path, PathBuf};

use bookrack_audit_profile::{AuditData, AuditProfile, HeadingPatterns};
use bookrack_cli::daemon_call::DEFAULT_AWAIT_STALL_TIMEOUT;
use bookrack_cli::error::BookrackCliError;
use bookrack_cli::render::human::basename_or_dash;
use bookrack_cli::render::progress::SurfaceKind;
use bookrack_cli_grammar::IngestArgs;
use bookrack_config::{Config, ConfigError, LibrarySelection};
use bookrack_core::Explain;
use bookrack_ops::rule::RuleDryrunReport;
use bookrack_runtime::audit_helpers;
use eyre::{Context, Result};
use serde_json::json;

use super::helpers;
use super::job_wait::{self, Daemon};

/// `path` as the daemon has to be handed it: absolute, resolved against
/// this process's working directory, and not required to exist — a
/// path with nothing behind it is the daemon's to refuse.
fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("make {} absolute", path.display()))
}

pub async fn run(
    args: IngestArgs,
    runtime_dir: Option<PathBuf>,
    audit_profile: Option<String>,
) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;

    // Subscribe before issuing the RPC so `queue.tick` events fired
    // by the worker between submit-ack and the wait loop's first
    // `recv` cannot slip past us.
    let rx = client
        .subscribe()
        .await
        .map_err(eyre::Report::from)
        .map_err(|e| e.wrap_err("subscribe to control-plane events"))?;

    // The grammar admits exactly one of the two forms.
    let (mut params, submitted) = match (&args.rule, &args.path) {
        (Some(rule), _) => {
            let mut params = json!({
                "rule": absolute(rule)?,
                "force": args.force,
                "allow_partial": args.allow_partial,
            });
            if let Some(source) = &args.source {
                params["source"] = json!(absolute(source)?);
            }
            (params, rule)
        }
        (None, Some(path)) => (
            json!({
                "paths": [path],
                "force": args.force,
                "recursive": args.recursive,
                "hold_for_metadata": args.hold_for_metadata,
            }),
            path,
        ),
        (None, None) => unreachable!("clap requires a path or a rule"),
    };
    if let Some(name) = audit_profile {
        params["audit_profile"] = serde_json::Value::String(name);
    }
    if let Some(level) = args.priority {
        params["priority"] = serde_json::Value::String(level);
    }
    let response = helpers::dispatch(&client, "ingest.submit", params).await?;
    let job_ids = helpers::extract_job_ids(&response);
    let label = basename_or_dash(submitted.to_str());

    if args.no_wait || job_ids.is_empty() {
        helpers::emit_outcome(&response, helpers::queued_sentence(&job_ids, label));
        return Ok(());
    }

    let end = job_wait::wait_on_ids(
        &Daemon(&client),
        rx,
        &job_ids,
        SurfaceKind::from_context().build(),
        DEFAULT_AWAIT_STALL_TIMEOUT,
    )
    .await?;
    job_wait::finalize(end, "ingest", "Ingested", label)
}

/// Where a rule preview's audit rules came from.
enum AuditRules {
    /// The overlay directory of the selected library.
    Library(PathBuf),
    /// The rules this build ships: no library is selected, or the
    /// selected one carries no overlay directory.
    Shipped,
}

/// `bookrack ingest --rule <RULE> --dry-run`: print what the rule would
/// produce as a book.
///
/// Runs in this process. The audit profile, audit data, and heading
/// patterns are the selected library's when a selection resolves and
/// its overlay directory exists, and the shipped ones otherwise;
/// a selection that names a library and resolves nowhere is an error.
/// No database is opened and nothing is written.
pub fn dry_run(
    selection: &LibrarySelection,
    args: &IngestArgs,
    audit_profile: Option<String>,
) -> Result<()> {
    let rule = args
        .rule
        .as_deref()
        .expect("clap requires --rule with --dry-run");
    audit_helpers::require_known_profile(
        audit_profile.as_deref(),
        bookrack_audit_profile::ALL_BUILT_IN_NAMES,
    )
    .map_err(|refusal| BookrackCliError::InputRefused {
        problem: refusal.explain(),
    })?;

    // root identity: read-only
    let cfg = match Config::resolve(selection) {
        Ok(cfg) => Some(cfg),
        Err(ConfigError::MissingDataDir) => None,
        Err(e) => return Err(e).context("resolve configuration"),
    };
    let overlay = cfg.as_ref().filter(|cfg| cfg.audit_rules_dir().is_dir());
    let (rules, data, profile, headings) = match overlay {
        Some(cfg) => (
            AuditRules::Library(cfg.audit_rules_dir()),
            audit_helpers::load_audit_data(cfg),
            audit_helpers::load_audit_profile(cfg, audit_profile.as_deref()),
            audit_helpers::load_heading_patterns(cfg),
        ),
        None => (
            AuditRules::Shipped,
            AuditData::default_data(),
            audit_profile
                .as_deref()
                .and_then(AuditProfile::from_named)
                .unwrap_or_default(),
            HeadingPatterns::default(),
        ),
    };

    let report = bookrack_ops::rule::dryrun(rule, data, profile, headings).map_err(|err| {
        match bookrack_runtime::control::error_map::input_refusal(&err) {
            Some(problem) => BookrackCliError::InputRefused { problem }.into(),
            None => err.wrap_err("rule dry run"),
        }
    })?;

    let output = bookrack_cli::render::ctx().output();
    if output.is_json() {
        let mut value = serde_json::to_value(&report).context("serialize the rule preview")?;
        value["audit_rules"] = match &rules {
            AuditRules::Library(dir) => json!(dir),
            AuditRules::Shipped => serde_json::Value::Null,
        };
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if !output.is_quiet() {
        print!("{}", preview_text(rule, &rules, &report));
    }
    Ok(())
}

/// The human reading of a rule preview: where its inputs came from,
/// how the rule cut and numbered its units, then what the book-side
/// stages made of the result.
fn preview_text(rule: &Path, rules: &AuditRules, report: &RuleDryrunReport) -> String {
    let join = |numbers: &[u32]| {
        numbers
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let dash = |value: Option<&str>| value.unwrap_or("-").to_string();
    let book = &report.book;
    let numbering = &report.completeness;

    let mut lines = vec![format!("rule: {}", rule.display())];
    lines.push(match rules {
        AuditRules::Library(dir) => format!("audit rules: {}", dir.display()),
        AuditRules::Shipped => "audit rules: shipped defaults".to_string(),
    });
    lines.push(format!("audit profile: {}", book.audit_profile));
    lines.push(match (numbering.min, numbering.max) {
        (Some(min), Some(max)) => format!(
            "units: {} ({} numbered, {min} to {max})",
            report.units, numbering.numbered
        ),
        _ => format!("units: {} (none numbered)", report.units),
    });
    if !numbering.gaps.is_empty() {
        lines.push(format!("missing numbers: {}", join(&numbering.gaps)));
    }
    if !numbering.duplicates.is_empty() {
        lines.push(format!("repeated numbers: {}", join(&numbering.duplicates)));
    }
    if !numbering.ordered {
        lines.push("numbers: not in ascending order".to_string());
    }
    if report.fallbacks.is_empty() {
        lines.push("fallbacks: none".to_string());
    } else {
        lines.push(format!("fallbacks: {}", report.fallbacks.len()));
        for fallback in &report.fallbacks {
            lines.push(match &fallback.detail {
                Some(detail) => format!("  {}: {detail}", fallback.kind),
                None => format!("  {}", fallback.kind),
            });
        }
    }
    lines.push(format!(
        "title: {}",
        dash(book.base_attrs.as_ref().and_then(|a| a.title.as_deref()))
    ));
    lines.push(format!(
        "blocks: {}",
        book.blocks.map_or("-".to_string(), |n| n.to_string())
    ));
    lines.push(format!(
        "toc entries: {}",
        book.toc_stats
            .as_ref()
            .map_or("-".to_string(), |toc| toc.total_toc_entries.to_string())
    ));
    lines.push(match &book.chunks {
        Some(chunks) => format!(
            "chunks: {} ({} to {} characters)",
            chunks.count, chunks.min_chars, chunks.max_chars
        ),
        None => "chunks: -".to_string(),
    });
    lines.push(format!(
        "verdict: {} (confidence {})",
        dash(book.verdict.as_deref()),
        dash(book.confidence.as_deref())
    ));
    if let Some(error) = &book.error {
        lines.push(format!("error: {error}"));
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}
