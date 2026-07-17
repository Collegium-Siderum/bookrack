// SPDX-License-Identifier: Apache-2.0

//! Effective index-profile assembly and the apply-plan derivation.
//!
//! A library's profile reference can sit in three places — its manifest,
//! its per-root `config.toml`, and its registry entry — and picking
//! between them lives in `bookrack_config`
//! ([`effective_profile_reference`]). This module reads those three
//! sources fresh per call, hands them to that chain, resolves the named
//! profile, and reconciles the result with the explicit `embed_model`
//! field and the env override; every embed-model consumer in this crate
//! goes through it so the `env > config.toml > profile > default` chain
//! applies uniformly.
//!
//! The second half of the module is the offline planning layer behind
//! `index-profile apply`: read the built stamps and the persisted ANN
//! configuration per pipeline, compare them against a target profile,
//! and derive the ordered action list that reconciles the difference.
//! The derivation is pure; executing the actions is the CLI
//! orchestrator's job.

use std::path::{Path, PathBuf};

use bookrack_config::{
    Config, ConfigError, EmbedConfig, RootConfig, effective_profile_reference, list_libraries,
    load_manifest, load_root_config, profile_reference_drift, registry_profile_ref_in,
    registry_target_path, root_config_env_override,
};
// The origin and drift types live in `bookrack_config` beside the
// resolution chain; re-exported so `crate::profile::ProfileRefOrigin`
// keeps naming them for this crate's callers.
pub use bookrack_config::{ProfileRefDrift, ProfileRefOrigin};
use bookrack_corpus::{
    CHUNK_VERSION_KEY, Corpus, EMBED_MODEL_KEY, IndexStamps, NORMALIZE_VERSION_KEY, VECTOR_DIM_KEY,
};
use bookrack_index_profile::{
    AnnKind, AnnSpec, IndexProfile, USER_PROFILE_DIR_NAME, has_errors, resolve, validate,
};
use bookrack_vectors::AnnConfig;
use eyre::{Result, eyre};

/// The profile a library effectively runs under, with the reference that
/// selected it.
#[derive(Debug, Clone)]
pub struct EffectiveProfile {
    /// Which source the reference was read from.
    pub origin: ProfileRefOrigin,
    /// The resolved profile.
    pub profile: IndexProfile,
    /// Sources naming a different profile than the effective one. Empty
    /// in the healthy case; a report, never a reason to refuse.
    pub drift: Vec<ProfileRefDrift>,
}

/// The per-user index-profile directory, beside `registry.toml`. `None`
/// when no config location resolves; built-in profiles still resolve by
/// name against the relative fallback the callers substitute.
pub fn user_profile_dir() -> Option<PathBuf> {
    registry_target_path()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .map(|d| d.join(USER_PROFILE_DIR_NAME))
}

/// [`user_profile_dir`] with the relative fallback every caller uses
/// when no config location resolves: user files then never match, so
/// only built-ins resolve.
pub fn user_profile_dir_or_default() -> PathBuf {
    user_profile_dir().unwrap_or_else(|| PathBuf::from(USER_PROFILE_DIR_NAME))
}

/// The `index_profile` the registry records for the library `cfg`
/// serves: matched by registry name when the selection carried one,
/// otherwise by data root. `None` when no entry matches or none records
/// a profile.
fn registry_profile_ref(cfg: &Config) -> Option<String> {
    let entries = list_libraries().ok().flatten()?;
    registry_profile_ref_in(&entries, cfg.library(), cfg.data_dir())
}

/// The three profile-reference sources, re-read from disk at call time.
///
/// Daemon RPC handlers hold a `Config` captured at bring-up; re-reading
/// keeps a declaration written after bring-up (e.g. by `index-profile
/// apply`, which declares before it acts) visible to every handler
/// without a restart. That is why the manifest is read here too and not
/// taken from a snapshot: the manifest is where a declaration now lands,
/// so a stale read would make declare-first silently ineffective.
/// Offline callers parse a fresh `Config` per process, so for them the
/// snapshot and the file agree anyway.
///
/// The root config falls back to `cfg`'s snapshot when the file cannot
/// be read. A manifest that cannot be read counts as absent: resolution
/// must not fail because a root carries a corrupt or future-versioned
/// identity file, matching how `identify_library` treats one.
fn fresh_profile_sources(cfg: &Config) -> (RootConfig, Option<String>) {
    let root = load_root_config(cfg.data_dir()).unwrap_or_else(|_| cfg.root_config().clone());
    let manifest_ref = load_manifest(cfg.data_dir())
        .ok()
        .flatten()
        .and_then(|m| m.index_profile);
    (root, manifest_ref)
}

