// SPDX-License-Identifier: Apache-2.0

//! The translation memory behind the `translate.*` tools.
//!
//! A sealed segment's source text is embedded with the library's own
//! model and kept, with its final text, in a LanceDB table of its own
//! under the data root. `translate.tm_search` and the `tm_hits` of a
//! fetched package query it by language pair; `translate.seal` extends
//! it; `translate.tm_rebuild` recomputes it from the sealed segments.
//!
//! Three stamps in `translate_meta` say which row layout, model and
//! width wrote the memory. Every path that reads or extends a memory
//! holding rows checks them first, so a memory another model wrote is
//! never searched with this one. The gate is strict on purpose: a
//! memory without stamps — what an interrupted rebuild leaves behind —
//! is refused until a full rebuild stamps it.
//!
//! Scores are cosine similarity weighted by `source_kind`, so a
//! person's rendering ranks above a machine's at equal similarity.
//! Both sides are embedded as bare text: the memory answers "which
//! sealed sentence resembles this one", a symmetric question, not the
//! query-to-passage retrieval the library search prefixes for.

use std::path::PathBuf;

use bookrack_catalog::Catalog;
use bookrack_core::{ItemKind, error_chain};
use bookrack_embed::Embedder;
use bookrack_ops::Ops;
use bookrack_translate::TranslateError;
use bookrack_translate::meta::TmStamps;
use bookrack_translate::segments::SegmentRow;
use bookrack_vectors::tm::is_lang_tag;
use bookrack_vectors::{TM_SCHEMA_VERSION, TmFilter, TmHitRow, TmRow, TmStore, VectorsError};

use crate::translate::{
    SpanRef, TmHit, ToolResult, TranslateFetchSegmentResult, TranslateToolError,
};

/// Weight of a rendering a person wrote or approved.
// setting: translate.tm_weight_human
pub const TM_WEIGHT_HUMAN: f64 = 1.0;

/// Weight of an imported rendering a person reviewed.
// setting: translate.tm_weight_edited
pub const TM_WEIGHT_EDITED: f64 = 0.95;

/// Weight of a machine rendering that went through a reflection.
// setting: translate.tm_weight_llm_reflected
pub const TM_WEIGHT_LLM_REFLECTED: f64 = 0.9;

/// Weight of a machine rendering sealed straight from its draft.
// setting: translate.tm_weight_llm_draft
pub const TM_WEIGHT_LLM_DRAFT: f64 = 0.8;

/// Weight of an imported rendering nobody reviewed, and of any
/// `source_kind` this build does not know.
// setting: translate.tm_weight_imported
pub const TM_WEIGHT_IMPORTED: f64 = 0.7;

/// Candidates fetched per hit wanted, so that weighting has rows to
/// reorder.
// setting: translate.tm_overfetch
pub const TM_OVERFETCH: usize = 3;

/// Most memory hits a fetched package carries.
// setting: translate.tm_package_hits
pub const TM_PACKAGE_HITS: usize = 3;

/// The source language a row records when the catalog names none for
/// the book, or names something that is not a language tag.
pub const UNKNOWN_LANG: &str = "und";

/// `tm_status` of a package when the memory was searched.
pub const TM_OK: &str = "ok";
/// `tm_status` of a package when the library has no memory.
pub const TM_ABSENT: &str = "absent";
/// `tm_status` of a package when the memory exists but could not be
/// consulted: no embedder, the backend unreachable, or stamps that do
/// not match this build.
pub const TM_UNAVAILABLE: &str = "unavailable";

/// What a tool needs to reach the memory: where it lives, and the
/// embedder with the model and width the memory is stamped with.
pub(crate) struct TmContext<'a, E: Embedder> {
    pub lancedb_dir: PathBuf,
    pub embedder: &'a E,
    pub embed_model: &'a str,
    pub dim: usize,
}

