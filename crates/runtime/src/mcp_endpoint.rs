// SPDX-License-Identifier: Apache-2.0

//! The MCP endpoint as a bring-up resource.
//!
//! The listening socket is the daemon's only agent-facing surface, so
//! it is taken during bring-up, next to the control-plane socket and
//! the data-root locks — not inside the task that serves it. Binding
//! inside the task makes the failure invisible: the address a daemon
//! reports would be the address it *meant* to serve, and every health
//! surface downstream reads that report.
//!
//! Holding the bound [`TcpListener`] is also what makes `port 0`
//! usable. The kernel assigns the port at bind time, so the address a
//! client connects to exists only once the socket is open; the caller
//! records `local_addr` and every reader sees the same value.
//!
//! Two questions live here, and they are not the same one. Bring-up
//! asks *can this process take the address*; the health report asks
//! *who is answering on it now* — which also covers the address being
//! taken after the daemon came up, and a client pointed somewhere
//! else entirely. They share this module and the wording that names
//! the way out, not a single function: a bind and an `initialize`
//! round trip cannot answer for each other.

use std::time::Duration;

use bookrack_config::MCP_SERVER_NAME;
use bookrack_core::{Explain, Problem};
use tokio::net::TcpListener;

/// How long the health probe waits for the endpoint to answer.
/// Loopback and a daemon that is already up: a second is generous,
/// and a report is not worth blocking on.
// setting: mcp.probe_timeout
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Bring-up's refusal to start without the MCP endpoint it announced.
///
/// Carries the rendered [`Problem`] so the CLI hands it to the same
/// three-part reporter every other classified failure goes through.
/// `Display` stays a self-sufficient single line for the log and for
/// hosts that have no exit code to set.
#[derive(Debug, thiserror::Error)]
#[error("{}", .problem.summary)]
pub struct McpBindRefusal {
    /// The three-part diagnostic for the failure.
    pub problem: Problem,
}

/// Take the MCP listening socket, or refuse.
///
/// `addr` is the configured address verbatim; `port 0` binds a
/// kernel-assigned port, which the caller reads back with
/// [`TcpListener::local_addr`].
pub async fn bind_listener(addr: &str) -> Result<TcpListener, McpBindRefusal> {
    TcpListener::bind(addr).await.map_err(|err| McpBindRefusal {
        problem: explain_bind_failure(addr, &err),
    })
}

/// Word a bind failure. Split from [`bind_listener`] so the wording
/// has a test that does not need a real socket in the failing state.
fn explain_bind_failure(addr: &str, err: &std::io::Error) -> Problem {
    let free_address_hint = "Choose a free address with BOOKRACK_MCP_ADDR (or --mcp-addr on \
                             `bookrack run`); 127.0.0.1:0 lets the operating system pick any \
                             free port.";
    match err.kind() {
        std::io::ErrorKind::AddrInUse => Problem::new(format!(
            "cannot serve MCP on {addr:?}: the address is already in use"
        ))
        .detail(format!("bind: {err}."))
        .hint(format!(
            "Another process holds it; `bookrack status` says whether that process is a \
                     bookrack daemon. {free_address_hint}"
        )),
        // Nothing else is worth splitting out: a refused permission, an
        // address the host does not own, and a malformed address all
        // leave the operator with the same next step, and the detail
        // line already carries the syscall's own wording.
        _ => Problem::new(format!("cannot serve MCP on {addr:?}"))
            .detail(format!("bind: {err}."))
            .hint(free_address_hint),
    }
}

impl Explain for McpBindRefusal {
    fn explain(&self) -> Problem {
        self.problem.clone()
    }
}

/// What answered at an MCP address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpEndpointState {
    /// A bookrack MCP server answered and named itself.
    Serving {
        /// The version it published, for a report that can say which
        /// build is actually serving.
        version: String,
    },
    /// Something answered, but not as a bookrack MCP server. The
    /// address is occupied by a stranger — the state in which a
    /// client that follows the documented URL reaches the wrong
    /// service.
    Foreign {
        /// What the answer looked like, as evidence.
        evidence: String,
    },
    /// Nothing answered: the address is free, or a daemon that should
    /// hold it is gone.
    Unreachable,
}

/// Ask an MCP address who is answering.
///
/// One `initialize` round trip against `http://{addr}/mcp`, matched on
/// the published `serverInfo.name`. Deliberately the protocol's own
/// entry point rather than a bookrack-specific health route: what the
/// report claims is that an agent client connecting here reaches this
/// daemon, and only the path an agent client takes can support that.
pub async fn probe_endpoint(addr: &str, timeout: Duration) -> McpEndpointState {
    match tokio::time::timeout(timeout, initialize_round_trip(addr)).await {
        Ok(Ok(state)) => state,
        // A transport failure and a timeout are the same answer to
        // the question asked: nothing usable is there.
        Ok(Err(_)) | Err(_) => McpEndpointState::Unreachable,
    }
}

