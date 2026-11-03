// SPDX-License-Identifier: Apache-2.0

//! A loopback stand-in for the Ollama surface daemon bring-up touches.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, BufRead, Read, Write};
use std::net::TcpListener;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bookrack_config::DEFAULT_EMBED_MODEL;

static SHARED: OnceLock<EmbedStub> = OnceLock::new();

/// The widest vector a stub answers with.
// setting: internal -- bounds what a stub spec may ask for, not a value to tune
const MAX_DIMENSION: usize = 8192;

/// FNV-1a 64-bit offset basis, which seeds a spread vector from its
/// input.
// setting: internal -- part of the FNV-1a definition
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64-bit prime.
// setting: internal -- part of the FNV-1a definition
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// splitmix64's increment, the golden-ratio gamma.
// setting: internal -- part of the splitmix64 definition
const SPLITMIX_GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

/// splitmix64's first finalizer multiplier.
// setting: internal -- part of the splitmix64 definition
const SPLITMIX_MIX1: u64 = 0xbf58_476d_1ce4_e5b9;

/// splitmix64's second finalizer multiplier.
// setting: internal -- part of the splitmix64 definition
const SPLITMIX_MIX2: u64 = 0x94d0_49bb_1331_11eb;

/// The weight of the input's spread vector inside a topic vector: small
/// enough that two inputs on one topic sit within a few thousandths of
/// each other in cosine distance, large enough that two inputs on one
/// topic are still distinct vectors.
// setting: internal -- the stub's topic geometry, fixed by the acceptance thresholds reading it
const TOPIC_SCATTER: f64 = 0.05;

/// The text that opens a topic mark; the ASCII digits that follow it
/// number the topic.
const TOPIC_MARK: &str = "sbx-topic-";

/// What the `/api/embed` arm answers.
///
/// The tags arm is unaffected by every mode: bring-up reads the model
/// list from it and refuses to start on anything but a 200, so a stub
/// that failed both arms could not be reached from a daemon at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedFailure {
    /// One vector per input, at the stub's width.
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

    /// The mode's word, as `POST /__stub/failure` takes it.
    pub fn token(self) -> &'static str {
        match self {
            EmbedFailure::None => "none",
            EmbedFailure::ModelNotFound => "model-not-found",
            EmbedFailure::Overloaded => "overloaded",
            EmbedFailure::UnusableVector => "unusable-vector",
        }
    }
}

/// Parses a mode's [`token`](EmbedFailure::token). The error names the
/// word it did not know.
impl FromStr for EmbedFailure {
    type Err = String;

    fn from_str(token: &str) -> Result<Self, Self::Err> {
        [
            EmbedFailure::None,
            EmbedFailure::ModelNotFound,
            EmbedFailure::Overloaded,
            EmbedFailure::UnusableVector,
        ]
        .into_iter()
        .find(|failure| failure.token() == token)
        .ok_or_else(|| format!("unknown failure mode {token:?}"))
    }
}

/// How a stub fills the vectors it answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vectors {
    /// Every component 0.5, whatever the input.
    Constant,
    /// A unit vector seeded by the input's bytes: the same input always
    /// gets the same vector, and different inputs point in different
    /// directions.
    Spread,
    /// A retrieval-shaped fill: inputs that carry the same topic mark
    /// point the same way, inputs on different topics are nearly
    /// orthogonal, and an input without a mark is its spread vector.
    ///
    /// A mark is `sbx-topic-` followed by the longest run of ASCII
    /// digits, read as the topic number `k`; a run that is empty or does
    /// not fit a `usize` is not a mark. `sbx-topic-1` and `sbx-topic-12`
    /// are two marks. An input's vector is the sum of one basis vector
    /// per distinct `k` it carries, at axis `k mod dimension`, plus
    /// [`TOPIC_SCATTER`] times its spread vector, normalized. The axis
    /// wraps when `k` reaches the width, so a caller keeps its topic
    /// numbers below the dimension; a query's instruction prefix carries
    /// no mark and so does not move the vector.
    Topic,
}

/// A stub's width, vector fill, and identity, valid by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StubSpec {
    dimension: usize,
    vectors: Vectors,
    identity: Option<String>,
}

/// Why [`StubSpec::new`] refused a spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StubSpecError {
    /// The width is outside `1..=max`.
    Dimension { got: usize, max: usize },
}

impl fmt::Display for StubSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StubSpecError::Dimension { got, max } => {
                write!(f, "a stub's vector width must be 1 to {max}, not {got}")
            }
        }
    }
}

impl std::error::Error for StubSpecError {}

