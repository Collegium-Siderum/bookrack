// SPDX-License-Identifier: Apache-2.0

//! This crate's compiled-in values, collected for `config fixed`.
//!
//! Each constant keeps its own home next to the code that applies it
//! and carries a `// setting:` marker naming the key it appears under
//! here; the gate in the CLI's `fixed_inventory_gate` holds the two
//! together. What lives here is only the operator-facing wording — the
//! line explaining a value and the surface it acts on — so a reader
//! asking "what does this daemon decide for me" has one page to read
//! rather than eight files to find.

bookrack_core::fixed_settings! {
    owner = "runtime";
    "control.event_channel_capacity" = crate::control::events::DEFAULT_EVENT_CHANNEL_CAPACITY,
        "events the daemon buffers before a slow client starts losing them",
        acts on "every control-plane client following the event stream";
    "control.plan_ttl" = crate::control::plan_registry::DEFAULT_PLAN_TTL,
        "how long a dry-run plan stays presentable before it must be re-planned",
        acts on "every destructive method's execute leg, which fails with plan not found";
    "daemon.nofile_target" = crate::rlimit::NOFILE_TARGET,
        "open files the daemon raises its soft limit to at startup",
        acts on "a large batch ingest, which holds a file per index fragment";
    "dryrun.paper_reports_kept" = crate::cmd::papers_dryrun::PAPERS_DRYRUN_KEEP,
        "paper dry-run reports kept under the data root before the oldest is pruned",
        acts on "papers dryrun";
    "dryrun.reports_kept" = crate::cmd::dryrun::DRYRUN_KEEP,
        "book dry-run reports kept under the data root before the oldest is pruned",
        acts on "dryrun";
    "mcp.probe_timeout" = crate::mcp_endpoint::PROBE_TIMEOUT,
        "how long the health probe waits for the MCP endpoint to answer",
        acts on "doctor and bring-up, which report the endpoint unreachable past it";
    "reranker.crashloop_attempts" = crate::daemon::RERANK_CRASHLOOP_ATTEMPTS,
        "respawns within one outage after which the backend counts as crash-looping",
        acts on "the degraded state doctor and the status card report";
    "reranker.install_progress_step" = crate::reranker_install::PROGRESS_STEP,
        "bytes downloaded between two progress lines while a reranker model installs",
        acts on "doctor --install-reranker and the setup wizard";
    "reranker.request_backoff_base" = crate::rerank_supervisor::RERANK_BACKOFF_BASE,
        "pause before the first retry of a rerank call, doubling per attempt",
        acts on "every reranked search";
    "reranker.request_retries_max" = crate::rerank_supervisor::RERANK_MAX_RETRIES,
        "transport retries one rerank call makes before the query fails",
        acts on "every reranked search";
    "reranker.request_timeout" = crate::rerank_supervisor::RERANK_REQUEST_TIMEOUT,
        "how long one rerank call waits for the backend before it is given up on",
        acts on "every reranked search";
    "reranker.restart_backoff_cap" = crate::rerank_supervisor::RESTART_BACKOFF_CAP,
        "longest pause between two attempts to restart a failed reranker backend",
        acts on "how quickly a reranked search recovers after the backend dies";
}
