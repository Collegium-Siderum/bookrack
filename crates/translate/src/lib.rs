// SPDX-License-Identifier: Apache-2.0

//! The translation working store.
//!
//! `translate.db` holds the state of translation work over books in the
//! library: immutable units mirroring corpus structure, mutable
//! sentence-level segments, the two-layer glossary, the witness-text
//! anchoring, and the append-only audit trail. It is a source of truth
//! — sealed translations and glossary decisions are not derivable from
//! any other store — and it lives beside the other databases under one
//! library data root.
//!
//! ## URI semantics
//!
//! `refs://<book_slug>#<entry_key>` values (the `authority_ref` column)
//! are **library-relative** identifiers: `reference.db` and
//! `translate.db` hang off the same data root, and the URI names an
//! entry of that library's reference store. It must not be treated as a
//! global identifier when data moves between libraries. Parsing splits
//! at the first `#`.
//!
//! ## Soft-reference boundary
//!
//! `intake_id` and `witness_intake_id` point into `catalog.db`;
//! `node_id` and the segment span endpoints point into `corpus.db`.
//! All of them are bare integers, never foreign keys: a re-ingest does
//! not cascade here. Units re-anchor through the
//! `translate_units.source_outline` snapshot, and segments relocate by
//! content through `translate_segments.source_text_sha`; see those
//! columns' comments.
//!
//! ## State machine
//!
//! A segment's lifecycle is `draft -> proposed -> sealed`, carried by
//! `translate_segments.status`. The `translate_audit` table is a
//! recording of that machine, not the machine itself.

use std::path::Path;

use rusqlite::Connection;

use bookrack_dbkit::{OpenDecision, READER_VERSION, TableSpec, reader_version_decision};

pub mod audit;
pub mod glossary_terms;
pub mod glossary_translations;
pub mod meta;
pub mod migrate;
pub mod segments;
pub mod units;
pub mod witnesses;

pub use migrate::TARGET_VERSION;

/// The schema revision this build writes, mirrored into
/// `translate_meta` for audit. `user_version`, not this, decides
/// whether a database can be opened.
pub const SCHEMA_VERSION: u32 = TARGET_VERSION as u32;

/// `translate_meta` key under which [`SCHEMA_VERSION`] is mirrored.
const SCHEMA_VERSION_KEY: &str = "schema_version";

/// The minimum reader version a binary must speak to interpret this
/// store's data, stamped into `translate_meta` at open.
pub const MIN_READER_VERSION: u32 = 1;

/// `translate_meta` key under which [`MIN_READER_VERSION`] is recorded.
const MIN_READER_VERSION_KEY: &str = "min_reader_version";

/// Every table spec, in dependency order: referenced tables precede
/// their referrers so the rendered baseline creates them first.
const SPECS: &[&TableSpec] = &[
    &units::SPEC,
    &segments::SPEC,
    &glossary_terms::SPEC,
    &glossary_translations::SPEC,
    &audit::SPEC,
    &witnesses::SPEC,
    &meta::SPEC,
];

