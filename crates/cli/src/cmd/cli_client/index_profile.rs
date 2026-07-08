// SPDX-License-Identifier: Apache-2.0

//! `bookrack index-profile apply` — derive the reconciliation plan
//! offline, confirm it, write the target declaration, then drive the
//! low-level control-plane methods in plan order.
//!
//! The declaration (`config.toml` + registry entry) is written before
//! the first action runs: every action handler resolves its target
//! model through the effective-profile chain at execution time, so a
//! declaration written afterwards would have the actions run against
//! the old profile. A failure mid-plan therefore leaves the declaration
//! ahead of the stamps; `index-profile current` reports the divergence
//! and re-running `apply` re-derives only the remaining actions.

use std::path::Path;
use std::sync::Arc;

use bookrack_cli::error::BookrackCliError;
use bookrack_cli::render::confirm::{ConfirmMode, confirm_destructive};
use bookrack_config::{
    LibraryEntryFields, LibrarySelection, registry_target_path, set_root_config_values,
    upsert_library_entry,
};
use bookrack_control_client::ControlClient;
use bookrack_runtime::cmd::index_profile::{
    ApplyPlan, ApplyRefusal, PipelineFilter, plan_apply, render_apply_plan,
};
use bookrack_runtime::profile::{Pipeline, PlannedAction};
use eyre::{Context, Result};
use serde_json::{Value, json};

use super::helpers;

pub async fn run(
    name: &str,
    library: Option<&str>,
    pipeline: PipelineFilter,
    dry_run: bool,
    yes: bool,
    runtime_dir: Option<&Path>,
) -> Result<()> {
    // Plan derivation is offline and read-only; refusals that are the
    // operator's to fix map to the user-error exit code.
    let plan = plan_apply(name, library, pipeline)
        .await
        .map_err(as_user_error)?;

    if dry_run {
        render_apply_plan(&plan, None);
        if plan.is_noop() {
            println!("nothing to do: the library already matches '{name}'");
        } else if plan.has_destructive() {
            print_destructive_paths(&plan.entry.name);
        }
        return Ok(());
    }

    if plan.is_noop() {
        render_apply_plan(&plan, None);
        println!("nothing to do: the library already matches '{name}'");
        return Ok(());
    }

    // Executing routes through the daemon that owns the library; refuse
    // up front when a running daemon serves a different one than the
    // plan targets, instead of resetting the wrong store.
    crate::preflight::enforce_selection_mismatch(&LibrarySelection {
        data_dir: None,
        library: Some(plan.entry.name.clone()),
    })?;

    let client = helpers::connect(runtime_dir).await?;
    let status = helpers::dispatch(&client, "status", Value::Null).await?;
    if !status
        .get("queue_worker_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true)
    {
        return Err(BookrackCliError::LocalUserError {
            message: "queue worker unavailable: the daemon runs headless without one, so \
                      apply cannot execute any action; restart the daemon with a queue \
                      worker and re-run"
                .to_string(),
        }
        .into());
    }
    let queue_busy = ["queue_pending", "queue_running"]
        .iter()
        .map(|k| status.get(k).and_then(Value::as_u64).unwrap_or(0))
        .sum::<u64>() as u32;

    render_apply_plan(&plan, Some(queue_busy));
    if !confirm_plan(&plan, yes)? {
        println!("aborted; no changes written");
        if plan.has_destructive() {
            print_destructive_paths(&plan.entry.name);
        }
        return Ok(());
    }

    declare_target(&plan, name)?;
    execute_plan(&client, &plan, name).await?;

    // Self-check: re-derive against the on-disk state the actions left
    // behind. Anything but a no-op plan means the apply did not
    // converge and the operator should look before re-running.
    let verify = plan_apply(name, library, pipeline).await?;
    if !verify.is_noop() {
        eyre::bail!(
            "every action completed but the library still diverges from '{name}'; \
             `bookrack index-profile current --library {}` shows the remaining findings",
            plan.entry.name
        );
    }
    println!("profile applied and verified");
    if plan
        .actions()
        .iter()
        .any(|(_, a)| matches!(a, PlannedAction::Reset | PlannedAction::Reembed))
    {
        println!("restart the daemon so search picks up the new model.");
    }
    Ok(())
}

/// Map a planning refusal to the CLI's user-error exit path; every
/// other failure keeps the internal-error one.
fn as_user_error(err: eyre::Report) -> eyre::Report {
    match err.downcast_ref::<ApplyRefusal>() {
        Some(refusal) => BookrackCliError::LocalUserError {
            message: refusal.to_string(),
        }
        .into(),
        None => err,
    }
}

/// The two ways forward when the destructive guardrail stops a plan.
fn print_destructive_paths(library: &str) {
    println!("the plan contains a destructive reset; two ways to proceed:");
    println!(
        "  1. rehearse: `bookrack libraries fork` a copy, apply there, verify, then apply here"
    );
    println!(
        "  2. execute in place: re-run with `--yes`, or confirm with the library name '{library}'"
    );
}

/// How strongly a plan must be confirmed, from its worst action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmStrength {
    /// Only non-destructive index/metadata work: run unprompted, same
    /// as the underlying verbs.
    None,
    /// A re-embed overwrites vectors in place: a soft `yes`.
    Soft,
    /// A reset discards data irrecoverably: the library name retyped
    /// verbatim — the confirmation is about this library, not one verb.
    Hard,
}

