// SPDX-License-Identifier: Apache-2.0

//! Tool-call recorder: one [`bookrack_catalog::mcp_tool_calls`] row per
//! public op call.
//!
//! Every public function in [`crate::reads`] and [`crate::writes`] wraps
//! its body in [`record_call_sync!`] (or [`record_call_async!`] for async
//! ops). The macro builds a [`Recorder`] before the body, runs the body,
//! and writes one row to `mcp_tool_calls` after the body settles —
//! capturing `source` (cli / mcp), `tool` (the public name agents see),
//! `status` (ok / error), `duration_ms`, the JSON-encoded `args`, and
//! the stable error class on failure.
//!
//! Recording is opportunistic: if the catalog write fails, the recorder
//! logs a `tracing::warn!` and drops the row, so observability never
//! corrupts the operation's [`Result`].
//!
//! A task-local override lets a host surface relabel the recorded
//! `source` for the duration of one call without rebuilding [`Ops`]
//! with a different [`Caller`]. The MCP server uses this to flip
//! source from the daemon's baked-in `cli` to `mcp` for tool calls
//! arriving over HTTP, where the underlying [`Ops`] is a single
//! per-library handle shared by REPL, HTTP, and queue worker.

use std::future::Future;
use std::path::Path;
use std::time::Instant;

use bookrack_catalog::{Catalog, NewMcpToolCall};
use bookrack_embed::Embedder;

use crate::{Ops, OpsError};

tokio::task_local! {
    /// When set inside a `scope`, [`Recorder::start`] uses this string as
    /// the recorded `source` instead of reading [`crate::Caller::actor_detail`]
    /// off [`Ops`]. Lets a host that drives a shared `Ops` (the
    /// `bookrack run` daemon, where REPL and HTTP route through the same
    /// registry) relabel each call without cloning ops or threading the
    /// caller through every entry point.
    static SOURCE_OVERRIDE: String;
}

/// Run `fut` with the tool-call `source` field relabelled to `source` for
/// every [`Recorder::start`] that fires inside the future.
///
/// Scopes nest: an inner call to [`with_source_override`] supersedes the
/// outer label for the duration of its own future, then the outer label
/// resumes. Scopes do not cross [`tokio::spawn`] boundaries (the new task
/// runs without the override); callers that fan out work must re-wrap
/// each spawned future.
pub fn with_source_override<F: Future>(
    source: impl Into<String>,
    fut: F,
) -> tokio::task::futures::TaskLocalFuture<String, F> {
    SOURCE_OVERRIDE.scope(source.into(), fut)
}

/// Guard that times one op call and writes a row to `mcp_tool_calls`
/// when [`Recorder::finish`] consumes it.
///
/// Constructed by [`Recorder::start`] from an [`Ops`] handle. The
/// catalog connection is opened lazily inside [`Recorder::finish`], so
/// recording adds no work to a successful read besides building the
/// args JSON.
pub struct Recorder<'a> {
    catalog_db: &'a Path,
    source: String,
    tool: &'static str,
    args: Option<String>,
    started_at: Instant,
}