/// The memory context of a warm library, or `NoEmbedder` on an `Ops`
/// built without one.
pub(crate) fn tm_context<E: Embedder>(ops: &Ops<E>) -> ToolResult<TmContext<'_, E>> {
    let (Some(embedder), Some(embed_model), Some(dim)) =
        (ops.embedder(), ops.embed_model(), ops.dimension())
    else {
        return Err(TranslateToolError::NoEmbedder);
    };
    Ok(TmContext {
        lancedb_dir: ops.translate_lancedb_dir(),
        embedder,
        embed_model,
        dim,
    })
}

impl<E: Embedder> TmContext<'_, E> {
    /// The stamps this build writes on the memory.
    pub fn expected_stamps(&self) -> TmStamps {
        TmStamps {
            schema_version: TM_SCHEMA_VERSION,
            embed_model: self.embed_model.to_owned(),
            vector_dim: u32::try_from(self.dim).unwrap_or(u32::MAX),
        }
    }

    /// The memory table, or `None` when the library has none. Never
    /// creates it.
    pub async fn try_open(&self) -> ToolResult<Option<TmStore>> {
        Ok(TmStore::try_open(&self.lancedb_dir).await?)
    }

    /// Embed `texts` as bare text, one vector per input, each checked
    /// against the memory's width.
    pub async fn embed_many(&self, texts: &[String]) -> ToolResult<Vec<Vec<f32>>> {
        let vectors = self.embedder.embed_batch(texts).await?;
        if vectors.len() != texts.len() {
            return Err(TranslateToolError::EmbeddingShape(format!(
                "the embedder answered {} vectors for {} texts",
                vectors.len(),
                texts.len()
            )));
        }
        for vector in &vectors {
            if vector.len() != self.dim {
                return Err(VectorsError::DimensionMismatch {
                    got: vector.len(),
                    expected: self.dim,
                }
                .into());
            }
        }
        Ok(vectors)
    }

    /// Embed one text.
    pub async fn embed_one(&self, text: &str) -> ToolResult<Vec<f32>> {
        let mut vectors = self
            .embed_many(std::slice::from_ref(&text.to_owned()))
            .await?;
        Ok(vectors.remove(0))
    }
}

/// The gate every path through a memory holding rows passes: the
/// stored stamps must be present and equal to what this build writes.
pub(crate) fn check_stamps(
    stored: Option<&TmStamps>,
    expected: &TmStamps,
) -> Result<(), TranslateError> {
    match stored {
        None => Err(TranslateError::TmUnstamped),
        Some(stored) => stored.check_against(expected),
    }
}

/// The source language of a book as the catalog records it, lowered
/// and trimmed, or [`UNKNOWN_LANG`] when it records none or something
/// that is not a language tag. Without a catalog, every book is
/// [`UNKNOWN_LANG`].
pub(crate) fn source_lang_of(catalog: Option<&Catalog>, intake_id: i64) -> ToolResult<String> {
    let Some(catalog) = catalog else {
        return Ok(UNKNOWN_LANG.to_owned());
    };
    let attrs = catalog.effective_publication_attrs(intake_id, ItemKind::Book)?;
    let tag = attrs
        .get("language")
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_default();
    Ok(if is_lang_tag(&tag) {
        tag
    } else {
        UNKNOWN_LANG.to_owned()
    })
}

/// The memory row of a sealed segment.
pub(crate) fn tm_row_for(
    segment: &SegmentRow,
    intake_id: i64,
    target_lang: &str,
    source_lang: &str,
    src_text: String,
    vector: Vec<f32>,
) -> ToolResult<TmRow> {
    let field = |name: &'static str, value: &Option<String>| {
        value.clone().ok_or(TranslateError::MissingField {
            stage: "tm",
            field: name,
        })
    };
    let offset = |value: i64| i32::try_from(value).unwrap_or(i32::MAX);
    Ok(TmRow {
        vector,
        segment_id: segment.segment_id,
        unit_id: segment.unit_id,
        intake_id,
        target_lang: target_lang.to_owned(),
        source_lang: source_lang.to_owned(),
        start_node_id: segment.start_node_id,
        start_char_offset: offset(segment.start_char_offset),
        end_node_id: segment.end_node_id,
        end_char_offset: offset(segment.end_char_offset),
        src_text,
        dst_text: field("final_text", &segment.final_text)?,
        source_kind: field("source_kind", &segment.source_kind)?,
        sealed_at: field("sealed_at", &segment.sealed_at)?,
    })
}