fn confirm_strength(has_destructive: bool, has_reembed: bool) -> ConfirmStrength {
    if has_destructive {
        ConfirmStrength::Hard
    } else if has_reembed {
        ConfirmStrength::Soft
    } else {
        ConfirmStrength::None
    }
}

/// Confirm the plan at [`confirm_strength`]. `--yes` skips every
/// prompt; without it a non-interactive stdin is refused rather than
/// left hanging on a read.
fn confirm_plan(plan: &ApplyPlan, yes: bool) -> Result<bool> {
    use std::io::IsTerminal;

    let mode = match confirm_strength(plan.has_destructive(), plan.has_reembed()) {
        ConfirmStrength::None => None,
        ConfirmStrength::Hard => Some((
            ConfirmMode::Hard {
                token: &plan.entry.name,
            },
            format!(
                "This plan RESETS the vector store: existing vectors are dropped and\n\
                 re-embedded; the old vectors are unrecoverable.\n\
                 Type the library name '{}' (exact) to continue:",
                plan.entry.name
            ),
        )),
        ConfirmStrength::Soft => Some((
            ConfirmMode::Soft,
            "This plan re-embeds existing chunks in place; vectors are overwritten\n\
             by fresh embeddings. Type 'yes' to continue:"
                .to_string(),
        )),
    };
    let Some((mode, prompt)) = mode else {
        return Ok(true);
    };
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Err(BookrackCliError::LocalUserError {
            message: format!(
                "index-profile apply needs confirmation for this plan and stdin is not a \
                 TTY; re-run with --yes, or rehearse on a fork first \
                 (`bookrack libraries fork`); library: '{}'",
                plan.entry.name
            ),
        }
        .into());
    }
    confirm_destructive(&prompt, mode, false).context("read index-profile apply confirmation")
}

