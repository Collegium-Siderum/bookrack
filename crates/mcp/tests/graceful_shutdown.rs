// SPDX-License-Identifier: Apache-2.0

//! Regression for the daemon's graceful-shutdown wiring shape.
//!
//! `bookrack_mcp::serve` runs `axum::serve(listener, app)
//! .with_graceful_shutdown(ctrl_c)`. This test exercises the same
//! pattern in-process with a controllable shutdown future — proving
//! that completing the shutdown future stops the server and releases
//! the listening port. The `tokio::signal::ctrl_c()` -> shutdown
//! future linkage in `main.rs` is one line of glue and is not
//! exercised here; F5 verifies that end-to-end manually.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::timeout;

#[tokio::test]
async fn graceful_shutdown_signal_releases_the_listening_port() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr: SocketAddr = listener.local_addr().expect("local addr");

    let app = axum::Router::<()>::new().route("/", axum::routing::get(|| async { "ok" }));
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("serve");
    });

    // The port must be live until the shutdown signal lands.
    let probe = tokio::net::TcpStream::connect(addr)
        .await
        .expect("first connect");
    drop(probe);

    // Signal shutdown and wait for the server task to wind down. A
    // bounded wait turns a hang into a test failure instead of a CI
    // timeout.
    shutdown_tx.send(()).expect("send shutdown");
    timeout(Duration::from_secs(5), server)
        .await
        .expect("server task winds down within 5s")
        .expect("server task panicked");

    // Once shutdown returns, the port is free: a follow-up bind on
    // the same address must succeed without `AddrInUse`.
    let rebound = TcpListener::bind(addr)
        .await
        .expect("rebind on released port");
    drop(rebound);
}