/// Resolve the effective index profile for the library `cfg` serves.
///
/// `Ok(None)` when no source references a profile — the library then
/// runs on field-level configuration alone. Sources that disagree do not
/// fail: the highest-priority one wins and the rest are reported as
/// [`EffectiveProfile::drift`]. Errors when the referenced profile does
/// not resolve, or when the profile's model contradicts an explicit
/// `embed_model` in `config.toml`
/// ([`ConfigError::ProfileConfigConflict`]).
pub fn effective_index_profile(cfg: &Config) -> Result<Option<EffectiveProfile>> {
    let (root, manifest_ref) = fresh_profile_sources(cfg);
    effective_index_profile_in(cfg, &root, manifest_ref.as_deref())
}

/// [`effective_index_profile`] against already-read sources, so
/// [`effective_embed_config`] reads each file once per call.
fn effective_index_profile_in(
    cfg: &Config,
    root: &RootConfig,
    manifest_ref: Option<&str>,
) -> Result<Option<EffectiveProfile>> {
    let config_ref = root.index_profile.clone();
    let registry_ref = registry_profile_ref(cfg);
    let Some((name, origin)) =
        effective_profile_reference(manifest_ref, config_ref.as_deref(), registry_ref.as_deref())
    else {
        return Ok(None);
    };
    let drift =
        profile_reference_drift(manifest_ref, config_ref.as_deref(), registry_ref.as_deref());

    let dir = user_profile_dir_or_default();
    let profile = resolve(&dir, &name)
        .map_err(|e| eyre!("index profile '{name}' failed to load: {e}"))?
        .ok_or_else(|| {
            eyre!(
                "index profile '{name}' is not defined; \
                 `bookrack index-profile list` shows the available names"
            )
        })?;

    if let Some(explicit) = root.embed_model.as_deref()
        && explicit != profile.embed.model
    {
        return Err(
            ConfigError::profile_model_conflict(&name, &profile.embed.model, explicit).into(),
        );
    }
    Ok(Some(EffectiveProfile {
        origin,
        profile,
        drift,
    }))
}

/// The [`EmbedConfig`] for `cfg`'s library with every layer of the model
/// chain applied: env var > explicit `config.toml` field > the effective
/// profile's model > hardcoded default. Fails when the profile layer is
/// itself in conflict, so a handler cannot embed under a configuration
/// the daemon would refuse to start with.
pub fn effective_embed_config(cfg: &Config) -> Result<EmbedConfig> {
    let (root, manifest_ref) = fresh_profile_sources(cfg);
    let effective = effective_index_profile_in(cfg, &root, manifest_ref.as_deref())?;
    let model = effective.as_ref().map(|e| e.profile.embed.model.as_str());
    Ok(EmbedConfig::resolve(&root, model))
}

/// Write gate for an `index_profile` reference: the name must resolve
/// and pass static validation. Returns the human-readable refusal when
/// it does not; the caller maps it to its user-error exit path. Stamp
/// consistency is deliberately not checked — reconciling a valid
/// profile against a built index is `index-profile apply`'s job.
pub fn refuse_bad_profile_reference(name: &str) -> Option<String> {
    let dir = user_profile_dir_or_default();
    match resolve(&dir, name) {
        Err(e) => Some(format!("index profile '{name}' failed to load: {e}")),
        Ok(None) => Some(format!(
            "index profile '{name}' is not defined; \
             `bookrack index-profile list` shows the available names"
        )),
        Ok(Some(profile)) => has_errors(&validate(&profile, false)).then(|| {
            format!(
                "index profile '{name}' has validation errors; \
                 run `bookrack index-profile validate {name}`"
            )
        }),
    }
}

