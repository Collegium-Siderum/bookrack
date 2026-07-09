// SPDX-License-Identifier: Apache-2.0

//! `rerank`: the llama-server `/v1/rerank` HTTP client.
//!
//! This crate scores query/document pairs by calling a local
//! llama-server over HTTP. It is a leaf of the query dependency graph:
//! it holds no model weights, owns no process, and depends only on
//! `reqwest` and a serving backend — whether that backend is the
//! supervised subprocess the runtime manages or an operator-run server
//! named by an override URL is invisible here.
//!
//! The typed [`RerankError`] mirrors the embed client's contract:
//! distinguishing an overloaded server from an operator error and from
//! a transport failure lets the caller react correctly instead of
//! retrying blindly. The client sends bare query and document texts;
//! the server applies the reranker model's own input template.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Why a rerank request failed. Callers branch on the variant, not the
/// message text.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RerankError {
    /// HTTP 5xx: the server is out of memory or otherwise overloaded.
    /// 503 in particular is also what a supervised server answers while
    /// its model is still loading — worth retrying after a pause.
    #[error("reranker overloaded (HTTP {status}): {body}")]
    Overloaded {
        /// The 5xx status code returned.
        status: u16,
        /// A truncated prefix of the response body, for diagnosis.
        body: String,
    },

    /// Could not establish HTTP communication at all — connection
    /// refused, timeout, or an HTTP client that would not initialize.
    /// Under the supervised backend this is the restart window of a
    /// crashed server: transient and self-healing.
    #[error("reranker unreachable: {0}")]
    Unreachable(String),

    /// HTTP 4xx: a bad request. An operator or programming error —
    /// fail fast, since retrying cannot fix it.
    #[error("reranker rejected the request (HTTP {status}): {body}")]
    BadRequest {
        /// The 4xx status code returned.
        status: u16,
        /// A truncated prefix of the response body, for diagnosis.
        body: String,
    },

    /// The response was not the expected `{"results": [...]}` shape,
    /// or a returned index does not name an input document.
    #[error("reranker returned a malformed response: {0}")]
    MalformedResponse(String),
}

impl RerankError {
    /// Whether the client should transparently retry, with backoff.
    /// Only a transport failure qualifies; the other variants surface
    /// at once so the caller can decide.
    pub fn is_transient(&self) -> bool {
        matches!(self, RerankError::Unreachable(_))
    }
}

/// A fallible rerank operation.
pub type Result<T> = std::result::Result<T, RerankError>;

/// Cap on how much of an error response body is kept, in characters —
/// a diagnostic prefix, not a transcript.
const ERROR_BODY_CAP: usize = 300;

/// Longest backoff between retries, regardless of attempt count.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// One scored document out of a rerank call: the index of the document
/// in the request's input order, and the model's relevance score.
/// Higher scores rank higher.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankedDocument {
    pub index: usize,
    pub score: f32,
}

/// Request body for llama-server `/v1/rerank`.
#[derive(Serialize)]
struct RerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    top_n: usize,
    documents: &'a [String],
}

/// Response body from llama-server `/v1/rerank`. Extra fields
/// (`object`, `usage`) are ignored.
#[derive(Deserialize)]
struct RerankResponse {
    results: Vec<RerankResultEntry>,
}

/// One entry of the response's `results` array.
#[derive(Deserialize)]
struct RerankResultEntry {
    index: usize,
    relevance_score: f32,
}

/// A client for the llama-server `/v1/rerank` endpoint.
///
/// Stateless between calls and cheap to share — one instance serves a
/// whole run.
pub struct RerankClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    max_retries: u32,
    backoff_base: Duration,
}

