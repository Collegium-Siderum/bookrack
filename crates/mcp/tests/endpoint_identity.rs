// SPDX-License-Identifier: Apache-2.0

//! What the server publishes about itself, and what the health check
//! looks for, are the same thing.
//!
//! `bookrack doctor` decides whether the configured MCP address is
//! answered by this daemon or by a stranger, and it decides it from
//! `serverInfo.name` in a real `initialize` result. The check and the
//! server live in different crates, so the two halves are asserted
//! together here: the probe from `bookrack_runtime` against a server
//! from this crate, over the transport an agent client uses. Either
//! half drifting — a renamed server, a probe matching something else —
//! turns every future health report into a false verdict, and no
//! single-crate test would see it.
//!
//! The library handle behind the registry is a `catalog_only` stub
//! over paths that are never opened: `initialize` reaches no store.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bookrack_config::Config;
use bookrack_core::queue::QueueState;
use bookrack_embed::OllamaEmbedClient;
use bookrack_obs::LogStreamHandle;
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::{Caller, Ops};
use bookrack_runtime::mcp_endpoint::{McpEndpointState, probe_endpoint};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

/// A registry over one catalog-only handle whose store paths point
/// into the OS temp dir and are never created or opened.
fn stub_registry() -> Arc<LibraryRegistry<OllamaEmbedClient>> {
    let root = std::env::temp_dir().join("bookrack-mcp-identity-stub");
    let cfg = Arc::new(Config::new(root, "http://127.0.0.1:1".to_string()));
    let ops = Ops::<OllamaEmbedClient>::catalog_only(
        cfg.corpus_db(),
        cfg.catalog_db(),
        &cfg.lancedb_dir(),
        cfg.books_dir(),
        cfg.backup_dir(),
        Caller::mcp(),
    );
    LibraryRegistry::single(LibraryHandle::new("identity-stub", cfg, ops))
}

fn stub_info_context(addr: &str) -> LibraryInfoContext {
    LibraryInfoContext {
        data_dir: "unused".to_string(),
        library_name: Some("identity-stub".to_string()),
        resolution_source: "test fixture".to_string(),
        shadowed_default: None,
        library_identification: None,
        ollama_url: "http://127.0.0.1:1".to_string(),
        embed_model_configured: "unused".to_string(),
        mcp_addr: addr.to_string(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_health_probe_recognises_a_real_bookrack_server() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(4);

    let serve_tx = shutdown_tx.clone();
    let info_context = stub_info_context(&addr);
    let server = tokio::spawn(async move {
        bookrack_mcp::serve(
            stub_registry(),
            info_context,
            Instant::now(),
            LogStreamHandle::default(),
            Arc::new(Mutex::new(QueueState::default())),
            serve_tx,
            listener,
            shutdown_rx,
        )
        .await
    });

    let state = probe_endpoint(&addr, Duration::from_secs(5)).await;
    let McpEndpointState::Serving { version } = state else {
        panic!("the probe did not recognise a real bookrack server: {state:?}");
    };
    assert_eq!(
        version,
        env!("CARGO_PKG_VERSION"),
        "the server publishes a version that is not this build's",
    );

    shutdown_tx.send(()).expect("send shutdown");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("serve winds down")
        .expect("serve task panicked")
        .expect("serve returns Ok after graceful shutdown");
}
