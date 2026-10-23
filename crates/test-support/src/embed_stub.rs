// SPDX-License-Identifier: Apache-2.0

//! A loopback stand-in for the Ollama surface daemon bring-up touches.

use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use bookrack_config::DEFAULT_EMBED_MODEL;

static SHARED: OnceLock<EmbedStub> = OnceLock::new();

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
    /// A well-formed 200 in which every input containing
    /// [`EmbedStub::UNUSABLE_MARKER`] gets an all-zero vector, which no
    /// cosine index can use. Inputs without the marker — bring-up's
    /// dimension probe among them — are answered normally, so a fixture
    /// decides which batch fails by where it puts the marker.
    UnusableVector,
}

impl EmbedFailure {
    /// Wire form for a stub's switch. Written out rather than cast from
    /// the discriminant so adding a variant is a compile error here.
    fn as_u8(self) -> u8 {
        match self {
            EmbedFailure::None => 0,
            EmbedFailure::ModelNotFound => 1,
            EmbedFailure::Overloaded => 2,
            EmbedFailure::UnusableVector => 3,
        }
    }

    /// Inverse of [`EmbedFailure::as_u8`]; any other byte is treated as
    /// [`EmbedFailure::None`], which no writer can produce.
    fn from_u8(byte: u8) -> EmbedFailure {
        match byte {
            1 => EmbedFailure::ModelNotFound,
            2 => EmbedFailure::Overloaded,
            3 => EmbedFailure::UnusableVector,
            _ => EmbedFailure::None,
        }
    }
}

/// A loopback HTTP server answering the two shapes daemon bring-up
/// touches: `POST /api/embed` returns one [`EmbedStub::DIMENSION`]-wide
/// vector per input, and every other path returns a model list holding
/// [`bookrack_config::DEFAULT_EMBED_MODEL`], which the bring-up
/// pre-flight requires before it will start, plus
/// [`EmbedStub::ALTERNATE_MODEL`] so a fixture can give two libraries
/// different embedding models and still pass that pre-flight.
///
/// Each stub owns a listener and a failure switch. [`EmbedStub::start`]
/// makes one that nothing else in the process can reach, for a test
/// that hands the URL to its daemon itself; [`EmbedStub::shared`] is
/// the one the whole binary shares, whose URL `process_env` installs in
/// the environment. Flipping the shared switch flips it for every test
/// in the process, so a test that does so relies on the one process per
/// test `cargo nextest` runs it in, exactly as the process environment
/// it was installed into does.
///
/// A listener runs on detached threads for the process's lifetime; a
/// dropped stub stops answering only when the process ends.
pub struct EmbedStub {
    url: String,
    failure: Arc<AtomicU8>,
}

impl EmbedStub {
    /// Vector width the stub reports for every input.
    // setting: internal -- the stub embedder's vector width, fixed by the fixtures reading it
    pub const DIMENSION: usize = 8;

    /// A second model tag the stub reports as available, at the same
    /// [`EmbedStub::DIMENSION`]. Exists so a multi-library fixture can
    /// declare two libraries with different embedding models: the
    /// bring-up pre-flight refuses a model the backend does not hold,
    /// so a one-model stub cannot express that case at all.
    // setting: internal -- the stub embedder's second model tag, fixed by the fixtures naming it
    pub const ALTERNATE_MODEL: &'static str = "stub-embedding:alternate";

    /// The text that marks an input for an all-zero vector under
    /// [`EmbedFailure::UnusableVector`].
    pub const UNUSABLE_MARKER: &'static str = "zero-vector-marker";

    /// Start a stub of its own on a kernel-assigned port, answering
    /// healthily until its switch is flipped.
    pub fn start() -> EmbedStub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind embed stub");
        let url = format!("http://{}", listener.local_addr().expect("embed stub addr"));
        let failure = Arc::new(AtomicU8::new(EmbedFailure::None.as_u8()));
        let switch = Arc::clone(&failure);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let switch = Arc::clone(&switch);
                std::thread::spawn(move || {
                    let _ = serve(stream, &switch);
                });
            }
        });
        EmbedStub { url, failure }
    }

    /// The stub shared by the whole binary, started on first call.
    pub fn shared() -> &'static EmbedStub {
        SHARED.get_or_init(EmbedStub::start)
    }

    /// The stub's base URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Make the embed arm answer `failure` from the next request on.
    /// Takes effect for requests already in flight only after they are
    /// answered; a test flips it before the call it is pinning.
    pub fn set_failure(&self, failure: EmbedFailure) {
        self.failure.store(failure.as_u8(), Ordering::SeqCst);
    }

    /// The mode the embed arm is currently answering.
    pub fn failure(&self) -> EmbedFailure {
        EmbedFailure::from_u8(self.failure.load(Ordering::SeqCst))
    }
}

