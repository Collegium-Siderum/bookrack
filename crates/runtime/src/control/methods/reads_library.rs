// SPDX-License-Identifier: Apache-2.0

//! Read-side control-plane methods that mirror the MCP `library.*`
//! read tools. Each method accepts the same JSON shape as its MCP
//! counterpart, runs the same `bookrack_ops::reads::*` call, and
//! returns the same body. Together they form the operator-side read
//! pathway: `bookrack exec <method> '<json>'` over the control socket
//! reaches the same code path agents exercise over MCP HTTP.

use std::sync::Arc;

use bookrack_core::{ItemKind, KindedNodeId, NodeId};
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::dto::{BookFilter, PaperFilter};
use bookrack_ops::registry::LibraryHandle;
use bookrack_ops::{OpsError, SearchOptions, reads};
use serde::Deserialize;
use serde_json::{Value, json};

use super::MethodContext;
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};

/// Mirror of the MCP-side `READ_CONTEXT_DEFAULT_RADIUS`. The
/// `library.read_context` tool returns this many leaves on each side
/// of the anchor when the caller omits `before` / `after`.
const READ_CONTEXT_DEFAULT_RADIUS: u32 = 3;

#[derive(Debug, Deserialize, Default)]
pub struct LibraryOnlyParams {
    #[serde(default)]
    pub library: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BookIdParams {
    pub intake_id: i64,
    #[serde(default)]
    pub library: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct PageParams {
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub offset: Option<u32>,
    #[serde(default)]
    pub library: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FindBooksParams {
    #[serde(default)]
    pub title_substring: Option<String>,
    #[serde(default)]
    pub contributor_name: Option<String>,
    #[serde(default)]
    pub contributor_role: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub categories: Option<Vec<String>>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub offset: Option<u32>,
    #[serde(default)]
    pub library: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub query: String,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default)]
    pub bypass_index: bool,
    #[serde(default)]
    pub nprobes: Option<usize>,
    #[serde(default)]
    pub refine_factor: Option<u32>,
    #[serde(default)]
    pub library: Option<String>,
    /// Which side of the library to search: `"book"` (the default —
    /// existing behaviour), `"paper"` (only the paper-side store), or
    /// `"all"` (both stores, merged by ascending distance).
    #[serde(default)]
    pub kind: Option<String>,
}

impl SearchParams {
    fn overrides(&self) -> SearchOptions {
        SearchOptions {
            bypass_index: self.bypass_index,
            nprobes: self.nprobes,
            refine_factor: self.refine_factor,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SearchInBookParams {
    pub intake_id: i64,
    pub query: String,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default)]
    pub bypass_index: bool,
    #[serde(default)]
    pub nprobes: Option<usize>,
    #[serde(default)]
    pub refine_factor: Option<u32>,
    #[serde(default)]
    pub library: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FindPapersParams {
    #[serde(default)]
    pub title_substring: Option<String>,
    #[serde(default)]
    pub contributor_name: Option<String>,
    #[serde(default)]
    pub year: Option<String>,
    #[serde(default)]
    pub venue_substring: Option<String>,
    #[serde(default)]
    pub doi: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub offset: Option<u32>,
    #[serde(default)]
    pub library: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchInPaperParams {
    pub intake_id: i64,
    pub query: String,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default)]
    pub bypass_index: bool,
    #[serde(default)]
    pub nprobes: Option<usize>,
    #[serde(default)]
    pub refine_factor: Option<u32>,
    #[serde(default)]
    pub library: Option<String>,
}

impl SearchInPaperParams {
    fn overrides(&self) -> SearchOptions {
        SearchOptions {
            bypass_index: self.bypass_index,
            nprobes: self.nprobes,
            refine_factor: self.refine_factor,
        }
    }
}

impl SearchInBookParams {
    fn overrides(&self) -> SearchOptions {
        SearchOptions {
            bypass_index: self.bypass_index,
            nprobes: self.nprobes,
            refine_factor: self.refine_factor,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ReadContextParams {
    pub node_id: i64,
    /// Pipeline kind of the anchor node. Defaults to
    /// [`ItemKind::Book`] so existing RPC clients stay green.
    #[serde(default)]
    pub kind: ItemKind,
    #[serde(default)]
    pub before: Option<u32>,
    #[serde(default)]
    pub after: Option<u32>,
    #[serde(default)]
    pub library: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReadSpanParams {
    pub node_id: i64,
    /// Pipeline kind of the target organizing node. Defaults to
    /// [`ItemKind::Book`] so existing RPC clients stay green.
    #[serde(default)]
    pub kind: ItemKind,
    #[serde(default)]
    pub start_after: Option<i64>,
    #[serde(default)]
    pub library: Option<String>,
}

fn parse<T>(params: &Option<Value>, method: &str) -> Result<T, RpcError>
where
    T: for<'de> Deserialize<'de>,
{
    let v = params
        .as_ref()
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| json!({}));
    serde_json::from_value(v)
        .map_err(|e| RpcError::new(INVALID_PARAMS, format!("invalid {method} params: {e}")))
}

fn resolve(
    ctx: &MethodContext,
    library: Option<&str>,
) -> Result<Arc<LibraryHandle<OllamaEmbedClient>>, RpcError> {
    ctx.registry
        .get(library)
        .map_err(|e| RpcError::new(INVALID_PARAMS, format!("registry: {e}")))
}

fn ops_internal(e: OpsError) -> RpcError {
    RpcError::new(INTERNAL_ERROR, e.to_string())
}

fn ops_invalid(e: OpsError) -> RpcError {
    RpcError::new(INVALID_PARAMS, e.to_string())
}

fn to_value<T: serde::Serialize + ?Sized>(v: &T) -> Result<Value, RpcError> {
    serde_json::to_value(v)
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("serialise response: {e}")))
}

pub fn stats(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: LibraryOnlyParams = parse(params, "library.stats")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    let stats = reads::books::show_stats(handle.ops()).map_err(ops_internal)?;
    to_value(&stats)
}

pub fn list_books(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: PageParams = parse(params, "library.list_books")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    let page = reads::books::list_books(handle.ops(), p.limit.unwrap_or(0), p.offset.unwrap_or(0))
        .map_err(ops_internal)?;
    to_value(&page)
}

pub fn find_books(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: FindBooksParams = parse(params, "library.find_books")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    let filter = BookFilter {
        title_substring: p.title_substring,
        contributor_name: p.contributor_name,
        contributor_role: p.contributor_role,
        format: p.format,
        categories: p.categories.unwrap_or_default(),
        ..BookFilter::default()
    };
    let page = reads::books::find_books(
        handle.ops(),
        filter,
        p.limit.unwrap_or(0),
        p.offset.unwrap_or(0),
    )
    .map_err(ops_internal)?;
    to_value(&page)
}

pub fn show_book(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: BookIdParams = parse(params, "library.show_book")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    match reads::books::show_book(handle.ops(), p.intake_id) {
        Ok(detail) => to_value(&Some(detail)),
        Err(OpsError::IntakeNotFound { .. }) => Ok(Value::Null),
        Err(e) => Err(ops_internal(e)),
    }
}

pub fn show_toc(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: BookIdParams = parse(params, "library.show_toc")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    match reads::books::show_toc(handle.ops(), p.intake_id) {
        Ok(toc) => to_value(&Some(toc)),
        Err(OpsError::IntakeNotFound { .. }) => Ok(Value::Null),
        Err(e) => Err(ops_internal(e)),
    }
}

pub fn read_context(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: ReadContextParams = parse(params, "library.read_context")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    let before = p.before.unwrap_or(READ_CONTEXT_DEFAULT_RADIUS);
    let after = p.after.unwrap_or(READ_CONTEXT_DEFAULT_RADIUS);
    let target = KindedNodeId {
        kind: p.kind,
        node_id: NodeId::new(p.node_id),
    };
    match reads::passages::read_context(handle.ops(), target, before, after) {
        Ok(window) => to_value(&Some(window)),
        Err(OpsError::NodeNotFound { .. }) => Ok(Value::Null),
        Err(e @ OpsError::NotALeaf { .. }) => Err(ops_invalid(e)),
        Err(e) => Err(ops_internal(e)),
    }
}

pub fn read_span(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: ReadSpanParams = parse(params, "library.read_span")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    let target = KindedNodeId {
        kind: p.kind,
        node_id: NodeId::new(p.node_id),
    };
    match reads::passages::read_span(handle.ops(), target, p.start_after) {
        Ok(span) => to_value(&Some(span)),
        Err(OpsError::NodeNotFound { .. }) => Ok(Value::Null),
        Err(e @ OpsError::NotOrganizing { .. }) => Err(ops_invalid(e)),
        Err(e) => Err(ops_internal(e)),
    }
}

pub fn show_metadata_audit(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: BookIdParams = parse(params, "library.show_metadata_audit")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    match reads::metadata::show_metadata_audit(handle.ops(), p.intake_id) {
        Ok(report) => to_value(&Some(report)),
        Err(OpsError::IntakeNotFound { .. }) => Ok(Value::Null),
        Err(e) => Err(ops_internal(e)),
    }
}

pub fn show_metadata_report(
    params: &Option<Value>,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    let p: BookIdParams = parse(params, "library.show_metadata_report")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    let audit_data = bookrack_ops::AuditData::default_data();
    let audit_profile = bookrack_ops::AuditProfile::default();
    match reads::metadata::show_metadata_report(
        handle.ops(),
        p.intake_id,
        &audit_data,
        &audit_profile,
    ) {
        Ok(report) => to_value(&Some(report)),
        Err(OpsError::IntakeNotFound { .. }) => Ok(Value::Null),
        Err(e) => Err(ops_internal(e)),
    }
}

pub fn list_metadata(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: PageParams = parse(params, "library.list_metadata")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    let page =
        reads::metadata::list_metadata(handle.ops(), p.limit.unwrap_or(0), p.offset.unwrap_or(0))
            .map_err(ops_internal)?;
    to_value(&page)
}

pub fn list_pending_reviews(
    params: &Option<Value>,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    let p: PageParams = parse(params, "library.list_pending_reviews")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    let page = reads::metadata::list_pending_reviews(
        handle.ops(),
        p.limit.unwrap_or(0),
        p.offset.unwrap_or(0),
    )
    .map_err(ops_internal)?;
    to_value(&page)
}

pub fn show_audit_trail(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: BookIdParams = parse(params, "library.show_audit_trail")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    match reads::metadata::show_audit_trail(handle.ops(), p.intake_id) {
        Ok(trail) => to_value(&Some(trail)),
        Err(OpsError::IntakeNotFound { .. }) => Ok(Value::Null),
        Err(e) => Err(ops_internal(e)),
    }
}

pub fn show_pipeline_trail(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: BookIdParams = parse(params, "library.show_pipeline_trail")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    match reads::pipeline::show_pipeline_trail(handle.ops(), p.intake_id) {
        Ok(trail) => to_value(&Some(trail)),
        Err(OpsError::IntakeNotFound { .. }) => Ok(Value::Null),
        Err(e) => Err(ops_internal(e)),
    }
}

pub async fn search(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: SearchParams = parse(params, "library.search")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    let overrides = p.overrides();
    let kind = p.kind.as_deref().unwrap_or("book");
    let hits = match kind {
        "book" => reads::search::search(handle.ops(), &p.query, overrides, p.top_k)
            .await
            .map_err(ops_internal)?,
        "paper" => reads::search::search_paper(handle.ops(), &p.query, overrides, p.top_k)
            .await
            .map_err(ops_internal)?,
        "all" => reads::search::search_unified(handle.ops(), &p.query, overrides, p.top_k)
            .await
            .map_err(ops_internal)?,
        other => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                format!(
                    "library.search: kind={other:?} is not one of \"book\", \"paper\", \"all\""
                ),
            ));
        }
    };
    tracing::info!(kind = kind, hits = hits.len(), "control library.search");
    to_value(&hits)
}