/// The weight a `source_kind` carries in a score.
fn weight_of(source_kind: &str) -> f64 {
    match source_kind {
        "human" => TM_WEIGHT_HUMAN,
        "edited" => TM_WEIGHT_EDITED,
        "llm-reflected" => TM_WEIGHT_LLM_REFLECTED,
        "llm-draft" => TM_WEIGHT_LLM_DRAFT,
        _ => TM_WEIGHT_IMPORTED,
    }
}

/// Score raw hits, drop the excluded segment, order by score and keep
/// the first `limit`. The score is cosine similarity, floored at zero,
/// times the `source_kind` weight; ties break on segment id so the
/// order is stable.
pub(crate) fn weigh_and_rank(
    hits: Vec<TmHitRow>,
    limit: usize,
    exclude: Option<i64>,
) -> Vec<TmHit> {
    let mut scored: Vec<TmHit> = hits
        .into_iter()
        .filter(|hit| Some(hit.row.segment_id) != exclude)
        .map(|hit| {
            let similarity = f64::from(1.0 - hit.distance).max(0.0);
            let row = hit.row;
            TmHit {
                segment_id: row.segment_id,
                intake_id: row.intake_id,
                span: SpanRef {
                    start_node_id: row.start_node_id,
                    start_char_offset: i64::from(row.start_char_offset),
                    end_node_id: row.end_node_id,
                    end_char_offset: i64::from(row.end_char_offset),
                },
                score: similarity * weight_of(&row.source_kind),
                src_text: row.src_text,
                dst_text: row.dst_text,
                source_kind: row.source_kind,
            }
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.segment_id.cmp(&b.segment_id))
    });
    scored.truncate(limit);
    scored
}

/// One question to the memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TmQuery {
    pub text: String,
    pub target_lang: String,
    pub source_lang: String,
    pub intake_id: Option<i64>,
    /// A segment to leave out of the answer: the one being translated.
    pub exclude: Option<i64>,
}

impl TmQuery {
    /// The store filter for this query, validated.
    pub fn filter(&self) -> ToolResult<TmFilter<'_>> {
        let filter = TmFilter {
            target_lang: &self.target_lang,
            source_lang: &self.source_lang,
            intake_id: self.intake_id,
        };
        filter.predicate()?;
        Ok(filter)
    }
}

/// Answer `query` from the memory: `None` when the library has no
/// memory, otherwise the weighted hits. A memory holding rows is
/// consulted only when `stored` carries this build's stamps.
pub(crate) async fn search_memory<E: Embedder>(
    tm: &TmContext<'_, E>,
    stored: Option<&TmStamps>,
    query: &TmQuery,
    limit: usize,
) -> ToolResult<Option<Vec<TmHit>>> {
    let filter = query.filter()?;
    let Some(store) = tm.try_open().await? else {
        return Ok(None);
    };
    check_stamps(stored, &tm.expected_stamps())?;
    let vector = tm.embed_one(&query.text).await?;
    let wanted = limit + usize::from(query.exclude.is_some());
    let raw = store
        .search(&vector, &filter, wanted * TM_OVERFETCH)
        .await?;
    Ok(Some(weigh_and_rank(raw, limit, query.exclude)))
}