/// Serve one connection until the client hangs up, reading `switch`
/// afresh for every request. Connections stay open across requests so
/// a pooled HTTP client can reuse them.
fn serve(stream: TcpStream, switch: &AtomicU8) -> std::io::Result<()> {
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
        let failure = EmbedFailure::from_u8(switch.load(Ordering::SeqCst));
        let (status, response) = respond(&request_line, &body, failure);
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
/// `failure` is passed in rather than read from a switch, so a unit
/// test can drive every mode without starting a listener.
fn respond(request_line: &str, body: &[u8], failure: EmbedFailure) -> (u16, serde_json::Value) {
    if request_line.starts_with("POST /api/embed") {
        let inputs: Vec<String> = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                v["input"].as_array().map(|inputs| {
                    inputs
                        .iter()
                        .map(|i| i.as_str().unwrap_or_default().to_string())
                        .collect()
                })
            })
            .unwrap_or_default();
        match failure {
            EmbedFailure::None => (
                200,
                serde_json::json!({
                    "embeddings": vec![vec![0.5f32; EmbedStub::DIMENSION]; inputs.len()],
                }),
            ),
            EmbedFailure::UnusableVector => {
                let embeddings: Vec<Vec<f32>> = inputs
                    .iter()
                    .map(|input| {
                        let fill = if input.contains(EmbedStub::UNUSABLE_MARKER) {
                            0.0
                        } else {
                            0.5
                        };
                        vec![fill; EmbedStub::DIMENSION]
                    })
                    .collect();
                (200, serde_json::json!({ "embeddings": embeddings }))
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
            serde_json::json!({
                "models": [
                    { "name": DEFAULT_EMBED_MODEL },
                    { "name": EmbedStub::ALTERNATE_MODEL },
                ],
            }),
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
        let models: Vec<&str> = tags["models"]
            .as_array()
            .expect("models array")
            .iter()
            .filter_map(|m| m["name"].as_str())
            .collect();
        assert_eq!(models, [DEFAULT_EMBED_MODEL, EmbedStub::ALTERNATE_MODEL]);
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

    /// Under the unusable-vector mode only the marked inputs come back
    /// as zero vectors; the rest, and the tags arm, are answered as in
    /// the healthy mode.
    #[test]
    fn the_unusable_vector_mode_zeroes_only_marked_inputs() {
        let body = format!(
            r#"{{"input":["plain","a {} b","plain again"]}}"#,
            EmbedStub::UNUSABLE_MARKER
        );
        let (status, embed) = respond(
            "POST /api/embed HTTP/1.1",
            body.as_bytes(),
            EmbedFailure::UnusableVector,
        );
        assert_eq!(status, 200);
        let firsts: Vec<f64> = embed["embeddings"]
            .as_array()
            .expect("embeddings array")
            .iter()
            .map(|v| {
                assert_eq!(v.as_array().expect("vector").len(), EmbedStub::DIMENSION);
                v[0].as_f64().expect("component")
            })
            .collect();
        assert_eq!(firsts, [0.5, 0.0, 0.5]);

        let (status, _) = respond("GET /api/tags HTTP/1.1", b"", EmbedFailure::UnusableVector);
        assert_eq!(status, 200);
    }

    /// A stub's switch round-trips through its atomic representation:
    /// a mode stored is the mode `serve` reads back.
    #[test]
    fn a_switch_round_trips_every_mode() {
        let stub = EmbedStub::start();
        for failure in [
            EmbedFailure::ModelNotFound,
            EmbedFailure::Overloaded,
            EmbedFailure::UnusableVector,
            EmbedFailure::None,
        ] {
            stub.set_failure(failure);
            assert_eq!(stub.failure(), failure);
        }
    }

    /// Send one embed request to `url` over a fresh connection and
    /// return the HTTP status the stub answers with.
    fn embed_status(url: &str) -> u16 {
        let addr = url.strip_prefix("http://").expect("loopback url");
        let mut stream = TcpStream::connect(addr).expect("connect to the stub");
        let body = br#"{"input":["a"]}"#;
        write!(
            stream,
            "POST /api/embed HTTP/1.1\r\nhost: {addr}\r\ncontent-length: {}\r\n\r\n",
            body.len(),
        )
        .expect("write request head");
        stream.write_all(body).expect("write request body");
        stream.flush().expect("flush request");
        let mut reader = std::io::BufReader::new(stream);
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .expect("read status line");
        status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("no status in {status_line:?}"))
    }

    /// Two stubs started in one process keep their switches apart:
    /// flipping one leaves the other answering healthily, over the
    /// socket, which is the only path a daemon reaches a stub by.
    #[test]
    fn two_stubs_keep_their_switches_apart() {
        let flipped = EmbedStub::start();
        let untouched = EmbedStub::start();
        assert_ne!(flipped.url(), untouched.url());

        flipped.set_failure(EmbedFailure::ModelNotFound);
        assert_eq!(embed_status(flipped.url()), 404);
        assert_eq!(embed_status(untouched.url()), 200);

        flipped.set_failure(EmbedFailure::None);
        assert_eq!(embed_status(flipped.url()), 200);
    }

    /// The shared stub is one stub: every call hands back the same
    /// listener, so the URL the process environment installs and the
    /// switch a test flips belong to the same server.
    #[test]
    fn the_shared_stub_is_started_once() {
        assert_eq!(EmbedStub::shared().url(), EmbedStub::shared().url());
        assert!(std::ptr::eq(EmbedStub::shared(), EmbedStub::shared()));
    }
}
