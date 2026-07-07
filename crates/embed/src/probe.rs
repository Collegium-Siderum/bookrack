// SPDX-License-Identifier: Apache-2.0

//! Lightweight health probe of the Ollama daemon, used by the install
//! wizard and `bookrack doctor` to decide whether the embedder is
//! reachable before any embedding work begins.
//!
//! Distinct from [`OllamaEmbedClient`](crate::OllamaEmbedClient) by
//! design: the probe never embeds, never retries, holds no long-lived
//! client, and folds every reachability failure into a structured
//! `reachable = false` so callers can render a status table without
//! unwrapping. Only a 2xx response whose body is not shaped like
//! `/api/tags` surfaces as a real error.

use std::time::Duration;

use serde::Deserialize;

/// Probe result for the Ollama daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    /// Whether `<base_url>/api/tags` answered with a 2xx that decoded
    /// as the expected shape. False on transport failure (refused
    /// connection, timeout, DNS) and on a non-2xx response.
    pub reachable: bool,
    /// Model tags the daemon currently holds, in the order Ollama
    /// returned them. Empty when `reachable` is false, and also when
    /// the daemon answered with an empty model list (a working but
    /// un-pulled install).
    pub models: Vec<String>,
}

/// Why a probe failed terminally. A daemon that is simply down does
/// *not* produce this — it resolves to `Ok(ProbeReport { reachable:
/// false, .. })`. This variant is reserved for cases the operator must
/// notice: the HTTP client could not be built, or the daemon answered
/// but is not speaking the expected protocol.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProbeError {
    /// The HTTP client itself could not be built.
    #[error("HTTP client init failed: {0}")]
    ClientInit(String),
    /// Ollama answered with a 2xx but the body could not be decoded
    /// as `/api/tags`. The daemon is talking but speaking the wrong
    /// protocol — typically a non-Ollama server bound on the same port.
    #[error("Ollama returned a malformed /api/tags response: {0}")]
    MalformedResponse(String),
}

/// Default probe timeout. Short by design: the probe runs before the
/// user's first interaction and a hung daemon should fall through to
/// "unreachable" within a couple of seconds.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// GET `<base_url>/api/tags` and report whether the daemon is up and
/// what models it carries. See [`probe_ollama_with_timeout`] for the
/// full contract; this convenience form uses [`DEFAULT_PROBE_TIMEOUT`].
pub async fn probe_ollama(base_url: &str) -> Result<ProbeReport, ProbeError> {
    probe_ollama_with_timeout(base_url, DEFAULT_PROBE_TIMEOUT).await
}

/// As [`probe_ollama`], but with a caller-supplied timeout.
///
/// A transport failure (refused connection, timeout, DNS, TLS) and a
/// non-2xx status both resolve to `Ok(ProbeReport { reachable: false,
/// models: vec![] })`. A 2xx whose body does not decode as the
/// `/api/tags` shape returns [`ProbeError::MalformedResponse`].
pub async fn probe_ollama_with_timeout(
    base_url: &str,
    timeout: Duration,
) -> Result<ProbeReport, ProbeError> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| ProbeError::ClientInit(e.to_string()))?;
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let response = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => return Ok(unreachable_report()),
    };
    if !response.status().is_success() {
        return Ok(unreachable_report());
    }
    let body: TagsResponse = match response.json().await {
        Ok(body) => body,
        // A timeout or connection failure while reading the body means the
        // daemon is reachable but not usable — the same "not usable"
        // outcome as the send() and non-2xx arms above. `reqwest` reports
        // such a read failure with the same decode kind as genuinely
        // unparseable bytes, so the timeout / connect source is what
        // separates a real protocol mismatch from a transport blip.
        Err(e) if e.is_timeout() || e.is_connect() => return Ok(unreachable_report()),
        Err(e) => return Err(ProbeError::MalformedResponse(e.to_string())),
    };
    Ok(ProbeReport {
        reachable: true,
        models: body.models.into_iter().map(|m| m.name).collect(),
    })
}

/// The empty-but-honest report returned on every "daemon is not
/// usable" branch, centralised so the variants do not drift.
fn unreachable_report() -> ProbeReport {
    ProbeReport {
        reachable: false,
        models: Vec::new(),
    }
}

/// Decoded shape of Ollama's `/api/tags` response. `models` is
/// permissive: a daemon that omits the field entirely (an empty
/// install, in practice) still decodes as reachable with zero models.
#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagsModel>,
}

#[derive(Deserialize)]
struct TagsModel {
    name: String,
}
