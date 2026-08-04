// SPDX-License-Identifier: Apache-2.0

//! `bookrack index-profile` — list, show, validate, or apply index
//! profiles. Every verb except `apply` is a local reflection-and-check
//! surface: built-in profiles are compiled into the binary and user
//! profiles are plain files under the per-user profile directory.
//! `apply` additionally reads the library's on-disk stamps to derive a
//! reconciliation plan; the planning here stays offline and read-only,
//! while executing the plan is the CLI orchestrator's job.
//!
//! The two verbs that need a library — `current` and `apply` — take it
//! from a [`LibrarySelection`] handed down whole and resolved by
//! [`Config::resolve`], so they sit on the same precedence ladder as
//! every other command. Neither carries a selector of its own: a verb
//! that re-declared one field of the selection would silently drop the
//! rest.

use std::path::{Path, PathBuf};

use bookrack_config::{
    Config, EmbedConfig, LibraryEntry, LibraryKind, LibrarySelection, list_libraries, load_manifest,
};
use bookrack_index_profile::{
    Finding, IndexProfile, ProfileOrigin, RerankerKind, Severity, builtin_toml, has_errors,
    list_profiles, resolve, validate,
};
use bookrack_vectors::ChunkStore;
use eyre::{Result, bail};

use crate::profile::{
    Pipeline, PipelinePlan, PlannedAction, derive_pipeline_plan, read_pipeline_state,
};

#[derive(clap::Subcommand, Debug)]
pub enum IndexProfileAction {
    /// List every profile — built-ins merged with the user directory.
    ///
    /// Each entry is marked `[builtin]` or `[user]`, and a user profile
    /// that shadows a built-in is called out.
    #[command(after_long_help = bookrack_cli_grammar::examples![
        "index-profile list",
        "index-profile list --json",
    ])]
    List {
        /// Emit machine-readable JSON instead of the plain listing.
        #[arg(long)]
        json: bool,
    },
    /// Print a profile's source and its static validation result.
    ///
    /// A user profile that shadows a built-in prints both.
    #[command(after_long_help = bookrack_cli_grammar::examples![
        "index-profile show qwen3-0.6b-default",
        "index-profile show qwen3-4b-quality --json",
    ])]
    Show {
        /// Profile name.
        name: String,
    },
    /// Validate a profile statically and exit non-zero if any finding is
    /// an error.
    #[command(after_long_help = bookrack_cli_grammar::examples![
        "index-profile validate qwen3-0.6b-default",
        "index-profile validate qwen3-0.6b-default --allow-unknown-model",
    ])]
    Validate {
        /// Profile name.
        name: String,
        /// Skip the "model is known" and "declared dimension matches"
        /// registry checks so an out-of-tree model can still be checked
        /// structurally.
        #[arg(long)]
        allow_unknown_model: bool,
    },
    /// Print the profile a library effectively runs under and compare it
    /// against the built stamps.
    ///
    /// Reports the profile's name, where the reference was declared, and
    /// the resolved combination, and marks where the built index stamps
    /// disagree. Offline and read-only.
    ///
    /// The library is the one the global selection resolves to:
    /// `--data-dir`, `--library`, the data-root variable, the portable
    /// layout, then the registry default.
    #[command(after_long_help = bookrack_cli_grammar::examples![
        "index-profile current",
        "index-profile current --json",
    ])]
    Current {
        /// Emit machine-readable JSON instead of the plain report.
        #[arg(long)]
        json: bool,
    },
    /// Compare two profiles field by field.
    #[command(after_long_help = bookrack_cli_grammar::examples![
        "index-profile diff qwen3-0.6b-default qwen3-4b-quality",
        "index-profile diff qwen3-0.6b-default qwen3-4b-quality --json",
    ])]
    Diff {
        /// First profile name.
        a: String,
        /// Second profile name.
        b: String,
        /// Emit a machine-readable JSON array instead of the table.
        #[arg(long)]
        json: bool,
    },
    /// Reconcile a library's built index with a profile, executing the
    /// plan through the daemon.
    ///
    /// Statically validates the profile, compares it against the
    /// recorded stamps and ANN configuration, derives the action plan,
    /// and executes it only after explicit confirmation. The preferred
    /// entry point for switching embedding models or ANN parameters; the
    /// `vectors` / `stamps` namespaces remain as low-level escape
    /// hatches.
    ///
    /// The library is the one the global selection resolves to, the same
    /// as for `current`.
    #[command(after_long_help = bookrack_cli_grammar::examples![
        "index-profile apply qwen3-0.6b-default",
        "index-profile apply qwen3-0.6b-default --dry-run",
    ])]
    Apply {
        /// Profile name to apply.
        name: String,
        /// Limit which pipeline's actions run. The profile always
        /// describes the whole library; the filter narrows execution
        /// only.
        #[arg(long, value_enum, default_value_t = PipelineFilter::All)]
        pipeline: PipelineFilter,
        /// Print the derived action plan and exit without declaring or
        /// executing anything. Works offline.
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt. Required for non-interactive
        /// runs whose plan contains a destructive action.
        #[arg(long)]
        yes: bool,
    },
}