/// Write the target declaration into both reference sites —
/// `<data_root>/config.toml` and the registry entry — before any action
/// runs, so every handler resolves the new profile at execution time.
/// The two sites are written to the same name, which keeps the
/// reference-conflict check green.
fn declare_target(plan: &ApplyPlan, name: &str) -> Result<()> {
    set_root_config_values(
        &plan.entry.data_dir,
        &[("index_profile".to_string(), name.to_string())],
        &[],
    )
    .with_context(|| {
        format!(
            "write index_profile into {}/config.toml",
            plan.entry.data_dir.display()
        )
    })?;
    let registry_path = registry_target_path().ok_or_else(|| {
        eyre::eyre!(
            "no registry location: set BOOKRACK_REGISTRY=<path> or ensure the platform \
             config directory is available"
        )
    })?;
    let fields = LibraryEntryFields {
        data_dir: plan.entry.data_dir.clone(),
        kind: plan.entry.kind,
        description: plan.entry.description.clone(),
        index_profile: Some(name.to_string()),
        created_at: plan.entry.created_at.clone(),
        uuid: plan.entry.uuid.clone(),
    };
    upsert_library_entry(&registry_path, &plan.entry.name, &fields).with_context(|| {
        format!(
            "record index_profile on '{}' in {}",
            plan.entry.name,
            registry_path.display()
        )
    })?;
    println!("declared: index_profile = '{name}' (config.toml + registry entry)");
    Ok(())
}

/// Execute the plan's actions in order, one RPC at a time. A failure
/// stops the run immediately, prints what completed and what never
/// started, and leaves recovery to a re-derived apply — plans and
/// `plan_id`s are never persisted or reused across runs.
async fn execute_plan(client: &Arc<ControlClient>, plan: &ApplyPlan, name: &str) -> Result<()> {
    let actions = plan.actions();
    let total = actions.len();
    for (index, (pipeline, action)) in actions.iter().enumerate() {
        let label = action_label(*pipeline, *action);
        println!("[{}/{total}] {label} ...", index + 1);
        if let Err(err) = execute_action(client, *pipeline, *action, plan).await {
            print_interruption(&actions, index, name);
            return Err(err);
        }
        println!("[{}/{total}] {label}: done", index + 1);
    }
    Ok(())
}

/// One action's display label, namespaced by pipeline.
fn action_label(pipeline: Pipeline, action: PlannedAction) -> String {
    format!("{} {}", pipeline.as_str(), action.as_str())
}

/// The completed / not-started summary and the recovery pointers an
/// interrupted apply leaves behind.
fn print_interruption(actions: &[(Pipeline, PlannedAction)], failed: usize, name: &str) {
    println!();
    println!(
        "apply interrupted at {}",
        action_label(actions[failed].0, actions[failed].1)
    );
    if failed == 0 {
        println!("completed: none");
    } else {
        for (pipeline, action) in &actions[..failed] {
            println!("completed: {}", action_label(*pipeline, *action));
        }
    }
    for (pipeline, action) in &actions[failed + 1..] {
        println!("not started: {}", action_label(*pipeline, *action));
    }
    println!(
        "the declaration is already written, so `bookrack index-profile current` \
         reports the remaining divergence"
    );
    println!(
        "check `bookrack queue list` before retrying — the daemon-side job may still be running"
    );
    println!(
        "rerun `bookrack index-profile apply {name}` to re-derive the remaining actions; \
         an interrupted reset can also be finished with `bookrack vectors reset --resume` \
         (books) or `bookrack papers vectors reset --resume` (papers)"
    );
}

/// The control-plane method one action maps to for one pipeline.
fn method_for(pipeline: Pipeline, action: PlannedAction) -> &'static str {
    match (pipeline, action) {
        (Pipeline::Books, PlannedAction::Reset) => "vectors.reset",
        (Pipeline::Books, PlannedAction::Reembed) => "vectors.reembed",
        (Pipeline::Books, PlannedAction::Rebuild) => "vectors.rebuild",
        (Pipeline::Books, PlannedAction::ReconcileStamps) => "stamps.reconcile",
        (Pipeline::Papers, PlannedAction::Reset) => "papers.vectors_reset",
        (Pipeline::Papers, PlannedAction::Reembed) => "papers.vectors_reembed",
        (Pipeline::Papers, PlannedAction::Rebuild) => "papers.vectors_rebuild",
        (Pipeline::Papers, PlannedAction::ReconcileStamps) => "papers.stamps_reconcile",
    }
}

