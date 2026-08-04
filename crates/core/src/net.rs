// SPDX-License-Identifier: Apache-2.0

//! Which outbound URLs leave this machine.

use std::net::IpAddr;

/// Whether an HTTP client aimed at `url` must be built with proxies
/// disabled.
///
/// A proxy is a hop on the way somewhere else, and loopback is not
/// somewhere else: a request for `127.0.0.1` handed to a proxy arrives
/// at the proxy host's own loopback, or is refused outright. So an
/// environment that names a proxy — `HTTP_PROXY`, `ALL_PROXY`, either
/// case, from the shell or from a `.env` — would otherwise misroute
/// every call to a locally served model, and the failure reads as the
/// model being down.
///
/// The rule is the destination's, not the caller's: a model served from
/// another host still goes through the proxy the operator configured.
/// `NO_PROXY` remains the way to exempt anything else, and disabling
/// proxies here does not disable them for the download of an installer
/// archive, which genuinely does leave the machine.
///
/// A URL whose host cannot be read is not loopback: the safe direction
/// is to honour the configured proxy rather than to bypass it on a
/// guess.
pub fn bypasses_proxy(url: &str) -> bool {
    host_of(url).is_some_and(is_loopback_host)
}

/// The host part of `url`, without userinfo or port.
///
/// Written by hand rather than parsed, because the one question asked
/// of it is whether the authority names this machine — a judgement
/// that must not depend on the rest of a URL being well-formed.
fn host_of(url: &str) -> Option<&str> {
    let after_scheme = match url.split_once("://") {
        Some((_, rest)) => rest,
        // A bare authority (`localhost:11434`) is what a hand-edited
        // setting often holds, and it names the same host.
        None => url,
    };
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .filter(|s| !s.is_empty())?;
    // Userinfo may itself contain `@`; the host is what follows the
    // last one.
    let host_port = authority.rsplit('@').next()?;
    if let Some(rest) = host_port.strip_prefix('[') {
        // An IPv6 literal is bracketed, and the brackets are what keep
        // its colons apart from the port's.
        return rest.split_once(']').map(|(host, _)| host);
    }
    Some(match host_port.split_once(':') {
        Some((host, _)) => host,
        None => host_port,
    })
}

/// Whether `host` names this machine's loopback interface.
///
/// `localhost` is matched as a whole name, so `localhost.example.com`
/// — a name that resolves wherever its owner points it — is not
/// loopback.
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|addr| addr.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_loopback_forms_a_local_service_is_addressed_by_all_bypass() {
        for url in [
            "http://127.0.0.1:11434",
            "http://127.0.0.1:11434/api/tags",
            "http://localhost:11434",
            "http://LocalHost:11434/",
            "https://127.0.0.2:8080",
            "http://[::1]:8765/mcp",
            "http://user:pass@127.0.0.1:11434/x",
            "localhost:11434",
            "http://localhost",
        ] {
            assert!(bypasses_proxy(url), "{url} must not be sent to a proxy");
        }
    }

    /// The rule is about the destination, so anything that leaves the
    /// machine keeps the operator's proxy — including the names most
    /// likely to be mistaken for loopback.
    #[test]
    fn anything_that_leaves_the_machine_keeps_the_proxy() {
        for url in [
            "http://ollama.example.com:11434",
            "http://10.0.0.4:11434",
            "https://github.com/owner/repo/releases/download/x.tar.gz",
            // Neither is loopback: one is a domain whose left label
            // merely reads like it, the other is not an address at all.
            "http://localhost.example.com:11434",
            "http://127.0.0.1.example.com/",
            "",
            "http://",
        ] {
            assert!(!bypasses_proxy(url), "{url} must keep the proxy");
        }
    }
}
