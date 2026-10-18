// SPDX-License-Identifier: Apache-2.0

//! Embedding client construction. Wraps `OllamaEmbedClient::new` so
//! every `cmd/*` module that needs an embedder builds it the same way
//! from the same `Config` + `EmbedConfig` pair.
//!
//! Also the dimension check a destructive rebuild runs before it touches
//! anything: the profile's declared `embed.dim` against what the backend
//! returns.

use bookrack_config::{Config, EmbedConfig};
use bookrack_embed::{DIMENSION_PROBE_TEXT, Embedder, OllamaEmbedClient};
use bookrack_index_profile::{Severity, embed_model, validate};
use eyre::{Context, Result, eyre};

use crate::cmd::input_error::CmdInputError;
use crate::profile::EmbedDeclaration;

/// The text the confirming request embeds when the probe disagrees with
/// a declaration. Only [`DIMENSION_PROBE_TEXT`] is answered from the
/// client's dimension cache, so any other text reaches the backend.
const DIMENSION_CHECK_TEXT: &str = "dimension check";

/// Build the embedding client from the environment-resolved knobs.
pub fn embedder(cfg: &Config, embed_cfg: &EmbedConfig) -> Result<OllamaEmbedClient> {
    OllamaEmbedClient::new(
        cfg.ollama_url(),
        &embed_cfg.model,
        embed_cfg.request_timeout,
        embed_cfg.max_retries,
        embed_cfg.backoff_base,
    )
    .context("build embedding client")
}

/// Refuse when the backend's output dimension for `model` differs from
/// the one `declared` names. `None` — no profile in effect — has nothing
/// to compare and passes.
///
/// The first probe may be answered from the client's dimension cache; a
/// disagreement is confirmed with a request the cache cannot answer
/// before it is reported, so a stale cache cannot refuse a backend that
/// has since been fixed. A refusal is a [`CmdInputError::Refused`] whose
/// hint names only a configuration that would pass static validation.
/// A backend that answers a one-text request with anything but one
/// non-empty vector is a fault, not caller input.
pub async fn refuse_dimension_mismatch<E: Embedder>(
    embedder: &E,
    model: &str,
    declared: Option<&EmbedDeclaration>,
) -> Result<()> {
    let Some(declared) = declared else {
        return Ok(());
    };
    let wanted = declared.dim() as usize;
    let mut observed = output_dimension(embedder, DIMENSION_PROBE_TEXT).await?;
    if observed != wanted {
        observed = output_dimension(embedder, DIMENSION_CHECK_TEXT).await?;
    }
    if observed == wanted {
        return Ok(());
    }
    Err(dimension_refusal(model, declared, observed as u32).into())
}

/// The width of the one vector the backend returns for `text`.
async fn output_dimension<E: Embedder>(embedder: &E, text: &str) -> Result<usize> {
    let vectors = embedder
        .embed_batch(&[text.to_string()])
        .await
        .context("probe the embedding dimension")?;
    match vectors.as_slice() {
        [only] if !only.is_empty() => Ok(only.len()),
        [_] => Err(eyre!(
            "the embed backend returned a zero-width vector for a dimension probe"
        )),
        other => Err(eyre!(
            "the embed backend returned {} vectors for a one-text dimension probe",
            other.len()
        )),
    }
}

