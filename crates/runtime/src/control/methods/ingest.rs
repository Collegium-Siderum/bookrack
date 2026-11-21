// SPDX-License-Identifier: Apache-2.0

//! `ingest.submit` and `ingest.cancel` JSON-RPC handlers.
//!
//! Neither method writes the catalog, corpus, or vector store — both
//! only mutate the on-disk queue document. A rule submission reads: the
//! rule file, the listing of its directory, and, when it names a
//! source PDF, that file and the library's catalog. The actual
//! ingest runs out of the worker loop, where the daemon-state flag and
//! the broadcast notifications already fire. As a result these
//! handlers do not take the write mutex and do not transition the
//! daemon into [`crate::control::events::DaemonState::Writing`].
//! They still broadcast a [`crate::control::events::QueueTick`] after
//! persisting so connected clients update their view immediately.

// Parallel implementation: control-intake.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{MethodContext, input_err};
use crate::audit_helpers::require_known_profile;
use crate::cmd::input_error::CmdInputError;
use crate::control::error_map::handler_err;
use crate::control::events::Event;
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};
use crate::queue::{self, Priority};

#[derive(Debug, Deserialize)]
pub struct IngestSubmitParams {
    /// The files to ingest, one job each. Absent in the rule form.
    #[serde(default)]
    paths: Vec<PathBuf>,
    /// A rule file to ingest as one book, in place of `paths`. An
    /// absolute path.
    #[serde(default)]
    rule: Option<PathBuf>,
    /// The scan PDF the rule's text was produced from. An absolute
    /// path; only with `rule`.
    #[serde(default)]
    source: Option<PathBuf>,
    /// Accept rule text that leaves pages of `source` uncovered. Only
    /// with `source`.
    #[serde(default)]
    allow_partial: bool,
    #[serde(default)]
    library: Option<String>,
    #[serde(default)]
    priority: Option<PriorityRepr>,
    #[serde(default)]
    force: bool,
    /// When `true`, every directory in `paths` is walked depth-first
    /// and expanded to the set of supported-format files inside it
    /// before enqueueing. Files passed directly are still enqueued
    /// verbatim. When `false` (default), directory paths reach the
    /// queue worker as-is, which treats them as ingest failures.
    #[serde(default)]
    recursive: bool,
    /// Retired. `true` is refused; `false`, which every earlier client
    /// sends, is accepted and ignored.
    #[serde(default)]
    hold_for_metadata: bool,
    /// Optional book-side audit profile name. When unset, the
    /// daemon's startup profile applies. When it names a built-in
    /// (`default` / `trust-source` / `strict`), every enqueued job
    /// carries the override and the worker reloads it before running
    /// the ingest. Any other name is refused as invalid params, before
    /// a job is queued.
    #[serde(default)]
    audit_profile: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PriorityRepr {
    Low,
    Normal,
    High,
}

impl PriorityRepr {
    pub(super) fn into_priority(self) -> Priority {
        match self {
            PriorityRepr::Low => Priority::Low,
            PriorityRepr::Normal => Priority::Normal,
            PriorityRepr::High => Priority::High,
        }
    }
}

pub async fn submit(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let mut parsed: IngestSubmitParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(INVALID_PARAMS, format!("invalid ingest.submit params: {e}"))
        })?,
        _ => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "missing ingest.submit params",
            ));
        }
    };
    if let Some(refusal) = form_refusal(&parsed) {
        return Err(input_err(refusal));
    }
    if parsed.rule.is_none() && parsed.paths.is_empty() {
        return Err(RpcError::new(INVALID_PARAMS, "paths: must be non-empty"));
    }
    require_known_profile(
        parsed.audit_profile.as_deref(),
        bookrack_audit_profile::ALL_BUILT_IN_NAMES,
    )
    .map_err(input_err)?;
    let library = super::resolve_submit_target(ctx, parsed.library.as_deref())?;
    let priority = parsed
        .priority
        .take()
        .map(PriorityRepr::into_priority)
        .unwrap_or_default();
    if let Some(rule) = parsed.rule.clone() {
        return submit_rule(ctx, rule, parsed, library, priority).await;
    }
    // Paths that arrive without a directory walk are checked against the
    // extractor's allowlist up front, so an unsupported file is refused
    // here instead of becoming a job that fails at EXTRACT. Walked paths
    // are already filtered by `collect_supported_files`.
    let check = |path: &PathBuf| -> Result<(), RpcError> {
        queue::check_extension_supported(path).map_err(|msg| RpcError::new(INVALID_PARAMS, msg))
    };
    let expanded: Vec<PathBuf> = if parsed.recursive {
        let mut out = Vec::new();
        for path in &parsed.paths {
            if path.is_dir() {
                let mut found = queue::collect_supported_files(path).map_err(|e| {
                    RpcError::new(INTERNAL_ERROR, format!("walk {}: {e}", path.display()))
                })?;
                out.append(&mut found);
            } else {
                check(path)?;
                out.push(path.clone());
            }
        }
        out
    } else {
        for path in &parsed.paths {
            check(path)?;
        }
        parsed.paths.clone()
    };
    if expanded.is_empty() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "no supported files found in submitted paths",
        ));
    }
    let (ids, tick) = super::queue_writes::enqueue_against(ctx, &library, |guard| {
        queue::enqueue_files(
            guard,
            &expanded,
            &library,
            bookrack_core::ItemKind::Book,
            priority,
            parsed.force,
            parsed.audit_profile.clone(),
        )
    })?;
    ctx.event_stream.publish(Event::QueueTick(tick));
    Ok(json!({ "job_ids": ids }))
}

