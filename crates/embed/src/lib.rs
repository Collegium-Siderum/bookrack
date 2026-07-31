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

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use bookrack_core::{Explain, Problem};
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

    /// Ollama answered 404 with its error envelope: the configured model
    /// is not present on the daemon. Pulling it is the only repair, so
    /// this never retries and never shrinks the batch.
    #[error("Ollama does not have the model {model:?}")]
    ModelNotFound {
        /// The model this client is configured to embed with.
        model: String,
        /// The `error` string from Ollama's response envelope.
        reason: String,
    },

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

impl Explain for EmbedError {
    fn explain(&self) -> Problem {
        match self {
            // Present tense: no amount of waiting puts the model on the
            // daemon. The response body is evidence, not the message.
            EmbedError::ModelNotFound { model, reason } => Problem::new(format!(
                "cannot embed: the model {model:?} is not available on the Ollama daemon"
            ))
            .detail(format!("Ollama answered HTTP 404: {reason}."))
            .hint(format!("Pull it first: {}.", pull_command(model))),

            // Past tense: the daemon may be up by the next attempt.
            EmbedError::Unreachable(reason) => Problem::new("could not reach Ollama")
                .detail(format!(
                    "The request failed before a response arrived: {reason}."
                ))
                .hint(
                    "Start Ollama, or point BOOKRACK_OLLAMA_URL at the host that runs it. \
                     Run `bookrack doctor` to check.",
                )
                .retryable(true),

            EmbedError::Overloaded { status, body } => {
                Problem::new("could not embed: the Ollama daemon is overloaded")
                    .detail(format!("Ollama answered HTTP {status}: {body}."))
                    .hint("Wait for the current load to clear, then retry with a smaller batch.")
                    .retryable(true)
            }

            // No wording written for these yet, so the flattening
            // fallback applies rather than a guessed hint.
            other => Problem::from_error_chain(other),
        }
    }
}