/// Which pipelines `index-profile apply` executes actions for.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineFilter {
    /// The book pipeline only.
    Books,
    /// The paper pipeline only.
    Papers,
    /// Every pipeline (the default).
    All,
}

impl PipelineFilter {
    /// Whether the filter selects `pipeline`.
    pub fn selects(self, pipeline: Pipeline) -> bool {
        match self {
            PipelineFilter::Books => pipeline == Pipeline::Books,
            PipelineFilter::Papers => pipeline == Pipeline::Papers,
            PipelineFilter::All => true,
        }
    }
}

pub fn run(action: IndexProfileAction, selection: &LibrarySelection) -> Result<()> {
    match action {
        IndexProfileAction::List { json } => list(json),
        IndexProfileAction::Show { name } => show(&name),
        IndexProfileAction::Validate {
            name,
            allow_unknown_model,
        } => validate_cmd(&name, allow_unknown_model),
        IndexProfileAction::Current { json } => current(selection, json),
        IndexProfileAction::Diff { a, b, json } => diff(&a, &b, json),
        // Apply connects to the daemon and confirms interactively, so
        // the CLI dispatches it to its control-plane client before this
        // local surface is reached.
        IndexProfileAction::Apply { .. } => {
            bail!("index-profile apply is dispatched through the daemon client")
        }
    }
}

/// The per-user profile directory, beside `registry.toml`. `None` when no
/// config location can be resolved; the caller then lists built-ins only.
fn user_profile_dir() -> Option<PathBuf> {
    crate::profile::user_profile_dir()
}

fn list(json: bool) -> Result<()> {
    // A directory that could not be located lists built-ins only —
    // stated as `None`, rather than as a path relative to whatever
    // directory the command happened to start in.
    let entries = list_profiles(user_profile_dir().as_deref());

    if json {
        let value = serde_json::Value::Array(
            entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "name": e.name,
                        "origin": origin_str(e.origin),
                        "shadows_builtin": e.shadows_builtin,
                    })
                })
                .collect(),
        );
        println!("{value}");
        return Ok(());
    }

    for entry in &entries {
        let tag = format!("[{}]", origin_str(entry.origin));
        if entry.shadows_builtin {
            println!("{:<8} {} (shadows a built-in)", tag, entry.name);
        } else {
            println!("{:<8} {}", tag, entry.name);
        }
    }
    Ok(())
}

fn show(name: &str) -> Result<()> {
    let dir = user_profile_dir();
    let user_path = dir
        .as_deref()
        .map(|d| bookrack_index_profile::user_profile_path(d, name));
    let user_text = user_path
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok());
    let builtin_text = builtin_toml(name);

    match (&user_text, builtin_text) {
        (Some(user), Some(builtin)) => {
            println!("note: user profile shadows a builtin of the same name");
            println!(
                "# user (effective): {}",
                user_path.as_deref().unwrap_or(Path::new("?")).display(),
            );
            print!("{user}");
            ensure_trailing_newline(user);
            println!();
            println!("# builtin (shadowed)");
            print!("{builtin}");
            ensure_trailing_newline(builtin);
        }
        (Some(user), None) => {
            println!(
                "# user: {}",
                user_path.as_deref().unwrap_or(Path::new("?")).display(),
            );
            print!("{user}");
            ensure_trailing_newline(user);
        }
        (None, Some(builtin)) => {
            println!("# builtin: {name}");
            print!("{builtin}");
            ensure_trailing_newline(builtin);
        }
        (None, None) => bail!("unknown index profile '{name}'"),
    }

    // The effective profile (user wins) drives the validation summary.
    let (profile, _) = resolve(dir.as_deref(), name)?
        .ok_or_else(|| eyre::eyre!("unknown index profile '{name}'"))?;
    println!();
    render_findings(&validate(&profile, false));
    Ok(())
}

