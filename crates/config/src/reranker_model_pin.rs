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
/// The per-user managed directory that `bookrack setup reranker`
/// populates is the one stop, and the file name in it comes from the
/// tag's pin: a tag with no pin has no candidate. There is no
/// executable-adjacent stop, because model weights do not ship in
/// release archives, and nothing in the environment names a file.
pub fn locate_reranker_model(tag: &str) -> RerankerModelLocation {
    locate_reranker_model_from(models_managed_dir(), tag, &|path| path.is_file())
}

/// Pure search logic for [`locate_reranker_model`], factored out so
/// the chain can be tested without touching the filesystem.
// Parallel implementation: managed-lookup-chain.
fn locate_reranker_model_from(
    managed_dir: Option<PathBuf>,
    tag: &str,
    is_file: &dyn Fn(&std::path::Path) -> bool,
) -> RerankerModelLocation {
    let candidates: Vec<PathBuf> = managed_dir
        .zip(reranker_model_pin(tag))
        .map(|(d, pin)| d.join(pin.hf_file))
        .into_iter()
        .collect();
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
    fn managed_candidate_is_the_pinned_filename() {
        let found = locate_reranker_model_from(
            Some(PathBuf::from("managed")),
            "Qwen3-Reranker-0.6B",
            &|_| false,
        );
        assert_eq!(
            found.probed,
            vec![PathBuf::from("managed").join("qwen3-reranker-0.6b-q8_0.gguf")]
        );
        assert!(found.path.is_none());

        let present = locate_reranker_model_from(
            Some(PathBuf::from("managed")),
            "Qwen3-Reranker-0.6B",
            &|_| true,
        );
        assert_eq!(present.path, present.probed.first().cloned());
    }

    #[test]
    fn unknown_tag_has_no_managed_candidate() {
        let found = locate_reranker_model_from(
            Some(PathBuf::from("managed")),
            "Qwen3-Reranker-4B",
            &|_| true,
        );
        assert!(found.path.is_none());
        assert!(found.probed.is_empty());
    }
}
