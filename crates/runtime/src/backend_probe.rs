// SPDX-License-Identifier: Apache-2.0

//! Is the embed backend usable, as a typed answer.
//!
//! One judgement, two consumers. `doctor` lays it out as table rows;
//! bring-up refuses to start on it. Keeping the judgement free of
//! wording is what lets both hold their own layout — a table cell and
//! a full sentence cannot be the same string — while still agreeing
//! on the facts and on the command that repairs them.
//!
//! The check is necessary, not sufficient: it reads the daemon's model
//! list from `/api/tags`, whereas embedding goes through `/api/embed`.
//! A model that is listed but broken (corrupt weights, a runner that
//! dies on load) still fails later, which is why
//! [`bookrack_embed::EmbedError`] keeps its own wording.

use bookrack_core::{Explain, Problem};
use bookrack_embed::{DEFAULT_PROBE_TIMEOUT, ProbeReport, probe_ollama, pull_command};

/// What a probe of the embed backend found.
///
/// Carries no `base_url` or `model`: every caller already holds both,
/// and duplicating them into the state invites the two copies to
/// disagree. `Unreachable` carries no elapsed time either — the probe
/// measures none, and quoting a real elapsed time would make the
/// diagnostic wording drift run to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbedBackendState {
    /// The daemon answered and holds the configured model.
    Ready {
        /// Every model tag the daemon reported, so a caller can say
        /// how many are pulled without probing again.
        models: Vec<String>,
    },
    /// The daemon answered but does not hold the configured model.
    ModelMissing {
        /// The model that is missing.
        model: String,
        /// What the daemon does hold, for a diagnostic that can say
        /// "these, but not that one".
        available: Vec<String>,
    },
    /// Nothing answered within the probe timeout.
    Unreachable,
    /// Something answered, but not Ollama — or the HTTP client could
    /// not be built at all.
    ProbeFailed {
        /// The probe's own error text.
        reason: String,
    },
}

/// Probe `base_url` and judge it against `model`.
pub async fn check_embed_backend(base_url: &str, model: &str) -> EmbedBackendState {
    match probe_ollama(base_url).await {
        Ok(report) if report.reachable => classify_reachable(report, model),
        Ok(_) => EmbedBackendState::Unreachable,
        Err(e) => EmbedBackendState::ProbeFailed {
            reason: e.to_string(),
        },
    }
}

fn classify_reachable(report: ProbeReport, model: &str) -> EmbedBackendState {
    if report.models.iter().any(|m| m == model) {
        EmbedBackendState::Ready {
            models: report.models,
        }
    } else {
        EmbedBackendState::ModelMissing {
            model: model.to_string(),
            available: report.models,
        }
    }
}

impl EmbedBackendState {
    /// Whether a library can be opened against this backend.
    pub fn is_usable(&self) -> bool {
        matches!(self, EmbedBackendState::Ready { .. })
    }
}

/// Bring-up's refusal to start against an unusable embed backend.
///
/// Carries the rendered [`Problem`] rather than a flat string so the
/// CLI can hand it to the same three-part reporter every other
/// classified failure goes through. `Display` stays a self-sufficient
/// single line for the log and for the desktop shell, which has no
/// exit code to set.
#[derive(Debug, thiserror::Error)]
#[error("library '{library}': {}", .problem.summary)]
pub struct PreflightRefusal {
    /// The mounted library whose backend failed the check.
    pub library: String,
    /// The three-part diagnostic for the failure.
    pub problem: Problem,
}

/// Check every distinct `(base_url, model)` pair the mount plan will
/// use, and refuse on the first unusable one.
///
/// Per mount, not once for the primary library: each mount resolves
/// its own effective embed configuration, so a registry with several
/// entries can legitimately point at different models or different
/// Ollama hosts. Checking only the primary would pass and then let
/// bring-up fail on library six anyway.
pub async fn preflight_embed_backends(
    mounts: &[(String, std::sync::Arc<bookrack_config::Config>)],
) -> Result<(), PreflightRefusal> {
    let mut seen: Vec<(String, String)> = Vec::new();
    for (name, cfg) in mounts {
        let embed = match crate::profile::effective_embed_config(cfg) {
            Ok(embed) => embed,
            // Resolution failures are not this check's business; the
            // step that owns them reports them with its own wording.
            Err(_) => continue,
        };
        let pair = (cfg.ollama_url().to_string(), embed.model.clone());
        if seen.contains(&pair) {
            continue;
        }
        seen.push(pair.clone());
        let state = check_embed_backend(&pair.0, &pair.1).await;
        if !state.is_usable() {
            return Err(PreflightRefusal {
                library: name.clone(),
                problem: state.explain(),
            });
        }
    }
    Ok(())
}