impl StubSpec {
    /// A spec answering `dimension`-wide vectors filled as `vectors`
    /// says. `identity`, when set, is what `GET /__stub/identity`
    /// reports and the nonce `POST /__stub/shutdown` requires.
    pub fn new(
        dimension: usize,
        vectors: Vectors,
        identity: Option<String>,
    ) -> Result<StubSpec, StubSpecError> {
        if !(1..=MAX_DIMENSION).contains(&dimension) {
            return Err(StubSpecError::Dimension {
                got: dimension,
                max: MAX_DIMENSION,
            });
        }
        Ok(StubSpec {
            dimension,
            vectors,
            identity,
        })
    }

    pub fn dimension(&self) -> usize {
        self.dimension
    }

    pub fn vectors(&self) -> Vectors {
        self.vectors
    }

    pub fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }
}

/// [`EmbedStub::DIMENSION`] wide, [`Vectors::Constant`], no identity:
/// the shape every daemon-backed fixture reads.
impl Default for StubSpec {
    fn default() -> Self {
        StubSpec {
            dimension: EmbedStub::DIMENSION,
            vectors: Vectors::Constant,
            identity: None,
        }
    }
}

/// A loopback HTTP server answering the two shapes daemon bring-up
/// touches: `POST /api/embed` returns one vector per input at the
/// stub's width, and every other path returns a model list holding
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
/// Three control paths reach a stub from outside the process, each
/// answered with `connection: close`: `POST /__stub/failure` takes a
/// mode's [`token`](EmbedFailure::token) and flips the switch;
/// `GET /__stub/identity` reports the spec's identity; and
/// `POST /__stub/shutdown`, given the identity as its nonce, tells the
/// holder through [`EmbedStub::wait_shutdown`]. The stub itself keeps
/// serving: what a shutdown means is the holder's call.
///
/// A listener runs on detached threads for the process's lifetime; a
/// dropped stub stops answering only when the process ends.
pub struct EmbedStub {
    url: String,
    spec: Arc<StubSpec>,
    failure: Arc<AtomicU8>,
    /// Receives one `()` per accepted shutdown. Behind a mutex because a
    /// receiver is not `Sync`, and the shared stub lives in a static.
    shutdown: Mutex<Receiver<()>>,
    /// Latched once a shutdown has been received.
    shut_down: AtomicBool,
}

impl EmbedStub {
    /// Default vector width, the one [`EmbedStub::start`] and
    /// [`EmbedStub::shared`] answer with.
    // setting: internal -- the stub embedder's vector width, fixed by the fixtures reading it
    pub const DIMENSION: usize = 8;

    /// A second model tag the stub reports as available, at the same
    /// width. Exists so a multi-library fixture can declare two
    /// libraries with different embedding models: the bring-up
    /// pre-flight refuses a model the backend does not hold, so a
    /// one-model stub cannot express that case at all.
    // setting: internal -- the stub embedder's second model tag, fixed by the fixtures naming it
    pub const ALTERNATE_MODEL: &'static str = "stub-embedding:alternate";

    /// The text that marks an input for an all-zero vector under
    /// [`EmbedFailure::UnusableVector`].
    pub const UNUSABLE_MARKER: &'static str = "zero-vector-marker";

    /// Start a stub of its own on a kernel-assigned port, answering
    /// healthily with [`StubSpec::default`] until its switch is flipped.
    pub fn start() -> EmbedStub {
        EmbedStub::start_with(StubSpec::default())
    }

    /// [`start`](EmbedStub::start) with `spec`'s width, fill, and
    /// identity.
    pub fn start_with(spec: StubSpec) -> EmbedStub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind embed stub");
        let url = format!("http://{}", listener.local_addr().expect("embed stub addr"));
        let failure = Arc::new(AtomicU8::new(EmbedFailure::None.as_u8()));
        let switch = Arc::clone(&failure);
        let spec = Arc::new(spec);
        let served = Arc::clone(&spec);
        let (announce, shutdown) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let switch = Arc::clone(&switch);
                let spec = Arc::clone(&served);
                let announce: Sender<()> = announce.clone();
                std::thread::spawn(move || {
                    let _ = serve(stream, &switch, &spec, &|| {
                        let _ = announce.send(());
                    });
                });
            }
        });
        EmbedStub {
            url,
            spec,
            failure,
            shutdown: Mutex::new(shutdown),
            shut_down: AtomicBool::new(false),
        }
    }

    /// The width, fill and identity this stub answers with.
    pub fn spec(&self) -> &StubSpec {
        &self.spec
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

    /// Whether a shutdown was accepted, waiting up to `within` for one;
    /// [`Duration::ZERO`] only looks. Once one is seen, every later call
    /// answers `true` at once. A stub without an identity never accepts
    /// one.
    pub fn wait_shutdown(&self, within: Duration) -> bool {
        if self.shut_down.load(Ordering::SeqCst) {
            return true;
        }
        let shutdown = self.shutdown.lock().unwrap();
        let heard = if within.is_zero() {
            shutdown.try_recv().is_ok()
        } else {
            shutdown.recv_timeout(within).is_ok()
        };
        if heard {
            self.shut_down.store(true, Ordering::SeqCst);
        }
        heard
    }
}

