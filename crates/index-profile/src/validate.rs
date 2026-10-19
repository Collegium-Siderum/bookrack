// SPDX-License-Identifier: Apache-2.0

//! Static validation of a profile's knob combination. The rules mirror
//! the runtime constraints in `bookrack_vectors` (product-quantization
//! coarseness, the HNSW recall regression) and the reranker contract,
//! lifted to a pre-flight check so a bad combination is caught before an
//! index build rather than after.
//!
//! What the model registry has to say is split in two. A fact it knows —
//! the dimension a listed model emits — is a constraint like any other.
//! Its silence — a model it does not list, a pairing it has not seen —
//! is [`Advice`]: a remark for the operator, never a reason to refuse.

use crate::{AnnKind, IndexProfile, RerankerKind, models};

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

    #[allow(dead_code)] // No validator emits a note today; the severity tier stays.
    fn note(field_path: &str, message: impl Into<String>) -> Finding {
        Finding {
            severity: Severity::Note,
            field_path: field_path.to_string(),
            message: message.into(),
        }
    }
}

/// Run every static check against `profile`: the constraints it must
/// meet to be used. A model outside the registry is not one of them;
/// [`advise`] reports it.
pub fn validate(profile: &IndexProfile) -> Vec<Finding> {
    let mut findings = Vec::new();
    validate_embed(profile, &mut findings);
    validate_ann(profile, &mut findings);
    validate_reranker(profile, &mut findings);
    findings
}

/// A registry-derived remark on a profile: never a reason to refuse it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Advice {
    /// The embed model is not in the registry, so its dimension is the
    /// profile's word alone.
    UnlistedEmbedModel {
        /// The model tag.
        model: String,
        /// The declared dimension.
        dim: u32,
    },
    /// The cross-encoder model is not in the registry, so no pinned
    /// download provides it.
    UnlistedReranker {
        /// The model tag.
        model: String,
    },
    /// Both models are listed, in different families; the pairing is
    /// unverified.
    CrossFamilyPair {
        /// The embed model's family.
        embed_family: &'static str,
        /// The reranker model's family.
        reranker_family: &'static str,
    },
}

impl Advice {
    /// Stable identity of the remark, carrying the data that would make
    /// it a different remark, for recording that it was shown.
    pub fn key(&self) -> String {
        match self {
            Advice::UnlistedEmbedModel { model, dim } => {
                format!("unlisted-embed-model/{model}/{dim}")
            }
            Advice::UnlistedReranker { model } => format!("unlisted-reranker/{model}"),
            Advice::CrossFamilyPair {
                embed_family,
                reranker_family,
            } => format!("cross-family/{embed_family}/{reranker_family}"),
        }
    }
}

/// The registry's remarks on `profile`, in a stable order: embed first,
/// then the reranker. A model name that fails [`validate`] draws none.
pub fn advise(profile: &IndexProfile) -> Vec<Advice> {
    let mut advice = Vec::new();
    let embed = &profile.embed;
    let embed_family = match models::embed_model(&embed.model) {
        Some(info) => Some(info.family),
        None => {
            if !embed.model.trim().is_empty() {
                advice.push(Advice::UnlistedEmbedModel {
                    model: embed.model.clone(),
                    dim: embed.dim,
                });
            }
            None
        }
    };
    let reranker = &profile.reranker;
    if reranker.kind == RerankerKind::CrossEncoder
        && let Some(model) = reranker.model.as_deref()
        && !model.trim().is_empty()
    {
        advice.extend(reranker_advice(embed_family, model));
    }
    advice
}

/// The remark on a cross-encoder `model` beside an embed model of
/// `embed_family` (`None` when the embed model is not listed). The
/// official pairing table is unknown, so a cross-family pair is only a
/// remark.
fn reranker_advice(embed_family: Option<&'static str>, model: &str) -> Option<Advice> {
    match models::reranker_model(model) {
        Some(info) => embed_family
            .filter(|family| *family != info.family)
            .map(|embed_family| Advice::CrossFamilyPair {
                embed_family,
                reranker_family: info.family,
            }),
        None => Some(Advice::UnlistedReranker {
            model: model.to_string(),
        }),
    }
}

/// True when `findings` carries at least one [`Severity::Error`].
pub fn has_errors(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.severity == Severity::Error)
}