/// One of the two indexed pipelines a library can carry. The same
/// profile governs both; each pipeline keeps its own corpus database,
/// LanceDB directory, and stamp record, so plans derive per pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pipeline {
    /// The book pipeline: `corpus.db` and `lancedb/`.
    Books,
    /// The paper pipeline: `papers_corpus.db` and `lancedb_papers/`.
    Papers,
}

impl Pipeline {
    /// Both pipelines, in rendering order.
    pub const ALL: [Pipeline; 2] = [Pipeline::Books, Pipeline::Papers];

    /// Stable label for section headers and JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Pipeline::Books => "books",
            Pipeline::Papers => "papers",
        }
    }

    /// The pipeline's corpus database under `data_dir`. Mirrors
    /// [`bookrack_config::Config::corpus_db`] and
    /// [`bookrack_config::Config::papers_corpus_db`], which take a full
    /// resolved `Config` this registry-driven path does not have.
    pub fn corpus_db(self, data_dir: &Path) -> PathBuf {
        match self {
            Pipeline::Books => data_dir.join("corpus.db"),
            Pipeline::Papers => data_dir.join("papers_corpus.db"),
        }
    }

    /// The pipeline's LanceDB directory under `data_dir`. Mirrors
    /// [`bookrack_config::Config::lancedb_dir`] and
    /// [`bookrack_config::Config::papers_lancedb_dir`].
    pub fn lancedb_dir(self, data_dir: &Path) -> PathBuf {
        match self {
            Pipeline::Books => data_dir.join("lancedb"),
            Pipeline::Papers => data_dir.join("lancedb_papers"),
        }
    }

    /// The stamps this binary would record for the pipeline after a
    /// clean build under `model`/`dim`. The chunking constant differs
    /// per pipeline: books chunk under `bookrack_ingest`, papers under
    /// `bookrack_glean`.
    pub fn target_stamps(self, model: &str, dim: u32) -> IndexStamps {
        match self {
            Pipeline::Books => bookrack_ingest::current_index_stamps(model, dim),
            Pipeline::Papers => bookrack_glean::stamps::current_index_stamps(model, dim),
        }
    }
}

/// The four index stamps as recorded in a corpus database, each `None`
/// when its key is unset. Distinguishes "stamped with a different
/// value" (a real divergence) from "not stamped at all" (metadata
/// drift a `stamps reconcile` repairs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuiltStamps {
    /// `index_meta` `embed_model`.
    pub embed_model: Option<String>,
    /// `index_meta` `vector_dim`.
    pub vector_dim: Option<u32>,
    /// `index_meta` `chunk_version`.
    pub chunk_version: Option<u32>,
    /// `index_meta` `normalize_version`.
    pub normalize_version: Option<u32>,
}

impl BuiltStamps {
    /// `true` when no stamp key is set at all.
    pub fn is_unstamped(&self) -> bool {
        self.embed_model.is_none()
            && self.vector_dim.is_none()
            && self.chunk_version.is_none()
            && self.normalize_version.is_none()
    }

    /// `true` when every stamp key is set.
    pub fn is_fully_stamped(&self) -> bool {
        self.embed_model.is_some()
            && self.vector_dim.is_some()
            && self.chunk_version.is_some()
            && self.normalize_version.is_some()
    }

    /// The `(embed_model, vector_dim)` pair when both are stamped — the
    /// two-field view callers that predate the four-stamp record use.
    pub fn embed_pair(&self) -> Option<(String, u32)> {
        Some((self.embed_model.clone()?, self.vector_dim?))
    }
}

/// The index stamps recorded in the corpus database at `corpus_db`, or
/// `None` when the database is missing or cannot be opened (so a check
/// skips rather than racing a live writer). A database that opens but
/// carries no stamps returns an unstamped record, not `None`.
pub fn built_stamps(corpus_db: &Path) -> Option<BuiltStamps> {
    let corpus = Corpus::open_read_only(corpus_db).ok()?;
    let get = |key: &str| corpus.meta_get(key).ok().flatten();
    Some(BuiltStamps {
        embed_model: get(EMBED_MODEL_KEY),
        vector_dim: get(VECTOR_DIM_KEY).and_then(|v| v.parse().ok()),
        chunk_version: get(CHUNK_VERSION_KEY).and_then(|v| v.parse().ok()),
        normalize_version: get(NORMALIZE_VERSION_KEY).and_then(|v| v.parse().ok()),
    })
}