/// The round trip itself. `Err` means the exchange did not complete;
/// a completed exchange is classified into a state.
async fn initialize_round_trip(addr: &str) -> Result<McpEndpointState, reqwest::Error> {
    let url = format!("http://{addr}/mcp");
    let client = reqwest::Client::builder().build()?;
    let response = client
        .post(&url)
        .header("content-type", "application/json")
        // The streamable-HTTP server requires a client that accepts
        // both, and answers `initialize` as a one-message SSE stream.
        .header("accept", "application/json, text/event-stream")
        .body(INITIALIZE_REQUEST)
        .send()
        .await?;

    let session = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let status = response.status();
    let body = response.text().await?;

    // The session this probe opened is closed again: a health report
    // that leaks one session per run would grow the server's session
    // table for as long as the daemon lives.
    if let Some(id) = session {
        let _ = client
            .delete(&url)
            .header("mcp-session-id", id)
            .send()
            .await;
    }

    Ok(classify_initialize_response(status.as_u16(), &body))
}

/// The `initialize` request the probe sends, as a literal: the probe
/// speaks the wire protocol, so building it through the SDK's client
/// types would add a dependency to say the same fourteen fields.
const INITIALIZE_REQUEST: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"#,
    r#""protocolVersion":"2025-06-18","capabilities":{},"#,
    r#""clientInfo":{"name":"bookrack-doctor","version":"1"}}}"#
);

/// Decide what answered, from the status and body of one `initialize`
/// exchange. Split out so the classification has tests that do not
/// need a server in each state.
fn classify_initialize_response(status: u16, body: &str) -> McpEndpointState {
    let evidence = || {
        let head: String = body.chars().take(120).collect();
        format!("HTTP {status}, body {head:?}")
    };
    if status != 200 {
        return McpEndpointState::Foreign {
            evidence: evidence(),
        };
    }
    let Some(payload) = json_payload(body) else {
        return McpEndpointState::Foreign {
            evidence: evidence(),
        };
    };
    let info = &payload["result"]["serverInfo"];
    if info["name"].as_str() == Some(MCP_SERVER_NAME) {
        McpEndpointState::Serving {
            version: info["version"].as_str().unwrap_or("unknown").to_string(),
        }
    } else {
        McpEndpointState::Foreign {
            evidence: evidence(),
        }
    }
}

/// Read the JSON-RPC message out of a response body that is either
/// plain JSON or a Server-Sent Events stream carrying one message.
fn json_payload(body: &str) -> Option<serde_json::Value> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        return Some(value);
    }
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .find_map(|data| serde_json::from_str::<serde_json::Value>(data.trim()).ok())
}

