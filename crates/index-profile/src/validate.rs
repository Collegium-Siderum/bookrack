// SPDX-License-Identifier: Apache-2.0

//! Static validation of a profile's knob combination. The rules mirror
//! the runtime constraints in `bookrack_vectors` (product-quantization
//! coarseness, the HNSW recall regression) and the reranker contract,
//! lifted to a pre-flight check so a bad combination is caught before an
//! index build rather than after.

use crate::{IndexProfile, RerankerKind, models};

/// How serious one validation finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A hard constraint violation; the profile cannot be used as-is.
    Error,
    /// A discouraged but permitted choice.
    Warning,
    /// An informational remark, e.g. a stage that is planned but not yet
    /// implemented.
    Note,
}

impl Severity {
    /// The label rendered for this severity.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

/// One outcome of validating a profile: a severity, the field path it
/// concerns, and a human-readable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// How serious the finding is.
    pub severity: Severity,
    /// Dotted path of the field the finding concerns (e.g. `ann.num_sub_vectors`).
    pub field_path: String,
    /// The explanation.
    pub message: String,
}

impl Finding {
    fn error(field_path: &str, message: impl Into<String>) -> Finding {
        Finding {
            severity: Severity::Error,
            field_path: field_path.to_string(),
            message: message.into(),
        }
    }

    fn warning(field_path: &str, message: impl Into<String>) -> Finding {
        Finding {
            severity: Severity::Warning,
            field_path: field_path.to_string(),
            message: message.into(),
        }
    }

    fn note(field_path: &str, message: impl Into<String>) -> Finding {
        Finding {
            severity: Severity::Note,
            field_path: field_path.to_string(),
            message: message.into(),
        }
    }
}

/// Run every static check against `profile`. With `allow_unknown_model`,
/// the two model-registry checks — "the embed model is known" and "the
/// declared dimension matches the registry" — are skipped, so an
/// out-of-tree model can still exercise the structural constraints.
pub fn validate(profile: &IndexProfile, allow_unknown_model: bool) -> Vec<Finding> {
    let mut findings = Vec::new();
    validate_embed(profile, allow_unknown_model, &mut findings);
    validate_ann(profile, &mut findings);
    validate_reranker(profile, allow_unknown_model, &mut findings);
    findings
}

/// True when `findings` carries at least one [`Severity::Error`].
pub fn has_errors(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.severity == Severity::Error)
}

fn validate_embed(profile: &IndexProfile, allow_unknown_model: bool, findings: &mut Vec<Finding>) {
    if allow_unknown_model {
        return;
    }
    match models::embed_model(&profile.embed.model) {
        Some(info) => {
            if info.dim != profile.embed.dim {
                findings.push(Finding::error(
                    "embed.dim",
                    format!(
                        "model '{}' emits dimension {}, but the profile declares {}",
                        profile.embed.model, info.dim, profile.embed.dim
                    ),
                ));
            }
        }
        None => findings.push(Finding::error(
            "embed.model",
            format!(
                "unknown embed model '{}'; pass --allow-unknown-model to skip the registry check",
                profile.embed.model
            ),
        )),
    }
}

fn validate_ann(profile: &IndexProfile, findings: &mut Vec<Finding>) {
    let ann = &profile.ann;
    let dim = profile.embed.dim;

    if ann.kind.is_pq() {
        match ann.num_sub_vectors {
            None => findings.push(Finding::error(
                "ann.num_sub_vectors",
                format!("{} requires num_sub_vectors", ann.kind.as_str()),
            )),
            Some(0) => findings.push(Finding::error(
                "ann.num_sub_vectors",
                "num_sub_vectors must be greater than 0",
            )),
            Some(nsv) => {
                if !dim.is_multiple_of(nsv) {
                    findings.push(Finding::error(
                        "ann.num_sub_vectors",
                        format!("dim {dim} is not divisible by num_sub_vectors {nsv}"),
                    ));
                } else if dim / nsv > 8 {
                    findings.push(Finding::error(
                        "ann.num_sub_vectors",
                        format!(
                            "quantization too coarse: dim {dim} / num_sub_vectors {nsv} = {} > 8 \
                             (use num_sub_vectors >= {})",
                            dim / nsv,
                            dim.div_ceil(8),
                        ),
                    ));
                }
            }
        }
    }

    if ann.kind.is_hnsw() {
        findings.push(Finding::warning(
            "ann.kind",
            format!(
                "{} is unstable on the pinned LanceDB (upstream recall regression); \
                 prefer an ivf-* index unless you have re-verified it",
                ann.kind.as_str()
            ),
        ));
    }
}

fn validate_reranker(
    profile: &IndexProfile,
    allow_unknown_model: bool,
    findings: &mut Vec<Finding>,
) {
    let reranker = &profile.reranker;
    if reranker.kind == RerankerKind::None {
        return;
    }

    // The reranker stage is schema-first: the fields validate, but the
    // stage is not wired yet. A live-effect error is raised elsewhere
    // (library startup, apply); here it is only a note.
    findings.push(Finding::note(
        "reranker.kind",
        "reranker stage not implemented yet (planned)",
    ));

    if reranker.kind == RerankerKind::CrossEncoder {
        match reranker.backend.as_deref() {
            None => findings.push(Finding::error(
                "reranker.backend",
                "a cross-encoder reranker requires a backend",
            )),
            Some("ollama") => findings.push(Finding::error(
                "reranker.backend",
                "ollama does not serve rerankers; choose another backend",
            )),
            Some(_) => {}
        }

        match reranker.model.as_deref() {
            None => findings.push(Finding::error(
                "reranker.model",
                "a cross-encoder reranker requires a model",
            )),
            Some(model) => validate_reranker_family(profile, model, allow_unknown_model, findings),
        }

        match (reranker.top_k_in, reranker.top_k_out) {
            (Some(top_k_in), Some(top_k_out)) => {
                if top_k_out == 0 {
                    findings.push(Finding::error(
                        "reranker.top_k_out",
                        "top_k_out must be greater than 0",
                    ));
                } else if top_k_in < top_k_out {
                    findings.push(Finding::error(
                        "reranker.top_k_in",
                        format!("top_k_in {top_k_in} must be >= top_k_out {top_k_out}"),
                    ));
                }
            }
            _ => findings.push(Finding::error(
                "reranker.top_k_in",
                "a cross-encoder reranker requires top_k_in and top_k_out",
            )),
        }
    }
}