/// Fill a fetched package's `tm_hits` and `tm_status`. A memory that
/// cannot be consulted degrades the package rather than failing the
/// fetch, and says so in `tm_status` and the log.
pub(crate) async fn complete_package_tm<E: Embedder>(
    tm: Option<&TmContext<'_, E>>,
    stored: Option<&TmStamps>,
    package: &mut TranslateFetchSegmentResult,
) {
    let Some(tm) = tm else {
        tracing::warn!(
            segment_id = package.segment_id,
            "translation memory not consulted: this server has no embedder"
        );
        package.tm_status = TM_UNAVAILABLE.to_owned();
        return;
    };
    let query = TmQuery {
        text: package.source_text.clone(),
        target_lang: package.target_lang.clone(),
        source_lang: package.source_lang.clone(),
        intake_id: None,
        exclude: Some(package.segment_id),
    };
    match search_memory(tm, stored, &query, TM_PACKAGE_HITS).await {
        Ok(None) => package.tm_status = TM_ABSENT.to_owned(),
        Ok(Some(hits)) => {
            package.tm_hits = hits;
            package.tm_status = TM_OK.to_owned();
        }
        Err(err) => {
            tracing::warn!(
                segment_id = package.segment_id,
                reason = %error_chain(&err),
                "translation memory not consulted"
            );
            package.tm_status = TM_UNAVAILABLE.to_owned();
        }
    }
}

/// Embedders for the tests of this crate's translate tools.
#[cfg(test)]
pub(crate) mod test_embed {
    use std::collections::HashMap;

    use bookrack_embed::{EmbedError, Embedder};

    /// Answers each text with the vector a table assigns it, or with
    /// one constant vector for every text.
    pub(crate) struct Fake {
        table: HashMap<String, Vec<f32>>,
        constant: Option<Vec<f32>>,
    }

    impl Fake {
        pub(crate) fn table(entries: &[(&str, [f32; 4])]) -> Fake {
            Fake {
                table: entries
                    .iter()
                    .map(|(text, vector)| ((*text).to_owned(), vector.to_vec()))
                    .collect(),
                constant: None,
            }
        }

        pub(crate) fn constant(vector: [f32; 4]) -> Fake {
            Fake {
                table: HashMap::new(),
                constant: Some(vector.to_vec()),
            }
        }
    }