impl RerankClient {
    /// Build a client for `base_url`, reranking with `model` (the
    /// registry tag; a single-model server scores with the model it
    /// loaded regardless, but the name keeps the request
    /// self-describing).
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
    ) -> Result<RerankClient> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| RerankError::Unreachable(format!("HTTP client init failed: {e}")))?;
        Ok(RerankClient {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            max_retries,
            backoff_base,
        })
    }

    /// Score `documents` against `query` in one HTTP POST, returning at
    /// most `top_n` entries ordered by descending relevance, each
    /// naming an input document by index. Empty input returns an empty
    /// vector with no call.
    ///
    /// A transient transport failure is retried with backoff; the
    /// other failures are returned at once, so the caller can fail
    /// fast.
    pub async fn rerank(
        &self,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<RankedDocument>> {
        if documents.is_empty() || top_n == 0 {
            return Ok(Vec::new());
        }
        let mut attempt = 0u32;
        loop {
            match self.rerank_once(query, documents, top_n).await {
                Ok(ranked) => return Ok(ranked),
                Err(e) => {
                    if !e.is_transient() || attempt >= self.max_retries {
                        return Err(e);
                    }
                    let backoff = capped_backoff(self.backoff_base, attempt);
                    tokio::time::sleep(backoff).await;
                    attempt += 1;
                }
            }
        }
    }

    /// One HTTP attempt, with no retry.
    async fn rerank_once(
        &self,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<RankedDocument>> {
        let url = format!("{}/v1/rerank", self.base_url);
        let request = RerankRequest {
            model: &self.model,
            query,
            top_n,
            documents,
        };
        let response = match self.http.post(&url).json(&request).send().await {
            Ok(response) => response,
            // A connection-time failure — refused, DNS, TLS, or a
            // timeout before the response head arrives. All transient.
            Err(e) => return Err(RerankError::Unreachable(e.to_string())),
        };

        let status = response.status();
        if status.is_success() {
            let parsed: RerankResponse = response.json().await.map_err(|e| {
                // A timeout or connection failure while reading the body
                // is a transient blip worth a retry; only a real decode
                // of a fully received body stays a malformed response.
                if e.is_timeout() || e.is_connect() {
                    RerankError::Unreachable(e.to_string())
                } else {
                    RerankError::MalformedResponse(e.to_string())
                }
            })?;
            let mut ranked = Vec::with_capacity(parsed.results.len());
            for entry in parsed.results {
                if entry.index >= documents.len() {
                    return Err(RerankError::MalformedResponse(format!(
                        "result index {} out of range for {} documents",
                        entry.index,
                        documents.len()
                    )));
                }
                ranked.push(RankedDocument {
                    index: entry.index,
                    score: entry.relevance_score,
                });
            }
            Ok(ranked)
        } else {
            let code = status.as_u16();
            let body = error_body(response).await;
            if status.is_client_error() {
                Err(RerankError::BadRequest { status: code, body })
            } else {
                Err(RerankError::Overloaded { status: code, body })
            }
        }
    }
}

/// What a `/health` probe found the server to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerHealth {
    /// HTTP 200: the model is loaded and requests are being served.
    Ready,
    /// HTTP 503: the server answers but the model is still loading.
    Starting,
    /// No usable answer — connection failure, timeout, or an
    /// unexpected status. The detail is diagnostic text.
    Unreachable(String),
}

/// Timeout for a single [`probe_health`] request.
pub const DEFAULT_HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

/// Probe a server's `/health` endpoint once, with the default timeout.
pub async fn probe_health(base_url: &str) -> ServerHealth {
    probe_health_with_timeout(base_url, DEFAULT_HEALTH_TIMEOUT).await
}

/// Probe a server's `/health` endpoint once. Never fails: every
/// outcome, including a refused connection, is a [`ServerHealth`]
/// value, so readiness loops and doctor rows consume it directly.
pub async fn probe_health_with_timeout(base_url: &str, timeout: Duration) -> ServerHealth {
    let url = format!("{}/health", base_url.trim_end_matches('/'));
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(client) => client,
        Err(e) => return ServerHealth::Unreachable(format!("HTTP client init failed: {e}")),
    };
    match client.get(&url).send().await {
        Ok(response) => match response.status().as_u16() {
            200 => ServerHealth::Ready,
            503 => ServerHealth::Starting,
            status => ServerHealth::Unreachable(format!("unexpected HTTP {status} from /health")),
        },
        Err(e) => ServerHealth::Unreachable(e.to_string()),
    }
}