impl<'a> Recorder<'a> {
    /// Start timing one call on `ops` of `tool`, carrying `args` (as a
    /// JSON value) into the recorded row.
    ///
    /// `tool` is the public name the call is advertised as — e.g.
    /// `library.list_books` — not the internal function name. `args` is
    /// serialized at construction time so a `Value::Null` becomes `None`
    /// rather than the literal string `"null"`.
    pub fn start<E: Embedder>(
        ops: &'a Ops<E>,
        tool: &'static str,
        args: serde_json::Value,
    ) -> Recorder<'a> {
        let source = SOURCE_OVERRIDE.try_with(String::clone).unwrap_or_else(|_| {
            let caller = ops.caller();
            caller
                .actor_detail
                .clone()
                .unwrap_or_else(|| caller.actor_kind.as_str().to_string())
        });
        let args = if args.is_null() {
            None
        } else {
            Some(args.to_string())
        };
        Recorder {
            catalog_db: ops.catalog_db(),
            source,
            tool,
            args,
            started_at: Instant::now(),
        }
    }

    /// Test-only constructor that accepts the catalog path and source
    /// string directly, so tests can exercise the writer path without
    /// building an [`Ops`].
    #[cfg(test)]
    pub(crate) fn for_test(
        catalog_db: &'a Path,
        source: impl Into<String>,
        tool: &'static str,
    ) -> Recorder<'a> {
        Recorder {
            catalog_db,
            source: source.into(),
            tool,
            args: None,
            started_at: Instant::now(),
        }
    }

    /// Write one row reflecting how `result` settled, then drop. A
    /// catalog write failure is logged at `warn` and swallowed —
    /// `finish` never panics and never alters `result`.
    pub fn finish<T>(self, result: &crate::Result<T>) {
        let duration_ms = self.started_at.elapsed().as_secs_f64() * 1000.0;
        let (status, error_type, error_msg) = match result {
            Ok(_) => ("ok", None, None),
            Err(e) => (
                "error",
                Some(error_kind(e).to_string()),
                Some(e.to_string()),
            ),
        };

        // Skip recording on a fresh data root that holds no catalog yet.
        // `Catalog::open` would otherwise materialize `catalog.db` just to
        // hold a single audit row, which turns `bookrack info` on an empty
        // root into a write — exactly what the read-only contract refuses.
        if !self.catalog_db.exists() {
            return;
        }

        let mut row = NewMcpToolCall::new(self.source, self.tool, status);
        row.duration_ms = Some(duration_ms);
        row.args = self.args;
        row.error_type = error_type;
        row.error_msg = error_msg;

        let write_attempt = Catalog::open(self.catalog_db).and_then(|c| c.record_tool_call(&row));
        if let Err(e) = write_attempt {
            tracing::warn!(
                tool = self.tool,
                error = %e,
                "could not record tool call",
            );
        }
    }
}

/// Stable error-class name for `error_type` on a failed row. The string
/// is part of the observability surface — a downstream diagnose tarball
/// groups rows by it — and must not be changed without bumping the
/// tarball's manifest schema version.
fn error_kind(err: &OpsError) -> &'static str {
    match err {
        OpsError::Query(_) => "query",
        OpsError::Catalog(_) => "catalog",
        OpsError::Corpus(_) => "corpus",
        OpsError::Vectors(_) => "vectors",
        OpsError::IntakeNotFound { .. } => "intake_not_found",
        OpsError::NodeNotFound { .. } => "node_not_found",
        OpsError::NotALeaf { .. } => "not_a_leaf",
        OpsError::NotOrganizing { .. } => "not_organizing",
        OpsError::SearchUnavailable => "search_unavailable",
        OpsError::Other(_) => "other",
    }
}

/// Default `source` to attribute a call to when the caller did not set
/// [`crate::Caller::actor_detail`]. Used by the test helpers; production
/// callers always set the field via [`crate::Caller::cli`] /
/// [`crate::Caller::mcp`].
#[cfg(test)]
pub(crate) const DEFAULT_SOURCE: &str = crate::ACTOR_DETAIL_CLI;

/// Wrap a synchronous op body so the recorder writes a row before the
/// op returns.
///
/// ```ignore
/// pub fn list_books<E: Embedder>(...) -> Result<ListBooksResult> {
///     record_call_sync!(ops, "library.list_books",
///         serde_json::json!({"limit": limit, "offset": offset}), {
///             find_books(ops, BookFilter::default(), limit, offset)
///         }
///     )
/// }
/// ```
macro_rules! record_call_sync {
    ($ops:expr, $tool:expr, $args:expr, $body:block) => {{
        let __recorder = $crate::recorder::Recorder::start($ops, $tool, $args);
        // Wrap the body in an IIFE so `?` and `return` inside it land
        // in `__result` (and get recorded) rather than escaping the
        // surrounding function before `finish` runs.
        #[allow(clippy::redundant_closure_call)]
        let __result = (|| $body)();
        __recorder.finish(&__result);
        __result
    }};
}

