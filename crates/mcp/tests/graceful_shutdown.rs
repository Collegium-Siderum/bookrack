// SPDX-License-Identifier: Apache-2.0

//! Pins the daemon's graceful-shutdown wiring through the real entry
//! point: `bookrack_mcp::serve` must stop and release its listening
//! port when the session-wide shutdown broadcast fires.
//!
//! The library handle behind the registry is a `catalog_only` stub
//! over paths that are never opened — `serve` constructs per-session
//! MCP services lazily, so no store I/O happens unless a client calls
//! a tool, which this test never does. The
//! `tokio::signal::ctrl_c()` -> broadcast linkage in `main.rs` is one
//! line of glue and is not exercised here.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bookrack_config::Config;
use bookrack_core::queue::QueueState;
use bookrack_embed::OllamaEmbedClient;
use bookrack_mcp::ProcessEndpoints;
use bookrack_obs::LogStreamHandle;
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::{Caller, Ops};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::time::timeout;

/// Take an ephemeral port the way the daemon does — bound during
/// bring-up and handed to `serve` — and report the address alongside
/// it, so the test can re-bind after shutdown to prove the release.
async fn bound_local_listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    (listener, addr)
}

/// A registry over one catalog-only handle whose store paths point
/// into the OS temp dir and are never created or opened.
fn stub_registry() -> Arc<LibraryRegistry<OllamaEmbedClient>> {
    let root = std::env::temp_dir().join("bookrack-mcp-shutdown-stub");
    let cfg = Arc::new(Config::new(root));
    let ops = Ops::<OllamaEmbedClient>::catalog_only(
        cfg.corpus_db(),
        cfg.catalog_db(),
        &cfg.lancedb_dir(),
        cfg.books_dir(),
        cfg.backup_dir(),
        Caller::mcp(),
    );
    LibraryRegistry::single(LibraryHandle::new("shutdown-stub", cfg, ops))
}

fn stub_endpoints(mcp_addr: &SocketAddr) -> ProcessEndpoints {
    ProcessEndpoints {
        mcp_addr: mcp_addr.to_string(),
        ollama_url: "http://127.0.0.1:1".to_string(),
    }
}

#[tokio::test]
async fn shutdown_broadcast_stops_serve_and_releases_the_listening_port() {
    let (listener, addr) = bound_local_listener().await;
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(4);

    let registry = stub_registry();
    let endpoints = stub_endpoints(&addr);
    let serve_tx = shutdown_tx.clone();
    let server = tokio::spawn(async move {
        bookrack_mcp::serve(
            registry,
            endpoints,
            Instant::now(),
            test_identity(),
            LogStreamHandle::default(),
            Arc::new(Mutex::new(QueueState::default())),
            serve_tx,
            test_event_stream(),
            listener,
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

/// The `session.shutdown` tool publishes `daemon.state = stopping`
/// before it fires the shutdown broadcast: a control-plane subscriber
/// flushes only what is queued when the broadcast reaches it, so the
/// transition has to be there already.
#[tokio::test]
async fn session_shutdown_publishes_stopping_before_broadcast() {
    use bookrack_runtime::control::events::{DaemonState, Event};
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::StreamableHttpClientTransport;

    let (listener, addr) = bound_local_listener().await;
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(4);
    let event_stream = test_event_stream();
    let mut events_rx = event_stream.subscribe();
    let mut observer = shutdown_tx.subscribe();

    let registry = stub_registry();
    let endpoints = stub_endpoints(&addr);
    let serve_tx = shutdown_tx.clone();
    let server = tokio::spawn(async move {
        bookrack_mcp::serve(
            registry,
            endpoints,
            Instant::now(),
            test_identity(),
            LogStreamHandle::default(),
            Arc::new(Mutex::new(QueueState::default())),
            serve_tx,
            event_stream,
            listener,
            shutdown_rx,
        )
        .await
    });

    let client = timeout(Duration::from_secs(5), async {
        loop {
            let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}/mcp"));
            match ().serve(transport).await {
                Ok(client) => break client,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("an MCP client connects within 5s");

    // The call may end in a transport error once the listener winds
    // down under it; the broadcast below is what says it was served.
    let _ = client
        .call_tool(
            CallToolRequestParams::new("session.shutdown").with_arguments(serde_json::Map::new()),
        )
        .await;

    timeout(Duration::from_secs(5), observer.recv())
        .await
        .expect("the shutdown broadcast fires within 5s")
        .expect("the broadcast is delivered");
    match events_rx.try_recv() {
        Ok(Event::DaemonState(DaemonState::Stopping)) => {}
        other => panic!("no `stopping` transition was queued when the broadcast fired: {other:?}"),
    }

    drop(client);
    let _ = timeout(Duration::from_secs(5), server).await;
}

/// A daemon identity for a server no daemon stands behind.
fn test_identity() -> bookrack_core::DaemonIdentity {
    bookrack_core::DaemonIdentity {
        instance_id: "018f0000-0000-7000-8000-000000000000".to_string(),
        version: "0.0.0-test".to_string(),
        pid: 4242,
        runtime_dir: "/run/bookrack-test".to_string(),
        control_sock: None,
        started_at: "2026-01-01T00:00:00+00:00".to_string(),
    }
}

/// An event stream no control-plane client is attached to.
fn test_event_stream() -> bookrack_runtime::control::events::EventStreamHandle {
    use bookrack_runtime::control::events::{DaemonState, DaemonStateFlag, EventStreamHandle};

    EventStreamHandle::new(8, Arc::new(DaemonStateFlag::new(DaemonState::Idle)))
}
