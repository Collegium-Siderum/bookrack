// SPDX-License-Identifier: Apache-2.0

//! `embed`: the Ollama `/api/embed` HTTP client.
//!
//! This crate turns text into embedding vectors by calling a local
//! Ollama daemon over HTTP. It is a leaf of the ingest / search
//! dependency graph: it holds no model weights, owns no scheduling
//! policy, and depends only on `reqwest` and a running Ollama.
//!
//! The load-bearing design choice is the typed [`EmbedError`].
//! Distinguishing an overloaded server (HTTP 5xx — out of GPU memory)
//! from an operator error (HTTP 4xx — model not pulled) and from a
//! transport failure lets the caller react correctly: shrink the batch
//! on overload, fail fast on operator error, retry a transport blip.
//! A single untyped error would force one blunt response to all three.
//!
//! Batching, resource-sensitive scheduling, the cross-batch retry
//! policy and the embed cache are deliberately *not* here — they belong
//! to the ingest pipeline, which owns the whole book stream. This
//! crate's retry loop only smooths a transient transport failure of a
//! single request.

use std::time::Duration;

use serde::{Deserialize, Serialize};

mod probe;

pub use probe::{
    DEFAULT_PROBE_TIMEOUT, ProbeError, ProbeReport, probe_ollama, probe_ollama_with_timeout,
};

/// Why an embed request failed. Callers branch on the variant, not the
/// message text.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EmbedError {
    /// HTTP 5xx, or a model-runner crash: the server is out of GPU
    /// memory or otherwise overloaded. Retrying the *same* batch will
    /// not help — the caller must shrink it.
    #[error("Ollama overloaded (HTTP {status}): {body}")]
    Overloaded {
        /// The 5xx status code returned.
        status: u16,
        /// A truncated prefix of the response body, for diagnosis.
        body: String,
    },

    /// Could not establish HTTP communication with Ollama at all —
    /// connection refused, DNS failure, timeout, or an HTTP client that
    /// would not initialize. Transient: worth retrying with backoff,
    /// and never a reason to shrink the batch.
    #[error("Ollama unreachable: {0}")]
    Unreachable(String),

    /// HTTP 4xx: a bad request, an unpulled model, or malformed input.
    /// An operator error — fail fast, since retrying cannot fix it.
    #[error("Ollama rejected the request (HTTP {status}): {body}")]
    BadRequest {
        /// The 4xx status code returned.
        status: u16,
        /// A truncated prefix of the response body, for diagnosis.
        body: String,
    },

    /// The response was not the expected `{"embeddings": [...]}` shape,
    /// or its vector count did not match the input count.
    #[error("Ollama returned a malformed response: {0}")]
    MalformedResponse(String),
}

impl EmbedError {
    /// Whether the client should transparently retry, with backoff.
    /// Only a transport failure qualifies: [`EmbedError::Overloaded`] is
    /// handed back so the caller can shrink, and an operator error must
    /// surface at once.
    pub fn is_transient(&self) -> bool {
        matches!(self, EmbedError::Unreachable(_))
    }

    /// Whether the caller should respond by shrinking the batch.
    pub fn is_overload(&self) -> bool {
        matches!(self, EmbedError::Overloaded { .. })
    }
}

/// A fallible `embed` operation.
pub type Result<T> = std::result::Result<T, EmbedError>;

/// Cap on how much of an error response body is kept, in characters —
/// a diagnostic prefix, not a transcript.
const ERROR_BODY_CAP: usize = 300;

/// Longest backoff between retries, regardless of attempt count.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Request body for Ollama `/api/embed`.
#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

/// Response body from Ollama `/api/embed`.
#[derive(Deserialize)]
struct EmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

/// Wrap a search query with the asymmetric instruction prefix the
/// embedding model expects on the query side. The document side is
/// embedded as bare normalized text, with no prefix — the asymmetry is
/// deliberate and part of the embedding contract.
pub fn build_query_input(query: &str) -> String {
    format!(
        "Instruct: Given a query about books in a personal library, \
         retrieve relevant passages\nQuery: {query}"
    )
}

/// A client for the Ollama `/api/embed` endpoint.
///
/// Stateless between calls and cheap to share — one instance serves a
/// whole run.
pub struct OllamaEmbedClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    max_retries: u32,
    backoff_base: Duration,
}

