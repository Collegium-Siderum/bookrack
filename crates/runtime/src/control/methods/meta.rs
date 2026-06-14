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
/// the runtime [`REGISTRY`] verbatim.
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
            names: REGISTRY.iter().map(|m| m.name.to_string()).collect(),
        }
    }
}

/// Static registry of every method [`super::dispatch`] routes. Kept
/// in lockstep with the match table by hand; `clippy::pedantic`
/// would normally flag stringly-typed registries, but this one is the
/// single source of truth a client uses to populate completion and
/// help surfaces, so duplication is the lesser cost.
pub const REGISTRY: &[MethodSignature] = &[
    MethodSignature {
        name: "daemon.version",
        kind: "read",
    },
    MethodSignature {
        name: "daemon.shutdown",
        kind: "write",
    },
    MethodSignature {
        name: "daemon.methods",
        kind: "read",
    },
    MethodSignature {
        name: "daemon.mcp_tools",
        kind: "read",
    },
    MethodSignature {
        name: "status",
        kind: "read",
    },
    MethodSignature {
        name: "doctor.gather",
        kind: "read",
    },
    MethodSignature {
        name: "queue.list",
        kind: "read",
    },
    MethodSignature {
        name: "queue.pause",
        kind: "write",
    },
    MethodSignature {
        name: "queue.resume",
        kind: "write",
    },
    MethodSignature {
        name: "queue.clear",
        kind: "write",
    },
    MethodSignature {
        name: "library.list",
        kind: "read",
    },
    MethodSignature {
        name: "library.info",
        kind: "read",
    },
    MethodSignature {
        name: "library.stats",
        kind: "read",
    },
    MethodSignature {
        name: "library.list_books",
        kind: "read",
    },
    MethodSignature {
        name: "library.find_books",
        kind: "read",
    },
    MethodSignature {
        name: "library.show_book",
        kind: "read",
    },
    MethodSignature {
        name: "library.show_toc",
        kind: "read",
    },
    MethodSignature {
        name: "library.read_context",
        kind: "read",
    },
    MethodSignature {
        name: "library.read_span",
        kind: "read",
    },
    MethodSignature {
        name: "library.show_metadata_audit",
        kind: "read",
    },
    MethodSignature {
        name: "library.show_metadata_report",
        kind: "read",
    },
    MethodSignature {
        name: "library.list_metadata",
        kind: "read",
    },
    MethodSignature {
        name: "library.list_pending_reviews",
        kind: "read",
    },
    MethodSignature {
        name: "library.show_audit_trail",
        kind: "read",
    },
    MethodSignature {
        name: "library.show_pipeline_trail",
        kind: "read",
    },
    MethodSignature {
        name: "library.search",
        kind: "read",
    },
    MethodSignature {
        name: "library.search_in_book",
        kind: "read",
    },
    MethodSignature {
        name: "library.list_papers",
        kind: "read",
    },
    MethodSignature {
        name: "library.find_papers",
        kind: "read",
    },
    MethodSignature {
        name: "library.show_paper",
        kind: "read",
    },
    MethodSignature {
        name: "library.show_paper_toc",
        kind: "read",
    },
    MethodSignature {
        name: "library.search_in_paper",
        kind: "read",
    },
    MethodSignature {
        name: "papers.export_csl",
        kind: "read",
    },
    MethodSignature {
        name: "library.vectors_status",
        kind: "read",
    },
    MethodSignature {
        name: "library.fork",
        kind: "write",
    },
    MethodSignature {
        name: "events.subscribe",
        kind: "stream",
    },
    MethodSignature {
        name: "events.snapshot",
        kind: "read",
    },
    MethodSignature {
        name: "ingest.submit",
        kind: "write",
    },
    MethodSignature {
        name: "ingest.cancel",
        kind: "write",
    },
    MethodSignature {
        name: "glean.submit",
        kind: "write",
    },
    MethodSignature {
        name: "metadata.set",
        kind: "write",
    },
    MethodSignature {
        name: "metadata.clear",
        kind: "write",
    },
    MethodSignature {
        name: "metadata.void",
        kind: "write",
    },
    MethodSignature {
        name: "metadata.reaudit",
        kind: "write",
    },
    MethodSignature {
        name: "metadata.contributor_add",
        kind: "write",
    },
    MethodSignature {
        name: "metadata.contributor_remove",
        kind: "write",
    },
    MethodSignature {
        name: "metadata.ack",
        kind: "write",
    },
    MethodSignature {
        name: "metadata.approve",
        kind: "write",
    },
    MethodSignature {
        name: "metadata.reject",
        kind: "write",
    },
    MethodSignature {
        name: "vectors.rebuild",
        kind: "write",
    },
    MethodSignature {
        name: "vectors.reembed",
        kind: "write",
    },
    MethodSignature {
        name: "vectors.reset",
        kind: "write",
    },
    MethodSignature {
        name: "vectors.drop",
        kind: "write",
    },
    MethodSignature {
        name: "corpus.rebuild",
        kind: "write",
    },
    MethodSignature {
        name: "stamps.reconcile",
        kind: "write",
    },
    MethodSignature {
        name: "remove",
        kind: "write",
    },
    MethodSignature {
        name: "papers.remove",
        kind: "write",
    },
    MethodSignature {
        name: "dryrun",
        kind: "write",
    },
    MethodSignature {
        name: "verify.run",
        kind: "read",
    },
    MethodSignature {
        name: "diagnose.run",
        kind: "read",
    },
    MethodSignature {
        name: "tray.focus",
        kind: "write",
    },
];

pub fn methods(_ctx: &MethodContext) -> Value {
    json!({ "methods": REGISTRY })
}

pub fn mcp_tools(ctx: &MethodContext) -> Value {
    let tools: &Vec<McpToolInfo> = ctx.mcp_tools.as_ref();
    json!({ "tools": tools })
}

#[cfg(test)]
mod tests {
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
}