/// Serve one connection until the client hangs up, reading `switch`
/// afresh for every request. Model and embed answers keep the
/// connection open so a pooled HTTP client can reuse it; a control
/// answer closes it.
///
/// `announce` is called for an accepted shutdown, and only once its
/// answer was written and flushed: that it reached the local transport
/// is all this promises, not that the client read it.
fn serve<RW: Read + Write>(
    stream: RW,
    switch: &AtomicU8,
    spec: &StubSpec,
    announce: &dyn Fn(),
) -> io::Result<()> {
    let mut reader = io::BufReader::new(stream);
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
        if let Some((status, response, shutdown)) = control(&request_line, &body, switch, spec) {
            send(reader.get_mut(), status, &response, true)?;
            if shutdown {
                announce();
            }
            return Ok(());
        }
        let failure = EmbedFailure::from_u8(switch.load(Ordering::SeqCst));
        let (status, response) = respond(&request_line, &body, failure, spec);
        send(reader.get_mut(), status, &response, false)?;
    }
}

/// Write one response and flush it; `close` marks it as the last on
/// the connection.
fn send(
    out: &mut impl Write,
    status: u16,
    response: &serde_json::Value,
    close: bool,
) -> io::Result<()> {
    let payload = response.to_string();
    let connection = if close { "connection: close\r\n" } else { "" };
    let message = format!(
        "HTTP/1.1 {}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n{connection}\r\n{payload}",
        status_line(status),
        payload.len(),
    );
    out.write_all(message.as_bytes())?;
    out.flush()
}

/// The answer to a control path, or `None` for any other request. The
/// flag says the answer accepts a shutdown.
fn control(
    request_line: &str,
    body: &[u8],
    switch: &AtomicU8,
    spec: &StubSpec,
) -> Option<(u16, serde_json::Value, bool)> {
    let mut parts = request_line.split_whitespace();
    let (method, path) = (parts.next()?, parts.next()?);
    match (method, path) {
        ("POST", "/__stub/failure") => {
            let token = String::from_utf8_lossy(body);
            Some(match token.trim().parse::<EmbedFailure>() {
                Ok(failure) => {
                    switch.store(failure.as_u8(), Ordering::SeqCst);
                    (
                        200,
                        serde_json::json!({ "failure": failure.token() }),
                        false,
                    )
                }
                Err(err) => (400, serde_json::json!({ "error": err }), false),
            })
        }
        ("GET", "/__stub/identity") => Some((
            200,
            serde_json::json!({ "identity": spec.identity() }),
            false,
        )),
        ("POST", "/__stub/shutdown") => Some(match spec.identity() {
            Some(identity) if identity.as_bytes() == body => {
                (200, serde_json::json!({ "shutdown": true }), true)
            }
            _ => (
                403,
                serde_json::json!({ "error": "the nonce is not this stub's identity" }),
                false,
            ),
        }),
        _ => None,
    }
}

/// Reason phrase for every status the stub produces.
fn status_line(status: u16) -> &'static str {
    match status {
        200 => "200 OK",
        400 => "400 Bad Request",
        403 => "403 Forbidden",
        404 => "404 Not Found",
        503 => "503 Service Unavailable",
        other => panic!("the stub has no status line for HTTP {other}"),
    }
}