/// The command that puts `model` on the local Ollama daemon.
///
/// Shared as a fragment rather than as a finished sentence: the
/// callers that need it wrap it differently — a hint ends in a period,
/// a diagnostic table cell does not — and a shared sentence would force
/// one of them to change wording.
pub fn pull_command(model: &str) -> String {
    format!("ollama pull {model}")
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

/// The fixed text a dimension probe embeds to learn a model's output
/// dimension. Public so the probing caller and the cache below agree
/// on one string: a single-element batch of exactly this text is
/// answered from [`DIMENSION_CACHE`] once any earlier request for the
/// same `(model, base_url)` succeeded.
pub const DIMENSION_PROBE_TEXT: &str = "dimension probe";

/// Process-wide record of the embedding dimension observed per
/// `(model, base_url)` pair, filled by every successful
/// [`OllamaEmbedClient::embed_batch`] response. Clients are cheap and
/// short-lived while the daemon may open several per bring-up (books
/// and papers sides, once per mounted library); sharing the observed
/// dimension across them keeps the probe's network round-trip to one
/// per distinct pair.
static DIMENSION_CACHE: OnceLock<Mutex<HashMap<(String, String), usize>>> = OnceLock::new();

/// Read the cached dimension for a `(model, base_url)` pair.
fn cached_dimension(model: &str, base_url: &str) -> Option<usize> {
    DIMENSION_CACHE
        .get()?
        .lock()
        .ok()?
        .get(&(model.to_string(), base_url.to_string()))
        .copied()
}

/// Record the dimension observed in a successful response.
fn record_dimension(model: &str, base_url: &str, dimension: usize) {
    if let Ok(mut map) = DIMENSION_CACHE.get_or_init(Mutex::default).lock() {
        map.insert((model.to_string(), base_url.to_string()), dimension);
    }
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
    ///
    /// A batch that is exactly `[`[`DIMENSION_PROBE_TEXT`]`]` is
    /// answered without HTTP once any earlier request for the same
    /// `(model, base_url)` succeeded: the probe contract consumes only
    /// the vector's length, so the returned zero vector of the
    /// observed dimension is a valid answer. Any other batch always
    /// goes to the server.
    pub async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        if let [only] = texts
            && only == DIMENSION_PROBE_TEXT
            && let Some(dimension) = cached_dimension(&self.model, &self.base_url)
        {
            return Ok(vec![vec![0.0; dimension]]);
        }
        let mut attempt = 0u32;
        loop {
            match self.embed_once(texts).await {
                Ok(vectors) => {
                    if let Some(first) = vectors.first() {
                        record_dimension(&self.model, &self.base_url, first.len());
                    }
                    return Ok(vectors);
                }
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
    async fn embed_once(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let url = format!("{}/api/embed", self.base_url);
        let request = EmbedRequest {
            model: &self.model,
            input: texts,
        };
        let response = match self.http.post(&url).json(&request).send().await {
            Ok(response) => response,
            // A connection-time failure — refused, DNS, TLS, or a timeout
            // before the response head arrives — lands here; none is a
            // definite out-of-memory signal, so all are transient. A
            // genuine OOM surfaces as HTTP 5xx below; a timeout while
            // reading the body is classified at the decode step.
            Err(e) => return Err(EmbedError::Unreachable(e.to_string())),
        };

        let status = response.status();
        if status.is_success() {
            let parsed: EmbedResponse = response.json().await.map_err(|e| {
                // A timeout or connection failure while reading the body is
                // a transient blip worth a retry. `reqwest` reports such a
                // read failure with the same decode kind as genuinely
                // unparseable bytes, so the timeout / connect source is
                // what separates the two: only a real decode of a fully
                // received body stays a malformed response.
                if e.is_timeout() || e.is_connect() {
                    EmbedError::Unreachable(e.to_string())
                } else {
                    EmbedError::MalformedResponse(e.to_string())
                }
            })?;
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
            // Ollama serves "model not found" from three call sites whose
            // wording differs (quote style, and whether the sentence ends
            // in "try pulling it first") within one release, so the body
            // text is not a judgement. The status is: `EmbedHandler`
            // reserves 404 for an absent model and answers every other
            // failure 400 / 499 / 500 / 503. Requiring the error envelope
            // on top of the status keeps a 404 page from some unrelated
            // service at `BOOKRACK_OLLAMA_URL` out of this arm.
            if let (404, Some(reason)) = (code, error_envelope(&body)) {
                return Err(EmbedError::ModelNotFound {
                    model: self.model.clone(),
                    reason,
                });
            }
            if status.is_client_error() {
                Err(EmbedError::BadRequest { status: code, body })
            } else {
                Err(EmbedError::Overloaded { status: code, body })
            }
        }
    }
}

/// Exponential backoff for `embed_batch`'s retry loop, with two
/// guards against the previous `base * 2u32.pow(attempt)` shape:
/// `saturating_pow` keeps the `2^attempt` factor from panicking on
/// `attempt >= 32`, and `Duration::saturating_mul` keeps the
/// per-step multiplication from overflowing `Duration` even when
/// `base` is non-trivial. The final `.min(MAX_BACKOFF)` clamps to
/// the configured cap so the retry cadence stays predictable
/// regardless of how many attempts the loop has already burned.
fn capped_backoff(base: std::time::Duration, attempt: u32) -> std::time::Duration {
    base.saturating_mul(2u32.saturating_pow(attempt))
        .min(MAX_BACKOFF)
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

/// The `error` string of an Ollama failure envelope
/// (`{"error": "..."}`), or `None` when `body` is not one.
///
/// Every failure `EmbedHandler` reports goes out through `gin.H{"error":
/// ...}`, so the envelope is the shape that identifies the responder.
fn error_envelope(body: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Envelope {
        error: String,
    }
    serde_json::from_str::<Envelope>(body).ok().map(|e| e.error)
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

    /// Spawn a counting mock: the first `failures` connections are
    /// dropped after the request is read (a transient transport
    /// failure), every later request is answered with `status_line`
    /// and `body`. Returns the base URL and the connection counter.
    async fn counting_mock(
        failures: usize,
        status_line: &'static str,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let attempt = counter.fetch_add(1, Ordering::SeqCst);
                let mut scratch = [0u8; 8192];
                let _ = socket.read(&mut scratch).await;
                if attempt < failures {
                    // Close without a response: the client sees the
                    // connection die mid-request, a transient failure.
                    continue;
                }
                let response = format!(
                    "HTTP/1.1 {status_line}\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        (format!("http://{addr}"), hits)
    }

    /// A client with a short request timeout and no retries, for the
    /// body-read-timeout path.
    fn short_timeout_client(base_url: &str) -> OllamaEmbedClient {
        OllamaEmbedClient::new(
            base_url,
            "test-model",
            Duration::from_millis(200),
            0,
            Duration::from_millis(1),
        )
        .expect("client builds")
    }

    /// Spawn a mock that returns 2xx headers advertising a body it never
    /// finishes sending, then holds the connection open so the client's
    /// body read hits the request timeout rather than a decode error.
    async fn mock_headers_then_stall() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut scratch = [0u8; 8192];
            let _ = socket.read(&mut scratch).await;
            // Promise far more body than is ever delivered, then stall.
            let head = "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: 4096\r\n\r\n{";
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.flush().await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_until_the_server_recovers() {
        use std::sync::atomic::Ordering;
        let (url, hits) = counting_mock(2, "200 OK", r#"{"embeddings":[[1.0,2.0]]}"#).await;
        let client = OllamaEmbedClient::new(
            &url,
            "test-model",
            Duration::from_secs(5),
            3,
            Duration::from_millis(1),
        )
        .expect("client builds");
        let vectors = client
            .embed_batch(&["a".to_string()])
            .await
            .expect("recovers inside the retry budget");
        assert_eq!(vectors, vec![vec![1.0, 2.0]]);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            3,
            "two dropped attempts plus the success"
        );
    }

    #[tokio::test]
    async fn retries_stop_after_max_retries_attempts() {
        use std::sync::atomic::Ordering;
        let (url, hits) = counting_mock(usize::MAX, "200 OK", "{}").await;
        let client = OllamaEmbedClient::new(
            &url,
            "test-model",
            Duration::from_secs(5),
            2,
            Duration::from_millis(1),
        )
        .expect("client builds");
        let err = client.embed_batch(&["a".to_string()]).await.unwrap_err();
        assert!(matches!(err, EmbedError::Unreachable(_)), "got {err:?}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            3,
            "the initial attempt plus exactly max_retries retries"
        );
    }

    #[tokio::test]
    async fn a_non_transient_failure_does_not_consume_the_retry_budget() {
        use std::sync::atomic::Ordering;
        let (url, hits) = counting_mock(0, "400 Bad Request", r#"{"error":"bad"}"#).await;
        let client = OllamaEmbedClient::new(
            &url,
            "test-model",
            Duration::from_secs(5),
            3,
            Duration::from_millis(1),
        )
        .expect("client builds");
        let err = client.embed_batch(&["a".to_string()]).await.unwrap_err();
        assert!(
            matches!(err, EmbedError::BadRequest { status: 400, .. }),
            "got {err:?}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a 4xx fails fast instead of retrying"
        );
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
    async fn an_envelope_404_is_classified_as_model_not_found() {
        let url = mock_once(
            "404 Not Found",
            r#"{"error":"model \"test-model\" not found, try pulling it first"}"#,
        )
        .await;
        let err = test_client(&url)
            .embed_batch(&["x".to_string()])
            .await
            .unwrap_err();
        let EmbedError::ModelNotFound { model, reason } = &err else {
            panic!("got {err:?}");
        };
        assert_eq!(model, "test-model");
        assert!(reason.contains("not found"), "{reason}");
    }

    /// A 404 without Ollama's error envelope is somebody else's 404 —
    /// a proxy, a static file server, whatever `BOOKRACK_OLLAMA_URL`
    /// was pointed at by mistake. It must not be reported as an
    /// absent model.
    #[tokio::test]
    async fn a_bare_404_stays_a_bad_request() {
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
    async fn model_not_found_names_the_configured_model_not_the_response_body() {
        let url = mock_once(
            "404 Not Found",
            r#"{"error":"model 'some-other-model' not found"}"#,
        )
        .await;
        let err = test_client(&url)
            .embed_batch(&["x".to_string()])
            .await
            .unwrap_err();
        let EmbedError::ModelNotFound { model, .. } = &err else {
            panic!("got {err:?}");
        };
        assert_eq!(
            model, "test-model",
            "the model name must come from this client's configuration, not the response body"
        );
    }

    #[test]
    fn model_not_found_names_the_pull_command_in_hint() {
        let p = EmbedError::ModelNotFound {
            model: "test-model".into(),
            reason: "model not found".into(),
        }
        .explain();
        let hint = p.data.hint.expect("hint");
        assert!(hint.contains("ollama pull test-model"), "{hint}");
    }

    #[test]
    fn model_not_found_is_not_retryable_and_uses_present_tense() {
        let p = EmbedError::ModelNotFound {
            model: "test-model".into(),
            reason: "model not found".into(),
        }
        .explain();
        assert!(!p.data.retryable);
        assert!(p.summary.starts_with("cannot "), "{}", p.summary);
    }

    #[test]
    fn unreachable_is_retryable_and_uses_past_tense() {
        let p = EmbedError::Unreachable("connection refused".into()).explain();
        assert!(p.data.retryable);
        assert!(p.summary.starts_with("could not "), "{}", p.summary);
    }

    /// The raw HTTP payload is evidence, not the headline: it belongs
    /// in `detail`, where a terse renderer can drop it. Covers the
    /// variants `Explain` writes wording for; `BadRequest` and
    /// `MalformedResponse` still take the flattening fallback.
    #[test]
    fn raw_http_body_stays_out_of_the_summary() {
        let body = "ggml_backend_cuda_buffer_type_alloc_buffer: allocating 512.00 MiB failed";
        for e in [
            EmbedError::ModelNotFound {
                model: "test-model".into(),
                reason: body.into(),
            },
            EmbedError::Overloaded {
                status: 500,
                body: body.into(),
            },
        ] {
            let p = e.explain();
            assert!(!p.summary.contains(body), "{}", p.summary);
            assert!(
                p.data.detail.as_deref().is_some_and(|d| d.contains(body)),
                "{:?}",
                p.data.detail
            );
        }
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
    async fn a_body_read_timeout_is_transient_not_malformed() {
        // Headers arrive (so `send()` succeeds) but the body never
        // completes: the request timeout fires during the body read. That
        // is a transient blip, not a malformed response, and must retry.
        let url = mock_headers_then_stall().await;
        let err = short_timeout_client(&url)
            .embed_batch(&["x".to_string()])
            .await
            .unwrap_err();
        assert!(matches!(err, EmbedError::Unreachable(_)), "got {err:?}");
        assert!(err.is_transient(), "got {err:?}");
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

    /// Spawn a mock HTTP server that answers every request with the
    /// same 2-dimensional embedding response and counts the requests
    /// it serves. Returns the base URL and the request counter.
    async fn mock_counting() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = Arc::clone(&hits);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                hits_srv.fetch_add(1, Ordering::SeqCst);
                let mut scratch = [0u8; 8192];
                let _ = socket.read(&mut scratch).await;
                let body = r#"{"embeddings":[[1.0,2.0]]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        (format!("http://{addr}"), hits)
    }

    fn probe_batch() -> Vec<String> {
        vec![DIMENSION_PROBE_TEXT.to_string()]
    }

    #[tokio::test]
    async fn a_probe_for_a_known_model_and_url_skips_the_network() {
        use std::sync::atomic::Ordering;

        let (url, hits) = mock_counting().await;
        // Two clients, same (model, url): the first probe pays one
        // round-trip, the second is served from the cache.
        let first = OllamaEmbedClient::new(
            &url,
            "cache-model-shared",
            Duration::from_secs(5),
            0,
            Duration::from_millis(1),
        )
        .expect("client builds");
        let probed = first.embed_batch(&probe_batch()).await.expect("probe ok");
        assert_eq!(probed[0].len(), 2);
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let second = OllamaEmbedClient::new(
            &url,
            "cache-model-shared",
            Duration::from_secs(5),
            0,
            Duration::from_millis(1),
        )
        .expect("client builds");
        let cached = second.embed_batch(&probe_batch()).await.expect("probe ok");
        // Only the length carries meaning; the cached answer is a zero
        // vector of the observed dimension.
        assert_eq!(cached, vec![vec![0.0, 0.0]]);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "second probe must not hit HTTP"
        );
    }

    #[tokio::test]
    async fn a_probe_for_a_different_model_pays_its_own_round_trip() {
        use std::sync::atomic::Ordering;

        let (url, hits) = mock_counting().await;
        let a = OllamaEmbedClient::new(
            &url,
            "cache-model-a",
            Duration::from_secs(5),
            0,
            Duration::from_millis(1),
        )
        .expect("client builds");
        a.embed_batch(&probe_batch()).await.expect("probe ok");
        let b = OllamaEmbedClient::new(
            &url,
            "cache-model-b",
            Duration::from_secs(5),
            0,
            Duration::from_millis(1),
        )
        .expect("client builds");
        b.embed_batch(&probe_batch()).await.expect("probe ok");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_real_text_batch_never_consults_the_cache() {
        use std::sync::atomic::Ordering;

        let (url, hits) = mock_counting().await;
        let client = OllamaEmbedClient::new(
            &url,
            "cache-model-real-text",
            Duration::from_secs(5),
            0,
            Duration::from_millis(1),
        )
        .expect("client builds");
        client.embed_batch(&probe_batch()).await.expect("probe ok");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // A non-probe batch goes to the server even though the pair's
        // dimension is cached.
        client
            .embed_batch(&["real text".to_string()])
            .await
            .expect("batch ok");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_query_prefix_wraps_the_query() {
        let wrapped = build_query_input("dragons");
        assert!(wrapped.starts_with("Instruct:"));
        assert!(wrapped.ends_with("Query: dragons"));
    }

    /// The retry loop's exponential backoff must stay panic-free
    /// across the whole `u32` range and never exceed `MAX_BACKOFF`,
    /// so a retry pool that has accumulated dozens of failures does
    /// not abort the worker on overflow.
    #[test]
    fn capped_backoff_is_panic_free_and_bounded_above() {
        for attempt in [0u32, 1, 2, 16, 31, 32, 64, u32::MAX] {
            let backoff = capped_backoff(std::time::Duration::from_millis(100), attempt);
            assert!(
                backoff <= MAX_BACKOFF,
                "attempt {attempt}: backoff {backoff:?} exceeds {MAX_BACKOFF:?}"
            );
        }
    }

    /// A non-trivial `base` saturates `Duration` rather than wrapping
    /// when multiplied by a large `2^attempt`, and still respects the
    /// `MAX_BACKOFF` ceiling.
    #[test]
    fn capped_backoff_clamps_a_large_base() {
        let backoff = capped_backoff(std::time::Duration::from_secs(60), 32);
        assert_eq!(backoff, MAX_BACKOFF);
    }

    /// The first attempt with the historical default base returns
    /// `base` itself: the cap is never hit on the very first retry.
    #[test]
    fn capped_backoff_returns_base_at_attempt_zero() {
        let base = std::time::Duration::from_millis(125);
        assert_eq!(capped_backoff(base, 0), base);
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
    async fn probe_resolves_a_body_read_timeout_to_unreachable() {
        // A daemon whose headers arrive but whose body stalls past the
        // timeout is reachable-but-not-usable, not a protocol mismatch.
        let url = mock_headers_then_stall().await;
        let report = probe_ollama_with_timeout(&url, Duration::from_millis(200))
            .await
            .expect("probe returns ok");
        assert!(!report.reachable);
        assert!(report.models.is_empty());
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