/// Field-level differences between the stamps a clean build would
/// record (`target`) and the stamps actually recorded (`built`). Empty
/// means consistent. A key that is stamped with another value reports
/// the divergence; a key that is missing entirely reports the gap.
pub fn profile_stamp_findings(target: &IndexStamps, built: &BuiltStamps) -> Vec<String> {
    let mut findings = Vec::new();
    match built.embed_model.as_deref() {
        Some(model) if model != target.embed_model => findings.push(format!(
            "embed.model: profile declares '{}' but the built index is stamped '{model}'",
            target.embed_model
        )),
        Some(_) => {}
        None => findings.push("embed.model: the built index has no embed_model stamp".to_string()),
    }
    match built.vector_dim {
        Some(dim) if dim != target.vector_dim => findings.push(format!(
            "embed.dim: profile declares {} but the built index is stamped {dim}",
            target.vector_dim
        )),
        Some(_) => {}
        None => findings.push("embed.dim: the built index has no vector_dim stamp".to_string()),
    }
    match built.chunk_version {
        Some(v) if v != target.chunk_version => findings.push(format!(
            "chunk_version: this binary chunks at version {} but the built index \
             is stamped {v}",
            target.chunk_version
        )),
        Some(_) => {}
        None => {
            findings.push("chunk_version: the built index has no chunk_version stamp".to_string())
        }
    }
    match built.normalize_version {
        Some(v) if v != target.normalize_version => findings.push(format!(
            "normalize_version: this binary normalizes at version {} but the built \
             index is stamped {v}",
            target.normalize_version
        )),
        Some(_) => {}
        None => findings
            .push("normalize_version: the built index has no normalize_version stamp".to_string()),
    }
    findings
}

/// Everything the plan derivation reads from one pipeline's on-disk
/// state. Assembled by [`read_pipeline_state`]; kept as plain data so
/// the derivation itself stays pure and table-testable.
#[derive(Debug, Clone, Default)]
pub struct PipelineState {
    /// The recorded stamps, or `None` when the corpus database is
    /// missing or unopenable.
    pub built: Option<BuiltStamps>,
    /// The persisted ANN configuration from `vectors_meta.json`, or
    /// `None` when no meta file exists (a fresh or legacy store).
    pub ann: Option<AnnConfig>,
    /// Whether the pipeline's LanceDB chunks table exists on disk.
    pub has_chunks: bool,
}

/// Read one pipeline's [`PipelineState`] from disk. Offline and
/// read-only. A malformed `vectors_meta.json` is an error rather than a
/// silent "no ANN": deriving a rebuild over a store this binary cannot
/// interpret would guess where the operator must decide.
pub fn read_pipeline_state(data_dir: &Path, pipeline: Pipeline) -> Result<PipelineState> {
    let lancedb_dir = pipeline.lancedb_dir(data_dir);
    let ann = match bookrack_vectors::meta::load(&lancedb_dir) {
        Ok(None) => None,
        Ok(Some(meta)) => Some(AnnConfig::from_meta(&meta).map_err(|e| {
            eyre!(
                "{} vectors_meta.json is not readable by this binary: {e}",
                pipeline.as_str()
            )
        })?),
        Err(e) => {
            return Err(eyre!(
                "{} vectors_meta.json failed to load: {e}",
                pipeline.as_str()
            ));
        }
    };
    Ok(PipelineState {
        built: built_stamps(&pipeline.corpus_db(data_dir)),
        ann,
        // The chunks table materializes as a `chunks.lance` directory;
        // checking for it avoids opening the store just to probe
        // existence.
        has_chunks: lancedb_dir.join("chunks.lance").is_dir(),
    })
}

