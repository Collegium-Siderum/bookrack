// SPDX-License-Identifier: Apache-2.0

//! A profile that promises a reranker model the managed install cannot
//! provide is refused at bring-up with a way out that works.
//!
//! The managed installer downloads only the pinned models, so a tag with
//! no pin is never fixed by `doctor --install-reranker`; the refusal has
//! to point at the two routes that do serve it — an explicit model file,
//! or an operator-run server behind `reranker.url`.

#![cfg(unix)]

use bookrack_config::{
    Config, DEFAULT_EMBED_MODEL, LibraryKind, ManifestIdentitySeed, RERANKER_MODEL_ENV,
    set_manifest_index_profile,
};
use bookrack_index_profile::USER_PROFILE_DIR_NAME;
use bookrack_runtime::rerank_supervisor::{RerankerModelRefusal, RerankerModelState};
use bookrack_test_support::{ProcessEnv, process_env};

const PROFILE: &str = "unpinned-reranker";
const UNPINNED_TAG: &str = "synthetic-reranker:unpinned";

fn profile_toml() -> String {
    format!(
        "schema_version = 1\n\
         name = \"{PROFILE}\"\n\
         description = \"A cross-encoder stage on a model with no pinned download.\"\n\
         \n\
         [embed]\n\
         backend = \"ollama\"\n\
         model = \"{DEFAULT_EMBED_MODEL}\"\n\
         dim = 1024\n\
         \n\
         [ann]\n\
         kind = \"brute-force\"\n\
         num_partitions = 1\n\
         num_bits = 8\n\
         nprobes = 1\n\
         refine_factor = 1\n\
         \n\
         [reranker]\n\
         kind = \"cross-encoder\"\n\
         backend = \"llama-server\"\n\
         model = \"{UNPINNED_TAG}\"\n\
         top_k_in = 50\n\
         top_k_out = 10\n"
    )
}

#[tokio::test]
async fn an_unpinned_reranker_model_is_not_sent_to_the_installer() {
    let sandbox = process_env(ProcessEnv::isolated());
    let profiles = sandbox
        .registry_path()
        .parent()
        .expect("the sandbox registry has a parent")
        .join(USER_PROFILE_DIR_NAME);
    std::fs::create_dir_all(&profiles).expect("create the user profile directory");
    std::fs::write(profiles.join(format!("{PROFILE}.toml")), profile_toml())
        .expect("write the user profile");
    let root = tempfile::tempdir().expect("tempdir");
    set_manifest_index_profile(
        root.path(),
        Some(PROFILE),
        ManifestIdentitySeed {
            name: "default",
            kind: LibraryKind::Test,
            description: None,
        },
    )
    .expect("declare the profile");
    let cfg = Config::new(root.path().to_path_buf(), "http://127.0.0.1:9".to_string());
    let runtime_dir = tempfile::tempdir().expect("tempdir");

    let err =
        bookrack_runtime::rerank_supervisor::bring_up_reranker(&cfg, runtime_dir.path(), None)
            .await
            .map(|_| ())
            .expect_err("a reranker model nothing can provide must refuse bring-up");
    let refusal = err
        .downcast_ref::<RerankerModelRefusal>()
        .unwrap_or_else(|| panic!("a typed refusal the CLI can render: {err:#}"));
    assert_eq!(
        refusal.state,
        RerankerModelState::Unpinned {
            tag: UNPINNED_TAG.to_string()
        }
    );
    assert_eq!(refusal.profile, PROFILE);
    let text = format!("{err:#}");
    assert!(
        !text.contains("--install-reranker"),
        "the installer cannot provide an unpinned model: {text}"
    );
    assert!(text.contains(RERANKER_MODEL_ENV), "{text}");
    assert!(text.contains("reranker.url"), "{text}");
}