impl OllamaEmbedClient {
    /// Build a client for `base_url`, embedding with `model`.
    ///
    /// `timeout` bounds each HTTP request. A transient transport
    /// failure is retried up to `max_retries` times, with exponential
    /// backoff starting at `backoff_base` and capped at 30 seconds.
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        timeout: Duration,
        max_retries: u32,
        backoff_base: Duration,
    ) -> Result<OllamaEmbedClient> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| EmbedError::Unreachable(format!("HTTP client init failed: {e}")))?;
        Ok(OllamaEmbedClient {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            max_retries,
            backoff_base,
        })
    }

    /// Embed a batch of texts in one HTTP POST. Ollama batches the whole
    /// request on the GPU in a single forward pass; vectors come back in
    /// input order. Empty input returns an empty vector with no call.
    ///
    /// A transient transport failure is retried with backoff; an
    /// overloaded server and operator errors are returned at once, so
    /// the caller can shrink the batch or fail fast.
    pub async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut attempt = 0u32;
        loop {
            match self.embed_once(texts).await {
                Ok(vectors) => return Ok(vectors),
                Err(e) => {
                    if !e.is_transient() || attempt >= self.max_retries {
                        return Err(e);
                    }
                    let backoff = (self.backoff_base * 2u32.pow(attempt)).min(MAX_BACKOFF);
                    tokio::time::sleep(backoff).await;
                    attempt += 1;
                }
            }
        }
    }

    /// One HTTP attempt, with no retry.
    async fn embed_once(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let url = format!("{}/api/embed", self.base_url);
        let request = EmbedRequest {
            model: &self.model,
            input: texts,
        };
        let response = match self.http.post(&url).json(&request).send().await {
            Ok(response) => response,
            // Connection refused, DNS failure and read timeout all land
            // here; none is a definite out-of-memory signal, so all are
            // transient. A genuine OOM surfaces as HTTP 5xx below.
            Err(e) => return Err(EmbedError::Unreachable(e.to_string())),
        };

        let status = response.status();
        if status.is_success() {
            let parsed: EmbedResponse = response
                .json()
                .await
                .map_err(|e| EmbedError::MalformedResponse(e.to_string()))?;
            if parsed.embeddings.len() != texts.len() {
                return Err(EmbedError::MalformedResponse(format!(
                    "got {} vectors for {} inputs",
                    parsed.embeddings.len(),
                    texts.len()
                )));
            }
            Ok(parsed.embeddings)
        } else {
            let code = status.as_u16();
            let body = error_body(response).await;
            if status.is_client_error() {
                Err(EmbedError::BadRequest { status: code, body })
            } else {
                Err(EmbedError::Overloaded { status: code, body })
            }
        }
    }
}