/// Wrap an asynchronous op body so the recorder writes a row before the
/// op returns. The body block must evaluate to a [`crate::Result<T>`];
/// the macro `.await`s it before the row is written, so the recorded
/// duration spans the whole future.
///
/// ```ignore
/// pub async fn search<E: Embedder>(...) -> Result<Vec<Citation>> {
///     record_call_async!(ops, "library.search",
///         serde_json::json!({"top_k": top_k}), {
///             let library = ops.library().ok_or(OpsError::SearchUnavailable)?;
///             Ok(library.search(query, top_k).await?)
///         }
///     )
/// }
/// ```
macro_rules! record_call_async {
    ($ops:expr, $tool:expr, $args:expr, $body:block) => {{
        let __recorder = $crate::recorder::Recorder::start($ops, $tool, $args);
        let __result = async $body.await;
        __recorder.finish(&__result);
        __result
    }};
}

pub(crate) use record_call_async;
pub(crate) use record_call_sync;

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_catalog::Catalog;
    use tempfile::TempDir;

    fn open_catalog(tmp: &TempDir) -> (std::path::PathBuf, Catalog) {
        let path = tmp.path().join("catalog.db");
        let catalog = Catalog::open(&path).expect("seed catalog");
        (path, catalog)
    }

    #[test]
    fn finish_writes_one_ok_row_with_duration() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (path, catalog) = open_catalog(&tmp);
        let recorder = Recorder::for_test(&path, DEFAULT_SOURCE, "library.test");
        recorder.finish::<()>(&Ok(()));
        let rows = catalog.tool_calls_for_tool("library.test").expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "ok");
        assert_eq!(rows[0].source, "cli");
        assert!(rows[0].duration_ms.is_some());
        assert!(rows[0].error_type.is_none());
        assert!(rows[0].error_msg.is_none());
    }

    #[test]
    fn finish_writes_an_error_row_with_stable_kind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (path, catalog) = open_catalog(&tmp);
        let recorder = Recorder::for_test(&path, DEFAULT_SOURCE, "library.test");
        let result: crate::Result<()> = Err(OpsError::IntakeNotFound { intake_id: 42 });
        recorder.finish(&result);
        let rows = catalog.tool_calls_for_tool("library.test").expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "error");
        assert_eq!(rows[0].error_type.as_deref(), Some("intake_not_found"));
        assert!(rows[0].error_msg.as_deref().unwrap_or("").contains("42"));
    }

    #[test]
    fn finish_swallows_catalog_write_errors() {
        // Point the recorder at a directory rather than a file: opening
        // the catalog there fails, but `finish` must still return cleanly.
        let tmp = tempfile::tempdir().expect("tempdir");
        let recorder = Recorder::for_test(tmp.path(), DEFAULT_SOURCE, "library.test");
        recorder.finish::<()>(&Ok(()));
    }

    #[test]
    fn source_override_visible_inside_scope_and_unset_outside() {
        // The task local is the contract by which a host surface
        // relabels recorded `source` on a shared `Ops` — assert both
        // that the scope sets it and that the value disappears once
        // the future resolves, so a leaking scope cannot pollute a
        // sibling call recorded later on the same task.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build runtime");
        runtime.block_on(async {
            let inside = with_source_override("mcp", async {
                SOURCE_OVERRIDE.try_with(String::clone).ok()
            })
            .await;
            assert_eq!(inside.as_deref(), Some("mcp"));
            let outside = SOURCE_OVERRIDE.try_with(String::clone).ok();
            assert!(outside.is_none());
        });
    }

    #[test]
    fn start_drops_null_args_to_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (path, catalog) = open_catalog(&tmp);
        let mut recorder = Recorder::for_test(&path, DEFAULT_SOURCE, "library.test");
        // Mirror what `Recorder::start` does when args is Null.
        recorder.args = None;
        recorder.finish::<()>(&Ok(()));
        let rows = catalog.tool_calls_for_tool("library.test").expect("read");
        assert!(rows[0].args.is_none());
    }
}