/// One reconciliation step `index-profile apply` can execute. Ordering
/// within a plan follows the declaration order of this enum: re-embed
/// before rebuilding the ANN index, stamp reconciliation last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlannedAction {
    /// Drop the chunks table and re-chunk + re-embed from the corpus
    /// node tree (`vectors reset`). Destructive: the old vectors are
    /// unrecoverable.
    Reset,
    /// Re-derive chunks in place and re-embed them with the target
    /// model (`vectors reembed`).
    Reembed,
    /// Rebuild the ANN index with the profile's ANN parameters
    /// (`vectors rebuild`). Non-destructive.
    Rebuild,
    /// Drop the ANN index so search runs as an exhaustive scan
    /// (`vectors drop`), for a profile that declares `brute-force`.
    /// Non-destructive; a rebuild re-creates the index.
    DropIndex,
    /// Rewrite the four index stamps from a live embedder probe
    /// (`stamps reconcile`). Non-destructive; repairs metadata drift.
    ReconcileStamps,
}

impl PlannedAction {
    /// Stable label for plan rendering.
    pub fn as_str(self) -> &'static str {
        match self {
            PlannedAction::Reset => "reset",
            PlannedAction::Reembed => "reembed",
            PlannedAction::Rebuild => "rebuild",
            PlannedAction::DropIndex => "drop-index",
            PlannedAction::ReconcileStamps => "reconcile-stamps",
        }
    }

    /// `true` for actions that discard data irrecoverably.
    pub fn is_destructive(self) -> bool {
        matches!(self, PlannedAction::Reset)
    }
}

/// What one pipeline needs to reach the target profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelinePlan {
    /// The pipeline is not in use: its corpus database is absent, or
    /// nothing is stamped and no chunks exist. No action derives —
    /// running one would fail against the empty store.
    Empty,
    /// Stamps and ANN configuration already match the target.
    Consistent,
    /// The ordered actions that reconcile the pipeline.
    Run(Vec<PlannedAction>),
}

/// Derive one pipeline's plan from the target profile and the on-disk
/// state. Pure. `target` carries the stamps a clean build under the
/// profile would record ([`Pipeline::target_stamps`]).
///
/// Rules, most destructive first, each later rule applying only when no
/// earlier one fired:
///
/// 1. A stamped embed model or vector dimension that differs from the
///    profile derives a [`PlannedAction::Reset`]. The reset re-chunks
///    and re-embeds everything — absorbing any re-embed or reconcile —
///    but it also removes `vectors_meta.json` with the chunks table, so
///    a profile that declares an index gets the follow-up
///    [`PlannedAction::Rebuild`] that realizes it.
/// 2. A stamped chunk or normalize version that differs from this
///    binary derives a [`PlannedAction::Reembed`].
/// 3. The declared ANN state is realized: an index-declaring profile
///    derives a [`PlannedAction::Rebuild`] when the persisted
///    configuration differs or is absent; a `brute-force` profile
///    derives a [`PlannedAction::DropIndex`] when an index is
///    persisted (an absent configuration already is the scan state).
/// 4. Stamp keys that are missing while every present one matches
///    derive a [`PlannedAction::ReconcileStamps`], unless a re-embed is
///    already planned (it rewrites the stamps itself).
pub fn derive_pipeline_plan(
    profile: &IndexProfile,
    target: &IndexStamps,
    state: &PipelineState,
) -> PipelinePlan {
    let Some(built) = &state.built else {
        return PipelinePlan::Empty;
    };
    if built.is_unstamped() && !state.has_chunks {
        return PipelinePlan::Empty;
    }

    let model_diverges = built
        .embed_model
        .as_deref()
        .is_some_and(|m| m != target.embed_model);
    let dim_diverges = built.vector_dim.is_some_and(|d| d != target.vector_dim);
    if model_diverges || dim_diverges {
        let mut actions = vec![PlannedAction::Reset];
        if profile.ann.kind != AnnKind::BruteForce {
            actions.push(PlannedAction::Rebuild);
        }
        return PipelinePlan::Run(actions);
    }

    let mut actions = Vec::new();
    let chunks_stale = built
        .chunk_version
        .is_some_and(|v| v != target.chunk_version)
        || built
            .normalize_version
            .is_some_and(|v| v != target.normalize_version);
    if chunks_stale {
        actions.push(PlannedAction::Reembed);
    }
    if state.has_chunks
        && let Some(action) = ann_action(state.ann.as_ref(), &profile.ann)
    {
        actions.push(action);
    }
    if !built.is_fully_stamped() && !actions.contains(&PlannedAction::Reembed) {
        actions.push(PlannedAction::ReconcileStamps);
    }

    if actions.is_empty() {
        PipelinePlan::Consistent
    } else {
        PipelinePlan::Run(actions)
    }
}