/// Status and body for one model or embed request, factored out so the
/// contract is testable without a socket.
///
/// `failure` is passed in rather than read from a switch, so a unit
/// test can drive every mode without starting a listener.
fn respond(
    request_line: &str,
    body: &[u8],
    failure: EmbedFailure,
    spec: &StubSpec,
) -> (u16, serde_json::Value) {
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
            EmbedFailure::None => {
                let embeddings: Vec<Vec<f32>> =
                    inputs.iter().map(|input| vector(input, spec)).collect();
                (200, serde_json::json!({ "embeddings": embeddings }))
            }
            EmbedFailure::UnusableVector => {
                let embeddings: Vec<Vec<f32>> = inputs
                    .iter()
                    .map(|input| {
                        if input.contains(EmbedStub::UNUSABLE_MARKER) {
                            vec![0.0; spec.dimension]
                        } else {
                            vector(input, spec)
                        }
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

/// The healthy vector for `input` under `spec`.
fn vector(input: &str, spec: &StubSpec) -> Vec<f32> {
    match spec.vectors {
        Vectors::Constant => vec![0.5; spec.dimension],
        Vectors::Spread => spread(input.as_bytes(), spec.dimension),
        Vectors::Topic => topic(input, spec.dimension),
    }
}

/// The topic numbers `input` carries, each once; see [`Vectors::Topic`].
fn topic_marks(input: &str) -> BTreeSet<usize> {
    let mut marks = BTreeSet::new();
    for (at, _) in input.match_indices(TOPIC_MARK) {
        let rest = &input[at + TOPIC_MARK.len()..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if let Ok(k) = rest[..end].parse::<usize>() {
            marks.insert(k);
        }
    }
    marks
}

/// The [`Vectors::Topic`] vector for `input`: its spread vector when it
/// carries no mark, otherwise the marked axes plus the scattered spread
/// vector, normalized.
fn topic(input: &str, dimension: usize) -> Vec<f32> {
    let scatter = spread(input.as_bytes(), dimension);
    let marks = topic_marks(input);
    if marks.is_empty() {
        return scatter;
    }
    let mut components: Vec<f64> = scatter
        .iter()
        .map(|c| f64::from(*c) * TOPIC_SCATTER)
        .collect();
    for k in marks {
        components[k % dimension] += 1.0;
    }
    let norm = components.iter().map(|c| c * c).sum::<f64>().sqrt();
    components.iter().map(|c| (c / norm) as f32).collect()
}

/// A unit vector of `dimension` components drawn from splitmix64,
/// seeded with the FNV-1a hash of `input`, each component mapped onto
/// [-1, 1) before the vector is normalized. A vector that comes out
/// all zero falls back to the first basis vector.
fn spread(input: &[u8], dimension: usize) -> Vec<f32> {
    let mut state = input.iter().fold(FNV_OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    });
    let components: Vec<f64> = (0..dimension)
        .map(|_| {
            state = state.wrapping_add(SPLITMIX_GAMMA);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(SPLITMIX_MIX1);
            z = (z ^ (z >> 27)).wrapping_mul(SPLITMIX_MIX2);
            z ^= z >> 31;
            (z >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
        })
        .collect();
    let norm = components.iter().map(|c| c * c).sum::<f64>().sqrt();
    if norm == 0.0 {
        let mut basis = vec![0.0; dimension];
        basis[0] = 1.0;
        return basis;
    }
    components.iter().map(|c| (c / norm) as f32).collect()
}

#[cfg(test)]
mod tests {
    use std::net::TcpStream;

    use super::*;

    /// Both shapes, pinned. The embed arm must produce one vector per
    /// input at the declared width — a stub that returned a fixed count
    /// would let a dimension mismatch through — and the tags arm must
    /// list the default model, because an empty list fails every
    /// daemon-backed test before it begins.
    #[test]
    fn the_embed_stub_answers_the_two_shapes_the_daemon_touches() {
        let body = br#"{"input":["a","b","c"]}"#;
        let (status, embed) = respond(
            "POST /api/embed HTTP/1.1",
            body,
            EmbedFailure::None,
            &StubSpec::default(),
        );
        assert_eq!(status, 200);
        let vectors = embed["embeddings"].as_array().expect("embeddings array");
        assert_eq!(vectors.len(), 3);
        for vector in vectors {
            assert_eq!(
                vector.as_array().expect("vector array").len(),
                EmbedStub::DIMENSION,
            );
        }

        let (status, tags) = respond(
            "GET /api/tags HTTP/1.1",
            b"",
            EmbedFailure::None,
            &StubSpec::default(),
        );
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
            let (status, embed) = respond(
                "POST /api/embed HTTP/1.1",
                br#"{"input":["a"]}"#,
                failure,
                &StubSpec::default(),
            );
            assert_eq!(status, expected, "{failure:?}");
            assert!(
                embed["error"].as_str().is_some(),
                "{failure:?} must answer with Ollama's error envelope: {embed}"
            );
            assert!(embed["embeddings"].is_null(), "{failure:?}: {embed}");
            assert!(!status_line(status).is_empty());

            let (status, tags) =
                respond("GET /api/tags HTTP/1.1", b"", failure, &StubSpec::default());
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
            &StubSpec::default(),
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

        let (status, _) = respond(
            "GET /api/tags HTTP/1.1",
            b"",
            EmbedFailure::UnusableVector,
            &StubSpec::default(),
        );
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

    /// One HTTP answer, read whole.
    struct Answer {
        status: u16,
        /// Header lines, lowercased.
        headers: Vec<String>,
        body: serde_json::Value,
        /// Whether the stub closed the connection after the answer.
        closed: bool,
    }

    /// Send one request to `url` over a fresh connection and read the
    /// answer by its `content-length`, then whether the connection
    /// closed after it.
    fn http(url: &str, method: &str, path: &str, body: &[u8]) -> Answer {
        let addr = url.strip_prefix("http://").expect("loopback url");
        let mut stream = TcpStream::connect(addr).expect("connect to the stub");
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nhost: {addr}\r\ncontent-length: {}\r\n\r\n",
            body.len(),
        )
        .expect("write request head");
        stream.write_all(body).expect("write request body");
        stream.flush().expect("flush request");
        let mut reader = io::BufReader::new(stream);
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .expect("read status line");
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("no status in {status_line:?}"));
        let mut headers = Vec::new();
        let mut length = 0usize;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).expect("read header");
            let header = header.trim().to_ascii_lowercase();
            if header.is_empty() {
                break;
            }
            if let Some(value) = header.strip_prefix("content-length:") {
                length = value.trim().parse().expect("content-length");
            }
            headers.push(header);
        }
        let mut payload = vec![0u8; length];
        reader.read_exact(&mut payload).expect("read body");
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("set a read timeout");
        let mut probe = [0u8; 1];
        let closed = matches!(reader.read(&mut probe), Ok(0));
        let body = serde_json::from_slice(&payload).expect("a JSON body");
        Answer {
            status,
            headers,
            body,
            closed,
        }
    }

    /// The vectors a stub at `url` answers for `inputs`.
    fn embed(url: &str, inputs: &[&str]) -> Vec<Vec<f64>> {
        let request = serde_json::json!({ "input": inputs }).to_string();
        let answer = http(url, "POST", "/api/embed", request.as_bytes());
        assert_eq!(answer.status, 200, "{}", answer.body);
        vectors(&answer.body)
    }

    fn vectors(body: &serde_json::Value) -> Vec<Vec<f64>> {
        body["embeddings"]
            .as_array()
            .expect("embeddings array")
            .iter()
            .map(|v| {
                v.as_array()
                    .expect("vector array")
                    .iter()
                    .map(|c| c.as_f64().expect("component"))
                    .collect()
            })
            .collect()
    }

    fn norm(v: &[f64]) -> f64 {
        v.iter().map(|c| c * c).sum::<f64>().sqrt()
    }

    fn cosine(a: &[f64], b: &[f64]) -> f64 {
        a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>() / (norm(a) * norm(b))
    }

    fn spec(dimension: usize, vectors: Vectors, identity: Option<&str>) -> StubSpec {
        StubSpec::new(dimension, vectors, identity.map(str::to_string)).expect("a valid spec")
    }

    /// Three inputs on one topic, one of them behind a query-side
    /// instruction prefix, point the same way.
    #[test]
    fn the_topic_mode_points_one_topic_one_way() {
        let stub = EmbedStub::start_with(spec(1024, Vectors::Topic, None));
        let got = embed(
            stub.url(),
            &[
                "The marker sbx-alpha-b1-c1 opens this chapter under sbx-topic-3. A lantern.",
                "Represent this sentence for searching relevant passages: sbx-topic-3",
                "sbx-topic-3 is the only thing these three inputs share, in different words",
            ],
        );
        for (i, j) in [(0, 1), (0, 2), (1, 2)] {
            let c = cosine(&got[i], &got[j]);
            assert!(c > 0.99, "inputs {i} and {j} have cosine {c}");
        }
    }

    /// Different topics are nearly orthogonal, and a mark is read to
    /// the end of its digits: `sbx-topic-1` is not a prefix of
    /// `sbx-topic-12`.
    #[test]
    fn different_topics_are_nearly_orthogonal() {
        let stub = EmbedStub::start_with(spec(1024, Vectors::Topic, None));
        let got = embed(
            stub.url(),
            &[
                "about sbx-topic-1",
                "about sbx-topic-2",
                "about sbx-topic-12",
            ],
        );
        for (i, j) in [(0, 1), (0, 2), (1, 2)] {
            let c = cosine(&got[i], &got[j]);
            assert!(c.abs() < 0.1, "inputs {i} and {j} have cosine {c}");
        }
    }

    /// An input on two topics sits between them.
    #[test]
    fn two_topics_split_the_vector_between_them() {
        let stub = EmbedStub::start_with(spec(1024, Vectors::Topic, None));
        let got = embed(
            stub.url(),
            &[
                "sbx-topic-1 and sbx-topic-2 together",
                "only sbx-topic-1",
                "only sbx-topic-2",
            ],
        );
        for j in [1, 2] {
            let c = cosine(&got[0], &got[j]);
            assert!((0.6..=0.8).contains(&c), "input {j}: cosine {c}");
        }
    }

    /// A mark repeated in one input counts once: {1, 1, 2} and {1, 2}
    /// point the same way, where counting twice would tilt the first
    /// towards axis 1 (cosine about 0.95).
    #[test]
    fn a_repeated_mark_counts_once() {
        let stub = EmbedStub::start_with(spec(1024, Vectors::Topic, None));
        let got = embed(
            stub.url(),
            &[
                "sbx-topic-1 then sbx-topic-1 again, then sbx-topic-2",
                "sbx-topic-1 with sbx-topic-2",
            ],
        );
        let c = cosine(&got[0], &got[1]);
        assert!(c > 0.99, "cosine {c}");
    }

    /// The topic number selects the axis directly, modulo the width.
    #[test]
    fn the_mark_picks_its_axis() {
        fn argmax(v: &[f64]) -> usize {
            v.iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                .map(|(i, _)| i)
                .expect("a component")
        }
        let wide = EmbedStub::start_with(spec(1024, Vectors::Topic, None));
        assert_eq!(argmax(&embed(wide.url(), &["sbx-topic-7"])[0]), 7);
        let narrow = EmbedStub::start_with(spec(8, Vectors::Topic, None));
        assert_eq!(argmax(&embed(narrow.url(), &["sbx-topic-9"])[0]), 1);
    }

    /// `sbx-topic-` with no digits, or with digits past `usize`, marks
    /// nothing: the input reads as it would in the spread mode.
    #[test]
    fn a_malformed_mark_reads_as_no_mark() {
        let topic = EmbedStub::start_with(spec(64, Vectors::Topic, None));
        let spread = EmbedStub::start_with(spec(64, Vectors::Spread, None));
        let inputs = [
            "sbx-topic- has nothing after it",
            "sbx-topic-99999999999999999999999999 does not fit",
            "sbx-topic-x is not a number",
        ];
        assert_eq!(embed(topic.url(), &inputs), embed(spread.url(), &inputs));
    }

    /// An input without a mark is its spread vector, component for
    /// component.
    #[test]
    fn an_input_without_a_topic_reads_as_spread() {
        let topic = EmbedStub::start_with(spec(1024, Vectors::Topic, None));
        let spread = EmbedStub::start_with(spec(1024, Vectors::Spread, None));
        let inputs = [
            "a chapter with no mark at all",
            "sbx-alpha-b1-c1 is another kind of mark",
        ];
        assert_eq!(embed(topic.url(), &inputs), embed(spread.url(), &inputs));
    }

    /// Topic vectors are unit vectors at any width, and the axis wraps:
    /// at width 8, topics 9 and 1 share an axis.
    #[test]
    fn the_topic_mode_is_a_unit_vector_at_any_width() {
        for width in [8, 1024] {
            let stub = EmbedStub::start_with(spec(width, Vectors::Topic, None));
            let got = embed(
                stub.url(),
                &["sbx-topic-9 here", "sbx-topic-1 there", "no mark"],
            );
            for v in &got {
                assert_eq!(v.len(), width);
                assert!(
                    (norm(v) - 1.0).abs() < 1e-5,
                    "width {width}: norm {}",
                    norm(v)
                );
            }
            let c = cosine(&got[0], &got[1]);
            if width == 8 {
                assert!(
                    c > 0.99,
                    "width {width}: 9 and 1 should wrap onto one axis: {c}"
                );
            } else {
                assert!(
                    c.abs() < 0.1,
                    "width {width}: 9 and 1 are distinct topics: {c}"
                );
            }
        }
    }

    /// Spread vectors are unit vectors, and different inputs point in
    /// different directions, at a realistic width and at the default.
    #[test]
    fn the_spread_mode_gives_each_input_its_own_unit_vector() {
        for width in [1024, 8] {
            let (status, body) = respond(
                "POST /api/embed HTTP/1.1",
                br#"{"input":["alpha","beta","gamma"]}"#,
                EmbedFailure::None,
                &spec(width, Vectors::Spread, None),
            );
            assert_eq!(status, 200);
            let got = vectors(&body);
            assert_eq!(got.len(), 3);
            for v in &got {
                assert_eq!(v.len(), width);
                assert!(
                    (norm(v) - 1.0).abs() < 1e-5,
                    "width {width}: norm {}",
                    norm(v)
                );
            }
            for (i, j) in [(0, 1), (0, 2), (1, 2)] {
                let c = cosine(&got[i], &got[j]);
                assert!(
                    c < 0.99,
                    "width {width}: inputs {i} and {j} have cosine {c}"
                );
            }
        }
    }

    /// A spread vector depends on the input alone: two stubs, and the
    /// same stub across other requests, answer the same input alike.
    #[test]
    fn the_spread_mode_is_a_function_of_the_input() {
        let first = EmbedStub::start_with(spec(64, Vectors::Spread, None));
        let second = EmbedStub::start_with(spec(64, Vectors::Spread, None));
        let before = embed(first.url(), &["same input"]);
        embed(first.url(), &["something else"]);
        let after = embed(first.url(), &["same input"]);
        let elsewhere = embed(second.url(), &["same input"]);
        assert_eq!(before, after);
        assert_eq!(before, elsewhere);
    }

    /// Every mode answers at the spec's width, over the socket: the
    /// constant fill, the spread fill, and the unusable-vector mode
    /// flipped through the control path.
    #[test]
    fn every_mode_answers_at_the_configured_width() {
        let marked = format!("a {} b", EmbedStub::UNUSABLE_MARKER);
        for width in [1, 16] {
            for fill in [Vectors::Constant, Vectors::Spread] {
                let stub = EmbedStub::start_with(spec(width, fill, None));
                let healthy = embed(stub.url(), &["plain", &marked]);
                for v in &healthy {
                    assert_eq!(v.len(), width, "{fill:?} at {width}");
                    if fill == Vectors::Constant {
                        assert!(v.iter().all(|c| *c == 0.5), "{fill:?} at {width}: {v:?}");
                    }
                }
                let flipped = http(stub.url(), "POST", "/__stub/failure", b"unusable-vector");
                assert_eq!(flipped.status, 200);
                let unusable = embed(stub.url(), &["plain", &marked]);
                assert_eq!(
                    unusable[0], healthy[0],
                    "{fill:?} at {width}: unmarked input"
                );
                assert_eq!(
                    unusable[1],
                    vec![0.0; width],
                    "{fill:?} at {width}: marked input"
                );
            }
        }
    }

    /// Under the unusable-vector mode with spread vectors, the marked
    /// inputs come back as zero vectors and the rest as unit vectors.
    #[test]
    fn the_unusable_vector_mode_zeroes_only_marked_inputs_when_spread() {
        let body = format!(
            r#"{{"input":["plain","a {} b","plain again"]}}"#,
            EmbedStub::UNUSABLE_MARKER
        );
        let (status, embed) = respond(
            "POST /api/embed HTTP/1.1",
            body.as_bytes(),
            EmbedFailure::UnusableVector,
            &spec(32, Vectors::Spread, None),
        );
        assert_eq!(status, 200);
        let got = vectors(&embed);
        assert!((norm(&got[0]) - 1.0).abs() < 1e-5);
        assert_eq!(got[1], vec![0.0; 32]);
        assert!((norm(&got[2]) - 1.0).abs() < 1e-5);
    }

    /// A width outside `1..=MAX_DIMENSION` cannot be built into a spec,
    /// and the refusal names the width and the bound.
    #[test]
    fn a_width_outside_the_range_is_refused() {
        for width in [0, MAX_DIMENSION + 1] {
            let err = StubSpec::new(width, Vectors::Constant, None)
                .expect_err("a width outside the range is refused");
            assert_eq!(
                err,
                StubSpecError::Dimension {
                    got: width,
                    max: MAX_DIMENSION
                }
            );
            let message = err.to_string();
            assert!(
                message.contains(&width.to_string())
                    && message.contains(&MAX_DIMENSION.to_string()),
                "{message}"
            );
        }
        for width in [1, MAX_DIMENSION] {
            StubSpec::new(width, Vectors::Spread, None).expect("a width inside the range");
        }
    }

    /// The failure switch flips over the socket, confined to the embed
    /// arm, and an unknown mode is refused without touching it.
    #[test]
    fn the_failure_switch_answers_over_http() {
        let stub = EmbedStub::start();
        let flipped = http(stub.url(), "POST", "/__stub/failure", b"overloaded");
        assert_eq!(flipped.status, 200);
        assert_eq!(flipped.body, serde_json::json!({ "failure": "overloaded" }));
        assert_eq!(embed_status(stub.url()), 503);
        assert_eq!(http(stub.url(), "GET", "/api/tags", b"").status, 200);

        assert_eq!(
            http(stub.url(), "POST", "/__stub/failure", b"none").status,
            200
        );
        assert_eq!(embed_status(stub.url()), 200);

        let refused = http(stub.url(), "POST", "/__stub/failure", b"bogus");
        assert_eq!(refused.status, 400, "{}", refused.body);
        assert_eq!(stub.failure(), EmbedFailure::None);
        assert_eq!(embed_status(stub.url()), 200);
    }

    /// The identity path reports the spec's identity, or null.
    #[test]
    fn the_identity_endpoint_answers_its_identity() {
        let named = EmbedStub::start_with(spec(8, Vectors::Constant, Some("n1")));
        let answer = http(named.url(), "GET", "/__stub/identity", b"");
        assert_eq!(answer.status, 200);
        assert_eq!(answer.body, serde_json::json!({ "identity": "n1" }));

        let anonymous = EmbedStub::start();
        let answer = http(anonymous.url(), "GET", "/__stub/identity", b"");
        assert_eq!(answer.body, serde_json::json!({ "identity": null }));
    }

    /// Every control answer carries `connection: close` and closes the
    /// connection after its body; a model answer keeps it open for the
    /// next request.
    #[test]
    fn a_control_answer_closes_its_connection() {
        let stub = EmbedStub::start_with(spec(8, Vectors::Constant, Some("n1")));
        for (method, path, body) in [
            ("POST", "/__stub/failure", &b"none"[..]),
            ("GET", "/__stub/identity", &b""[..]),
            ("POST", "/__stub/shutdown", &b"wrong"[..]),
            ("POST", "/__stub/shutdown", &b"n1"[..]),
        ] {
            let answer = http(stub.url(), method, path, body);
            assert!(
                answer.headers.iter().any(|h| h == "connection: close"),
                "{method} {path}: {:?}",
                answer.headers
            );
            assert!(answer.closed, "{method} {path}: the connection stayed open");
        }
        let tags = http(stub.url(), "GET", "/api/tags", b"");
        assert!(!tags.closed, "a model answer keeps the connection open");
    }

    /// A shutdown takes the stub's identity as its nonce; once accepted,
    /// it stays heard.
    #[test]
    fn a_shutdown_needs_the_stub_identity() {
        let stub = EmbedStub::start_with(spec(8, Vectors::Constant, Some("n1")));
        let refused = http(stub.url(), "POST", "/__stub/shutdown", b"n2");
        assert_eq!(refused.status, 403);
        assert!(!stub.wait_shutdown(Duration::from_secs(1)));

        let accepted = http(stub.url(), "POST", "/__stub/shutdown", b"n1");
        assert_eq!(accepted.status, 200);
        assert!(stub.wait_shutdown(Duration::from_secs(1)));
        assert!(
            stub.wait_shutdown(Duration::ZERO),
            "a heard shutdown stays heard"
        );
    }

    /// A stub without an identity refuses every shutdown, the empty
    /// nonce included.
    #[test]
    fn a_shutdown_without_an_identity_is_forbidden() {
        let stub = EmbedStub::start();
        for nonce in [&b"anything"[..], &b""[..]] {
            let answer = http(stub.url(), "POST", "/__stub/shutdown", nonce);
            assert_eq!(answer.status, 403, "nonce {nonce:?}");
        }
        assert!(!stub.wait_shutdown(Duration::from_secs(1)));
    }

    /// What a connection does to the server, step by step.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Fault {
        None,
        Write,
        Flush,
    }

    /// A connection that replays one request and logs each write, flush,
    /// and announcement into a shared event list.
    struct Recorded {
        request: io::Cursor<Vec<u8>>,
        events: Arc<Mutex<Vec<&'static str>>>,
        fault: Fault,
    }

    impl Read for Recorded {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.request.read(buf)
        }
    }

    impl Write for Recorded {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fault == Fault::Write {
                return Err(io::Error::other("injected write failure"));
            }
            self.events.lock().unwrap().push("write");
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fault == Fault::Flush {
                return Err(io::Error::other("injected flush failure"));
            }
            self.events.lock().unwrap().push("flush");
            Ok(())
        }
    }

    /// Serve one accepted shutdown over a recorded connection and return
    /// the events.
    fn serve_shutdown(fault: Fault) -> Vec<&'static str> {
        let events = Arc::new(Mutex::new(Vec::new()));
        let connection = Recorded {
            request: io::Cursor::new(
                b"POST /__stub/shutdown HTTP/1.1\r\ncontent-length: 2\r\n\r\nn1".to_vec(),
            ),
            events: Arc::clone(&events),
            fault,
        };
        let announced = Arc::clone(&events);
        let _ = serve(
            connection,
            &AtomicU8::new(EmbedFailure::None.as_u8()),
            &spec(8, Vectors::Constant, Some("n1")),
            &|| announced.lock().unwrap().push("announce"),
        );
        events.lock().unwrap().clone()
    }

    /// An accepted shutdown is announced only after its answer was
    /// written and flushed.
    #[test]
    fn a_shutdown_answer_is_flushed_before_the_holder_hears() {
        assert_eq!(serve_shutdown(Fault::None), ["write", "flush", "announce"]);
    }

    /// A shutdown whose answer could not be written, or not flushed, is
    /// not announced.
    #[test]
    fn a_shutdown_that_cannot_be_answered_is_not_announced() {
        assert!(serve_shutdown(Fault::Write).is_empty());
        assert_eq!(serve_shutdown(Fault::Flush), ["write"]);
    }
}
