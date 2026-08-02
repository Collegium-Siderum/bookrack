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

use bookrack_core::{Explain, Problem};
use tokio::net::TcpListener;

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