/// Errors from opening, migrating, or querying `translate.db`.
#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    /// The underlying SQLite layer reported an error.
    #[error("translate database error")]
    Sqlite(#[from] rusqlite::Error),

    /// The database was written by a newer schema revision than this
    /// binary understands: its `user_version` exceeds the highest
    /// migration defined. Rather than downgrade it, opening fails so
    /// the operator can run a newer build.
    #[error(
        "translate schema is newer than this build: database is at v{found}, \
         this build understands up to v{expected}"
    )]
    SchemaTooNew {
        /// `user_version` recorded in the opened database.
        found: i64,
        /// Highest schema version this binary defines.
        expected: i64,
    },

    /// The database carries a `min_reader_version` stamp this binary
    /// cannot meet.
    #[error(
        "translate store requires reader version {required}, \
         this build reads up to {current}"
    )]
    ReaderTooOld {
        /// The minimum reader version the writer stamped.
        required: u32,
        /// The reader version this binary speaks.
        current: u32,
    },

    /// The schema migration failed.
    #[error("translate schema migration failed")]
    Migrate(#[source] rusqlite_migration::Error),

    /// The live schema does not match the table specs.
    #[error("translate schema verification failed")]
    Verify(#[source] bookrack_dbkit::VerifyError),
}

/// The crate's `Result` alias.
pub type TranslateResult<T> = Result<T, TranslateError>;

/// The translation-store handle.
pub struct Translate {
    conn: Connection,
}

impl Translate {
    /// Open `translate.db` at `path`, bringing the schema to
    /// [`TARGET_VERSION`].
    pub fn open(path: &Path) -> TranslateResult<Translate> {
        Translate::from_connection(bookrack_dbkit::open_production(path)?)
    }

    /// Open an ephemeral `translate.db` held entirely in memory. The
    /// database vanishes when the handle is dropped.
    pub fn open_in_memory() -> TranslateResult<Translate> {
        Translate::from_connection(Connection::open_in_memory()?)
    }

    /// Migrate the schema to the current revision and return a handle.
    ///
    /// Three stages: `user_version` decides whether to refuse, migrate
    /// forward, or open unchanged; `verify_all` checks the live schema
    /// against the table specs; the reader-version stamp is enforced
    /// and seeded, and the schema version mirrored into
    /// `translate_meta`. There is no rederive branch — this store is a
    /// source of truth — and no backup hook yet: at
    /// [`TARGET_VERSION`] 1 the only migration is the `0 -> 1` create,
    /// with no pre-existing data to save.
    fn from_connection(mut conn: Connection) -> TranslateResult<Translate> {
        let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match decide(current) {
            OpenDecision::Refuse { .. } => {
                return Err(TranslateError::SchemaTooNew {
                    found: current,
                    expected: TARGET_VERSION,
                });
            }
            OpenDecision::Migrate { .. } => {
                // Foreign keys are toggled around the migration, not
                // inside it: a future table rebuild needs them off, and
                // `PRAGMA foreign_keys` is a no-op within the
                // migration's transaction.
                conn.pragma_update(None, "foreign_keys", "OFF")?;
                migrate::migrations()
                    .to_latest(&mut conn)
                    .map_err(TranslateError::Migrate)?;
                conn.pragma_update(None, "foreign_keys", "ON")?;
            }
            OpenDecision::Match => {}
            OpenDecision::Rederive { .. } => {
                unreachable!("translate.db is a source of truth and never rederived")
            }
        }

        // Acceptance gate, run on every open: `rusqlite_migration`
        // advances `user_version` but does not check the resulting
        // schema shape.
        bookrack_dbkit::verify_all(&conn, SPECS).map_err(TranslateError::Verify)?;

        let translate = Translate { conn };

        // Reader-version axis: refuse a stamp this build cannot meet,
        // seed the stamp when missing. Runs after `verify_all` so the
        // `translate_meta` table is guaranteed to exist.
        let stored = translate
            .meta_get(MIN_READER_VERSION_KEY)?
            .and_then(|s| s.parse::<u32>().ok());
        match reader_version_decision(stored) {
            OpenDecision::Refuse { .. } => {
                return Err(TranslateError::ReaderTooOld {
                    required: stored.expect("Refuse implies a stamp was present"),
                    current: READER_VERSION,
                });
            }
            OpenDecision::Match => {
                if stored.is_none() {
                    translate.meta_set(MIN_READER_VERSION_KEY, &MIN_READER_VERSION.to_string())?;
                }
            }
            OpenDecision::Migrate { .. } | OpenDecision::Rederive { .. } => {
                unreachable!("reader_version_decision emits only Match or Refuse")
            }
        }

        // Mirror the authoritative version into `translate_meta` for
        // audit.
        translate.meta_set(SCHEMA_VERSION_KEY, &SCHEMA_VERSION.to_string())?;
        Ok(translate)
    }
}

/// Reduce the on-disk `user_version` to an open-time verdict.
///
/// `translate.db` is source of truth, so its decision tree is the
/// minimal one: anything newer is refused, anything older is migrated
/// forward, an exact match opens unchanged. The rederive verdict is
/// never produced — there is no derived-stamp axis to disagree on.
fn decide(current: i64) -> OpenDecision {
    use std::cmp::Ordering;
    match current.cmp(&TARGET_VERSION) {
        Ordering::Greater => OpenDecision::Refuse {
            reason: "translate schema version newer than this binary",
        },
        Ordering::Less => OpenDecision::Migrate {
            from: current,
            to: TARGET_VERSION,
        },
        Ordering::Equal => OpenDecision::Match,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Translate {
        Translate::open_in_memory().expect("open in-memory translate")
    }

    fn insert_unit(t: &Translate, intake_id: i64, node_id: i64) -> i64 {
        t.conn
            .query_row(
                "INSERT INTO translate_units (intake_id, target_lang, node_id, unit_order) \
                 VALUES (?1, 'zh', ?2, 0) RETURNING unit_id",
                rusqlite::params![intake_id, node_id],
                |row| row.get(0),
            )
            .expect("insert unit")
    }

    #[test]
    fn fresh_open_reaches_target_version_and_seeds_meta() {
        let t = fresh();
        let version: i64 = t
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, TARGET_VERSION);

        assert_eq!(
            t.meta_get(SCHEMA_VERSION_KEY).expect("read"),
            Some(SCHEMA_VERSION.to_string())
        );
        assert_eq!(
            t.meta_get(MIN_READER_VERSION_KEY).expect("read"),
            Some(MIN_READER_VERSION.to_string())
        );
    }

    #[test]
    fn reopening_an_existing_database_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("translate.db");
        drop(Translate::open(&path).expect("first open"));
        drop(Translate::open(&path).expect("second open"));
    }

    #[test]
    fn a_newer_schema_version_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("translate.db");
        {
            let t = Translate::open(&path).expect("first open");
            t.conn
                .pragma_update(None, "user_version", 99)
                .expect("bump user_version");
        }
        let Err(err) = Translate::open(&path) else {
            panic!("must refuse a newer database");
        };
        assert!(
            matches!(
                err,
                TranslateError::SchemaTooNew {
                    found: 99,
                    expected: TARGET_VERSION
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_reader_version_stamp_above_this_build_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("translate.db");
        let too_new = READER_VERSION + 1;
        {
            let t = Translate::open(&path).expect("first open");
            t.meta_set(MIN_READER_VERSION_KEY, &too_new.to_string())
                .expect("raise the stamp");
        }
        let Err(err) = Translate::open(&path) else {
            panic!("must refuse a too-new reader stamp");
        };
        assert!(
            matches!(err, TranslateError::ReaderTooOld { required, current }
                if required == too_new && current == READER_VERSION),
            "{err:?}"
        );
    }

    #[test]
    fn duplicate_unit_identity_violates_the_unique_constraint() {
        let t = fresh();
        insert_unit(&t, 1, 10);
        let err = t
            .conn
            .execute(
                "INSERT INTO translate_units (intake_id, target_lang, node_id, unit_order) \
                 VALUES (1, 'zh', 10, 1)",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("UNIQUE"), "{err}");
    }

    #[test]
    fn closed_set_columns_reject_values_outside_their_check() {
        let t = fresh();
        let unit_id = insert_unit(&t, 1, 10);

        let bogus_status = t
            .conn
            .execute(
                "INSERT INTO translate_segments (unit_id, start_node_id, start_char_offset, \
                 end_node_id, end_char_offset, source_text_sha, status) \
                 VALUES (?1, 10, 0, 10, 5, 'sha', 'bogus')",
                [unit_id],
            )
            .unwrap_err();
        assert!(bogus_status.to_string().contains("CHECK"), "{bogus_status}");

        let bogus_actor = t
            .conn
            .execute(
                "INSERT INTO translate_audit (action, actor_kind, changed_at) \
                 VALUES ('propose', 'bogus', '2026-07-19T00:00:00Z')",
                [],
            )
            .unwrap_err();
        assert!(bogus_actor.to_string().contains("CHECK"), "{bogus_actor}");

        let bogus_scope = t
            .conn
            .execute(
                "INSERT INTO glossary_terms (scope, source_lang, source_term, source_norm, \
                 term_kind) VALUES ('bogus', 'fr', 'objet a', 'objeta', 'term')",
                [],
            )
            .unwrap_err();
        assert!(bogus_scope.to_string().contains("CHECK"), "{bogus_scope}");
    }

    #[test]
    fn a_segment_referencing_a_missing_unit_violates_the_foreign_key() {
        let t = fresh();
        let err = t
            .conn
            .execute(
                "INSERT INTO translate_segments (unit_id, start_node_id, start_char_offset, \
                 end_node_id, end_char_offset, source_text_sha, status) \
                 VALUES (999, 10, 0, 10, 5, 'sha', 'draft')",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("FOREIGN KEY"), "{err}");
    }
}
