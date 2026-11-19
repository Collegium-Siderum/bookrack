// SPDX-License-Identifier: Apache-2.0

//! Whether a source already on file needs a rerun.

use bookrack_catalog::{Catalog, CatalogError, Intake, IntakeStatus};
use bookrack_core::{ItemKind, NodeId, PartitionIdx};
use bookrack_corpus::{Corpus, CorpusError};

/// Why the stores could not be read for the decision. Each pipeline's
/// error type takes it apart into its own variant of the same name.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OnFileError {
    #[error("catalog error")]
    Catalog(#[from] CatalogError),
    #[error("corpus error")]
    Corpus(#[from] CorpusError),
}

/// What the catalog already holds for a source, against what this run
/// would write.
#[derive(Debug)]
pub(crate) enum OnFile {
    /// Run every stage.
    Rerun,
    /// Every stamp matches: report the item as it stands and do nothing.
    Current(Current),
    /// Only `extractor_version` is behind: extract, then compare the
    /// result with the envelope on file. Books only.
    ExtractorBehind(Box<Intake>),
    /// Every stamp matches and the rule's fingerprint does not: extract,
    /// then compare the result with the envelope on file. Books only.
    RuleChanged(Box<Intake>),
}

/// The item a no-op run reports.
#[derive(Debug)]
pub(crate) struct Current {
    pub(crate) intake_id: i64,
    /// The root node of the item's tree.
    pub(crate) root_id: NodeId,
    /// The audit verdict the previous run stored on the attrs row.
    pub(crate) audit_verdict: Option<String>,
    /// The audit confidence the previous run stored on the attrs row.
    pub(crate) audit_confidence: Option<String>,
}

/// Inspect the catalog and item state to decide how much of a resubmit
/// of `source_sha` has work to do.
///
/// Anything short of [`OnFile::Rerun`] needs the intake on file and
/// `Embedded`, the item embedded with `embed_model`, and the corpus
/// stamped with this binary's chunk and normalize versions; a stamp the
/// corpus does not carry counts as a mismatch. The extractor stamp then
/// decides: equal to this binary's is [`OnFile::Current`]; behind it is
/// [`OnFile::ExtractorBehind`] for a book and [`OnFile::Rerun`] for a
/// paper.
///
/// `rule_fingerprint` is the fingerprint of the rule this run extracts
/// under, `None` for a source no rule reads. With the extractor stamp
/// current, a row recording another fingerprint, or none, is
/// [`OnFile::RuleChanged`].
pub(crate) fn on_file(
    kind: ItemKind,
    catalog: &Catalog,
    corpus: &Corpus,
    source_sha: &str,
    embed_model: &str,
    rule_fingerprint: Option<&str>,
) -> Result<OnFile, OnFileError> {
    let Some(intake) = catalog.intake_by_sha(kind, source_sha)? else {
        return Ok(OnFile::Rerun);
    };
    if intake.status != IntakeStatus::Embedded {
        return Ok(OnFile::Rerun);
    }
    let root_id = PartitionIdx::new(intake.intake_id).root();
    let Some(state) = catalog.book_state(root_id.get())? else {
        return Ok(OnFile::Rerun);
    };
    if state.embed_model.as_deref() != Some(embed_model) {
        return Ok(OnFile::Rerun);
    }
    // The chunk and normalize versions are recorded in corpus index_meta
    // rather than on the catalog row; a bump on either invalidates the
    // existing chunks, so a noop is only safe when both still match the
    // versions this binary writes.
    let stored_chunk = corpus
        .meta_get(bookrack_corpus::CHUNK_VERSION_KEY)?
        .and_then(|v| v.parse::<u32>().ok());
    if stored_chunk != Some(bookrack_text::CHUNK_VERSION) {
        return Ok(OnFile::Rerun);
    }
    let stored_normalize = corpus
        .meta_get(bookrack_corpus::NORMALIZE_VERSION_KEY)?
        .and_then(|v| v.parse::<u32>().ok());
    if stored_normalize != Some(bookrack_text::NORMALIZE_VERSION) {
        return Ok(OnFile::Rerun);
    }
    if intake.extractor_version != bookrack_extract::EXTRACTOR_VERSION {
        return Ok(match kind {
            ItemKind::Book => OnFile::ExtractorBehind(Box::new(intake)),
            // Only a book is restamped. A paper's identify stage reads
            // the source file beside the envelope, so an envelope that
            // compares equal does not show the whole pipeline would
            // produce the same item.
            ItemKind::Paper | ItemKind::Reference => OnFile::Rerun,
        });
    }
    if let Some(fingerprint) = rule_fingerprint
        && intake.rule_fingerprint.as_deref() != Some(fingerprint)
    {
        return Ok(OnFile::RuleChanged(Box::new(intake)));
    }
    let (audit_verdict, audit_confidence) = stored_audit_outcome(kind, catalog, intake.intake_id)?;
    Ok(OnFile::Current(Current {
        intake_id: intake.intake_id,
        root_id,
        audit_verdict,
        audit_confidence,
    }))
}

/// The audit verdict and confidence stored on the item's attrs row by
/// its previous run, so a run that skips the metadata sub-step still
/// tells the operator whether the item landed `clean` or `needs_work`.
pub(crate) fn stored_audit_outcome(
    kind: ItemKind,
    catalog: &Catalog,
    intake_id: i64,
) -> Result<(Option<String>, Option<String>), CatalogError> {
    let attrs = catalog.publication_attrs(intake_id, kind)?;
    Ok(match attrs {
        Some(attrs) => (attrs.audit_verdict, attrs.confidence),
        None => (None, None),
    })
}

#[cfg(test)]
mod tests {
    use bookrack_catalog::{NewIntake, NewItemState, NewPublicationAttrs};
    use bookrack_corpus::{CHUNK_VERSION_KEY, NORMALIZE_VERSION_KEY};

    use super::*;

    const SHA: &str = "5ca1ab1e";
    const MODEL: &str = "fake-model";

    /// A catalog and corpus holding one item of `kind` that a resubmit
    /// finds current: embedded under `MODEL`, extracted by this build,
    /// audited, and stamped with this build's chunk and normalize
    /// versions.
    fn current(kind: ItemKind) -> (Catalog, Corpus, i64) {
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let corpus = Corpus::open_in_memory().expect("corpus");
        let intake_id = catalog
            .register_intake(
                kind,
                &NewIntake::new(SHA.to_string()).format("txt".to_string()),
            )
            .expect("register")
            .intake()
            .intake_id;
        catalog
            .set_extraction(kind, intake_id, "txt", bookrack_extract::EXTRACTOR_VERSION)
            .expect("extraction");
        catalog
            .set_intake_status(kind, intake_id, IntakeStatus::Embedded)
            .expect("status");
        let root = PartitionIdx::new(intake_id).root().get();
        catalog
            .upsert_book_state(&NewItemState::new(root, intake_id, "embedded").embed_model(MODEL))
            .expect("state");
        catalog
            .upsert_publication_attrs(&NewPublicationAttrs::new(intake_id, kind))
            .expect("attrs");
        catalog
            .update_audit_rollup(intake_id, kind, "low", "needs_work")
            .expect("rollup");
        corpus
            .meta_set(CHUNK_VERSION_KEY, &bookrack_text::CHUNK_VERSION.to_string())
            .expect("chunk stamp");
        corpus
            .meta_set(
                NORMALIZE_VERSION_KEY,
                &bookrack_text::NORMALIZE_VERSION.to_string(),
            )
            .expect("normalize stamp");
        (catalog, corpus, intake_id)
    }

    fn judge(kind: ItemKind, catalog: &Catalog, corpus: &Corpus) -> OnFile {
        on_file(kind, catalog, corpus, SHA, MODEL, None).expect("on_file")
    }

    /// Every precondition of a no-op, broken one at a time, sends an
    /// item of `kind` through the whole pipeline.
    fn each_unmet_precondition_is_a_rerun(kind: ItemKind) {
        let (catalog, corpus, intake_id) = current(kind);
        match judge(kind, &catalog, &corpus) {
            OnFile::Current(found) => {
                assert_eq!(found.intake_id, intake_id);
                assert_eq!(found.root_id, PartitionIdx::new(intake_id).root());
                assert_eq!(found.audit_verdict.as_deref(), Some("needs_work"));
                assert_eq!(found.audit_confidence.as_deref(), Some("low"));
            }
            other => panic!("{kind:?}: the untouched item is current, got {other:?}"),
        }

        let rerun = |what: &str, catalog: &Catalog, corpus: &Corpus| {
            let got = judge(kind, catalog, corpus);
            assert!(
                matches!(got, OnFile::Rerun),
                "{kind:?}: {what} must be a rerun, got {got:?}"
            );
        };

        let unknown = on_file(kind, &catalog, &corpus, "0ddba11", MODEL, None).expect("on_file");
        assert!(matches!(unknown, OnFile::Rerun), "{kind:?}: {unknown:?}");
        let other_model = on_file(kind, &catalog, &corpus, SHA, "other", None).expect("on_file");
        assert!(
            matches!(other_model, OnFile::Rerun),
            "{kind:?}: {other_model:?}"
        );

        for (key, value) in [
            (CHUNK_VERSION_KEY, bookrack_text::CHUNK_VERSION),
            (NORMALIZE_VERSION_KEY, bookrack_text::NORMALIZE_VERSION),
        ] {
            let (catalog, corpus, _) = current(kind);
            corpus
                .meta_set(key, &(value + 1).to_string())
                .expect("move stamp");
            rerun(&format!("a moved {key}"), &catalog, &corpus);
            corpus.meta_set(key, "not a number").expect("spoil stamp");
            rerun(&format!("an unreadable {key}"), &catalog, &corpus);
        }

        // A corpus that carries no stamp at all.
        let (catalog, _, _) = current(kind);
        let unstamped = Corpus::open_in_memory().expect("corpus");
        rerun("an unstamped corpus", &catalog, &unstamped);

        let (catalog, corpus, intake_id) = current(kind);
        catalog
            .set_intake_status(kind, intake_id, IntakeStatus::Extracted)
            .expect("status");
        rerun("an item short of Embedded", &catalog, &corpus);
    }

    #[test]
    fn each_unmet_precondition_is_a_rerun_for_a_book() {
        each_unmet_precondition_is_a_rerun(ItemKind::Book);
    }

    #[test]
    fn each_unmet_precondition_is_a_rerun_for_a_paper() {
        each_unmet_precondition_is_a_rerun(ItemKind::Paper);
    }

    fn behind_extractor(kind: ItemKind) -> OnFile {
        let (catalog, corpus, intake_id) = current(kind);
        catalog
            .set_extraction(
                kind,
                intake_id,
                "txt",
                bookrack_extract::EXTRACTOR_VERSION - 1,
            )
            .expect("extraction");
        judge(kind, &catalog, &corpus)
    }

    #[test]
    fn a_book_behind_on_the_extractor_alone_is_compared_with_its_envelope() {
        let got = behind_extractor(ItemKind::Book);
        assert!(matches!(got, OnFile::ExtractorBehind(_)), "{got:?}");
    }

    #[test]
    fn a_paper_behind_on_the_extractor_reruns() {
        let got = behind_extractor(ItemKind::Paper);
        assert!(matches!(got, OnFile::Rerun), "{got:?}");
    }

    #[test]
    fn a_rule_fingerprint_the_row_does_not_record_is_a_changed_rule() {
        let (catalog, corpus, intake_id) = current(ItemKind::Book);
        let judged = |fingerprint| {
            on_file(ItemKind::Book, &catalog, &corpus, SHA, MODEL, fingerprint).expect("on_file")
        };
        let got = judged(Some("f1"));
        assert!(matches!(got, OnFile::RuleChanged(_)), "{got:?}");
        catalog
            .set_rule_fingerprint(intake_id, "f1")
            .expect("fingerprint");
        let got = judged(Some("f1"));
        assert!(matches!(got, OnFile::Current(_)), "{got:?}");
        let got = judged(Some("f2"));
        assert!(matches!(got, OnFile::RuleChanged(_)), "{got:?}");
    }

    /// The one caller on the paper side passes no fingerprint, and
    /// treats every answer but `Current` as a rerun; this pins that it
    /// is never handed one of the two book-only answers.
    #[test]
    fn a_paper_is_never_handed_a_book_only_answer() {
        for stale_extractor in [false, true] {
            for stale_fingerprint in [false, true] {
                let (catalog, corpus, intake_id) = current(ItemKind::Paper);
                if stale_extractor {
                    catalog
                        .set_extraction(
                            ItemKind::Paper,
                            intake_id,
                            "txt",
                            bookrack_extract::EXTRACTOR_VERSION - 1,
                        )
                        .expect("extraction");
                }
                if stale_fingerprint {
                    catalog
                        .set_rule_fingerprint(intake_id, "f0")
                        .expect("fingerprint");
                }
                let got = judge(ItemKind::Paper, &catalog, &corpus);
                assert!(
                    matches!(got, OnFile::Rerun | OnFile::Current(_)),
                    "extractor stale {stale_extractor}, fingerprint set {stale_fingerprint}: {got:?}"
                );
            }
        }
    }
}