fn validate_cmd(name: &str, allow_unknown_model: bool) -> Result<()> {
    let dir = user_profile_dir();
    let (profile, _source) = resolve(dir.as_deref(), name)?
        .ok_or_else(|| eyre::eyre!("unknown index profile '{name}'"))?;
    let findings = validate(&profile, allow_unknown_model);
    render_findings(&findings);
    if has_errors(&findings) {
        let errors = findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .count();
        bail!("profile '{name}' has {errors} validation error(s)");
    }
    Ok(())
}

/// Report the profile a library effectively runs under and compare it
/// against the built index stamps. Offline: manifest, registry,
/// `config.toml`, profile files, and a read-only corpus open — no daemon
/// involved. The conflicts the daemon refuses to start with surface here
/// as the same errors; a stamp mismatch and a drifted reference are both
/// findings in the report, not errors, because reconciling them is
/// `index-profile apply`'s job (a stale registry copy is also what
/// `libraries scan` refreshes).
fn current(selection: &LibrarySelection, json: bool) -> Result<()> {
    let target = resolve_target(selection)?;
    let effective = crate::profile::effective_index_profile(target.config())?;
    let resolved = effective.as_ref().map(|e| (&e.profile, e.origin, e.source));
    let drift = effective.as_ref().map_or(&[][..], |e| e.drift.as_slice());

    let profile_model = resolved.as_ref().map(|(p, _, _)| p.embed.model.as_str());
    let effective_model = EmbedConfig::resolve(profile_model).model;
    let stamps = pipeline_stamps(target.data_dir(), resolved.as_ref().map(|(p, _, _)| *p));
    // Consistency is a property of the whole library: every pipeline
    // that could be compared agreed. `None` when none could be.
    let comparable: Vec<&Vec<String>> = stamps.iter().filter_map(|s| s.findings.as_ref()).collect();
    let consistent = (!comparable.is_empty()).then(|| comparable.iter().all(|f| f.is_empty()));

    if json {
        let value = serde_json::json!({
            "library": target.label(),
            "registered": target.entry.is_some(),
            "data_dir": target.data_dir().display().to_string(),
            "profile": resolved.as_ref().map(|(p, origin, source)| serde_json::json!({
                "name": p.name,
                "origin": origin,
                // Which file defined it, as distinct from which
                // reference named it: a user file and the built-in of
                // the same name were otherwise indistinguishable here.
                "defined_by": origin_str(*source),
            })),
            "drift": drift,
            "effective_embed_model": effective_model,
            // One object per pipeline: each keeps its own corpus, its
            // own stamp record, and its own chunking constant, so a
            // single flat comparison could only ever answer for one.
            "pipelines": stamps.iter().map(|s| serde_json::json!({
                "pipeline": s.pipeline.as_str(),
                "built_stamps": s.built.as_ref().map(|b| serde_json::json!({
                    "embed_model": b.embed_model,
                    "vector_dim": b.vector_dim,
                    "chunk_version": b.chunk_version,
                    "normalize_version": b.normalize_version,
                })),
                "built_stamps_error": s.error,
                "stamp_findings": s.findings,
                "consistent": s.findings.as_ref().map(|f| f.is_empty()),
            })).collect::<Vec<_>>(),
            "consistent": consistent,
        });
        println!("{value}");
        return Ok(());
    }

    println!(
        "library: {} ({})",
        target.label(),
        target.data_dir().display()
    );
    if target.entry.is_none() {
        println!(
            "note: this data root is not in the registry; \
             `bookrack libraries add` registers it"
        );
    }
    match &resolved {
        Some((profile, origin, source)) => {
            println!(
                "profile: {} (source: {}, defined by: {})",
                profile.name,
                origin.as_str(),
                origin_str(*source),
            );
            println!(
                "  embed: {}/{} dim {}",
                profile.embed.backend, profile.embed.model, profile.embed.dim
            );
            println!(
                "  ann: {}{}",
                profile.ann.kind.as_str(),
                ann_params(profile)
            );
            println!("  {}", reranker_line(profile));
        }
        None => println!("profile: none (built-in default embed model)"),
    }
    for d in drift {
        println!(
            "drift: {} still references '{}'",
            d.source.as_str(),
            d.stale_value
        );
    }
    if !drift.is_empty() {
        println!(
            "note: `bookrack index-profile apply` rewrites the stale copies, \
             or `bookrack libraries scan` refreshes the registry cache alone"
        );
    }
    println!("effective embed model: {effective_model}");
    let mut mismatched = false;
    for section in &stamps {
        let pipeline = section.pipeline.as_str();
        match (&section.built, &section.findings) {
            (None, _) => match &section.error {
                Some(reason) => {
                    println!("stamps ({pipeline}): corpus database cannot be opened ({reason})");
                }
                None => println!("stamps ({pipeline}): no built index to compare against"),
            },
            (Some(b), None) => {
                println!(
                    "stamps ({pipeline}): built index is {} (no profile to compare against)",
                    stamp_pair(b)
                );
            }
            (Some(b), Some(f)) if f.is_empty() => {
                println!(
                    "stamps ({pipeline}): consistent with the built index ({})",
                    stamp_pair(b)
                );
            }
            (Some(_), Some(f)) => {
                mismatched = true;
                for finding in f {
                    println!("stamp mismatch ({pipeline}): {finding}");
                }
            }
        }
    }
    if mismatched {
        println!("note: `bookrack index-profile apply` reconciles this");
    }
    Ok(())
}