/// Drive one action through its method. The orchestrator satisfies
/// every handler-side `yes` requirement itself — the operator already
/// confirmed the whole plan, so no step asks again.
async fn execute_action(
    client: &Arc<ControlClient>,
    pipeline: Pipeline,
    action: PlannedAction,
    plan: &ApplyPlan,
) -> Result<()> {
    let method = method_for(pipeline, action);
    match action {
        PlannedAction::Reset => {
            helpers::call_with_progress(
                Arc::clone(client),
                method,
                json!({ "resume": false, "yes": true }),
            )
            .await
        }
        PlannedAction::Rebuild => {
            let ann = &plan.profile.ann;
            helpers::call_with_progress(
                Arc::clone(client),
                method,
                json!({
                    "kind": ann.kind.as_str(),
                    "num_partitions": ann.num_partitions,
                    "num_sub_vectors": ann.num_sub_vectors,
                    "num_bits": ann.num_bits,
                    "nprobes": ann.nprobes,
                    "refine_factor": ann.refine_factor,
                }),
            )
            .await
        }
        PlannedAction::ReconcileStamps => {
            helpers::call_with_progress(Arc::clone(client), method, Value::Null).await
        }
        PlannedAction::Reembed => {
            // The pinned two-leg protocol: a fresh dry-run leg computes
            // and registers the plan, the execute leg presents its id.
            // The id is single-use and scoped to this connection, so an
            // interrupted run re-derives from scratch instead of
            // reusing a stale plan.
            let reembed_plan = helpers::call_with_progress_value(
                Arc::clone(client),
                method,
                json!({ "stale_only": false, "dry_run": true }),
            )
            .await?;
            helpers::print_value(&reembed_plan);
            let plan_id = reembed_plan
                .get("plan_id")
                .and_then(Value::as_str)
                .map(String::from)
                .ok_or_else(|| {
                    eyre::eyre!("{method}: daemon dry-run response did not include a plan_id")
                })?;
            let outcome = helpers::call_with_progress_value(
                Arc::clone(client),
                method,
                json!({ "plan_id": plan_id, "yes": true }),
            )
            .await?;
            helpers::print_value(&outcome);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_strength_tracks_the_worst_action() {
        // A destructive reset dominates everything else.
        assert_eq!(confirm_strength(true, false), ConfirmStrength::Hard);
        assert_eq!(confirm_strength(true, true), ConfirmStrength::Hard);
        // A re-embed alone is soft; anything else runs unprompted.
        assert_eq!(confirm_strength(false, true), ConfirmStrength::Soft);
        assert_eq!(confirm_strength(false, false), ConfirmStrength::None);
    }

    #[test]
    fn every_pipeline_action_pair_maps_to_its_namespace() {
        // Books map to the bare namespaces, papers to the prefixed
        // ones; the pairing is what keeps apply from resetting the
        // wrong store.
        assert_eq!(
            method_for(Pipeline::Books, PlannedAction::Reset),
            "vectors.reset"
        );
        assert_eq!(
            method_for(Pipeline::Books, PlannedAction::Reembed),
            "vectors.reembed"
        );
        assert_eq!(
            method_for(Pipeline::Books, PlannedAction::Rebuild),
            "vectors.rebuild"
        );
        assert_eq!(
            method_for(Pipeline::Books, PlannedAction::ReconcileStamps),
            "stamps.reconcile"
        );
        assert_eq!(
            method_for(Pipeline::Papers, PlannedAction::Reset),
            "papers.vectors_reset"
        );
        assert_eq!(
            method_for(Pipeline::Papers, PlannedAction::Reembed),
            "papers.vectors_reembed"
        );
        assert_eq!(
            method_for(Pipeline::Papers, PlannedAction::Rebuild),
            "papers.vectors_rebuild"
        );
        assert_eq!(
            method_for(Pipeline::Papers, PlannedAction::ReconcileStamps),
            "papers.stamps_reconcile"
        );
    }
}
