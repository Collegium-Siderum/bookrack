// SPDX-License-Identifier: Apache-2.0

//! An HTTP proxy in the environment must not capture a call to this
//! machine.
//!
//! `reqwest` reads the system proxy variables when a client is built,
//! and the matcher underneath it exempts nothing on its own — not
//! `localhost`, not `127.0.0.1`. So `HTTP_PROXY`, exported by a shell
//! or supplied by a `.env`, used to redirect every probe of a locally
//! served model to a host that cannot reach this machine's loopback.
//! The symptom is indistinguishable from the model being down.
//!
//! Both stand-ins here count what reaches them, because the claim has
//! two halves: nothing went to the proxy, and the request did arrive
//! where it was addressed. Asserting only the first passes just as well
//! when no call was made at all.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bookrack_test_support::{Sandbox, bookrack_cmd};
use tokio::process::Command;

/// Serve a TCP port with `response`, counting the connections that
/// arrive. Returns the `http://host:port` form and the counter.
fn spawn_counting_server(response: String) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let url = format!("http://{}", listener.local_addr().expect("stub addr"));
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut socket) = stream else { continue };
            counter.fetch_add(1, Ordering::SeqCst);
            let mut scratch = [0u8; 8192];
            let _ = socket.read(&mut scratch);
            let _ = socket.write_all(response.as_bytes());
            let _ = socket.flush();
        }
    });
    (url, hits)
}

/// An Ollama stand-in holding one model.
fn spawn_ollama() -> (String, Arc<AtomicUsize>) {
    let body = r#"{"models":[{"name":"qwen3-embedding:0.6b"}]}"#;
    spawn_counting_server(format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    ))
}

/// A proxy stand-in that refuses everything, so a captured request
/// fails fast instead of holding the probe open to its timeout.
fn spawn_proxy() -> (String, Arc<AtomicUsize>) {
    spawn_counting_server(
        "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
    )
}

/// The probe of a locally served model ignores the proxy the
/// environment names, and reaches the model.
#[tokio::test]
async fn a_proxy_in_the_environment_does_not_capture_a_loopback_probe() {
    let sandbox = Sandbox::new();
    let (ollama_url, ollama_hits) = spawn_ollama();
    let (proxy_url, proxy_hits) = spawn_proxy();

    let output = Command::from(
        bookrack_cmd!(&sandbox)
            .ollama_url(ollama_url.clone())
            // Both spellings the matcher reads, and the exemption list
            // emptied: an inherited `NO_PROXY` naming localhost would
            // otherwise make this pass without the code under test.
            .extra_env("HTTP_PROXY", &proxy_url)
            .extra_env("http_proxy", &proxy_url)
            .extra_env("ALL_PROXY", &proxy_url)
            .extra_env("all_proxy", &proxy_url)
            .extra_env("NO_PROXY", "")
            .extra_env("no_proxy", "")
            .build(),
    )
    .args(["doctor", "--json"])
    .output()
    .await
    .expect("run bookrack doctor");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        proxy_hits.load(Ordering::SeqCst),
        0,
        "a call to {ollama_url} was handed to the proxy at {proxy_url}, \
         which cannot reach this machine's loopback; doctor said:\n{stdout}"
    );
    assert!(
        ollama_hits.load(Ordering::SeqCst) > 0,
        "nothing reached {ollama_url}, so the run proves nothing about \
         where the call went; doctor said:\n{stdout}"
    );
}
