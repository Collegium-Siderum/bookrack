// SPDX-License-Identifier: Apache-2.0

//! A loopback stand-in for the Ollama surface daemon bring-up touches.

use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use bookrack_config::DEFAULT_EMBED_MODEL;

static URL: OnceLock<String> = OnceLock::new();
static FAILURE: AtomicU8 = AtomicU8::new(0);

/// What the `/api/embed` arm answers.
///
/// The tags arm is unaffected by every mode: bring-up reads the model
/// list from it and refuses to start on anything but a 200, so a stub
/// that failed both arms could not be reached from a daemon at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedFailure {
    /// One vector per input, at [`EmbedStub::DIMENSION`].
    None,
    /// HTTP 404 carrying Ollama's error envelope. The envelope is what
    /// separates an absent model from somebody else's 404, so a mode
    /// that dropped it would exercise a different classification.
    ModelNotFound,
    /// HTTP 503 with a body, the shape a loaded runner returns.
    Overloaded,
}

impl EmbedFailure {
    /// Wire form for [`FAILURE`]. Written out rather than cast from the
    /// discriminant so adding a variant is a compile error here.
    fn as_u8(self) -> u8 {
        match self {
            EmbedFailure::None => 0,
            EmbedFailure::ModelNotFound => 1,
            EmbedFailure::Overloaded => 2,
        }
    }

    /// Inverse of [`EmbedFailure::as_u8`]; any other byte is treated as
    /// [`EmbedFailure::None`], which no writer can produce.
    fn from_u8(byte: u8) -> EmbedFailure {
        match byte {
            1 => EmbedFailure::ModelNotFound,
            2 => EmbedFailure::Overloaded,
            _ => EmbedFailure::None,
        }
    }
}

/// A loopback HTTP server answering the two shapes daemon bring-up
/// touches: `POST /api/embed` returns one [`EmbedStub::DIMENSION`]-wide
/// vector per input, and every other path returns a model list holding
/// [`bookrack_config::DEFAULT_EMBED_MODEL`], which the bring-up
/// pre-flight requires before it will start.
///
/// One stub serves a whole test binary; the listener runs on detached
/// threads for the process's lifetime.
///
/// [`EmbedStub::set_failure`] switches the embed arm to a failure shape
/// so a client's error classification can be exercised end to end. The
/// switch is **process-global**: one stub serves the whole binary, so it
/// relies on the per-test process isolation `cargo nextest` gives.
/// Under a plain `cargo test` fallback, a flipped switch is visible to
/// every other test in the same binary.
pub struct EmbedStub;

impl EmbedStub {
    /// Vector width the stub reports for every input.
    // setting: internal -- the stub embedder's vector width, fixed by the fixtures reading it
    pub const DIMENSION: usize = 8;

    /// Make the embed arm answer `failure` from the next request on.
    /// Takes effect for requests already in flight only after they are
    /// answered; a test flips it before the call it is pinning.
    pub fn set_failure(failure: EmbedFailure) {
        FAILURE.store(failure.as_u8(), Ordering::SeqCst);
    }

    /// The mode the embed arm is currently answering.
    pub fn failure() -> EmbedFailure {
        EmbedFailure::from_u8(FAILURE.load(Ordering::SeqCst))
    }

    /// Start the stub on first call and return its base URL.
    pub fn url() -> &'static str {
        URL.get_or_init(|| {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind embed stub");
            let url = format!("http://{}", listener.local_addr().expect("embed stub addr"));
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    std::thread::spawn(move || {
                        let _ = serve(stream);
                    });
                }
            });
            url
        })
    }
}

/// Serve one connection until the client hangs up. Connections stay
/// open across requests so a pooled HTTP client can reuse them.
fn serve(stream: TcpStream) -> std::io::Result<()> {
    let mut reader = std::io::BufReader::new(stream.try_clone()?);
    let mut out = stream;
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }
        let mut content_length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header)? == 0 {
                return Ok(());
            }
            let header = header.trim();
            if header.is_empty() {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        let (status, response) = respond(&request_line, &body, EmbedStub::failure());
        let payload = response.to_string();
        write!(
            out,
            "HTTP/1.1 {}\r\ncontent-type: application/json\r\n\
             content-length: {}\r\n\r\n{payload}",
            status_line(status),
            payload.len(),
        )?;
        out.flush()?;
    }
}

