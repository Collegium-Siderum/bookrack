// SPDX-License-Identifier: Apache-2.0

//! `daemon.methods` and `daemon.mcp_tools` — runtime reflection for
//! `bookrack exec tools` and any future GUI surface that wants to
//! enumerate what is callable on this daemon.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::MethodContext;

/// One row in the `daemon.methods` response.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct MethodSignature {
    #[cfg_attr(test, ts(type = "string"))]
    pub name: &'static str,
    #[cfg_attr(test, ts(type = "string"))]
    pub kind: &'static str,
    /// `true` iff the runtime routes the method through the persistent
    /// queue worker. Headless `bookrack-mcp` profiles without
    /// `--with-queue-worker` short-circuit these to `-32002 not_ready`.
    pub queue_bound: bool,
}

/// One row in the `daemon.mcp_tools` response. Populated at daemon
/// startup from the live MCP server so the runtime crate does not
/// take a direct dependency on rmcp.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
}

/// Closed enumeration of every control-plane method routed by
/// [`super::dispatch`]. Exported as a TypeScript literal union so the
/// webview's `ControlClient` can type its method names against the
/// Rust-side registry with no hand maintenance. `current()` mirrors
/// the runtime [`super::REGISTRY`] verbatim.
#[cfg(test)]
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export, export_to = "./")]
pub struct MethodCatalog {
    pub names: Vec<String>,
}

#[cfg(test)]
impl MethodCatalog {
    pub fn current() -> Self {
        Self {
            names: super::REGISTRY.iter().map(|m| m.name.to_string()).collect(),
        }
    }
}

pub fn methods(_ctx: &MethodContext) -> Value {
    json!({ "methods": super::REGISTRY })
}

pub fn mcp_tools(ctx: &MethodContext) -> Value {
    let tools: &Vec<McpToolInfo> = ctx.mcp_tools.as_ref();
    json!({ "tools": tools })
}

pub fn methods_rpc(
    _params: &Option<Value>,
    ctx: &MethodContext,
) -> Result<Value, crate::control::jsonrpc::RpcError> {
    Ok(methods(ctx))
}

pub fn mcp_tools_rpc(
    _params: &Option<Value>,
    ctx: &MethodContext,
) -> Result<Value, crate::control::jsonrpc::RpcError> {
    Ok(mcp_tools(ctx))
}

#[cfg(test)]
mod tests {
    use super::super::REGISTRY;
    use super::*;
    use ts_rs::TS;

    #[test]
    fn method_catalog_ts_matches_registry_len() {
        MethodCatalog::export_all().expect("ts-rs export MethodCatalog");
        let dir = std::env::var("TS_RS_EXPORT_DIR").expect("TS_RS_EXPORT_DIR not set");
        let path = std::path::PathBuf::from(dir).join("MethodCatalog.ts");
        let contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert!(
            contents.contains("names"),
            "MethodCatalog.ts missing `names` field:\n{contents}"
        );
        assert_eq!(MethodCatalog::current().names.len(), REGISTRY.len());
    }

    #[test]
    fn registry_names_are_unique() {
        let mut names: Vec<&str> = REGISTRY.iter().map(|m| m.name).collect();
        names.sort_unstable();
        for pair in names.windows(2) {
            assert_ne!(
                pair[0], pair[1],
                "method {} appears twice in the registry",
                pair[0]
            );
        }
    }

    #[test]
    fn library_read_proxy_methods_are_registered() {
        for name in [
            "library.stats",
            "library.list_books",
            "library.find_books",
            "library.show_book",
            "library.show_toc",
            "library.read_context",
            "library.read_span",
            "library.show_metadata_audit",
            "library.show_metadata_report",
            "library.list_metadata",
            "library.list_pending_reviews",
            "library.show_audit_trail",
            "library.show_pipeline_trail",
            "library.search",
            "library.search_in_book",
            "library.list_papers",
            "library.find_papers",
            "library.show_paper",
            "library.show_paper_toc",
            "library.search_in_paper",
            "papers.export_csl",
            "library.vectors_status",
        ] {
            let entry = REGISTRY
                .iter()
                .find(|m| m.name == name)
                .unwrap_or_else(|| panic!("library read proxy {name} is missing from REGISTRY"));
            assert_eq!(
                entry.kind, "read",
                "{name} should be classified as a read method"
            );
        }
    }

    #[test]
    fn registry_kinds_are_known() {
        for sig in REGISTRY {
            assert!(
                matches!(sig.kind, "read" | "write" | "stream"),
                "method {} has unknown kind {:?}",
                sig.name,
                sig.kind
            );
        }
    }

    #[test]
    fn papers_maintenance_methods_are_registered_as_writes() {
        for name in [
            "papers.corpus_rebuild",
            "papers.vectors_rebuild",
            "papers.vectors_reembed",
            "papers.vectors_reset",
            "papers.vectors_drop",
            "papers.stamps_reconcile",
            "papers.dryrun",
        ] {
            let entry = REGISTRY
                .iter()
                .find(|m| m.name == name)
                .unwrap_or_else(|| panic!("paper maintenance method {name} missing from REGISTRY"));
            assert_eq!(
                entry.kind, "write",
                "{name} should be classified as a write method"
            );
        }
    }

    #[test]
    fn papers_maintenance_methods_are_queue_bound() {
        for name in [
            "papers.corpus_rebuild",
            "papers.vectors_rebuild",
            "papers.vectors_reembed",
            "papers.vectors_reset",
            "papers.vectors_drop",
            "papers.stamps_reconcile",
            "papers.dryrun",
        ] {
            assert!(
                super::super::is_queue_bound_method(name),
                "{name} must be queue-bound so headless bookrack-mcp short-circuits it"
            );
        }
    }
}