/// Exponential backoff for the retry loop: `saturating_pow` and
/// `saturating_mul` keep the arithmetic panic- and overflow-free at
/// any attempt count, and the final clamp keeps the cadence bounded.
fn capped_backoff(base: Duration, attempt: u32) -> Duration {
    base.saturating_mul(2u32.saturating_pow(attempt))
        .min(MAX_BACKOFF)
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
    fn test_client(base_url: &str) -> RerankClient {
        RerankClient::new(
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

    fn docs(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("doc {i}")).collect()
    }

    #[tokio::test]
    async fn a_successful_call_returns_ranked_documents() {
        // The response shape a real llama-server returns: descending
        // scores, `top_n` respected, extra fields present.
        let url = mock_once(
            "200 OK",
            r#"{"model":"m","object":"list","usage":{"prompt_tokens":9,"total_tokens":9},
               "results":[{"index":2,"relevance_score":0.9},{"index":0,"relevance_score":0.1}]}"#,
        )
        .await;
        let ranked = test_client(&url)
            .rerank("q", &docs(3), 2)
            .await
            .expect("ok");
        assert_eq!(
            ranked,
            vec![
                RankedDocument {
                    index: 2,
                    score: 0.9
                },
                RankedDocument {
                    index: 0,
                    score: 0.1
                },
            ]
        );
    }

    #[tokio::test]
    async fn http_400_is_classified_as_bad_request() {
        let url = mock_once("400 Bad Request", r#"{"error":"no documents"}"#).await;
        let err = test_client(&url)
            .rerank("q", &docs(1), 1)
            .await
            .unwrap_err();
        assert!(
            matches!(err, RerankError::BadRequest { status: 400, .. }),
            "got {err:?}"
        );
        assert!(!err.is_transient());
    }

    #[tokio::test]
    async fn http_500_is_classified_as_overloaded() {
        let url = mock_once("500 Internal Server Error", "out of memory").await;
        let err = test_client(&url)
            .rerank("q", &docs(1), 1)
            .await
            .unwrap_err();
        assert!(
            matches!(err, RerankError::Overloaded { status: 500, .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_non_json_body_is_a_malformed_response() {
        let url = mock_once("200 OK", "this is not json").await;
        let err = test_client(&url)
            .rerank("q", &docs(1), 1)
            .await
            .unwrap_err();
        assert!(
            matches!(err, RerankError::MalformedResponse(_)),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn an_out_of_range_index_is_a_malformed_response() {
        let url = mock_once(
            "200 OK",
            r#"{"results":[{"index":5,"relevance_score":0.5}]}"#,
        )
        .await;
        let err = test_client(&url)
            .rerank("q", &docs(2), 2)
            .await
            .unwrap_err();
        assert!(
            matches!(err, RerankError::MalformedResponse(_)),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_refused_connection_is_unreachable() {
        let err = test_client(&dead_address().await)
            .rerank("q", &docs(1), 1)
            .await
            .unwrap_err();
        assert!(matches!(err, RerankError::Unreachable(_)), "got {err:?}");
        assert!(err.is_transient());
    }

    #[tokio::test]
    async fn empty_documents_return_empty_without_a_call() {
        // Pointing at a dead address proves no HTTP request is made.
        let ranked = test_client(&dead_address().await)
            .rerank("q", &[], 3)
            .await
            .expect("ok");
        assert!(ranked.is_empty());
    }

    #[tokio::test]
    async fn top_n_zero_returns_empty_without_a_call() {
        let ranked = test_client(&dead_address().await)
            .rerank("q", &docs(2), 0)
            .await
            .expect("ok");
        assert!(ranked.is_empty());
    }

    #[tokio::test]
    async fn health_200_is_ready() {
        let url = mock_once("200 OK", r#"{"status":"ok"}"#).await;
        assert_eq!(probe_health(&url).await, ServerHealth::Ready);
    }

    #[tokio::test]
    async fn health_503_is_starting() {
        // The body a loading llama-server answers with.
        let url = mock_once(
            "503 Service Unavailable",
            r#"{"error":{"message":"Loading model","type":"unavailable_error","code":503}}"#,
        )
        .await;
        assert_eq!(probe_health(&url).await, ServerHealth::Starting);
    }

    #[tokio::test]
    async fn health_on_a_dead_address_is_unreachable() {
        let health = probe_health(&dead_address().await).await;
        assert!(
            matches!(health, ServerHealth::Unreachable(_)),
            "got {health:?}"
        );
    }

    #[tokio::test]
    async fn health_on_an_unexpected_status_is_unreachable() {
        let url = mock_once("404 Not Found", "").await;
        let health = probe_health(&url).await;
        assert!(
            matches!(health, ServerHealth::Unreachable(_)),
            "got {health:?}"
        );
    }

    #[test]
    fn capped_backoff_is_panic_free_and_bounded_above() {
        for attempt in [0u32, 1, 2, 16, 31, 32, 64, u32::MAX] {
            let backoff = capped_backoff(Duration::from_millis(100), attempt);
            assert!(backoff <= MAX_BACKOFF, "attempt {attempt}: {backoff:?}");
        }
    }
}
