// SPDX-License-Identifier: Apache-2.0

//! Opaque-intake-store envelope for an [`Extraction`].
//!
//! `bookrack ingest` writes one envelope per intake into
//! `<data_root>/books/<intake_id>.bookrack-extraction.json` as a cache
//! that lets `bookrack corpus rebuild` reproduce the post-EXTRACT
//! state without re-reading the original source file or re-running
//! any adapter.
//!
//! Format is versioned with an explicit `schema_version` field;
//! [`read_envelope`] is fail-closed on any mismatch — a future v2
//! schema will pick a different filename rather than reuse v1.

use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::Path;

use bookrack_extract::Extraction;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::embed_run::now_rfc3339;

/// The current envelope schema version. v2 changed
/// `extraction.provenance.extractor_version` from a per-adapter string
/// to the integer `bookrack_extract::EXTRACTOR_VERSION`.
pub const ENVELOPE_SCHEMA_VERSION: u32 = 2;

/// Default file extension. The v2 envelope picks a distinct extension
/// so an old v1 file on disk is never read as v2; the reader still
/// matches the file at this path against [`ENVELOPE_SCHEMA_VERSION`]
/// and fails closed on a mismatch.
pub const ENVELOPE_FILE_SUFFIX: &str = ".bookrack-extraction.v2.json";

/// Computed filename within the opaque store for one intake.
pub fn envelope_filename(intake_id: i64) -> String {
    format!("{intake_id}{ENVELOPE_FILE_SUFFIX}")
}

/// On-disk schema. The `extraction` payload is the value
/// [`bookrack_corpus::rebuild`] feeds into STRUCTURE + CHUNK.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionEnvelope {
    pub schema_version: u32,
    pub intake_id: i64,
    pub source_sha256: String,
    pub captured_at: String,
    pub extraction: Extraction,
}

#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("I/O error")]
    Io(#[from] io::Error),
    #[error("JSON error")]
    Json(#[from] serde_json::Error),
    #[error("envelope schema_version mismatch: expected {expected}, found {found}")]
    SchemaMismatch { expected: u32, found: u32 },
}

/// Serialize `extraction` into an envelope at `path` via tempfile +
/// atomic rename so a partial write never leaves a corrupt file in
/// the opaque store.
pub fn write_envelope(
    path: &Path,
    extraction: &Extraction,
    intake_id: i64,
    source_sha256: &str,
) -> Result<(), EnvelopeError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let envelope = ExtractionEnvelope {
        schema_version: ENVELOPE_SCHEMA_VERSION,
        intake_id,
        source_sha256: source_sha256.to_owned(),
        captured_at: now_rfc3339(),
        extraction: extraction.clone(),
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(parent)?;
    serde_json::to_writer(tmp.as_file_mut(), &envelope)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| EnvelopeError::Io(e.error))?;
    Ok(())
}

/// Parse the envelope at `path`. Returns [`EnvelopeError::SchemaMismatch`]
/// if the file's `schema_version` differs from [`ENVELOPE_SCHEMA_VERSION`].
pub fn read_envelope(path: &Path) -> Result<ExtractionEnvelope, EnvelopeError> {
    let file = File::open(path)?;
    let envelope: ExtractionEnvelope = serde_json::from_reader(BufReader::new(file))?;
    if envelope.schema_version != ENVELOPE_SCHEMA_VERSION {
        return Err(EnvelopeError::SchemaMismatch {
            expected: ENVELOPE_SCHEMA_VERSION,
            found: envelope.schema_version,
        });
    }
    Ok(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_extract::{
        Biblio, Block, BlockKind, Extraction, Provenance, TextLayerQuality, Toc,
    };
    use tempfile::tempdir;

    fn sample_extraction() -> Extraction {
        Extraction {
            blocks: vec![Block {
                kind: BlockKind::Body,
                text: "sample prose".into(),
                source_unit: 0,
            }],
            toc: Toc::default(),
            biblio: Biblio::default(),
            provenance: Provenance {
                adapter: "txt".into(),
                extractor_version: 1,
                text_layer_quality: TextLayerQuality::BornDigital,
                skipped_units: vec![],
                derived_from_sha256: None,
                partial_pages: None,
            },
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join(envelope_filename(42));
        let original = sample_extraction();
        write_envelope(&path, &original, 42, "deadbeef").expect("write");
        let parsed = read_envelope(&path).expect("read");
        assert_eq!(parsed.schema_version, ENVELOPE_SCHEMA_VERSION);
        assert_eq!(parsed.intake_id, 42);
        assert_eq!(parsed.source_sha256, "deadbeef");
        assert!(!parsed.captured_at.is_empty());
        assert_eq!(parsed.extraction, original);
    }

    #[test]
    fn schema_mismatch_is_rejected() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("forged.json");
        fs::write(
            &path,
            r#"{
              "schema_version": 99,
              "intake_id": 1,
              "source_sha256": "abc",
              "captured_at": "2026-01-01T00:00:00Z",
              "extraction": {
                "blocks": [],
                "toc": { "entries": [] },
                "biblio": { "contributors": [] },
                "provenance": {
                  "adapter": "txt",
                  "extractor_version": 1,
                  "text_layer_quality": "born_digital",
                  "skipped_units": []
                }
              }
            }"#,
        )
        .expect("write forged");
        match read_envelope(&path) {
            Err(EnvelopeError::SchemaMismatch { expected, found }) => {
                assert_eq!(expected, ENVELOPE_SCHEMA_VERSION);
                assert_eq!(found, 99);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn legacy_envelope_without_new_provenance_fields_reads_with_serde_default() {
        // A v2 envelope written before `derived_from_sha256` and
        // `partial_pages` existed on `Provenance`. Its `provenance`
        // object omits both fields; `#[serde(default)]` on the new
        // fields keeps this binary's reader from rejecting it, so
        // earlier envelopes on disk stay readable without an envelope
        // schema bump.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("legacy.json");
        fs::write(
            &path,
            format!(
                r#"{{
                  "schema_version": {ENVELOPE_SCHEMA_VERSION},
                  "intake_id": 7,
                  "source_sha256": "legacy-sha",
                  "captured_at": "2026-01-01T00:00:00Z",
                  "extraction": {{
                    "blocks": [],
                    "toc": {{ "entries": [] }},
                    "biblio": {{ "contributors": [] }},
                    "provenance": {{
                      "adapter": "txt",
                      "extractor_version": 1,
                      "text_layer_quality": "born_digital",
                      "skipped_units": []
                    }}
                  }}
                }}"#
            ),
        )
        .expect("write legacy envelope");

        let env = read_envelope(&path).expect("read legacy envelope");
        assert_eq!(env.schema_version, ENVELOPE_SCHEMA_VERSION);
        assert_eq!(env.intake_id, 7);
        assert_eq!(env.source_sha256, "legacy-sha");
        assert_eq!(env.extraction.provenance.derived_from_sha256, None);
        assert_eq!(env.extraction.provenance.partial_pages, None);
    }

    #[test]
    fn corrupt_json_is_reported_as_json_error() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("corrupt.json");
        fs::write(&path, b"{not valid json").expect("write corrupt");
        match read_envelope(&path) {
            Err(EnvelopeError::Json(_)) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn missing_file_is_reported_as_io_error() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("nope.json");
        match read_envelope(&path) {
            Err(EnvelopeError::Io(_)) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }
}
