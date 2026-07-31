// SPDX-License-Identifier: Apache-2.0

//! A loopback stand-in for the Ollama surface daemon bring-up touches.

use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::OnceLock;

use bookrack_config::DEFAULT_EMBED_MODEL;

static URL: OnceLock<String> = OnceLock::new();

/// A loopback HTTP server answering the two shapes daemon bring-up
/// touches: `POST /api/embed` returns one [`EmbedStub::DIMENSION`]-wide
/// vector per input, and every other path returns a model list holding
/// [`bookrack_config::DEFAULT_EMBED_MODEL`], which the bring-up
/// pre-flight requires before it will start.
///
/// One stub serves a whole test binary; the listener runs on detached
/// threads for the process's lifetime.
pub struct EmbedStub;

impl EmbedStub {
    /// Vector width the stub reports for every input.
    pub const DIMENSION: usize = 8;

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
        let response = respond(&request_line, &body);
        let payload = response.to_string();
        write!(
            out,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
             content-length: {}\r\n\r\n{payload}",
            payload.len(),
        )?;
        out.flush()?;
    }
}

/// Body for one request, factored out so the contract is testable
/// without a socket.
fn respond(request_line: &str, body: &[u8]) -> serde_json::Value {
    if request_line.starts_with("POST /api/embed") {
        let inputs = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v["input"].as_array().map(Vec::len))
            .unwrap_or(0);
        serde_json::json!({
            "embeddings": vec![vec![0.5f32; EmbedStub::DIMENSION]; inputs],
        })
    } else {
        serde_json::json!({ "models": [{ "name": DEFAULT_EMBED_MODEL }] })
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
        let embed = respond("POST /api/embed HTTP/1.1", body);
        let vectors = embed["embeddings"].as_array().expect("embeddings array");
        assert_eq!(vectors.len(), 3);
        for vector in vectors {
            assert_eq!(
                vector.as_array().expect("vector array").len(),
                EmbedStub::DIMENSION,
            );
        }

        let tags = respond("GET /api/tags HTTP/1.1", b"");
        let models = tags["models"].as_array().expect("models array");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["name"].as_str(), Some(DEFAULT_EMBED_MODEL));
    }
}
