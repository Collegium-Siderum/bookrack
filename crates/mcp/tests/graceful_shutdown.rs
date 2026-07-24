// SPDX-License-Identifier: Apache-2.0

//! Pins the daemon's graceful-shutdown wiring through the real entry
//! point: `bookrack_mcp::serve` must stop and release its listening
//! port when the session-wide shutdown broadcast fires.
//!
//! The library handle behind the registry is a `catalog_only` stub
//! over paths that are never opened — `serve` binds the listener and
//! constructs per-session MCP services lazily, so no store I/O happens
//! unless a client calls a tool, which this test never does. The
//! `tokio::signal::ctrl_c()` -> broadcast linkage in `main.rs` is one
//! line of glue and is not exercised here.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bookrack_core::queue::QueueState;
use bookrack_embed::OllamaEmbedClient;
use bookrack_obs::LogStreamHandle;
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::{Caller, Ops};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::time::timeout;

/// Reserve an ephemeral port by binding and immediately releasing it,
/// so the address can be handed to `serve` (which performs its own
/// bind) and re-bound after shutdown to prove the release.
async fn reserve_local_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    listener.local_addr().expect("local addr")
}

/// A registry over one catalog-only handle whose store paths point
/// into the OS temp dir and are never created or opened.
fn stub_registry() -> Arc<LibraryRegistry<OllamaEmbedClient>> {
    let root = std::env::temp_dir().join("bookrack-mcp-shutdown-stub");
    let ops = Ops::<OllamaEmbedClient>::catalog_only(
        root.join("corpus.db"),
        root.join("catalog.db"),
        &root.join("lancedb"),
        root.join("books"),
        root.join("backup"),
        Caller::mcp(),
    );
    LibraryRegistry::single(LibraryHandle::new("shutdown-stub", ops))
}

fn stub_info_context(mcp_addr: &SocketAddr) -> LibraryInfoContext {
    LibraryInfoContext {
        data_dir: "unused".to_string(),
        library_name: Some("shutdown-stub".to_string()),
        resolution_source: "test fixture".to_string(),
        shadowed_default: None,
        library_identification: None,
        ollama_url: "http://127.0.0.1:1".to_string(),
        embed_model_configured: "unused".to_string(),
        mcp_addr: mcp_addr.to_string(),
    }
}

#[tokio::test]
async fn shutdown_broadcast_stops_serve_and_releases_the_listening_port() {
    let addr = reserve_local_addr().await;
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(4);

    let registry = stub_registry();
    let info_context = stub_info_context(&addr);
    let serve_tx = shutdown_tx.clone();
    let server = tokio::spawn(async move {
        bookrack_mcp::serve(
            registry,
            info_context,
            Instant::now(),
            LogStreamHandle::default(),
            Arc::new(Mutex::new(QueueState::default())),
            serve_tx,
            &addr.to_string(),
            shutdown_rx,
        )
        .await
    });

    // The spawned task binds asynchronously; poll until the port
    // accepts so the shutdown signal cannot race the bind.
    let live = timeout(Duration::from_secs(5), async {
        loop {
            match TcpStream::connect(addr).await {
                Ok(stream) => break stream,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("serve accepts a connection within 5s");
    drop(live);

    // Fire the session-wide broadcast — the same channel the
    // `session.shutdown` tool and the Ctrl-C listener send on — and
    // require `serve` itself to return cleanly. A bounded wait turns
    // a hang (e.g. a dropped `with_graceful_shutdown` linkage) into a
    // test failure instead of a CI timeout.
    shutdown_tx.send(()).expect("send shutdown");
    timeout(Duration::from_secs(5), server)
        .await
        .expect("serve winds down within 5s of the shutdown broadcast")
        .expect("serve task panicked")
        .expect("serve returns Ok after graceful shutdown");

    // Once `serve` returns, the port is free: a follow-up bind on the
    // same address must succeed without `AddrInUse`.
    let rebound = TcpListener::bind(addr)
        .await
        .expect("rebind on released port");
    drop(rebound);
}
