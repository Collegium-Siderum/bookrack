// SPDX-License-Identifier: Apache-2.0

//! The pinned reranker model artifacts, as machine-readable constants.
//!
//! The model registry (in the index-profile crate) owns model
//! *identity* — the tag a profile's `reranker.model` names and its
//! family. This table owns the *artifact* behind a tag: which GGUF
//! file realizes it, where to download it from, and the digest that
//! proves the download is the pinned conversion. The two tables are
//! keyed by the same tag strings. `LLAMA_SERVER_VERSION.md` in this
//! crate documents the pin for humans.

use std::path::PathBuf;

/// One pinned reranker model artifact: the registry tag it realizes,
/// the Hugging Face repository and file the GGUF is downloaded from,
/// the SHA-256 of that file, and its size in bytes (for download
/// progress and a cheap sanity check).
#[derive(Debug, Clone, Copy)]
pub struct RerankerModelPin {
    pub tag: &'static str,
    pub hf_repo: &'static str,
    pub hf_file: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
}

/// Every pinned reranker model artifact. The 4B reranker is
/// deliberately absent: its GGUF conversions have a defect history and
/// its per-pair latency does not fit an interactive query budget.
pub const RERANKER_MODEL_PINS: &[RerankerModelPin] = &[RerankerModelPin {
    tag: "Qwen3-Reranker-0.6B",
    hf_repo: "ggml-org/Qwen3-Reranker-0.6B-Q8_0-GGUF",
    hf_file: "qwen3-reranker-0.6b-q8_0.gguf",
    sha256: "22c9979ce4fbcdc5acdc310c6641c32797eff1aa980b8f7a2db8a8ea23429a48",
    bytes: 639_153_184,
}];

/// Look up the pinned artifact for a registry model tag.
pub fn reranker_model_pin(tag: &str) -> Option<&'static RerankerModelPin> {
    RERANKER_MODEL_PINS.iter().find(|p| p.tag == tag)
}

/// Download URL for a pinned model file.
pub fn reranker_model_download_url(pin: &RerankerModelPin) -> String {
    format!(
        "https://huggingface.co/{repo}/resolve/main/{file}",
        repo = pin.hf_repo,
        file = pin.hf_file,
    )
}

/// Per-user directory where operator-initiated installs place model
/// files; the last stop in the [`locate_reranker_model`] search chain.
/// `None` when the platform data directory cannot be located.
pub fn models_managed_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("bookrack").join("models"))
}

/// Outcome of a reranker model file search.
#[derive(Debug)]
pub struct RerankerModelLocation {
    /// First candidate that exists as a file, if any.
    pub path: Option<PathBuf>,
    /// Every candidate that was checked, in search order.
    pub probed: Vec<PathBuf>,
}

/// Locate the GGUF file for a registry model tag.
///
/// [`crate::RERANKER_MODEL_ENV`], when set, is authoritative: it names
/// the `.gguf` file itself and only that path is checked, so a typo
/// surfaces as a miss instead of being papered over by a fallback. The
/// operator vouches for an explicit path, so no checksum applies to it
/// — and it wins for every tag, which is the point of an escape hatch.
/// When unset, the per-user managed directory that `bookrack doctor
/// --install-reranker` populates is checked. There is no
/// executable-adjacent stop: model weights do not ship in release
/// archives.
pub fn locate_reranker_model(tag: &str) -> RerankerModelLocation {
    locate_reranker_model_from(
        std::env::var(crate::RERANKER_MODEL_ENV).ok(),
        models_managed_dir(),
        tag,
        &|path| path.is_file(),
    )
}

/// Pure search logic for [`locate_reranker_model`], factored out so
/// the chain can be tested without mutating process-global environment
/// variables or touching the filesystem.
fn locate_reranker_model_from(
    override_path: Option<String>,
    managed_dir: Option<PathBuf>,
    tag: &str,
    is_file: &dyn Fn(&std::path::Path) -> bool,
) -> RerankerModelLocation {
    let candidates: Vec<PathBuf> = match override_path
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(path) => vec![PathBuf::from(path)],
        None => managed_dir
            .zip(reranker_model_pin(tag))
            .map(|(d, pin)| d.join(pin.hf_file))
            .into_iter()
            .collect(),
    };
    let path = candidates.iter().find(|p| is_file(p)).cloned();
    RerankerModelLocation {
        path,
        probed: candidates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_are_well_formed() {
        for pin in RERANKER_MODEL_PINS {
            assert_eq!(pin.sha256.len(), 64, "{}", pin.tag);
            assert!(
                pin.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{}",
                pin.tag
            );
            assert!(pin.hf_file.ends_with(".gguf"), "{}", pin.tag);
            assert!(pin.hf_repo.contains('/'), "{}", pin.tag);
            assert!(pin.bytes > 0, "{}", pin.tag);
        }
    }

    #[test]
    fn lookup_finds_the_pinned_tag_and_rejects_others() {
        assert!(reranker_model_pin("Qwen3-Reranker-0.6B").is_some());
        assert!(reranker_model_pin("Qwen3-Reranker-4B").is_none());
    }

    #[test]
    fn download_url_names_repo_and_file() {
        let pin = reranker_model_pin("Qwen3-Reranker-0.6B").expect("pinned");
        let url = reranker_model_download_url(pin);
        assert_eq!(
            url,
            "https://huggingface.co/ggml-org/Qwen3-Reranker-0.6B-Q8_0-GGUF\
             /resolve/main/qwen3-reranker-0.6b-q8_0.gguf"
        );
    }

    #[test]
    fn locate_only_checks_the_override_when_set() {
        let found = locate_reranker_model_from(
            Some("override/model.gguf".into()),
            Some(PathBuf::from("managed")),
            "Qwen3-Reranker-0.6B",
            &|_| true,
        );
        assert_eq!(
            found.path.as_deref(),
            Some(std::path::Path::new("override/model.gguf"))
        );
        assert_eq!(found.probed.len(), 1);

        let missing = locate_reranker_model_from(
            Some("override/model.gguf".into()),
            Some(PathBuf::from("managed")),
            "Qwen3-Reranker-0.6B",
            &|_| false,
        );
        assert!(missing.path.is_none());
        assert_eq!(missing.probed.len(), 1, "no fallback behind the override");
    }

    #[test]
    fn managed_candidate_is_the_pinned_filename() {
        let found = locate_reranker_model_from(
            None,
            Some(PathBuf::from("managed")),
            "Qwen3-Reranker-0.6B",
            &|_| false,
        );
        assert_eq!(
            found.probed,
            vec![PathBuf::from("managed").join("qwen3-reranker-0.6b-q8_0.gguf")]
        );
    }

    #[test]
    fn unknown_tag_has_no_managed_candidate() {
        let found = locate_reranker_model_from(
            None,
            Some(PathBuf::from("managed")),
            "Qwen3-Reranker-4B",
            &|_| true,
        );
        assert!(found.path.is_none());
        assert!(found.probed.is_empty());
    }
}