pub async fn search_in_book(
    params: &Option<Value>,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    let p: SearchInBookParams = parse(params, "library.search_in_book")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    let overrides = p.overrides();
    let result =
        reads::search::search_in_book(handle.ops(), p.intake_id, &p.query, overrides, p.top_k)
            .await;
    match result {
        Ok(hits) => to_value(&hits),
        Err(OpsError::IntakeNotFound { .. }) => {
            let empty: Vec<bookrack_ops::Citation> = Vec::new();
            to_value(&empty)
        }
        Err(e) => Err(ops_internal(e)),
    }
}

pub fn list_papers(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: PageParams = parse(params, "library.list_papers")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    let page =
        reads::papers::list_papers(handle.ops(), p.limit.unwrap_or(0), p.offset.unwrap_or(0))
            .map_err(ops_internal)?;
    to_value(&page)
}

pub fn find_papers(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: FindPapersParams = parse(params, "library.find_papers")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    let filter = PaperFilter {
        title_substring: p.title_substring,
        contributor_name: p.contributor_name,
        year: p.year,
        venue_substring: p.venue_substring,
        doi: p.doi,
    };
    let page = reads::papers::find_papers(
        handle.ops(),
        filter,
        p.limit.unwrap_or(0),
        p.offset.unwrap_or(0),
    )
    .map_err(ops_internal)?;
    to_value(&page)
}

