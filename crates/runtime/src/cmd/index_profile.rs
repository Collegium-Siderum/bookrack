// SPDX-License-Identifier: Apache-2.0

//! `bookrack index-profile` — list, show, validate, or apply index
//! profiles. Every verb except `apply` is a local reflection-and-check
//! surface: built-in profiles are compiled into the binary and user
//! profiles are plain files under the per-user profile directory.
//! `apply` additionally reads the library's on-disk stamps to derive a
//! reconciliation plan; the planning here stays offline and read-only,
//! while executing the plan is the CLI orchestrator's job.

use std::path::{Path, PathBuf};

use bookrack_config::{
    ConfigError, EmbedConfig, LibraryEntry, list_libraries, load_root_config,
    root_config_env_override,
};
use bookrack_index_profile::{
    Finding, IndexProfile, ProfileOrigin, RerankerKind, Severity, USER_PROFILE_DIR_NAME,
    builtin_toml, ensure_reranker_supported, has_errors, list_profiles, resolve, validate,
};
use bookrack_vectors::ChunkStore;
use eyre::{Result, bail};

use crate::profile::{
    Pipeline, PipelinePlan, PlannedAction, derive_pipeline_plan, read_pipeline_state,
};

#[derive(clap::Subcommand, Debug)]
pub enum IndexProfileAction {
    /// List every profile — built-ins merged with the user directory —
    /// marking each `[builtin]` or `[user]` and noting any that shadow a
    /// built-in.
    List {
        /// Emit machine-readable JSON instead of the plain listing.
        #[arg(long)]
        json: bool,
    },
    /// Print a profile's source and its static validation result. A user
    /// profile that shadows a built-in prints both.
    Show {
        /// Profile name.
        name: String,
    },
    /// Statically validate a profile and exit non-zero if any finding is
    /// an error.
    Validate {
        /// Profile name.
        name: String,
        /// Skip the "model is known" and "declared dimension matches"
        /// registry checks so an out-of-tree model can still be checked
        /// structurally.
        #[arg(long)]
        allow_unknown_model: bool,
    },
    /// Print the profile a library effectively runs under — its name,
    /// where the reference was declared, the resolved combination — and
    /// compare it against the built index stamps. Offline and read-only.
    Current {
        /// Library to inspect; the registry default when omitted.
        #[arg(long)]
        library: Option<String>,
        /// Emit machine-readable JSON instead of the plain report.
        #[arg(long)]
        json: bool,
    },
    /// Compare two profiles field by field.
    Diff {
        /// First profile name.
        a: String,
        /// Second profile name.
        b: String,
        /// Emit a machine-readable JSON array instead of the table.
        #[arg(long)]
        json: bool,
    },
    /// Reconcile a library's built index with a profile: statically
    /// validate it, compare it against the recorded stamps and ANN
    /// configuration, derive the action plan, and — after explicit
    /// confirmation — execute the plan through the daemon. The preferred
    /// entry point for switching embedding models or ANN parameters; the
    /// `vectors` / `stamps` namespaces remain as low-level escape
    /// hatches.
    Apply {
        /// Profile name to apply.
        name: String,
        /// Library to reconcile; the registry default when omitted.
        #[arg(long)]
        library: Option<String>,
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

pub fn run(action: IndexProfileAction) -> Result<()> {
    match action {
        IndexProfileAction::List { json } => list(json),
        IndexProfileAction::Show { name } => show(&name),
        IndexProfileAction::Validate {
            name,
            allow_unknown_model,
        } => validate_cmd(&name, allow_unknown_model),
        IndexProfileAction::Current { library, json } => current(library, json),
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
    // A missing or unresolved directory lists built-ins only.
    let dir = user_profile_dir().unwrap_or_else(|| PathBuf::from(USER_PROFILE_DIR_NAME));
    let entries = list_profiles(&dir);

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
    let dir = user_profile_dir().unwrap_or_else(|| PathBuf::from(USER_PROFILE_DIR_NAME));
    let user_path = bookrack_index_profile::user_profile_path(&dir, name);
    let user_text = std::fs::read_to_string(&user_path).ok();
    let builtin_text = builtin_toml(name);

    match (&user_text, builtin_text) {
        (Some(user), Some(builtin)) => {
            println!("note: user profile shadows a builtin of the same name");
            println!("# user (effective): {}", user_path.display());
            print!("{user}");
            ensure_trailing_newline(user);
            println!();
            println!("# builtin (shadowed)");
            print!("{builtin}");
            ensure_trailing_newline(builtin);
        }
        (Some(user), None) => {
            println!("# user: {}", user_path.display());
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
    let profile =
        resolve(&dir, name)?.ok_or_else(|| eyre::eyre!("unknown index profile '{name}'"))?;
    println!();
    render_findings(&validate(&profile, false));
    Ok(())
}

fn validate_cmd(name: &str, allow_unknown_model: bool) -> Result<()> {
    let dir = user_profile_dir().unwrap_or_else(|| PathBuf::from(USER_PROFILE_DIR_NAME));
    let profile =
        resolve(&dir, name)?.ok_or_else(|| eyre::eyre!("unknown index profile '{name}'"))?;
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
/// against the built index stamps. Offline: registry, `config.toml`,
/// profile files, and a read-only corpus open — no daemon involved. The
/// conflicts the daemon refuses to start with surface here as the same
/// errors; a stamp mismatch is a finding in the report, not an error,
/// because reconciling it is `index-profile apply`'s job.
fn current(library: Option<String>, json: bool) -> Result<()> {
    let entry = registry_entry(library.as_deref())?;
    let root = load_root_config(&entry.data_dir)?;
    let reference = crate::profile::effective_reference(
        root.index_profile.clone(),
        entry.index_profile.clone(),
    )?;

    let dir = user_profile_dir().unwrap_or_else(|| PathBuf::from(USER_PROFILE_DIR_NAME));
    let resolved = match &reference {
        Some((name, origin)) => {
            let profile = resolve(&dir, name)?.ok_or_else(|| {
                eyre::eyre!(
                    "index profile '{name}' is not defined; \
                     `bookrack index-profile list` shows the available names"
                )
            })?;
            Some((profile, *origin))
        }
        None => None,
    };
    if let (Some((profile, _)), Some(explicit)) = (&resolved, root.embed_model.as_deref())
        && explicit != profile.embed.model
    {
        return Err(ConfigError::profile_model_conflict(
            &profile.name,
            &profile.embed.model,
            explicit,
        )
        .into());
    }

    let profile_model = resolved.as_ref().map(|(p, _)| p.embed.model.as_str());
    let effective_model = EmbedConfig::resolve(&root, profile_model).model;
    // An unstamped corpus reports as "no built index", same as a
    // missing one: there is nothing to compare a profile against.
    let built = crate::profile::built_stamps(&Pipeline::Books.corpus_db(&entry.data_dir))
        .filter(|b| !b.is_unstamped());
    let findings = match (&resolved, &built) {
        (Some((profile, _)), Some(stamps)) => {
            let target = Pipeline::Books.target_stamps(&profile.embed.model, profile.embed.dim);
            Some(crate::profile::profile_stamp_findings(&target, stamps))
        }
        _ => None,
    };

    if json {
        let value = serde_json::json!({
            "library": entry.name,
            "data_dir": entry.data_dir.display().to_string(),
            "profile": resolved.as_ref().map(|(p, origin)| serde_json::json!({
                "name": p.name,
                "origin": origin.as_str(),
            })),
            "effective_embed_model": effective_model,
            "built_stamps": built.as_ref().map(|b| serde_json::json!({
                "embed_model": b.embed_model,
                "vector_dim": b.vector_dim,
                "chunk_version": b.chunk_version,
                "normalize_version": b.normalize_version,
            })),
            "stamp_findings": findings,
            "consistent": findings.as_ref().map(|f| f.is_empty()),
        });
        println!("{value}");
        return Ok(());
    }

    println!("library: {} ({})", entry.name, entry.data_dir.display());
    match &resolved {
        Some((profile, origin)) => {
            println!("profile: {} (source: {})", profile.name, origin.as_str());
            println!(
                "  embed: {}/{} dim {}",
                profile.embed.backend, profile.embed.model, profile.embed.dim
            );
            println!(
                "  ann: {}{}",
                profile.ann.kind.as_str(),
                ann_params(profile)
            );
            match profile.reranker.kind {
                RerankerKind::None => println!("  reranker: none"),
                RerankerKind::CrossEncoder => println!(
                    "  reranker: cross-encoder ({}) — not implemented; \
                     the daemon refuses to start under this profile",
                    profile.reranker.model.as_deref().unwrap_or("<unset>")
                ),
            }
        }
        None => println!("profile: none (field-level configuration)"),
    }
    println!("effective embed model: {effective_model}");
    match (&built, &findings) {
        (None, _) => println!("stamps: no built index to compare against"),
        (Some(b), None) => {
            println!(
                "stamps: built index is {} (no profile to compare against)",
                stamp_pair(b)
            );
        }
        (Some(b), Some(f)) if f.is_empty() => {
            println!(
                "stamps: consistent with the built index ({})",
                stamp_pair(b)
            );
        }
        (Some(_), Some(f)) => {
            for finding in f {
                println!("stamp mismatch: {finding}");
            }
            println!("note: `bookrack index-profile apply` reconciles this");
        }
    }
    Ok(())
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

/// The registry entry `--library` names, or the registry default when
/// no name is given.
fn registry_entry(library: Option<&str>) -> Result<LibraryEntry> {
    let entries = list_libraries()?.unwrap_or_default();
    match library {
        Some(name) => entries
            .into_iter()
            .find(|e| e.name == name)
            .ok_or_else(|| eyre::eyre!("no library named '{name}' in the registry")),
        None => entries.into_iter().find(|e| e.is_default).ok_or_else(|| {
            eyre::eyre!("the registry has no default library; pass --library <name>")
        }),
    }
}

/// Compare two profiles field by field.
fn diff(a: &str, b: &str, json: bool) -> Result<()> {
    let dir = user_profile_dir().unwrap_or_else(|| PathBuf::from(USER_PROFILE_DIR_NAME));
    let profile_a = resolve(&dir, a)?.ok_or_else(|| eyre::eyre!("unknown index profile '{a}'"))?;
    let profile_b = resolve(&dir, b)?.ok_or_else(|| eyre::eyre!("unknown index profile '{b}'"))?;
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
    /// The registry entry of the library being reconciled.
    pub entry: LibraryEntry,
    /// The target profile.
    pub profile: IndexProfile,
    /// Per-pipeline sections, in [`Pipeline::ALL`] order.
    pub sections: Vec<PipelineSection>,
}

impl ApplyPlan {
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

    /// Whether the plan contains a re-embed step.
    pub fn has_reembed(&self) -> bool {
        self.actions()
            .iter()
            .any(|(_, a)| *a == PlannedAction::Reembed)
    }

    /// Whether every selected pipeline is already consistent or empty.
    pub fn is_noop(&self) -> bool {
        self.actions().is_empty()
    }
}

/// Derive the apply plan for profile `name` against the library
/// `--library` selects (registry default when `None`). Offline and
/// read-only: registry, `config.toml`, profile files, corpus stamps,
/// and the vector-store meta — no daemon involved. Refusals that are
/// the operator's to fix surface as [`ApplyRefusal`].
pub async fn plan_apply(
    name: &str,
    library: Option<&str>,
    filter: PipelineFilter,
) -> Result<ApplyPlan> {
    let entry = registry_entry(library)?;

    if let Some(refusal) = crate::profile::refuse_bad_profile_reference(name) {
        return Err(ApplyRefusal(refusal).into());
    }
    let dir = user_profile_dir().unwrap_or_else(|| PathBuf::from(USER_PROFILE_DIR_NAME));
    let profile =
        resolve(&dir, name)?.ok_or_else(|| eyre::eyre!("unknown index profile '{name}'"))?;
    if let Err(e) = ensure_reranker_supported(&profile) {
        return Err(ApplyRefusal(e.to_string()).into());
    }

    // Masking check: a set env override or an explicit `embed_model`
    // field outranks the profile at execution time, so the actions
    // below would run against the masked value. Refuse instead of
    // silently editing the operator's other configuration.
    let root = load_root_config(&entry.data_dir)?;
    let env_var =
        root_config_env_override("embed_model").expect("embed_model key has an env override");
    let env_model = std::env::var(env_var).ok();
    let conflicts = crate::profile::masking_conflicts(&profile, &root, env_model.as_deref());
    if !conflicts.is_empty() {
        return Err(ApplyRefusal(conflicts.join("\n")).into());
    }

    let mut sections = Vec::new();
    for pipeline in Pipeline::ALL {
        if !filter.selects(pipeline) {
            continue;
        }
        let state = read_pipeline_state(&entry.data_dir, pipeline)?;
        let target = pipeline.target_stamps(&profile.embed.model, profile.embed.dim);
        let plan = derive_pipeline_plan(&profile, &target, &state);
        let reembed_chunks = match &plan {
            PipelinePlan::Run(actions) if actions.contains(&PlannedAction::Reembed) => {
                count_chunks(&pipeline.lancedb_dir(&entry.data_dir), profile.embed.dim).await
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
        entry,
        profile,
        sections,
    })
}

/// Best-effort chunk-row count for the plan's scale hint. Read-only;
/// any failure (a dimension mismatch, a locked store) degrades to "no
/// count" rather than blocking the plan.
async fn count_chunks(lancedb_dir: &Path, dim: u32) -> Option<usize> {
    let store = ChunkStore::open(lancedb_dir, dim as usize).await.ok()?;
    store.count_rows().await.ok()
}

/// Print an apply plan, one section per pipeline. `queue_busy` carries
/// the daemon's pending+running job count when the caller is connected;
/// non-zero adds the queueing note.
pub fn render_apply_plan(plan: &ApplyPlan, queue_busy: Option<u32>) {
    println!(
        "library: {} ({})",
        plan.entry.name,
        plan.entry.data_dir.display()
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
        PlannedAction::ReconcileStamps => {
            format!("{ns}stamps reconcile — rewrites the four index stamps (metadata only)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_index_profile::PROFILE_QWEN3_06B_DEFAULT;

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
}