/// The action (if any) that brings the persisted ANN state to what the
/// profile declares. `current` is the decoded `vectors_meta.json`;
/// `None` means no index was ever built, i.e. the exhaustive-scan
/// state.
fn ann_action(current: Option<&AnnConfig>, spec: &AnnSpec) -> Option<PlannedAction> {
    let declares_scan = spec.kind == AnnKind::BruteForce;
    match current {
        None => (!declares_scan).then_some(PlannedAction::Rebuild),
        Some(cur) if declares_scan => {
            // Parameters are meaningless for a scan; only the kind
            // decides whether an index must be dropped.
            (cur.kind.as_str() != AnnKind::BruteForce.as_str()).then_some(PlannedAction::DropIndex)
        }
        Some(cur) => (!ann_matches_profile(cur, spec)).then_some(PlannedAction::Rebuild),
    }
}

/// Whether a persisted ANN configuration already realizes a profile's
/// ANN knob: same kind and the same six parameters, query-time ones
/// included (they persist in `vectors_meta.json` as defaults).
pub fn ann_matches_profile(current: &AnnConfig, spec: &AnnSpec) -> bool {
    current.kind.as_str() == spec.kind.as_str()
        && current.num_partitions == spec.num_partitions
        && current.num_sub_vectors == spec.num_sub_vectors
        && current.num_bits == spec.num_bits
        && current.nprobes == spec.nprobes
        && current.refine_factor == spec.refine_factor
}

