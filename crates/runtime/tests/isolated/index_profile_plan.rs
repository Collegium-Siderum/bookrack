// SPDX-License-Identifier: Apache-2.0

//! Plan derivation must not materialize the store it counts.
//!
//! `index_profile::plan_apply` documents itself as offline and
//! read-only, and its scale hint counts chunk rows to say how much a
//! planned re-embed would touch. A library whose corpus is stamped but
//! whose vector store was never built is the case where the two meet:
//! the plan needs a count, and there is nothing on disk to count.
//!
//! The entry point is `plan_apply` itself, driven with an explicit
//! `--data-dir` selection so configuration resolution runs for real
//! against a sandbox data root.

use std::path::{Path, PathBuf};

use bookrack_config::LibrarySelection;
use bookrack_index_profile::{IndexProfile, PROFILE_QWEN3_06B_DEFAULT};
use bookrack_runtime::cmd::index_profile::{PipelineFilter, plan_apply};
use bookrack_runtime::profile::{Pipeline, PipelinePlan, PlannedAction};
use bookrack_test_support::{ProcessEnv, process_env};

/// A data root holding a book corpus stamped one chunk revision behind
/// the profile's target, and nothing else. The drift is on the chunk
/// axis alone so the plan is a re-embed rather than a reset: the model
/// and dimension match, which is what keeps the plan on the leg that
/// asks for a row count.
fn stamped_root(sandbox: &bookrack_test_support::Sandbox, name: &str) -> PathBuf {
    let root = sandbox.data_root(name);
    std::fs::create_dir_all(&root).expect("create the data root");

    let profile = IndexProfile::from_named(PROFILE_QWEN3_06B_DEFAULT).expect("built-in profile");
    let target = Pipeline::Books.target_stamps(&profile.embed.model, profile.embed.dim);

    let corpus = bookrack_corpus::Corpus::open(&Pipeline::Books.corpus_db(&root))
        .expect("create the book corpus");
    for (key, value) in [
        (bookrack_corpus::EMBED_MODEL_KEY, target.embed_model.clone()),
        (
            bookrack_corpus::VECTOR_DIM_KEY,
            target.vector_dim.to_string(),
        ),
        (
            bookrack_corpus::CHUNK_VERSION_KEY,
            (target.chunk_version + 1).to_string(),
        ),
        (
            bookrack_corpus::NORMALIZE_VERSION_KEY,
            target.normalize_version.to_string(),
        ),
    ] {
        corpus.meta_set(key, &value).expect("stamp the corpus");
    }
    drop(corpus);

    root
}

fn selection(root: &Path) -> LibrarySelection {
    LibrarySelection {
        data_dir: Some(root.to_path_buf()),
        library: None,
    }
}

#[tokio::test]
async fn planning_a_reembed_leaves_an_unbuilt_vector_store_unbuilt() {
    let sandbox = process_env(ProcessEnv::isolated());
    let root = stamped_root(sandbox, "plan-unbuilt");
    let lancedb_dir = Pipeline::Books.lancedb_dir(&root);
    assert!(!lancedb_dir.exists(), "the fixture starts with no store");

    let plan = plan_apply(
        PROFILE_QWEN3_06B_DEFAULT,
        &selection(&root),
        PipelineFilter::Books,
    )
    .await
    .expect("derive the plan");

    // Assert the plan reached the counting leg. Without this the
    // directory assertion below would hold for a plan that never asked
    // for a count at all.
    let books = plan
        .sections
        .iter()
        .find(|s| s.pipeline == Pipeline::Books)
        .expect("the book pipeline is planned");
    assert!(
        matches!(&books.plan, PipelinePlan::Run(actions)
            if actions.contains(&PlannedAction::Reembed)),
        "{:?}",
        books.plan
    );

    assert!(
        !lancedb_dir.exists(),
        "planning created {}",
        lancedb_dir.display()
    );
}