pub fn show_paper(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: BookIdParams = parse(params, "library.show_paper")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    match reads::papers::show_paper(handle.ops(), p.intake_id) {
        Ok(detail) => to_value(&Some(detail)),
        Err(OpsError::IntakeNotFound { .. }) => Ok(Value::Null),
        Err(e) => Err(ops_internal(e)),
    }
}

pub fn show_paper_toc(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: BookIdParams = parse(params, "library.show_paper_toc")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    match reads::papers::show_paper_toc(handle.ops(), p.intake_id) {
        Ok(toc) => to_value(&Some(toc)),
        Err(OpsError::IntakeNotFound { .. }) => Ok(Value::Null),
        Err(e) => Err(ops_internal(e)),
    }
}

pub async fn search_in_paper(
    params: &Option<Value>,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    let p: SearchInPaperParams = parse(params, "library.search_in_paper")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    let overrides = p.overrides();
    let result =
        reads::search::search_in_paper(handle.ops(), p.intake_id, &p.query, overrides, p.top_k)
            .await;
    match result {
        Ok(hits) => to_value(&hits),
        Err(OpsError::IntakeNotFound { .. }) => {
            let empty: Vec<bookrack_ops::Citation> = Vec::new();
            to_value(&empty)
        }
        Err(e) => Err(ops_internal(e)),
    }
}