/// Configuration layers that would mask the target profile's embed
/// model at execution time: the env override and the explicit
/// `config.toml` field both outrank the profile in the resolution
/// chain, so an apply running under either would embed with the masked
/// value while declaring the profile — the worst of both. One message
/// per conflicting layer, each with its removal instruction; empty
/// means the profile's model would take effect. Pure: the caller
/// supplies the current env value.
pub fn masking_conflicts(
    profile: &IndexProfile,
    root: &RootConfig,
    env_model: Option<&str>,
) -> Vec<String> {
    let env_var =
        root_config_env_override("embed_model").expect("embed_model key has an env override");
    let mut conflicts = Vec::new();
    if let Some(env) = env_model.map(str::trim).filter(|v| !v.is_empty())
        && env != profile.embed.model
    {
        conflicts.push(format!(
            "{env_var} is set to '{env}' and overrides every configuration layer, \
             masking profile '{}' (embed model '{}'); unset {env_var} and re-run",
            profile.name, profile.embed.model
        ));
    }
    if let Some(explicit) = root.embed_model.as_deref()
        && explicit != profile.embed.model
    {
        conflicts.push(
            ConfigError::profile_model_conflict(&profile.name, &profile.embed.model, explicit)
                .to_string(),
        );
    }
    conflicts
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_index_profile::PROFILE_QWEN3_06B_DEFAULT;

    fn profile() -> IndexProfile {
        IndexProfile::from_named(PROFILE_QWEN3_06B_DEFAULT).expect("built-in")
    }

    fn target(profile: &IndexProfile) -> IndexStamps {
        Pipeline::Books.target_stamps(&profile.embed.model, profile.embed.dim)
    }

    fn stamped(target: &IndexStamps) -> BuiltStamps {
        BuiltStamps {
            embed_model: Some(target.embed_model.clone()),
            vector_dim: Some(target.vector_dim),
            chunk_version: Some(target.chunk_version),
            normalize_version: Some(target.normalize_version),
        }
    }

    fn matching_ann(profile: &IndexProfile) -> AnnConfig {
        AnnConfig {
            kind: profile.ann.kind.as_str().parse().expect("kind round-trips"),
            num_partitions: profile.ann.num_partitions,
            num_sub_vectors: profile.ann.num_sub_vectors,
            num_bits: profile.ann.num_bits,
            nprobes: profile.ann.nprobes,
            refine_factor: profile.ann.refine_factor,
        }
    }

    fn consistent_state(profile: &IndexProfile, target: &IndexStamps) -> PipelineState {
        PipelineState {
            built: Some(stamped(target)),
            ann: Some(matching_ann(profile)),
            has_chunks: true,
        }
    }

    #[test]
    fn stamp_findings_report_each_divergent_or_missing_field() {
        let profile = profile();
        let target = target(&profile);

        assert!(profile_stamp_findings(&target, &stamped(&target)).is_empty());

        let mut wrong_model = stamped(&target);
        wrong_model.embed_model = Some("other-model".to_string());
        let findings = profile_stamp_findings(&target, &wrong_model);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].contains("embed.model"));

        let mut wrong_both = wrong_model.clone();
        wrong_both.vector_dim = Some(target.vector_dim + 1);
        assert_eq!(profile_stamp_findings(&target, &wrong_both).len(), 2);

        let mut missing = stamped(&target);
        missing.chunk_version = None;
        let findings = profile_stamp_findings(&target, &missing);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].contains("no chunk_version stamp"));

        let mut stale = stamped(&target);
        stale.normalize_version = Some(target.normalize_version + 1);
        let findings = profile_stamp_findings(&target, &stale);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].contains("normalize_version"));
    }

    #[test]
    fn derivation_covers_every_action_class() {
        let profile = profile();
        let target = target(&profile);
        let consistent = consistent_state(&profile, &target);

        // Missing corpus database: the pipeline is not in use.
        let missing_corpus = PipelineState {
            built: None,
            ..consistent.clone()
        };
        assert_eq!(
            derive_pipeline_plan(&profile, &target, &missing_corpus),
            PipelinePlan::Empty
        );

        // Unstamped and chunkless: also not in use.
        let bare = PipelineState {
            built: Some(BuiltStamps::default()),
            ann: None,
            has_chunks: false,
        };
        assert_eq!(
            derive_pipeline_plan(&profile, &target, &bare),
            PipelinePlan::Empty
        );

        // Everything matching: nothing to do.
        assert_eq!(
            derive_pipeline_plan(&profile, &target, &consistent),
            PipelinePlan::Consistent
        );

        // A different stamped model: destructive reset. The reset
        // removes the ANN meta with the chunks table, so realizing the
        // profile's declared index takes the follow-up rebuild.
        let mut other_model = consistent.clone();
        other_model.built.as_mut().expect("stamps").embed_model = Some("other".to_string());
        assert_eq!(
            derive_pipeline_plan(&profile, &target, &other_model),
            PipelinePlan::Run(vec![PlannedAction::Reset, PlannedAction::Rebuild])
        );

        // A different stamped dimension: same plan.
        let mut other_dim = consistent.clone();
        other_dim.built.as_mut().expect("stamps").vector_dim = Some(target.vector_dim + 1);
        assert_eq!(
            derive_pipeline_plan(&profile, &target, &other_dim),
            PipelinePlan::Run(vec![PlannedAction::Reset, PlannedAction::Rebuild])
        );

        // Model change combined with an ANN change: no separate rebuild
        // beyond the one the reset already entails.
        let mut mixed = other_model.clone();
        mixed.ann.as_mut().expect("ann").num_partitions += 1;
        assert_eq!(
            derive_pipeline_plan(&profile, &target, &mixed),
            PipelinePlan::Run(vec![PlannedAction::Reset, PlannedAction::Rebuild])
        );

        // A model change under a brute-force profile: the reset's
        // post-state (no index, exhaustive scan) is the declared state,
        // so nothing follows it.
        let mut scan_profile = profile.clone();
        scan_profile.ann.kind = AnnKind::BruteForce;
        assert_eq!(
            derive_pipeline_plan(&scan_profile, &target, &other_model),
            PipelinePlan::Run(vec![PlannedAction::Reset])
        );

        // A brute-force profile over a built index: drop it; over a
        // store with no index meta: already the declared state.
        assert_eq!(
            derive_pipeline_plan(&scan_profile, &target, &consistent),
            PipelinePlan::Run(vec![PlannedAction::DropIndex])
        );
        let mut no_index = consistent.clone();
        no_index.ann = None;
        assert_eq!(
            derive_pipeline_plan(&scan_profile, &target, &no_index),
            PipelinePlan::Consistent
        );

        // A stale chunk version under the same model: re-embed.
        let mut stale_chunks = consistent.clone();
        stale_chunks.built.as_mut().expect("stamps").chunk_version = Some(target.chunk_version + 1);
        assert_eq!(
            derive_pipeline_plan(&profile, &target, &stale_chunks),
            PipelinePlan::Run(vec![PlannedAction::Reembed])
        );

        // A stale normalize version: same class.
        let mut stale_norm = consistent.clone();
        stale_norm.built.as_mut().expect("stamps").normalize_version =
            Some(target.normalize_version + 1);
        assert_eq!(
            derive_pipeline_plan(&profile, &target, &stale_norm),
            PipelinePlan::Run(vec![PlannedAction::Reembed])
        );

        // Only the ANN parameters diverge: a rebuild with the profile's
        // parameters, nothing destructive.
        let mut ann_diverges = consistent.clone();
        ann_diverges.ann.as_mut().expect("ann").nprobes += 1;
        assert_eq!(
            derive_pipeline_plan(&profile, &target, &ann_diverges),
            PipelinePlan::Run(vec![PlannedAction::Rebuild])
        );

        // Chunks exist but no vectors_meta.json was ever written: the
        // declared ANN state is unrealized, so a rebuild establishes it.
        let mut no_meta = consistent.clone();
        no_meta.ann = None;
        assert_eq!(
            derive_pipeline_plan(&profile, &target, &no_meta),
            PipelinePlan::Run(vec![PlannedAction::Rebuild])
        );

        // Stale chunks and a divergent ANN combine, re-embed first.
        let mut both = stale_chunks.clone();
        both.ann.as_mut().expect("ann").nprobes += 1;
        assert_eq!(
            derive_pipeline_plan(&profile, &target, &both),
            PipelinePlan::Run(vec![PlannedAction::Reembed, PlannedAction::Rebuild])
        );

        // A missing stamp key while everything present matches:
        // metadata drift, repaired by a reconcile.
        let mut drift = consistent.clone();
        drift.built.as_mut().expect("stamps").vector_dim = None;
        assert_eq!(
            derive_pipeline_plan(&profile, &target, &drift),
            PipelinePlan::Run(vec![PlannedAction::ReconcileStamps])
        );

        // Drift alongside a planned re-embed: the re-embed rewrites the
        // stamps itself, so no separate reconcile derives.
        let mut drift_and_stale = stale_chunks.clone();
        drift_and_stale.built.as_mut().expect("stamps").vector_dim = None;
        assert_eq!(
            derive_pipeline_plan(&profile, &target, &drift_and_stale),
            PipelinePlan::Run(vec![PlannedAction::Reembed])
        );
    }

    #[test]
    fn masking_conflicts_flag_env_and_explicit_field() {
        let profile = profile();
        let clean = RootConfig::default();

        assert!(masking_conflicts(&profile, &clean, None).is_empty());
        // A matching env value or explicit field masks nothing.
        assert!(masking_conflicts(&profile, &clean, Some(&profile.embed.model)).is_empty());
        // Whitespace-only env values are treated as unset, matching the
        // resolution chain's own trimming.
        assert!(masking_conflicts(&profile, &clean, Some("  ")).is_empty());

        let conflicts = masking_conflicts(&profile, &clean, Some("other-model"));
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].contains("BOOKRACK_EMBED_MODEL"));
        assert!(conflicts[0].contains("unset"));

        let explicit = RootConfig {
            embed_model: Some("other-model".to_string()),
            ..RootConfig::default()
        };
        let conflicts = masking_conflicts(&profile, &explicit, None);
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].contains("embed_model"));

        // Both layers conflicting report both, env first.
        let conflicts = masking_conflicts(&profile, &explicit, Some("third-model"));
        assert_eq!(conflicts.len(), 2);
    }
}