/// The refusal for a declared dimension the backend does not produce.
/// The hint is chosen by validating the profile again at `observed`: it
/// recommends the one-field change only when that change validates.
fn dimension_refusal(model: &str, declared: &EmbedDeclaration, observed: u32) -> CmdInputError {
    let name = declared.name();
    let summary = format!(
        "profile '{name}' declares embed.dim {}, but the backend returns \
         {observed}-dimensional vectors for '{model}'",
        declared.dim()
    );
    let mut at_observed = declared.profile.clone();
    at_observed.embed.dim = observed;
    let errors_at = |profile| {
        validate(profile, false)
            .into_iter()
            .filter(|f| f.severity == Severity::Error)
            .collect::<Vec<_>>()
    };
    let before = errors_at(&declared.profile);
    let after = errors_at(&at_observed);
    let registered_dim = embed_model(model).map(|info| info.dim);
    let hint = if let Some(listed) =
        registered_dim.filter(|_| after.iter().any(|f| f.field_path == "embed.dim"))
    {
        format!(
            "The model registry lists '{model}' at {listed} dimensions, so the backend is \
             serving a different model under that tag. Check what the backend holds under \
             '{model}', or point the library at another profile."
        )
    } else if after
        .iter()
        .any(|f| f.field_path.starts_with("ann.") && !before.contains(f))
    {
        format!(
            "Set `embed.dim = {observed}` in profile '{name}' and adjust its [ann] section, \
             which does not validate at that dimension."
        )
    } else {
        format!("Set `embed.dim = {observed}` in profile '{name}' and re-run.")
    };
    CmdInputError::Refused {
        summary,
        hint: Some(hint),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_index_profile::{
        AnnKind, AnnSpec, EmbedSpec, IndexProfile, RerankerSpec, has_errors,
    };

    /// An embedder that answers the cached probe text and every other
    /// text with separately chosen vectors, standing in for a cache that
    /// disagrees with the live backend.
    struct Split {
        probe: Vec<Vec<f32>>,
        live: Vec<Vec<f32>>,
    }

    impl Split {
        fn uniform(width: usize) -> Split {
            Split {
                probe: vec![vec![0.5; width]],
                live: vec![vec![0.5; width]],
            }
        }
    }

    impl Embedder for Split {
        fn embed_batch(
            &self,
            texts: &[String],
        ) -> impl std::future::Future<Output = bookrack_embed::Result<Vec<Vec<f32>>>> + Send
        {
            let out = if texts == [DIMENSION_PROBE_TEXT.to_string()] {
                self.probe.clone()
            } else {
                self.live.clone()
            };
            async move { Ok(out) }
        }
    }

    fn declaration(model: &str, dim: u32, ann: AnnSpec) -> EmbedDeclaration {
        EmbedDeclaration {
            profile: IndexProfile {
                name: "p".to_string(),
                description: String::new(),
                embed: EmbedSpec {
                    backend: "ollama".to_string(),
                    model: model.to_string(),
                    dim,
                },
                ann,
                reranker: RerankerSpec::default(),
            },
        }
    }

    fn scan() -> AnnSpec {
        AnnSpec {
            kind: AnnKind::BruteForce,
            num_partitions: 1,
            num_sub_vectors: None,
            num_bits: None,
            nprobes: 1,
            refine_factor: None,
        }
    }

    fn pq(num_sub_vectors: u32) -> AnnSpec {
        AnnSpec {
            kind: AnnKind::IvfPq,
            num_partitions: 16,
            num_sub_vectors: Some(num_sub_vectors),
            num_bits: Some(8),
            nprobes: 8,
            refine_factor: None,
        }
    }

    const LISTED: &str = "qwen3-embedding:0.6b";
    const LISTED_DIM: u32 = 1024;
    const UNLISTED: &str = "synthetic-embed:unlisted";

    async fn refusal(embedder: &Split, decl: &EmbedDeclaration) -> (String, String) {
        let err = refuse_dimension_mismatch(embedder, &decl.profile.embed.model, Some(decl))
            .await
            .expect_err("the declared dimension must be refused");
        match err.downcast_ref::<CmdInputError>() {
            Some(CmdInputError::Refused {
                summary,
                hint: Some(hint),
            }) => (summary.clone(), hint.clone()),
            other => panic!("expected a refusal with a hint, got {other:?}: {err:?}"),
        }
    }

    /// The `embed.dim` value a hint recommends, if it recommends one.
    fn recommended_dim(hint: &str) -> Option<u32> {
        let rest = hint.split("`embed.dim = ").nth(1)?;
        rest.split('`').next()?.parse().ok()
    }

    #[tokio::test]
    async fn no_declaration_and_a_matching_one_pass() {
        let embedder = Split::uniform(8);
        refuse_dimension_mismatch(&embedder, UNLISTED, None)
            .await
            .expect("no profile, nothing to compare");
        let decl = declaration(UNLISTED, 8, scan());
        refuse_dimension_mismatch(&embedder, UNLISTED, Some(&decl))
            .await
            .expect("the declared dimension is the produced one");
    }

    #[tokio::test]
    async fn an_unlisted_model_is_told_the_dimension_that_validates() {
        let decl = declaration(UNLISTED, 16, scan());
        let (summary, hint) = refusal(&Split::uniform(8), &decl).await;
        assert!(summary.contains("declares embed.dim 16"), "{summary}");
        assert!(summary.contains("8-dimensional"), "{summary}");
        let dim = recommended_dim(&hint).unwrap_or_else(|| panic!("no dimension in {hint:?}"));
        let mut fixed = decl.profile.clone();
        fixed.embed.dim = dim;
        let findings = validate(&fixed, true);
        assert!(!has_errors(&findings), "{hint:?} leads to {findings:?}");
    }

    #[tokio::test]
    async fn a_listed_model_at_its_own_dimension_is_told_that_dimension() {
        let decl = declaration(LISTED, 512, scan());
        let (_, hint) = refusal(&Split::uniform(LISTED_DIM as usize), &decl).await;
        let dim = recommended_dim(&hint).unwrap_or_else(|| panic!("no dimension in {hint:?}"));
        let mut fixed = decl.profile.clone();
        fixed.embed.dim = dim;
        let findings = validate(&fixed, false);
        assert!(!has_errors(&findings), "{hint:?} leads to {findings:?}");
    }

    #[tokio::test]
    async fn a_listed_model_at_a_foreign_dimension_is_not_told_to_declare_it() {
        let decl = declaration(LISTED, LISTED_DIM, scan());
        let (_, hint) = refusal(&Split::uniform(8), &decl).await;
        assert_eq!(recommended_dim(&hint), None, "{hint:?}");
        assert!(hint.contains("different model"), "{hint:?}");
        assert!(hint.contains(&LISTED_DIM.to_string()), "{hint:?}");
    }

    #[tokio::test]
    async fn a_dimension_the_ann_shape_rejects_names_the_ann_section() {
        // 16 / 2 = 8 validates; 15 is not divisible by 2.
        let decl = declaration(UNLISTED, 16, pq(2));
        assert!(!has_errors(&validate(&decl.profile, true)));
        let (_, hint) = refusal(&Split::uniform(15), &decl).await;
        assert!(hint.contains("[ann]"), "{hint:?}");
        let dim = recommended_dim(&hint).unwrap_or_else(|| panic!("no dimension in {hint:?}"));
        let mut fixed = decl.profile.clone();
        fixed.embed.dim = dim;
        let findings = validate(&fixed, true);
        assert!(
            findings
                .iter()
                .filter(|f| f.severity == Severity::Error)
                .all(|f| f.field_path.starts_with("ann.")),
            "only the ann section is left to adjust: {findings:?}"
        );
    }

    #[tokio::test]
    async fn a_cached_answer_the_backend_no_longer_gives_is_not_refused() {
        let embedder = Split {
            probe: vec![vec![0.5; 8]],
            live: vec![vec![0.5; 16]],
        };
        let decl = declaration(UNLISTED, 16, scan());
        refuse_dimension_mismatch(&embedder, UNLISTED, Some(&decl))
            .await
            .expect("the live backend produces the declared dimension");
    }

    #[tokio::test]
    async fn a_backend_breaking_the_one_vector_contract_is_a_fault() {
        let decl = declaration(UNLISTED, 16, scan());
        let shapes: [Vec<Vec<f32>>; 3] = [vec![], vec![vec![]], vec![vec![0.5; 8], vec![0.5; 8]]];
        for bad in shapes {
            for embedder in [
                Split {
                    probe: bad.clone(),
                    live: vec![vec![0.5; 8]],
                },
                Split {
                    probe: vec![vec![0.5; 8]],
                    live: bad.clone(),
                },
            ] {
                let err = refuse_dimension_mismatch(&embedder, UNLISTED, Some(&decl))
                    .await
                    .expect_err("a broken answer must not pass");
                assert!(
                    err.downcast_ref::<CmdInputError>().is_none(),
                    "{bad:?} is a backend fault, not caller input: {err:?}"
                );
            }
        }
    }
}