/// One pipeline's built stamps, read from its own corpus database and
/// compared against what a clean build under the effective profile
/// would record.
#[derive(Debug)]
struct PipelineStamps {
    pipeline: Pipeline,
    /// The recorded stamps, `None` when no index has been built or the
    /// corpus could not be opened.
    built: Option<crate::profile::BuiltStamps>,
    /// Why the corpus could not be opened, when that is what happened.
    error: Option<String>,
    /// Field-level divergences, `None` when there was nothing to compare
    /// — no profile, or no built index.
    findings: Option<Vec<String>>,
}

/// Read and compare every pipeline's stamps under `data_dir`. An
/// unstamped corpus reports as "no built index", same as a missing one:
/// there is nothing to compare a profile against. An unreadable corpus
/// is reported alongside instead, this being a best-effort status
/// surface rather than a hard failure.
fn pipeline_stamps(data_dir: &Path, profile: Option<&IndexProfile>) -> Vec<PipelineStamps> {
    Pipeline::ALL
        .into_iter()
        .map(|pipeline| {
            let (built, error) = match crate::profile::built_stamps(&pipeline.corpus_db(data_dir)) {
                Ok(stamps) => (stamps.filter(|b| !b.is_unstamped()), None),
                Err(reason) => (None, Some(reason)),
            };
            let findings = match (profile, &built) {
                (Some(profile), Some(stamps)) => {
                    let target = pipeline.target_stamps(&profile.embed.model, profile.embed.dim);
                    Some(crate::profile::profile_stamp_findings(&target, stamps))
                }
                _ => None,
            };
            PipelineStamps {
                pipeline,
                built,
                error,
                findings,
            }
        })
        .collect()
}

/// The `model/dim` display pair for a built-stamp record, with a
/// placeholder for a key that is not stamped.
fn stamp_pair(built: &crate::profile::BuiltStamps) -> String {
    format!(
        "{}/{}",
        built.embed_model.as_deref().unwrap_or("<unstamped>"),
        built
            .vector_dim
            .map_or_else(|| "<unstamped>".to_string(), |d| d.to_string())
    )
}

/// The library a profile verb operates on: the data root the shared
/// resolver picked, plus the registry entry that root maps to when the
/// registry carries one.
#[derive(Debug, Clone)]
pub struct ProfileTarget {
    config: Config,
    /// The registry entry the resolved root was matched to. `None` for a
    /// root the registry does not carry: its manifest still holds the
    /// profile reference, there is simply no cache beside it.
    pub entry: Option<LibraryEntry>,
    label: String,
}

impl ProfileTarget {
    /// The resolved configuration the target was selected through.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The data root every read and every declaration goes to.
    pub fn data_dir(&self) -> &Path {
        self.config.data_dir()
    }

    /// The name this library goes by in output and in the apply
    /// confirmation prompt: its registry name, else its manifest birth
    /// name, else the data root's directory basename.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The kind the registry records, or the default for a root the
    /// registry does not carry.
    pub fn kind(&self) -> LibraryKind {
        match &self.entry {
            Some(entry) => entry.kind,
            None => LibraryKind::default(),
        }
    }

    /// The description the registry entry carries, when it has one.
    pub fn description(&self) -> Option<&str> {
        self.entry.as_ref().and_then(|e| e.description.as_deref())
    }
}

/// Resolve the library a profile verb operates on from a CLI selection.
///
/// The root comes from [`Config::resolve`], so these verbs sit on the
/// same precedence ladder as every other command: `--data-dir`, then
/// `--library`, then the data-root variable, then the portable layout,
/// then a registry `default`. The registry entry is then looked up by
/// the name the resolver settled on — which for a path-class root is the
/// entry it was matched to by manifest uuid or by path — so a root the
/// registry does not carry resolves without an entry instead of failing.
fn resolve_target(selection: &LibrarySelection) -> Result<ProfileTarget> {
    let config = Config::resolve(selection)?;
    let entry = match config.library() {
        Some(name) => list_libraries()?
            .unwrap_or_default()
            .into_iter()
            .find(|e| e.name == name),
        None => None,
    };
    let label = match &entry {
        Some(entry) => entry.name.clone(),
        None => root_label(config.data_dir()),
    };
    Ok(ProfileTarget {
        config,
        entry,
        label,
    })
}