impl Explain for EmbedBackendState {
    fn explain(&self) -> Problem {
        match self {
            EmbedBackendState::Ready { models } => Problem::new("the embed backend is ready")
                .detail(format!(
                    "The Ollama daemon holds {} model(s).",
                    models.len()
                )),

            // Present tense, and the same repair fragment `doctor`
            // puts in its table cell.
            EmbedBackendState::ModelMissing { model, available } => Problem::new(format!(
                "cannot start: the model {model:?} is not available on the Ollama daemon"
            ))
            .detail(if available.is_empty() {
                "The daemon holds no models at all.".to_string()
            } else {
                format!("The daemon holds: {}.", available.join(", "))
            })
            .hint(format!("Pull it first: {}.", pull_command(model))),

            // Past tense: the daemon may be up by the next attempt.
            EmbedBackendState::Unreachable => Problem::new("could not reach Ollama")
                .detail(format!(
                    "Nothing answered within {}s.",
                    DEFAULT_PROBE_TIMEOUT.as_secs()
                ))
                .hint(
                    "Start Ollama, or point BOOKRACK_OLLAMA_URL at the host that runs it. \
                     Run `bookrack doctor` to check.",
                )
                .retryable(true),

            EmbedBackendState::ProbeFailed { reason } => {
                Problem::new("cannot start: the embed endpoint did not answer as Ollama")
                    .detail(format!("{reason}."))
                    .hint(
                        "Check that BOOKRACK_OLLAMA_URL names an Ollama daemon and not \
                         another service on the same port.",
                    )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    /// A one-shot loopback responder. Written here rather than shared:
    /// the embed crate's equivalent lives inside its own `#[cfg(test)]`
    /// module and is not exported, and the workspace's habit is a local
    /// copy over a widened visibility surface.
    fn mock_once(status_line: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            let Ok((mut socket, _)) = listener.accept() else {
                return;
            };
            let mut scratch = [0u8; 8192];
            let _ = socket.read(&mut scratch);
            let response = format!(
                "HTTP/1.1 {status_line}\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes());
            let _ = socket.flush();
        });
        format!("http://{addr}")
    }

    /// An address with no listener — connecting to it is refused.
    fn dead_address() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn empty_model_list_is_reported_as_model_missing() {
        let url = mock_once("200 OK", r#"{"models":[]}"#);
        let state = check_embed_backend(&url, "wanted-model").await;
        assert_eq!(
            state,
            EmbedBackendState::ModelMissing {
                model: "wanted-model".to_string(),
                available: Vec::new(),
            }
        );
        assert!(!state.is_usable());
    }

    #[tokio::test]
    async fn refused_connection_is_reported_as_unreachable() {
        let state = check_embed_backend(&dead_address(), "wanted-model").await;
        assert_eq!(state, EmbedBackendState::Unreachable);
    }

    #[tokio::test]
    async fn a_reachable_daemon_reports_its_model_count() {
        let url = mock_once(
            "200 OK",
            r#"{"models":[{"name":"wanted-model"},{"name":"other"}]}"#,
        );
        let state = check_embed_backend(&url, "wanted-model").await;
        let EmbedBackendState::Ready { models } = &state else {
            panic!("got {state:?}");
        };
        assert_eq!(models.len(), 2, "the Ok row's model count comes from here");
        assert!(state.is_usable());
    }

    #[tokio::test]
    async fn a_non_ollama_responder_is_reported_as_probe_failed() {
        let url = mock_once("200 OK", "<html>not ollama</html>");
        let state = check_embed_backend(&url, "wanted-model").await;
        assert!(
            matches!(state, EmbedBackendState::ProbeFailed { .. }),
            "got {state:?}"
        );
    }

    #[test]
    fn a_missing_model_is_not_retryable_but_an_absent_daemon_is() {
        let missing = EmbedBackendState::ModelMissing {
            model: "m".to_string(),
            available: Vec::new(),
        }
        .explain();
        assert!(!missing.data.retryable);
        assert!(
            missing.summary.starts_with("cannot "),
            "{}",
            missing.summary
        );

        let down = EmbedBackendState::Unreachable.explain();
        assert!(down.data.retryable);
        assert!(down.summary.starts_with("could not "), "{}", down.summary);
    }
}
