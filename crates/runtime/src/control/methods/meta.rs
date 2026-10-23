// SPDX-License-Identifier: Apache-2.0

//! `daemon.methods` and `daemon.mcp_tools` — runtime reflection for
//! `bookrack rpc list` and any client that wants to enumerate what is
//! callable on this daemon.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::MethodContext;

/// One row in the `daemon.methods` response.
#[derive(Debug, Clone, Serialize)]
pub struct MethodSignature {
    pub name: &'static str,
    pub kind: &'static str,
    /// `true` iff the runtime routes the method through the persistent
    /// queue worker. A host that spawns no queue worker short-circuits
    /// these to `-32002 not_ready`.
    pub queue_bound: bool,
    /// How an explicit library selection reaches the method:
    /// `"routed"` — it takes one, under [`Self::library_key`];
    /// `"process"` — it describes the process rather than a library,
    /// so a selection is meaningless but harmless; `"unrouted"` — it
    /// answers about the daemon or about every library at once, so a
    /// selection naming one library cannot be honoured and a client
    /// must refuse rather than drop it.
    pub selection: &'static str,
    /// The params key that names the library, for `selection ==
    /// "routed"`. `"library"` everywhere but `library.info`, whose own
    /// `name` parameter predates the shared spelling.
    pub library_key: Option<&'static str>,
}

/// One row in the `daemon.mcp_tools` response. Populated at daemon
/// startup from the live MCP server so the runtime crate does not
/// take a direct dependency on rmcp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
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

    /// The `daemon.methods` response lists every registry row, in
    /// registry order, under its registered name.
    #[tokio::test]
    async fn daemon_methods_enumerates_the_whole_registry() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = super::super::test_method_context(dir.path(), None);
        let value = methods(&ctx);
        let names: Vec<&str> = value["methods"]
            .as_array()
            .expect("methods array")
            .iter()
            .map(|row| row["name"].as_str().expect("name"))
            .collect();
        let expected: Vec<&str> = REGISTRY.iter().map(|sig| sig.name).collect();
        assert_eq!(names, expected);
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
            "library.categories",
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
                "{name} must be queue-bound so a host without a queue worker short-circuits it"
            );
        }
    }
}