/// Flag a reranker whose family differs from the embed family. The
/// official pairing table is unknown, so a cross-family pair is only a
/// warning. An unknown reranker model is an error under the registry
/// check unless the caller opted out.
fn validate_reranker_family(
    profile: &IndexProfile,
    model: &str,
    allow_unknown_model: bool,
    findings: &mut Vec<Finding>,
) {
    match models::reranker_model(model) {
        Some(info) => {
            let embed_family = models::embed_model(&profile.embed.model).map(|m| m.family);
            if let Some(embed_family) = embed_family
                && embed_family != info.family
            {
                findings.push(Finding::warning(
                    "reranker.model",
                    format!(
                        "reranker family '{}' differs from embed family '{embed_family}'; \
                         cross-family pairs are unverified",
                        info.family
                    ),
                ));
            }
        }
        None if !allow_unknown_model => findings.push(Finding::error(
            "reranker.model",
            format!(
                "unknown reranker model '{model}'; pass --allow-unknown-model to skip the \
                 registry check"
            ),
        )),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use crate::{AnnKind, AnnSpec, EmbedSpec, IndexProfile, RerankerKind, RerankerSpec};

    use super::{Severity, has_errors, validate};

    fn pq_profile(dim: u32, nsv: Option<u32>) -> IndexProfile {
        IndexProfile {
            name: "t".to_string(),
            description: String::new(),
            embed: EmbedSpec {
                backend: "ollama".to_string(),
                model: "qwen3-embedding:0.6b".to_string(),
                dim,
            },
            ann: AnnSpec {
                kind: AnnKind::IvfPq,
                num_partitions: 16,
                num_sub_vectors: nsv,
                num_bits: Some(8),
                nprobes: 8,
                refine_factor: None,
            },
            reranker: RerankerSpec::default(),
        }
    }

    #[test]
    fn pq_missing_sub_vectors_is_an_error() {
        let findings = validate(&pq_profile(1024, None), false);
        assert!(has_errors(&findings));
        assert!(
            findings
                .iter()
                .any(|f| f.field_path == "ann.num_sub_vectors")
        );
    }

    #[test]
    fn pq_too_coarse_is_an_error() {
        // 1024 / 64 = 16 > 8.
        let findings = validate(&pq_profile(1024, Some(64)), false);
        assert!(has_errors(&findings));
    }

    #[test]
    fn pq_indivisible_is_an_error() {
        // 1024 % 100 != 0.
        let findings = validate(&pq_profile(1024, Some(100)), false);
        assert!(has_errors(&findings));
    }

    #[test]
    fn pq_within_bound_is_clean() {
        // 1024 / 128 = 8 <= 8, divisible.
        let findings = validate(&pq_profile(1024, Some(128)), false);
        assert!(!has_errors(&findings), "{findings:?}");
    }

    #[test]
    fn unknown_model_errors_unless_allowed() {
        let mut profile = pq_profile(1024, Some(128));
        profile.embed.model = "made-up:1b".to_string();
        assert!(has_errors(&validate(&profile, false)));
        assert!(!has_errors(&validate(&profile, true)));
    }

    #[test]
    fn dim_mismatch_is_an_error() {
        // Registry says qwen3-embedding:0.6b is 1024, not 512.
        let findings = validate(&pq_profile(512, Some(64)), false);
        assert!(findings.iter().any(|f| f.field_path == "embed.dim"));
    }

    #[test]
    fn hnsw_is_a_warning_not_an_error() {
        let mut profile = pq_profile(1024, Some(128));
        profile.ann.kind = AnnKind::IvfHnswSq;
        profile.ann.num_sub_vectors = None;
        let findings = validate(&profile, false);
        assert!(!has_errors(&findings));
        assert!(findings.iter().any(|f| f.severity == Severity::Warning));
    }

    #[test]
    fn cross_encoder_on_ollama_is_an_error_and_carries_a_note() {
        let mut profile = pq_profile(1024, Some(128));
        profile.reranker = RerankerSpec {
            kind: RerankerKind::CrossEncoder,
            backend: Some("ollama".to_string()),
            model: Some("Qwen3-Reranker-0.6B".to_string()),
            top_k_in: Some(50),
            top_k_out: Some(10),
        };
        let findings = validate(&profile, false);
        assert!(has_errors(&findings));
        assert!(findings.iter().any(|f| f.severity == Severity::Note));
    }

    #[test]
    fn cross_encoder_bad_top_k_is_an_error() {
        let mut profile = pq_profile(1024, Some(128));
        profile.reranker = RerankerSpec {
            kind: RerankerKind::CrossEncoder,
            backend: Some("candle".to_string()),
            model: Some("Qwen3-Reranker-0.6B".to_string()),
            top_k_in: Some(5),
            top_k_out: Some(10),
        };
        let findings = validate(&profile, false);
        assert!(findings.iter().any(|f| f.field_path == "reranker.top_k_in"));
    }
}