/// The name an unregistered data root goes by: its manifest's birth
/// name, else the root's directory basename. Mirrors how the registry
/// write path names a root it is asked to add without an explicit name.
fn root_label(data_dir: &Path) -> String {
    if let Ok(Some(manifest)) = load_manifest(data_dir) {
        return manifest.name;
    }
    data_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "library".to_string())
}

/// Compare two profiles field by field.
fn diff(a: &str, b: &str, json: bool) -> Result<()> {
    let dir = user_profile_dir();
    let (profile_a, _) =
        resolve(dir.as_deref(), a)?.ok_or_else(|| eyre::eyre!("unknown index profile '{a}'"))?;
    let (profile_b, _) =
        resolve(dir.as_deref(), b)?.ok_or_else(|| eyre::eyre!("unknown index profile '{b}'"))?;
    let rows = diff_rows(&profile_a, &profile_b);

    if json {
        let value = serde_json::Value::Array(
            rows.iter()
                .map(|(field, va, vb)| {
                    serde_json::json!({
                        "field": field,
                        "a": va,
                        "b": vb,
                        "same": va == vb,
                    })
                })
                .collect(),
        );
        println!("{value}");
        return Ok(());
    }

    let field_width = rows.iter().map(|(f, _, _)| f.len()).max().unwrap_or(0);
    let a_width = rows
        .iter()
        .map(|(_, va, _)| va.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(1)
        .max(a.len());
    println!("{:<field_width$}  {:<a_width$}  {}", "field", a, b);
    for (field, va, vb) in &rows {
        let marker = if va == vb { "" } else { "  <- differs" };
        println!(
            "{field:<field_width$}  {:<a_width$}  {}{marker}",
            va.as_deref().unwrap_or("-"),
            vb.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

/// The query-time ANN parameters a summary line shows, skipping the
/// optional ones a profile leaves unset.
fn ann_params(profile: &IndexProfile) -> String {
    let ann = &profile.ann;
    let mut out = format!(" partitions={} nprobes={}", ann.num_partitions, ann.nprobes);
    if let Some(nsv) = ann.num_sub_vectors {
        out.push_str(&format!(" sub_vectors={nsv}"));
    }
    if let Some(bits) = ann.num_bits {
        out.push_str(&format!(" bits={bits}"));
    }
    if let Some(refine) = ann.refine_factor {
        out.push_str(&format!(" refine={refine}"));
    }
    out
}

/// The reranker stage a summary line shows.
///
/// A cross-encoder stage carries a model and both candidate counts, so
/// each `<unset>` here marks a profile validation would reject rather
/// than a field with a default behind it.
fn reranker_line(profile: &IndexProfile) -> String {
    match profile.reranker.kind {
        RerankerKind::None => "reranker: none".to_string(),
        RerankerKind::CrossEncoder => format!(
            "reranker: cross-encoder ({}, top {} -> {})",
            profile.reranker.model.as_deref().unwrap_or("<unset>"),
            top_k(profile.reranker.top_k_in),
            top_k(profile.reranker.top_k_out),
        ),
    }
}

/// One candidate count for display, or `<unset>` when the profile omits
/// it.
fn top_k(count: Option<u32>) -> String {
    count.map_or_else(|| "<unset>".to_string(), |k| k.to_string())
}

/// Field-level comparison rows: every dotted field path either profile
/// carries, in profile-a-then-b-extras order, with each side's display
/// value (`None` when that side omits the field).
fn diff_rows(a: &IndexProfile, b: &IndexProfile) -> Vec<(String, Option<String>, Option<String>)> {
    let flat_a = flatten_profile(a);
    let flat_b = flatten_profile(b);
    let mut fields: Vec<String> = flat_a.iter().map(|(f, _)| f.clone()).collect();
    for (field, _) in &flat_b {
        if !fields.contains(field) {
            fields.push(field.clone());
        }
    }
    let value_of = |flat: &[(String, String)], field: &str| {
        flat.iter()
            .find(|(f, _)| f == field)
            .map(|(_, v)| v.clone())
    };
    fields
        .into_iter()
        .map(|field| {
            let va = value_of(&flat_a, &field);
            let vb = value_of(&flat_b, &field);
            (field, va, vb)
        })
        .collect()
}

/// Flatten a profile's serialized form into dotted field paths paired
/// with display values, preserving serialization order.
fn flatten_profile(profile: &IndexProfile) -> Vec<(String, String)> {
    let value = serde_json::to_value(profile).expect("profile serializes");
    let mut out = Vec::new();
    flatten_value("", &value, &mut out);
    out
}

fn flatten_value(prefix: &str, value: &serde_json::Value, out: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, nested) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_value(&path, nested, out);
            }
        }
        serde_json::Value::String(s) => out.push((prefix.to_string(), s.clone())),
        other => out.push((prefix.to_string(), other.to_string())),
    }
}