    impl Embedder for Fake {
        async fn embed_batch(&self, texts: &[String]) -> bookrack_embed::Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|text| {
                    self.table
                        .get(text)
                        .cloned()
                        .or_else(|| self.constant.clone())
                        .unwrap_or_else(|| panic!("no fake vector for {text:?}"))
                })
                .collect())
        }
    }

    /// An embedder whose backend is down.
    pub(crate) struct Down;

    impl Embedder for Down {
        async fn embed_batch(&self, _texts: &[String]) -> bookrack_embed::Result<Vec<Vec<f32>>> {
            Err(EmbedError::Unreachable("connection refused".to_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_embed::{Down, Fake};
    use super::*;
    use bookrack_catalog::NewPublicationAttrs;

    const DIM: usize = 4;

    fn ctx<'a, E: Embedder>(dir: &std::path::Path, embedder: &'a E) -> TmContext<'a, E> {
        TmContext {
            lancedb_dir: dir.join("lancedb_translate"),
            embedder,
            embed_model: "fake-model",
            dim: DIM,
        }
    }

    fn hit(segment_id: i64, source_kind: &str, distance: f32) -> TmHitRow {
        TmHitRow {
            row: TmRow {
                vector: Vec::new(),
                segment_id,
                unit_id: 1,
                intake_id: 1,
                target_lang: "zh".into(),
                source_lang: "de".into(),
                start_node_id: 100,
                start_char_offset: 0,
                end_node_id: 100,
                end_char_offset: 10,
                src_text: format!("source {segment_id}"),
                dst_text: format!("target {segment_id}"),
                source_kind: source_kind.into(),
                sealed_at: "2026-01-01T00:00:00Z".into(),
            },
            distance,
        }
    }

    fn row(segment_id: i64, source_kind: &str, vector: [f32; 4]) -> TmRow {
        let mut row = hit(segment_id, source_kind, 0.0).row;
        row.vector = vector.to_vec();
        row
    }

    fn query(text: &str) -> TmQuery {
        TmQuery {
            text: text.into(),
            target_lang: "zh".into(),
            source_lang: "de".into(),
            intake_id: None,
            exclude: None,
        }
    }

    #[test]
    fn hits_are_ranked_by_weighted_similarity_not_raw_distance() {
        // The machine draft is nearer, but a person's rendering at
        // slightly more distance outranks it once weighted.
        let ranked = weigh_and_rank(
            vec![
                hit(1, "llm-draft", 0.0),
                hit(2, "human", 0.1),
                hit(3, "imported", 0.0),
                hit(4, "edited", 0.1),
            ],
            10,
            None,
        );
        let order: Vec<(i64, f64)> = ranked.iter().map(|h| (h.segment_id, h.score)).collect();
        assert_eq!(order[0].0, 2, "{order:?}");
        assert!((order[0].1 - 0.9).abs() < 1e-6, "{order:?}");
        assert_eq!(order[1].0, 4, "{order:?}");
        assert!((order[1].1 - 0.9 * 0.95).abs() < 1e-6, "{order:?}");
        assert_eq!(order[2].0, 1, "{order:?}");
        assert!((order[2].1 - 0.8).abs() < 1e-6, "{order:?}");
        assert_eq!(order[3].0, 3, "{order:?}");
        assert!((order[3].1 - 0.7).abs() < 1e-6, "{order:?}");
        assert_eq!(ranked[0].span.end_char_offset, 10);
        assert_eq!(ranked[0].dst_text, "target 2");
    }

    #[test]
    fn ranking_drops_the_excluded_segment_floors_at_zero_and_truncates() {
        let ranked = weigh_and_rank(
            vec![
                hit(1, "human", 0.0),
                hit(2, "human", 1.5),
                hit(3, "human", 0.2),
                hit(4, "human", 0.1),
            ],
            2,
            Some(1),
        );
        let ids: Vec<i64> = ranked.iter().map(|h| h.segment_id).collect();
        assert_eq!(ids, vec![4, 3]);
        let far = weigh_and_rank(vec![hit(2, "human", 1.5)], 1, None);
        assert_eq!(far[0].score, 0.0, "a distance past one is not negative");
    }

    #[test]
    fn stamps_are_checked_key_by_key_and_absent_ones_are_unstamped() {
        let expected = TmStamps {
            schema_version: TM_SCHEMA_VERSION,
            embed_model: "fake-model".into(),
            vector_dim: 4,
        };
        assert!(matches!(
            check_stamps(None, &expected),
            Err(TranslateError::TmUnstamped)
        ));
        assert!(check_stamps(Some(&expected), &expected).is_ok());
        let other = TmStamps {
            embed_model: "other-model".into(),
            ..expected.clone()
        };
        assert!(matches!(
            check_stamps(Some(&other), &expected),
            Err(TranslateError::TmStampMismatch {
                key: "tm_embed_model",
                ..
            })
        ));
    }

    #[test]
    fn the_source_language_comes_from_the_catalog_lowered_or_is_und() {
        let catalog = Catalog::open_in_memory().expect("catalog");
        for (intake_id, language) in [(1, Some(" DE ")), (2, None), (3, Some("de;en"))] {
            let mut attrs = NewPublicationAttrs::new(intake_id, ItemKind::Book);
            attrs.language = language.map(str::to_owned);
            catalog.upsert_publication_attrs(&attrs).expect("attrs");
        }
        assert_eq!(source_lang_of(Some(&catalog), 1).expect("lang"), "de");
        assert_eq!(source_lang_of(Some(&catalog), 2).expect("lang"), "und");
        assert_eq!(source_lang_of(Some(&catalog), 3).expect("lang"), "und");
        assert_eq!(source_lang_of(Some(&catalog), 9).expect("lang"), "und");
        assert_eq!(source_lang_of(None, 1).expect("lang"), "und");
    }

    #[tokio::test]
    async fn a_library_without_a_memory_answers_none_without_creating_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = Fake::constant([1.0, 0.0, 0.0, 0.0]);
        let tm = ctx(dir.path(), &fake);
        let got = search_memory(&tm, None, &query("Erster Absatz."), 5)
            .await
            .expect("search");
        assert_eq!(got, None);
        assert!(!tm.lancedb_dir.exists());
    }

    #[tokio::test]
    async fn a_bad_language_tag_is_refused_even_when_no_memory_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = Fake::constant([1.0, 0.0, 0.0, 0.0]);
        let tm = ctx(dir.path(), &fake);
        let mut bad = query("Erster Absatz.");
        bad.target_lang = "zh'; DROP TABLE".into();
        let err = search_memory(&tm, None, &bad, 5)
            .await
            .expect_err("refused");
        assert!(
            matches!(
                err,
                TranslateToolError::Vectors(VectorsError::BadFilter("target_lang"))
            ),
            "{err:?}"
        );
        assert!(!tm.lancedb_dir.exists());
    }

    #[tokio::test]
    async fn a_memory_with_rows_is_searched_only_under_this_builds_stamps() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = Fake::table(&[("Erster Absatz.", [1.0, 0.0, 0.0, 0.0])]);
        let tm = ctx(dir.path(), &fake);
        let store = TmStore::open(&tm.lancedb_dir, DIM).await.expect("open");
        store
            .append(&[
                row(1, "human", [1.0, 0.0, 0.0, 0.0]),
                row(2, "llm-draft", [0.9, 0.1, 0.0, 0.0]),
            ])
            .await
            .expect("append");
        drop(store);

        let err = search_memory(&tm, None, &query("Erster Absatz."), 5)
            .await
            .expect_err("unstamped");
        assert!(
            matches!(
                err,
                TranslateToolError::Translate(TranslateError::TmUnstamped)
            ),
            "{err:?}"
        );

        let other = TmStamps {
            embed_model: "other-model".into(),
            ..tm.expected_stamps()
        };
        let err = search_memory(&tm, Some(&other), &query("Erster Absatz."), 5)
            .await
            .expect_err("mismatch");
        assert!(
            matches!(
                err,
                TranslateToolError::Translate(TranslateError::TmStampMismatch { .. })
            ),
            "{err:?}"
        );

        let hits = search_memory(
            &tm,
            Some(&tm.expected_stamps()),
            &query("Erster Absatz."),
            5,
        )
        .await
        .expect("search")
        .expect("memory exists");
        let ids: Vec<i64> = hits.iter().map(|h| h.segment_id).collect();
        assert_eq!(ids, vec![1, 2]);
        assert!(hits[0].score > hits[1].score);

        let mut scoped = query("Erster Absatz.");
        scoped.exclude = Some(1);
        let hits = search_memory(&tm, Some(&tm.expected_stamps()), &scoped, 5)
            .await
            .expect("search")
            .expect("memory exists");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].segment_id, 2);
    }

    #[tokio::test]
    async fn an_unreachable_embedder_is_an_embed_error_only_once_the_memory_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let down = Down;
        let tm = ctx(dir.path(), &down);
        assert_eq!(
            search_memory(&tm, None, &query("x"), 5)
                .await
                .expect("none"),
            None
        );
        TmStore::open(&tm.lancedb_dir, DIM).await.expect("open");
        let err = search_memory(&tm, Some(&tm.expected_stamps()), &query("x"), 5)
            .await
            .expect_err("down");
        assert!(matches!(err, TranslateToolError::Embed(_)), "{err:?}");
    }
}