fn validate_embed(profile: &IndexProfile, findings: &mut Vec<Finding>) {
    let embed = &profile.embed;
    if embed.backend != "ollama" {
        findings.push(Finding::error(
            "embed.backend",
            format!(
                "unsupported embed backend '{}'; only ollama is implemented",
                embed.backend
            ),
        ));
    }
    if embed.model.trim().is_empty() {
        findings.push(Finding::error("embed.model", "model must not be empty"));
        return;
    }
    if embed.dim == 0 {
        findings.push(Finding::error("embed.dim", "dim must be greater than 0"));
        return;
    }
    if let Some(info) = models::embed_model(&embed.model)
        && info.dim != embed.dim
    {
        findings.push(Finding::error(
            "embed.dim",
            format!(
                "model '{}' emits dimension {}; this build requests no other dimension, so \
                 the profile must declare {}",
                embed.model, info.dim, info.dim
            ),
        ));
    }
}

fn validate_ann(profile: &IndexProfile, findings: &mut Vec<Finding>) {
    let ann = &profile.ann;
    let dim = profile.embed.dim;

    // A scan builds nothing, so none of the build parameters apply.
    if ann.kind == AnnKind::BruteForce {
        return;
    }
    if ann.num_partitions == 0 {
        findings.push(Finding::error(
            "ann.num_partitions",
            "num_partitions must be greater than 0",
        ));
    }
    if ann.nprobes == 0 {
        findings.push(Finding::error(
            "ann.nprobes",
            "nprobes must be greater than 0",
        ));
    }
    if ann.refine_factor == Some(0) {
        findings.push(Finding::error(
            "ann.refine_factor",
            "refine_factor must be greater than 0 when set",
        ));
    }

    if ann.kind.is_pq() {
        if let Some(bits) = ann.num_bits
            && bits != 4
            && bits != 8
        {
            findings.push(Finding::error(
                "ann.num_bits",
                format!("num_bits must be 4 or 8, got {bits}"),
            ));
        }
        if ann.num_bits == Some(4)
            && let Some(nsv) = ann.num_sub_vectors
            && !nsv.is_multiple_of(2)
        {
            findings.push(Finding::error(
                "ann.num_sub_vectors",
                format!("num_sub_vectors must be even with 4-bit codes, got {nsv}"),
            ));
        }
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

fn validate_reranker(profile: &IndexProfile, findings: &mut Vec<Finding>) {
    let reranker = &profile.reranker;
    if reranker.kind == RerankerKind::None {
        return;
    }

    if reranker.kind == RerankerKind::CrossEncoder {
        match reranker.backend.as_deref() {
            None => findings.push(Finding::error(
                "reranker.backend",
                "a cross-encoder reranker requires a backend",
            )),
            Some("llama-server") => {}
            Some("ollama") => findings.push(Finding::error(
                "reranker.backend",
                "ollama does not serve rerankers; choose another backend",
            )),
            Some(other) => findings.push(Finding::error(
                "reranker.backend",
                format!("unsupported reranker backend '{other}'; only llama-server is implemented"),
            )),
        }

        match reranker.model.as_deref() {
            None => findings.push(Finding::error(
                "reranker.model",
                "a cross-encoder reranker requires a model",
            )),
            Some(model) if model.trim().is_empty() => findings.push(Finding::error(
                "reranker.model",
                "a cross-encoder reranker model must not be empty",
            )),
            Some(_) => {}
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

#[cfg(test)]
mod tests {
    use crate::{AnnKind, AnnSpec, EmbedSpec, IndexProfile, RerankerKind, RerankerSpec};

    use super::{Advice, Severity, advise, has_errors, validate};

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
        let findings = validate(&pq_profile(1024, None));
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
        let findings = validate(&pq_profile(1024, Some(64)));
        assert!(has_errors(&findings));
    }

    #[test]
    fn pq_indivisible_is_an_error() {
        // 1024 % 100 != 0.
        let findings = validate(&pq_profile(1024, Some(100)));
        assert!(has_errors(&findings));
    }

    #[test]
    fn pq_within_bound_is_clean() {
        // 1024 / 128 = 8 <= 8, divisible.
        let findings = validate(&pq_profile(1024, Some(128)));
        assert!(!has_errors(&findings), "{findings:?}");
    }

    #[test]
    fn an_unlisted_model_is_advice_not_a_finding() {
        let mut profile = pq_profile(1024, Some(128));
        profile.embed.model = "made-up:1b".to_string();
        assert!(validate(&profile).is_empty(), "{:?}", validate(&profile));
        assert_eq!(
            advise(&profile),
            vec![Advice::UnlistedEmbedModel {
                model: "made-up:1b".to_string(),
                dim: 1024,
            }]
        );
    }

    #[test]
    fn dim_mismatch_is_an_error() {
        // Registry says qwen3-embedding:0.6b is 1024, not 512.
        let findings = validate(&pq_profile(512, Some(64)));
        let finding = findings
            .iter()
            .find(|f| f.field_path == "embed.dim")
            .unwrap_or_else(|| panic!("{findings:?}"));
        assert_eq!(finding.severity, Severity::Error);
        assert!(!finding.message.contains("--"), "{finding:?}");
    }

    /// The `ann.*` error fields `validate` reports for `profile`.
    fn ann_error_fields(profile: &IndexProfile) -> Vec<String> {
        validate(profile)
            .into_iter()
            .filter(|f| f.severity == Severity::Error && f.field_path.starts_with("ann."))
            .map(|f| f.field_path)
            .collect()
    }

    /// An IvfPq profile over an unlisted 24-wide model: 24 divides by
    /// the odd 3 without passing the coarseness bound, which a
    /// registry dimension cannot offer.
    fn narrow_pq_profile(nsv: u32, num_bits: Option<u32>) -> IndexProfile {
        let mut profile = pq_profile(24, Some(nsv));
        profile.embed.model = "synthetic-embed:narrow".to_string();
        profile.ann.num_bits = num_bits;
        profile
    }

    #[test]
    fn partition_and_probe_counts_must_be_positive() {
        let mut profile = pq_profile(1024, Some(128));
        assert!(ann_error_fields(&profile).is_empty());
        profile.ann.num_partitions = 0;
        assert_eq!(ann_error_fields(&profile), ["ann.num_partitions"]);

        let mut profile = pq_profile(1024, Some(128));
        profile.ann.nprobes = 0;
        assert_eq!(ann_error_fields(&profile), ["ann.nprobes"]);
    }

    #[test]
    fn a_refine_factor_when_set_must_be_positive() {
        let mut profile = pq_profile(1024, Some(128));
        for ok in [None, Some(1)] {
            profile.ann.refine_factor = ok;
            assert!(ann_error_fields(&profile).is_empty(), "{ok:?}");
        }
        profile.ann.refine_factor = Some(0);
        assert_eq!(ann_error_fields(&profile), ["ann.refine_factor"]);
    }

    #[test]
    fn a_pq_code_width_must_be_four_or_eight_bits() {
        for ok in [None, Some(4), Some(8)] {
            let profile = narrow_pq_profile(4, ok);
            assert!(ann_error_fields(&profile).is_empty(), "{ok:?}");
        }
        for bad in [2, 16] {
            let profile = narrow_pq_profile(4, Some(bad));
            assert_eq!(ann_error_fields(&profile), ["ann.num_bits"], "{bad} bits");
        }
    }

    #[test]
    fn four_bit_codes_need_an_even_sub_vector_count() {
        assert!(ann_error_fields(&narrow_pq_profile(3, Some(8))).is_empty());
        assert_eq!(
            ann_error_fields(&narrow_pq_profile(3, Some(4))),
            ["ann.num_sub_vectors"]
        );
    }

    #[test]
    fn brute_force_skips_every_build_parameter() {
        let mut profile = pq_profile(1024, None);
        profile.ann = AnnSpec {
            kind: AnnKind::BruteForce,
            num_partitions: 0,
            num_sub_vectors: None,
            num_bits: Some(16),
            nprobes: 0,
            refine_factor: Some(0),
        };
        assert!(ann_error_fields(&profile).is_empty());
    }

    #[test]
    fn hnsw_is_a_warning_not_an_error() {
        let mut profile = pq_profile(1024, Some(128));
        profile.ann.kind = AnnKind::IvfHnswSq;
        profile.ann.num_sub_vectors = None;
        let findings = validate(&profile);
        assert!(!has_errors(&findings));
        assert!(findings.iter().any(|f| f.severity == Severity::Warning));
    }

    #[test]
    fn cross_encoder_backend_must_be_llama_server() {
        let reranker = |backend: &str| RerankerSpec {
            kind: RerankerKind::CrossEncoder,
            backend: Some(backend.to_string()),
            model: Some("Qwen3-Reranker-0.6B".to_string()),
            top_k_in: Some(50),
            top_k_out: Some(10),
        };

        let mut profile = pq_profile(1024, Some(128));
        profile.reranker = reranker("llama-server");
        assert!(!has_errors(&validate(&profile)));

        // ollama keeps its specific message: it has no rerank endpoint.
        profile.reranker = reranker("ollama");
        let findings = validate(&profile);
        assert!(
            findings
                .iter()
                .any(|f| f.field_path == "reranker.backend" && f.message.contains("ollama"))
        );

        // Any other backend names a stage the runtime cannot execute.
        profile.reranker = reranker("candle");
        let findings = validate(&profile);
        assert!(
            findings
                .iter()
                .any(|f| f.field_path == "reranker.backend" && f.message.contains("llama-server"))
        );
    }

    #[test]
    fn cross_encoder_bad_top_k_is_an_error() {
        let mut profile = pq_profile(1024, Some(128));
        profile.reranker = RerankerSpec {
            kind: RerankerKind::CrossEncoder,
            backend: Some("llama-server".to_string()),
            model: Some("Qwen3-Reranker-0.6B".to_string()),
            top_k_in: Some(5),
            top_k_out: Some(10),
        };
        let findings = validate(&profile);
        assert!(findings.iter().any(|f| f.field_path == "reranker.top_k_in"));
    }

    /// A well-formed cross-encoder reranker section, for the tests that
    /// knock out one field at a time.
    fn cross_encoder() -> RerankerSpec {
        RerankerSpec {
            kind: RerankerKind::CrossEncoder,
            backend: Some("llama-server".to_string()),
            model: Some("Qwen3-Reranker-0.6B".to_string()),
            top_k_in: Some(50),
            top_k_out: Some(10),
        }
    }

    #[test]
    fn pq_zero_sub_vectors_is_an_error() {
        // Zero gets its own arm: it must be reported as such, not fall
        // through to the divisibility check.
        let findings = validate(&pq_profile(1024, Some(0)));
        assert!(
            findings.iter().any(|f| {
                f.severity == Severity::Error
                    && f.field_path == "ann.num_sub_vectors"
                    && f.message.contains("greater than 0")
            }),
            "{findings:?}",
        );
    }

    #[test]
    fn cross_encoder_missing_backend_is_an_error() {
        let mut profile = pq_profile(1024, Some(128));
        profile.reranker = cross_encoder();
        profile.reranker.backend = None;
        let findings = validate(&profile);
        assert!(
            findings.iter().any(|f| {
                f.severity == Severity::Error
                    && f.field_path == "reranker.backend"
                    && f.message.contains("requires a backend")
            }),
            "{findings:?}",
        );
    }

    #[test]
    fn cross_encoder_missing_model_is_an_error() {
        let mut profile = pq_profile(1024, Some(128));
        profile.reranker = cross_encoder();
        profile.reranker.model = None;
        let findings = validate(&profile);
        assert!(
            findings.iter().any(|f| {
                f.severity == Severity::Error
                    && f.field_path == "reranker.model"
                    && f.message.contains("requires a model")
            }),
            "{findings:?}",
        );
    }

    #[test]
    fn cross_encoder_missing_top_k_half_is_an_error() {
        for (top_k_in, top_k_out) in [(None, Some(10)), (Some(50), None), (None, None)] {
            let mut profile = pq_profile(1024, Some(128));
            profile.reranker = cross_encoder();
            profile.reranker.top_k_in = top_k_in;
            profile.reranker.top_k_out = top_k_out;
            let findings = validate(&profile);
            assert!(
                findings.iter().any(|f| {
                    f.severity == Severity::Error
                        && f.field_path == "reranker.top_k_in"
                        && f.message.contains("requires top_k_in and top_k_out")
                }),
                "({top_k_in:?}, {top_k_out:?}): {findings:?}",
            );
        }
    }

    #[test]
    fn cross_encoder_zero_top_k_out_is_an_error() {
        let mut profile = pq_profile(1024, Some(128));
        profile.reranker = cross_encoder();
        profile.reranker.top_k_out = Some(0);
        let findings = validate(&profile);
        assert!(
            findings.iter().any(|f| {
                f.severity == Severity::Error
                    && f.field_path == "reranker.top_k_out"
                    && f.message.contains("greater than 0")
            }),
            "{findings:?}",
        );
    }

    #[test]
    fn an_unlisted_reranker_is_advice_not_a_finding() {
        let mut profile = pq_profile(1024, Some(128));
        profile.reranker = cross_encoder();
        profile.reranker.model = Some("made-up-reranker".to_string());
        assert!(!has_errors(&validate(&profile)), "{:?}", validate(&profile));
        assert_eq!(
            advise(&profile),
            vec![Advice::UnlistedReranker {
                model: "made-up-reranker".to_string(),
            }]
        );
    }

    #[test]
    fn cross_family_reranker_pair_is_advice() {
        // Exercised on the reranker helper directly: both shipped
        // registries are single-family today, so no profile reaches
        // this arm through `advise` until a second family lands.
        assert_eq!(
            super::reranker_advice(Some("other-family"), "Qwen3-Reranker-0.6B"),
            Some(Advice::CrossFamilyPair {
                embed_family: "other-family",
                reranker_family: "qwen3",
            })
        );
        // Same family, and an embed model outside the registry, both
        // stay silent.
        assert_eq!(
            super::reranker_advice(Some("qwen3"), "Qwen3-Reranker-0.6B"),
            None
        );
        assert_eq!(super::reranker_advice(None, "Qwen3-Reranker-0.6B"), None);
    }

    #[test]
    fn the_shipped_combination_draws_no_advice() {
        let mut profile = pq_profile(1024, Some(128));
        profile.reranker = cross_encoder();
        assert!(advise(&profile).is_empty(), "{:?}", advise(&profile));
    }

    #[test]
    fn advice_keys_are_distinct_and_carry_their_data() {
        let narrow = Advice::UnlistedEmbedModel {
            model: "m".to_string(),
            dim: 8,
        };
        let wide = Advice::UnlistedEmbedModel {
            model: "m".to_string(),
            dim: 16,
        };
        let reranker = Advice::UnlistedReranker {
            model: "m".to_string(),
        };
        let pair = Advice::CrossFamilyPair {
            embed_family: "a",
            reranker_family: "b",
        };
        let keys = [narrow.key(), wide.key(), reranker.key(), pair.key()];
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                assert_ne!(a, b);
            }
        }
        let mut profile = pq_profile(1024, Some(128));
        profile.embed.model = "made-up:1b".to_string();
        profile.reranker = cross_encoder();
        profile.reranker.model = Some("made-up-reranker".to_string());
        let once: Vec<String> = advise(&profile).iter().map(Advice::key).collect();
        let twice: Vec<String> = advise(&profile).iter().map(Advice::key).collect();
        assert_eq!(once, twice);
        assert_eq!(once.len(), 2, "{once:?}");
    }

    #[test]
    fn no_finding_names_a_cli_flag() {
        let mut profiles = vec![
            pq_profile(512, Some(64)),
            pq_profile(1024, None),
            pq_profile(1024, Some(100)),
            pq_profile(1024, Some(0)),
            unlisted(0),
        ];
        let mut backend = unlisted(8);
        backend.embed.backend = "synthetic-backend".to_string();
        profiles.push(backend);
        let mut reranker = pq_profile(1024, Some(128));
        reranker.reranker = RerankerSpec {
            backend: None,
            model: None,
            top_k_in: None,
            ..cross_encoder()
        };
        profiles.push(reranker);
        for profile in &profiles {
            let findings = validate(profile);
            assert!(!findings.is_empty(), "the fixture must violate something");
            for f in findings {
                assert!(!f.message.contains("--"), "{f:?}");
            }
        }
    }

    /// A profile outside the registry, so only the structural checks
    /// speak.
    fn unlisted(dim: u32) -> IndexProfile {
        let mut profile = pq_profile(dim, Some(dim.max(8) / 8));
        profile.embed.model = "synthetic-embed:unlisted".to_string();
        profile.ann.kind = AnnKind::BruteForce;
        profile.ann.num_sub_vectors = None;
        profile
    }

    fn errors_on(findings: &[super::Finding], field: &str) -> usize {
        findings
            .iter()
            .filter(|f| f.severity == Severity::Error && f.field_path == field)
            .count()
    }

    #[test]
    fn zero_dim_is_an_error_for_an_unlisted_model() {
        let findings = validate(&unlisted(0));
        assert_eq!(errors_on(&findings, "embed.dim"), 1, "{findings:?}");
    }

    #[test]
    fn an_embed_backend_other_than_ollama_is_an_error() {
        let mut profile = unlisted(8);
        profile.embed.backend = "synthetic-backend".to_string();
        let findings = validate(&profile);
        assert_eq!(errors_on(&findings, "embed.backend"), 1, "{findings:?}");
    }

    #[test]
    fn an_empty_model_name_is_an_error() {
        for blank in ["", "  "] {
            let mut profile = unlisted(8);
            profile.embed.model = blank.to_string();
            let findings = validate(&profile);
            assert_eq!(
                errors_on(&findings, "embed.model"),
                1,
                "{blank:?}: {findings:?}"
            );

            let mut profile = unlisted(8);
            profile.reranker = RerankerSpec {
                model: Some(blank.to_string()),
                ..cross_encoder()
            };
            let findings = validate(&profile);
            assert_eq!(
                errors_on(&findings, "reranker.model"),
                1,
                "{blank:?}: {findings:?}"
            );
        }
    }
}