pub fn papers_export_csl(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: BookIdParams = parse(params, "papers.export_csl")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    match reads::papers::export_csl(handle.ops(), p.intake_id) {
        Ok(item) => to_value(&item),
        Err(OpsError::IntakeNotFound { .. }) => Ok(Value::Null),
        Err(e) => Err(ops_internal(e)),
    }
}

pub fn papers_fetch_source(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: BookIdParams = parse(params, "papers.fetch_source")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    match reads::papers::fetch_source(handle.ops(), p.intake_id) {
        Ok(src) => to_value(&src),
        // A missing intake is reported as `null` so clients can pattern-
        // match on it without parsing an error envelope. A `null` here
        // matches the rest of the `library.show_*` / `papers.*` family.
        Err(OpsError::IntakeNotFound { .. }) => Ok(Value::Null),
        Err(e) => Err(ops_internal(e)),
    }
}

pub async fn vectors_status(
    params: &Option<Value>,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    let p: LibraryOnlyParams = parse(params, "library.vectors_status")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    let status = reads::vectors::status(handle.ops())
        .await
        .map_err(ops_internal)?;
    to_value(&status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_none_yields_default_when_all_fields_optional() {
        let params: Option<Value> = None;
        let p: LibraryOnlyParams = parse(&params, "library.stats").unwrap();
        assert!(p.library.is_none());
    }

    #[test]
    fn parse_null_yields_default_when_all_fields_optional() {
        let params = Some(Value::Null);
        let p: PageParams = parse(&params, "library.list_books").unwrap();
        assert!(p.limit.is_none());
        assert!(p.offset.is_none());
        assert!(p.library.is_none());
    }

    #[test]
    fn parse_rejects_missing_required_field() {
        let params: Option<Value> = None;
        let result: Result<BookIdParams, _> = parse(&params, "library.show_book");
        let err = result.unwrap_err();
        assert_eq!(err.code, INVALID_PARAMS);
        assert!(
            err.message.contains("library.show_book"),
            "error should name the method: {}",
            err.message
        );
    }

    #[test]
    fn parse_accepts_known_fields_and_ignores_extras() {
        let params = Some(json!({
            "intake_id": 42,
            "library": "demo",
            "extra_field": "ignored",
        }));
        let p: BookIdParams = parse(&params, "library.show_book").unwrap();
        assert_eq!(p.intake_id, 42);
        assert_eq!(p.library.as_deref(), Some("demo"));
    }

    #[test]
    fn search_params_overrides_project_to_search_options() {
        let p = SearchParams {
            query: "q".into(),
            top_k: Some(5),
            bypass_index: true,
            nprobes: Some(8),
            refine_factor: Some(3),
            library: None,
            kind: None,
        };
        let opts = p.overrides();
        assert!(opts.bypass_index);
        assert_eq!(opts.nprobes, Some(8));
        assert_eq!(opts.refine_factor, Some(3));
    }
}