/// Something that embeds a batch of texts into vectors.
///
/// The ingest and search stages are generic over this so they can be
/// driven by a test double with no running Ollama. The sole production
/// implementor is [`OllamaEmbedClient`]; the returned future is `Send`
/// so callers can drive it on a multi-threaded runtime.
pub trait Embedder {
    /// Embed `texts`, returning one vector per input, in input order.
    fn embed_batch(
        &self,
        texts: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<Vec<f32>>>> + Send;
}

impl Embedder for OllamaEmbedClient {
    fn embed_batch(
        &self,
        texts: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<Vec<f32>>>> + Send {
        OllamaEmbedClient::embed_batch(self, texts)
    }
}

/// Read a bounded, diagnostic prefix of an error response body.
async fn error_body(response: reqwest::Response) -> String {
    response
        .text()
        .await
        .unwrap_or_default()
        .chars()
        .take(ERROR_BODY_CAP)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A client wired for fast tests: no retries, negligible backoff.
    fn test_client(base_url: &str) -> OllamaEmbedClient {
        OllamaEmbedClient::new(
            base_url,
            "test-model",
            Duration::from_secs(5),
            0,
            Duration::from_millis(1),
        )
        .expect("client builds")
    }

    /// Spawn a one-shot mock HTTP server: it answers the first request
    /// with `status_line` (e.g. `"200 OK"`) and `body`, then closes.
    /// Returns the base URL to point a client at.
    async fn mock_once(status_line: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            // The test requests are a few hundred bytes; one read drains
            // the whole request so the client's write side never blocks.
            let mut scratch = [0u8; 8192];
            let _ = socket.read(&mut scratch).await;
            let response = format!(
                "HTTP/1.1 {status_line}\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        });
        format!("http://{addr}")
    }

    /// An address with no listener — connecting to it is refused.
    async fn dead_address() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener); // free the port; nothing listens there now
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_successful_batch_returns_vectors_in_order() {
        let url = mock_once("200 OK", r#"{"embeddings":[[1.0,2.0],[3.0,4.0]]}"#).await;
        let client = test_client(&url);
        let vectors = client
            .embed_batch(&["a".to_string(), "b".to_string()])
            .await
            .expect("ok");
        assert_eq!(vectors, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
    }

    #[tokio::test]
    async fn http_500_is_classified_as_overloaded() {
        let url = mock_once("500 Internal Server Error", "out of memory").await;
        let err = test_client(&url)
            .embed_batch(&["x".to_string()])
            .await
            .unwrap_err();
        assert!(err.is_overload(), "got {err:?}");
        assert!(!err.is_transient());
    }

    #[tokio::test]
    async fn http_404_is_classified_as_bad_request() {
        let url = mock_once("404 Not Found", "model not found").await;
        let err = test_client(&url)
            .embed_batch(&["x".to_string()])
            .await
            .unwrap_err();
        assert!(
            matches!(err, EmbedError::BadRequest { status: 404, .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_non_json_body_is_a_malformed_response() {
        let url = mock_once("200 OK", "this is not json").await;
        let err = test_client(&url)
            .embed_batch(&["x".to_string()])
            .await
            .unwrap_err();
        assert!(
            matches!(err, EmbedError::MalformedResponse(_)),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_vector_count_mismatch_is_a_malformed_response() {
        // One input, but the server returns zero vectors.
        let url = mock_once("200 OK", r#"{"embeddings":[]}"#).await;
        let err = test_client(&url)
            .embed_batch(&["x".to_string()])
            .await
            .unwrap_err();
        assert!(
            matches!(err, EmbedError::MalformedResponse(_)),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_refused_connection_is_unreachable() {
        let err = test_client(&dead_address().await)
            .embed_batch(&["x".to_string()])
            .await
            .unwrap_err();
        assert!(matches!(err, EmbedError::Unreachable(_)), "got {err:?}");
        assert!(err.is_transient());
    }

    #[tokio::test]
    async fn empty_input_returns_empty_without_a_call() {
        // Pointing at a dead address proves no HTTP request is made.
        let vectors = test_client(&dead_address().await)
            .embed_batch(&[])
            .await
            .expect("ok");
        assert!(vectors.is_empty());
    }

    #[test]
    fn the_query_prefix_wraps_the_query() {
        let wrapped = build_query_input("dragons");
        assert!(wrapped.starts_with("Instruct:"));
        assert!(wrapped.ends_with("Query: dragons"));
    }

    #[tokio::test]
    async fn probe_reports_models_on_a_healthy_daemon() {
        let url = mock_once(
            "200 OK",
            r#"{"models":[{"name":"qwen3-embedding:0.6b"},{"name":"llama3.2:3b"}]}"#,
        )
        .await;
        let report = probe_ollama(&url).await.expect("probe ok");
        assert!(report.reachable);
        assert_eq!(
            report.models,
            vec![
                "qwen3-embedding:0.6b".to_string(),
                "llama3.2:3b".to_string()
            ],
        );
    }

    #[tokio::test]
    async fn probe_reports_reachable_with_no_models_for_an_empty_install() {
        let url = mock_once("200 OK", r#"{"models":[]}"#).await;
        let report = probe_ollama(&url).await.expect("probe ok");
        assert!(report.reachable);
        assert!(report.models.is_empty());
    }

    #[tokio::test]
    async fn probe_tolerates_a_missing_models_field() {
        // A daemon answering `{}` is unusual but should not crash the
        // wizard: treat it as reachable with zero models.
        let url = mock_once("200 OK", "{}").await;
        let report = probe_ollama(&url).await.expect("probe ok");
        assert!(report.reachable);
        assert!(report.models.is_empty());
    }

    #[tokio::test]
    async fn probe_resolves_a_refused_connection_to_unreachable() {
        let url = dead_address().await;
        let report = probe_ollama(&url).await.expect("probe returns ok");
        assert!(!report.reachable);
        assert!(report.models.is_empty());
    }

    #[tokio::test]
    async fn probe_resolves_a_5xx_to_unreachable() {
        let url = mock_once("503 Service Unavailable", "down for maintenance").await;
        let report = probe_ollama(&url).await.expect("probe returns ok");
        assert!(!report.reachable);
        assert!(report.models.is_empty());
    }

    #[tokio::test]
    async fn probe_surfaces_a_malformed_body_as_an_error() {
        let url = mock_once("200 OK", "this is not json").await;
        let err = probe_ollama(&url).await.unwrap_err();
        assert!(
            matches!(err, ProbeError::MalformedResponse(_)),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn probe_trims_a_trailing_slash_from_base_url() {
        // A base URL written with a trailing slash must still produce
        // `<base>/api/tags`, not `<base>//api/tags`.
        let url = mock_once("200 OK", r#"{"models":[]}"#).await;
        let with_slash = format!("{url}/");
        let report = probe_ollama(&with_slash).await.expect("probe ok");
        assert!(report.reachable);
    }
}