impl Explain for McpEndpointState {
    fn explain(&self) -> Problem {
        match self {
            McpEndpointState::Serving { version } => {
                Problem::new("the MCP endpoint is served by bookrack")
                    .detail(format!("The server published version {version}."))
            }

            McpEndpointState::Foreign { evidence } => {
                Problem::new("the MCP address is answered by another service")
                    .detail(format!("{evidence}."))
                    .hint(
                        "An agent client pointed at this address reaches that service, not \
                         bookrack. Stop the process holding it, or move bookrack with \
                         BOOKRACK_MCP_ADDR and update the client's URL to match.",
                    )
            }

            // Past tense: a daemon that is starting, or one being
            // restarted, answers on the next attempt.
            McpEndpointState::Unreachable => Problem::new("nothing answered at the MCP address")
                .detail(format!("No response within {}s.", PROBE_TIMEOUT.as_secs()))
                .hint("Start the daemon with `bookrack run`.")
                .retryable(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn binding_a_held_address_refuses_and_names_it() {
        let held = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
        let addr = held.local_addr().expect("addr").to_string();

        let refusal = bind_listener(&addr).await.expect_err("second bind refused");

        assert!(
            refusal.problem.summary.contains(&addr),
            "summary does not name the address: {}",
            refusal.problem.summary
        );
        assert!(
            refusal.problem.summary.contains("already in use"),
            "summary does not state what failed: {}",
            refusal.problem.summary
        );
        let hint = refusal.problem.data.hint.expect("hint");
        assert!(
            hint.contains("BOOKRACK_MCP_ADDR") && hint.contains("127.0.0.1:0"),
            "hint offers no way out: {hint}"
        );
        assert!(
            !refusal.problem.data.retryable,
            "a held port does not clear on its own"
        );
    }

    /// `port 0` is the supported way to ask for any free port, so the
    /// caller must be able to learn which one it got.
    #[tokio::test]
    async fn port_zero_binds_a_concrete_port() {
        let listener = bind_listener("127.0.0.1:0").await.expect("bind");
        let bound = listener.local_addr().expect("addr");
        assert_ne!(bound.port(), 0, "kernel-assigned port not reported back");
    }

    /// One SSE frame, the shape the streamable-HTTP server answers
    /// `initialize` with.
    fn sse(payload: &str) -> String {
        format!("event: message\ndata: {payload}\n\n")
    }

    /// One line, because an SSE frame's `data:` field is one line.
    fn initialize_result(name: &str, version: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-06-18","capabilities":{{}},"serverInfo":{{"name":"{name}","version":"{version}"}}}}}}"#
        )
    }

    #[test]
    fn a_bookrack_server_is_recognised_through_sse_framing() {
        let body = sse(&initialize_result(MCP_SERVER_NAME, "0.11.0-dev"));
        assert_eq!(
            classify_initialize_response(200, &body),
            McpEndpointState::Serving {
                version: "0.11.0-dev".to_string()
            }
        );
    }

    /// The same message unframed: the server may answer `initialize`
    /// as plain JSON, and a probe that only understood one framing
    /// would call a healthy endpoint foreign.
    #[test]
    fn a_bookrack_server_is_recognised_through_plain_json() {
        let body = initialize_result(MCP_SERVER_NAME, "9.9.9");
        assert_eq!(
            classify_initialize_response(200, &body),
            McpEndpointState::Serving {
                version: "9.9.9".to_string()
            }
        );
    }

    /// Another MCP server on the address is exactly the case worth
    /// catching: it speaks the protocol, so anything short of matching
    /// the published name would pass it as healthy.
    #[test]
    fn another_mcp_server_is_foreign() {
        let body = sse(&initialize_result("some-other-server", "2.0"));
        let state = classify_initialize_response(200, &body);
        assert!(
            matches!(state, McpEndpointState::Foreign { .. }),
            "another server was accepted as bookrack: {state:?}"
        );
    }

    #[test]
    fn a_non_mcp_answer_is_foreign() {
        for (status, body) in [(200, "I AM NOT BOOKRACK"), (404, "not found"), (200, "")] {
            let state = classify_initialize_response(status, body);
            assert!(
                matches!(state, McpEndpointState::Foreign { .. }),
                "HTTP {status} {body:?} was not called foreign: {state:?}"
            );
        }
    }

    /// The evidence quotes what answered, bounded: a stranger's body
    /// can be a megabyte of HTML, and a health report is a table.
    #[test]
    fn the_evidence_is_bounded() {
        let body = "x".repeat(10_000);
        let McpEndpointState::Foreign { evidence } = classify_initialize_response(200, &body)
        else {
            panic!("expected a foreign verdict");
        };
        assert!(
            evidence.len() < 200,
            "unbounded evidence: {} chars",
            evidence.len()
        );
    }

    /// Nothing listening is not a foreign answer: the two states send
    /// the operator to different places.
    #[tokio::test]
    async fn an_address_nobody_serves_is_unreachable() {
        // Bind and release: the port is known-free for the moment
        // that follows, which is what "nothing answers" needs.
        let free = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = free.local_addr().expect("addr").to_string();
        drop(free);

        assert_eq!(
            probe_endpoint(&addr, Duration::from_secs(2)).await,
            McpEndpointState::Unreachable
        );
    }

    /// A live socket that answers something else — the failure this
    /// whole module exists for — through the real probe, not the
    /// classifier alone.
    #[tokio::test]
    async fn a_stranger_on_the_address_is_reported_as_foreign() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut scratch = [0u8; 4096];
                let _ = socket.read(&mut scratch).await;
                let body = "I AM NOT BOOKRACK";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        let state = probe_endpoint(&addr, Duration::from_secs(2)).await;
        let McpEndpointState::Foreign { evidence } = state else {
            panic!("a stranger on the address was not reported as foreign: {state:?}");
        };
        assert!(evidence.contains("NOT BOOKRACK"), "no evidence: {evidence}");

        let problem = McpEndpointState::Foreign { evidence }.explain();
        assert!(
            problem
                .data
                .hint
                .expect("hint")
                .contains("BOOKRACK_MCP_ADDR"),
            "the hint must name a way out"
        );
    }

    /// The summary states the failure without giving advice, and the
    /// address the operator configured is quoted in it either way.
    #[tokio::test]
    async fn an_unbindable_address_still_names_itself() {
        let refusal = bind_listener("203.0.113.1:9").await.expect_err("refused");
        assert!(
            refusal.problem.summary.contains("203.0.113.1:9"),
            "summary does not name the address: {}",
            refusal.problem.summary
        );
        assert!(
            refusal
                .problem
                .data
                .detail
                .expect("detail")
                .contains("bind:"),
            "detail does not carry the syscall evidence"
        );
    }
}