/// Print each finding as `severity: field_path: message`, or a clean-bill
/// line when there are none.
fn render_findings(findings: &[Finding]) {
    if findings.is_empty() {
        println!("ok: no findings");
        return;
    }
    for f in findings {
        println!("{}: {}: {}", f.severity.as_str(), f.field_path, f.message);
    }
}

fn origin_str(origin: ProfileOrigin) -> &'static str {
    match origin {
        ProfileOrigin::BuiltIn => "builtin",
        ProfileOrigin::User => "user",
    }
}

fn ensure_trailing_newline(text: &str) {
    if !text.ends_with('\n') {
        println!();
    }
}

/// A refusal `plan_apply` classifies as operator input rather than an
/// internal failure — a bad profile, a masked embed model, an
/// unsupported reranker. The CLI maps it to the user-error exit path.
#[derive(Debug)]
pub struct ApplyRefusal(pub String);

impl std::fmt::Display for ApplyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ApplyRefusal {}

/// One pipeline's slice of an apply plan.
#[derive(Debug, Clone)]
pub struct PipelineSection {
    /// Which pipeline the slice describes.
    pub pipeline: Pipeline,
    /// What the pipeline needs.
    pub plan: PipelinePlan,
    /// Chunk rows a planned re-embed touches, when the store could be
    /// counted; display-only scale hint.
    pub reembed_chunks: Option<usize>,
}

/// A fully derived apply plan: the target profile, the library it
/// reconciles, and one section per selected pipeline.
#[derive(Debug, Clone)]
pub struct ApplyPlan {
    /// The library being reconciled.
    pub target: ProfileTarget,
    /// The target profile.
    pub profile: IndexProfile,
    /// Per-pipeline sections, in [`Pipeline::ALL`] order.
    pub sections: Vec<PipelineSection>,
}

impl ApplyPlan {
    /// Whether the target profile enables a reranker stage. The stage
    /// is query-time behaviour with no index action, so the apply
    /// caller uses this only to hint at the daemon restart that brings
    /// the backend up or down.
    pub fn enables_reranker(&self) -> bool {
        self.profile.reranker.kind != RerankerKind::None
    }

    /// Every `(pipeline, action)` pair in execution order.
    pub fn actions(&self) -> Vec<(Pipeline, PlannedAction)> {
        self.sections
            .iter()
            .filter_map(|s| match &s.plan {
                PipelinePlan::Run(actions) => Some((s.pipeline, actions)),
                _ => None,
            })
            .flat_map(|(p, actions)| actions.iter().map(move |a| (p, *a)))
            .collect()
    }

    /// Whether any planned action discards data irrecoverably.
    pub fn has_destructive(&self) -> bool {
        self.actions().iter().any(|(_, a)| a.is_destructive())
    }

    /// Whether the plan overwrites vectors in place or degrades search
    /// to a scan — the tier that asks for a soft confirmation.
    pub fn needs_soft_confirm(&self) -> bool {
        self.actions()
            .iter()
            .any(|(_, a)| matches!(a, PlannedAction::Reembed | PlannedAction::DropIndex))
    }

    /// Whether every selected pipeline is already consistent or empty.
    pub fn is_noop(&self) -> bool {
        self.actions().is_empty()
    }
}