/// Why `params` asks for the retired metadata hold, mixes the file form
/// with the rule form, or names a rule-form field the rest of the call
/// gives no meaning to; `None` when the call is one form or the other.
fn form_refusal(params: &IngestSubmitParams) -> Option<CmdInputError> {
    let refused = |summary: &str, hint: &str| {
        Some(CmdInputError::Refused {
            summary: summary.to_string(),
            hint: Some(hint.to_string()),
        })
    };
    if params.hold_for_metadata {
        return refused(
            "`hold_for_metadata` is no longer accepted",
            "Submit without it: a book is embedded as it is ingested, and metadata edits \
             take effect without re-embedding. `bookrack dryrun` shows a source's audit \
             verdict without writing to the library. docs/UPGRADE.md covers the change.",
        );
    }
    let Some(rule) = &params.rule else {
        if params.source.is_some() {
            return refused(
                "`source` applies only to a `rule` submission",
                "Name the rule file in `rule`, or drop `source`.",
            );
        }
        if params.allow_partial {
            return refused(
                "`allow_partial` applies only to a `rule` submission",
                "Name the rule file in `rule`, or drop `allow_partial`.",
            );
        }
        return None;
    };
    if !params.paths.is_empty() {
        return refused(
            "`rule` cannot be submitted together with `paths`",
            "Send the rule file in one call and the files in another.",
        );
    }
    if params.recursive {
        return refused(
            "`recursive` does not apply to a `rule` submission",
            "Send the rule without `recursive`: the rule names its own member files.",
        );
    }
    if params.allow_partial && params.source.is_none() {
        return refused(
            "`allow_partial` needs a `source`",
            "Name the scan PDF in `source`, or drop `allow_partial`.",
        );
    }
    const ABSOLUTE: &str =
        "Send an absolute path: the daemon does not share the caller's working directory.";
    if !rule.is_absolute() {
        return refused(
            &format!("rule path \"{}\" is not absolute", rule.display()),
            ABSOLUTE,
        );
    }
    match &params.source {
        Some(source) if !source.is_absolute() => refused(
            &format!("source path \"{}\" is not absolute", source.display()),
            ABSOLUTE,
        ),
        _ => None,
    }
}

/// Queue one job for the rule file at `rule`, once the library's own
/// check of the submission passes. A rule that cannot run, or a source
/// PDF the library is not waiting on, is refused here and no row is
/// written.
async fn submit_rule(
    ctx: &MethodContext,
    rule: PathBuf,
    parsed: IngestSubmitParams,
    library: String,
    priority: Priority,
) -> Result<Value, RpcError> {
    let handle = super::select_library(ctx, Some(&library))?;
    let (checked_rule, checked_source) = (rule.clone(), parsed.source.clone());
    tokio::task::spawn_blocking(move || {
        handle.check_rule_submission(&checked_rule, checked_source.as_deref())
    })
    .await
    .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("ingest.submit: join error: {e}")))?
    .map_err(|e| handler_err("ingest.submit", e))?;
    let (id, tick) = super::queue_writes::enqueue_against(ctx, &library, |guard| {
        queue::enqueue_rule(
            guard,
            rule,
            parsed.source,
            parsed.allow_partial,
            &library,
            priority,
            parsed.force,
            parsed.audit_profile,
        )
    })?;
    ctx.event_stream.publish(Event::QueueTick(tick));
    Ok(json!({ "job_ids": [id] }))
}

#[derive(Debug, Deserialize)]
pub struct IngestCancelParams {
    job_id: String,
}

/// Cancel the one pending or running job whose id contains `job_id`
/// anywhere. The reply carries the full id the fragment resolved to.
pub async fn cancel(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: IngestCancelParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(INVALID_PARAMS, format!("invalid ingest.cancel params: {e}"))
        })?,
        _ => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "missing ingest.cancel params",
            ));
        }
    };
    let (full_id, tick) = super::queue_writes::mutate_jobs_and_persist(
        &ctx.queue_state,
        &ctx.queue_state_path,
        |guard| {
            queue::cancel_matching(guard, &parsed.job_id).map_err(|err| {
                RpcError::new(
                    crate::control::jsonrpc::JOB_NOT_FOUND,
                    format!("ingest.cancel: {err}"),
                )
            })
        },
    )?;
    ctx.event_stream.publish(Event::QueueTick(tick));
    Ok(json!({ "ok": true, "job_id": full_id }))
}

routed_params!(IngestSubmitParams);