/// Reason phrase for the three statuses [`respond`] produces.
fn status_line(status: u16) -> &'static str {
    match status {
        200 => "200 OK",
        404 => "404 Not Found",
        503 => "503 Service Unavailable",
        other => panic!("the stub has no status line for HTTP {other}"),
    }
}

/// Status and body for one request, factored out so the contract is
/// testable without a socket.
///
/// `failure` is passed in rather than read from the global switch, so a
/// unit test can drive every mode without the whole binary seeing it.
fn respond(request_line: &str, body: &[u8], failure: EmbedFailure) -> (u16, serde_json::Value) {
    if request_line.starts_with("POST /api/embed") {
        match failure {
            EmbedFailure::None => {
                let inputs = serde_json::from_slice::<serde_json::Value>(body)
                    .ok()
                    .and_then(|v| v["input"].as_array().map(Vec::len))
                    .unwrap_or(0);
                (
                    200,
                    serde_json::json!({
                        "embeddings": vec![vec![0.5f32; EmbedStub::DIMENSION]; inputs],
                    }),
                )
            }
            EmbedFailure::ModelNotFound => (
                404,
                serde_json::json!({
                    "error": format!(
                        "model {DEFAULT_EMBED_MODEL:?} not found, try pulling it first"
                    ),
                }),
            ),
            EmbedFailure::Overloaded => (
                503,
                serde_json::json!({ "error": "no runner slots are available" }),
            ),
        }
    } else {
        (
            200,
            serde_json::json!({ "models": [{ "name": DEFAULT_EMBED_MODEL }] }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both shapes, pinned. The embed arm must produce one vector per
    /// input at the declared width — a stub that returned a fixed count
    /// would let a dimension mismatch through — and the tags arm must
    /// list the default model, because an empty list fails every
    /// daemon-backed test before it begins.
    #[test]
    fn the_embed_stub_answers_the_two_shapes_the_daemon_touches() {
        let body = br#"{"input":["a","b","c"]}"#;
        let (status, embed) = respond("POST /api/embed HTTP/1.1", body, EmbedFailure::None);
        assert_eq!(status, 200);
        let vectors = embed["embeddings"].as_array().expect("embeddings array");
        assert_eq!(vectors.len(), 3);
        for vector in vectors {
            assert_eq!(
                vector.as_array().expect("vector array").len(),
                EmbedStub::DIMENSION,
            );
        }

        let (status, tags) = respond("GET /api/tags HTTP/1.1", b"", EmbedFailure::None);
        assert_eq!(status, 200);
        let models = tags["models"].as_array().expect("models array");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["name"].as_str(), Some(DEFAULT_EMBED_MODEL));
    }

    /// A failure mode is confined to the embed arm. The tags assertion
    /// is the sentinel: bring-up refuses to start on anything but a 200
    /// from that arm, so a switch that reached it would take every
    /// daemon-backed test in the workspace down with it.
    ///
    /// The 404 body must carry Ollama's `{"error": ...}` envelope —
    /// `bookrack_embed` classifies a bare 404 as a bad request instead
    /// of an absent model, so a stub without the envelope would pin the
    /// wrong branch.
    #[test]
    fn a_failure_mode_answers_only_the_embed_arm() {
        for (failure, expected) in [
            (EmbedFailure::ModelNotFound, 404u16),
            (EmbedFailure::Overloaded, 503),
        ] {
            let (status, embed) =
                respond("POST /api/embed HTTP/1.1", br#"{"input":["a"]}"#, failure);
            assert_eq!(status, expected, "{failure:?}");
            assert!(
                embed["error"].as_str().is_some(),
                "{failure:?} must answer with Ollama's error envelope: {embed}"
            );
            assert!(embed["embeddings"].is_null(), "{failure:?}: {embed}");
            assert!(!status_line(status).is_empty());

            let (status, tags) = respond("GET /api/tags HTTP/1.1", b"", failure);
            assert_eq!(status, 200, "the tags arm stays healthy under {failure:?}");
            assert_eq!(
                tags["models"][0]["name"].as_str(),
                Some(DEFAULT_EMBED_MODEL),
                "the tags arm stays healthy under {failure:?}",
            );
        }
    }

    /// The global switch round-trips through its atomic representation:
    /// a mode stored is the mode `serve` reads back.
    #[test]
    fn the_global_switch_round_trips_every_mode() {
        for failure in [
            EmbedFailure::ModelNotFound,
            EmbedFailure::Overloaded,
            EmbedFailure::None,
        ] {
            EmbedStub::set_failure(failure);
            assert_eq!(EmbedStub::failure(), failure);
        }
    }
}