/// Derive the apply plan for profile `name` against the library
/// `selection` resolves to. Offline and read-only: registry,
/// `config.toml`, profile files, corpus stamps, and the vector-store
/// meta — no daemon involved. Refusals that are the operator's to fix
/// surface as [`ApplyRefusal`].
pub async fn plan_apply(
    name: &str,
    selection: &LibrarySelection,
    filter: PipelineFilter,
) -> Result<ApplyPlan> {
    let target = resolve_target(selection)?;

    if let Some(refusal) = crate::profile::refuse_bad_profile_reference(name) {
        return Err(ApplyRefusal(refusal).into());
    }
    let dir = user_profile_dir();
    let (profile, _source) = resolve(dir.as_deref(), name)?
        .ok_or_else(|| eyre::eyre!("unknown index profile '{name}'"))?;

    let mut sections = Vec::new();
    for pipeline in Pipeline::ALL {
        if !filter.selects(pipeline) {
            continue;
        }
        let state = read_pipeline_state(target.data_dir(), pipeline)?;
        let stamps_target = pipeline.target_stamps(&profile.embed.model, profile.embed.dim);
        let plan = derive_pipeline_plan(&profile, &stamps_target, &state);
        let reembed_chunks = match &plan {
            PipelinePlan::Run(actions) if actions.contains(&PlannedAction::Reembed) => {
                count_chunks(&pipeline.lancedb_dir(target.data_dir())).await
            }
            _ => None,
        };
        sections.push(PipelineSection {
            pipeline,
            plan,
            reembed_chunks,
        });
    }

    Ok(ApplyPlan {
        target,
        profile,
        sections,
    })
}

/// Best-effort chunk-row count for the plan's scale hint. Read-only:
/// a store that was never built yields no count rather than being
/// materialized by the probe, and any failure on one that exists (a
/// locked store, a reader-version refusal) degrades to "no count"
/// rather than blocking the plan.
async fn count_chunks(lancedb_dir: &Path) -> Option<usize> {
    let store = ChunkStore::try_open(lancedb_dir).await.ok().flatten()?;
    store.count_rows().await.ok()
}

/// Print an apply plan, one section per pipeline. `queue_busy` carries
/// the daemon's pending+running job count when the caller is connected;
/// non-zero adds the queueing note.
pub fn render_apply_plan(plan: &ApplyPlan, queue_busy: Option<u32>) {
    println!(
        "library: {} ({})",
        plan.target.label(),
        plan.target.data_dir().display()
    );
    let p = &plan.profile;
    println!(
        "target profile: {} (embed {}/{} dim {}; ann {}{})",
        p.name,
        p.embed.backend,
        p.embed.model,
        p.embed.dim,
        p.ann.kind.as_str(),
        ann_params(p)
    );
    for section in &plan.sections {
        println!("[{}]", section.pipeline.as_str());
        match &section.plan {
            PipelinePlan::Empty => println!("  skipped (pipeline empty)"),
            PipelinePlan::Consistent => println!("  already consistent"),
            PipelinePlan::Run(actions) => {
                for action in actions {
                    println!(
                        "  {}",
                        describe_action(section.pipeline, *action, section.reembed_chunks, p)
                    );
                }
            }
        }
    }
    if let Some(busy) = queue_busy
        && busy > 0
    {
        println!(
            "note: the daemon queue has {busy} job(s) pending or running; apply actions \
             queue behind them (`bookrack queue list`)"
        );
    }
}

/// One plan line for an action: the CLI verb it maps to plus what it
/// does to the store.
fn describe_action(
    pipeline: Pipeline,
    action: PlannedAction,
    reembed_chunks: Option<usize>,
    profile: &IndexProfile,
) -> String {
    let ns = match pipeline {
        Pipeline::Books => "",
        Pipeline::Papers => "papers ",
    };
    match action {
        PlannedAction::Reset => format!(
            "{ns}vectors reset — DESTRUCTIVE: drops the chunks table and re-chunks + \
             re-embeds from the corpus tree; the old vectors are unrecoverable"
        ),
        PlannedAction::Reembed => {
            let scale = match reembed_chunks {
                Some(n) => format!(" ({n} chunk row(s) affected)"),
                None => String::new(),
            };
            format!("{ns}vectors reembed — re-derives chunks in place and re-embeds them{scale}")
        }
        PlannedAction::Rebuild => format!(
            "{ns}vectors rebuild — non-destructive; rebuilds the ANN index as {}{}",
            profile.ann.kind.as_str(),
            ann_params(profile)
        ),
        PlannedAction::DropIndex => format!(
            "{ns}vectors drop — non-destructive; drops the ANN index so search runs as \
             an exhaustive scan (the profile declares brute-force)"
        ),
        PlannedAction::ReconcileStamps => {
            format!("{ns}stamps reconcile — rewrites the four index stamps (metadata only)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_index_profile::{PROFILE_QWEN3_4B_QUALITY, PROFILE_QWEN3_06B_DEFAULT};

    #[test]
    fn pipeline_filter_selects_what_it_names() {
        assert!(PipelineFilter::All.selects(Pipeline::Books));
        assert!(PipelineFilter::All.selects(Pipeline::Papers));
        assert!(PipelineFilter::Books.selects(Pipeline::Books));
        assert!(!PipelineFilter::Books.selects(Pipeline::Papers));
        assert!(PipelineFilter::Papers.selects(Pipeline::Papers));
        assert!(!PipelineFilter::Papers.selects(Pipeline::Books));
    }

    #[test]
    fn the_stamp_comparison_covers_a_library_indexed_on_the_paper_side_only() {
        // One profile governs both pipelines, and papers chunk under
        // their own constant. Reading the book corpus alone answers
        // "no built index" for a library whose paper index is stamped —
        // and would answer it for a drifted paper index too.
        let root = tempfile::tempdir().expect("tempdir");
        let profile = IndexProfile::from_named(PROFILE_QWEN3_06B_DEFAULT).expect("built-in");
        let mut drifted = Pipeline::Papers.target_stamps(&profile.embed.model, profile.embed.dim);
        drifted.chunk_version += 1;
        let corpus = bookrack_corpus::Corpus::open(&Pipeline::Papers.corpus_db(root.path()))
            .expect("create the paper corpus");
        for (key, value) in [
            (
                bookrack_corpus::EMBED_MODEL_KEY,
                drifted.embed_model.clone(),
            ),
            (
                bookrack_corpus::VECTOR_DIM_KEY,
                drifted.vector_dim.to_string(),
            ),
            (
                bookrack_corpus::CHUNK_VERSION_KEY,
                drifted.chunk_version.to_string(),
            ),
            (
                bookrack_corpus::NORMALIZE_VERSION_KEY,
                drifted.normalize_version.to_string(),
            ),
        ] {
            corpus.meta_set(key, &value).expect("stamp the corpus");
        }
        drop(corpus);

        let stamps = pipeline_stamps(root.path(), Some(&profile));

        let papers = stamps
            .iter()
            .find(|s| s.pipeline == Pipeline::Papers)
            .expect("the paper pipeline is compared");
        let findings = papers
            .findings
            .as_ref()
            .expect("a stamped paper index is compared against the profile");
        assert!(
            findings.iter().any(|f| f.contains("chunk_version")),
            "{findings:?}"
        );
        let books = stamps
            .iter()
            .find(|s| s.pipeline == Pipeline::Books)
            .expect("the book pipeline is still reported");
        assert!(books.built.is_none(), "{books:?}");
        assert!(books.error.is_none(), "{books:?}");
    }

    #[test]
    fn plan_lines_carry_namespace_scale_and_severity() {
        let profile = IndexProfile::from_named(PROFILE_QWEN3_06B_DEFAULT).expect("built-in");

        let reset = describe_action(Pipeline::Papers, PlannedAction::Reset, None, &profile);
        assert!(reset.starts_with("papers vectors reset"));
        assert!(reset.contains("DESTRUCTIVE"));

        let reembed = describe_action(Pipeline::Books, PlannedAction::Reembed, Some(42), &profile);
        assert!(reembed.starts_with("vectors reembed"));
        assert!(reembed.contains("42 chunk row(s)"));
        // Without a count the scale hint is simply absent.
        assert!(
            !describe_action(Pipeline::Books, PlannedAction::Reembed, None, &profile)
                .contains("row(s)")
        );

        let rebuild = describe_action(Pipeline::Books, PlannedAction::Rebuild, None, &profile);
        assert!(rebuild.contains("non-destructive"));
        assert!(rebuild.contains(profile.ann.kind.as_str()));

        let reconcile = describe_action(
            Pipeline::Books,
            PlannedAction::ReconcileStamps,
            None,
            &profile,
        );
        assert!(reconcile.contains("metadata only"));
    }

    /// A cross-encoder profile missing its candidate counts is a state
    /// validation refuses, so nothing has a value to show for it. The
    /// line says so rather than printing a number no stage would use —
    /// `0` reads as a configured cap, and the supervisor's own fallback
    /// for the same state is a different number again.
    #[test]
    fn a_cross_encoder_without_candidate_counts_shows_them_unset() {
        let mut profile =
            IndexProfile::from_named(PROFILE_QWEN3_4B_QUALITY).expect("built-in profile");
        assert_eq!(profile.reranker.kind, RerankerKind::CrossEncoder);

        let configured = reranker_line(&profile);
        assert!(configured.contains("top 50 -> 10"), "{configured}");

        profile.reranker.top_k_in = None;
        profile.reranker.top_k_out = None;
        let line = reranker_line(&profile);
        assert!(line.contains("top <unset> -> <unset>"), "{line}");
        assert!(!line.contains("top 0"), "{line}");
    }
}
