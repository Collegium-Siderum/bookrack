// SPDX-License-Identifier: Apache-2.0

//! The `intake` table — file-manifestation identity and registration.
//!
//! An *intake* is one ingested source file. Its `source_sha256` (the
//! whole-file hash) is the identity anchor of the entire data model:
//! one file, one intake, one book. Registration is idempotent on that
//! hash, so re-offering a file that is already known returns the
//! existing row instead of creating a duplicate.

use rusqlite::{OptionalExtension, Row};

use crate::{Catalog, Result};

/// Column list shared by every `intake` `SELECT` and `RETURNING`. Its
/// order is the contract with [`Intake::from_row`].
const INTAKE_COLUMNS: &str = "intake_id, source_sha256, stored_path, original_path, \
     format, byte_size, adapter, intake_at, status, expression_id, notes";

/// Coarse lifecycle state of an intake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntakeStatus {
    /// Registered, not yet processed.
    Pending,
    /// The file has been extracted to blocks and a TOC.
    Extracted,
    /// Held for human review of a suspected fuzzy-duplicate overlap.
    DedupHold,
    /// Fully ingested; vectors exist in the vector store.
    Embedded,
    /// Processing failed and was abandoned.
    Aborted,
}

impl IntakeStatus {
    /// Every status, in lifecycle order.
    pub const ALL: [IntakeStatus; 5] = [
        IntakeStatus::Pending,
        IntakeStatus::Extracted,
        IntakeStatus::DedupHold,
        IntakeStatus::Embedded,
        IntakeStatus::Aborted,
    ];

    /// The database string form.
    pub const fn as_str(self) -> &'static str {
        match self {
            IntakeStatus::Pending => "pending",
            IntakeStatus::Extracted => "extracted",
            IntakeStatus::DedupHold => "dedup_hold",
            IntakeStatus::Embedded => "embedded",
            IntakeStatus::Aborted => "aborted",
        }
    }

    /// Parse the database string form, or `None` if unrecognized.
    pub fn from_db_str(s: &str) -> Option<IntakeStatus> {
        IntakeStatus::ALL.into_iter().find(|st| st.as_str() == s)
    }
}

/// One `intake` row read back from `catalog.db`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intake {
    /// Long-lived surrogate key; never reused after deletion.
    pub intake_id: i64,
    /// Whole-file SHA-256 — the identity anchor.
    pub source_sha256: String,
    /// Opaque store location; `None` until the file has been stored.
    pub stored_path: Option<String>,
    /// Where the file came from, kept for forensics.
    pub original_path: Option<String>,
    /// File format (`epub`, `pdf`, ...); determined during extraction.
    pub format: Option<String>,
    /// File size in bytes.
    pub byte_size: Option<i64>,
    /// Extraction adapter chosen for this file.
    pub adapter: Option<String>,
    /// Registration time, as an ISO-8601 UTC timestamp.
    pub intake_at: String,
    /// Coarse lifecycle state.
    pub status: IntakeStatus,
    /// Soft reference to a FRBR expression; backfilled at METADATA.
    pub expression_id: Option<i64>,
    /// Free-form notes.
    pub notes: Option<String>,
}

impl Intake {
    /// Build an [`Intake`] from a row whose columns are
    /// [`INTAKE_COLUMNS`].
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Intake> {
        Ok(Intake {
            intake_id: row.get(0)?,
            source_sha256: row.get(1)?,
            stored_path: row.get(2)?,
            original_path: row.get(3)?,
            format: row.get(4)?,
            byte_size: row.get(5)?,
            adapter: row.get(6)?,
            intake_at: row.get(7)?,
            status: status_from_row(row, 8)?,
            expression_id: row.get(9)?,
            notes: row.get(10)?,
        })
    }
}

/// The fields known when a file is first registered. The opaque
/// `stored_path` is deliberately absent: it depends on the
/// autoincremented `intake_id`, so it is filled in afterwards with
/// [`Catalog::set_stored_path`].
#[derive(Debug, Clone)]
pub struct NewIntake {
    source_sha256: String,
    original_path: Option<String>,
    format: Option<String>,
    byte_size: Option<i64>,
}

impl NewIntake {
    /// Start from the whole-file hash — the only mandatory field.
    pub fn new(source_sha256: impl Into<String>) -> NewIntake {
        NewIntake {
            source_sha256: source_sha256.into(),
            original_path: None,
            format: None,
            byte_size: None,
        }
    }

    /// Record where the file came from, for forensics.
    pub fn original_path(mut self, original_path: impl Into<String>) -> NewIntake {
        self.original_path = Some(original_path.into());
        self
    }

    /// Record the detected file format.
    pub fn format(mut self, format: impl Into<String>) -> NewIntake {
        self.format = Some(format.into());
        self
    }

    /// Record the file size in bytes.
    pub fn byte_size(mut self, byte_size: i64) -> NewIntake {
        self.byte_size = Some(byte_size);
        self
    }
}

/// The outcome of [`Catalog::register_intake`]: registration is
/// idempotent, so a file is either freshly recorded or already known.
#[derive(Debug)]
pub enum Registration {
    /// The file was not known and a new intake row was created.
    Created(Intake),
    /// The file was already registered; the existing row is returned.
    AlreadyPresent(Intake),
}

impl Registration {
    /// The intake row, however it was obtained.
    pub fn intake(&self) -> &Intake {
        match self {
            Registration::Created(intake) | Registration::AlreadyPresent(intake) => intake,
        }
    }

    /// Consume the outcome and take the intake row.
    pub fn into_intake(self) -> Intake {
        match self {
            Registration::Created(intake) | Registration::AlreadyPresent(intake) => intake,
        }
    }

    /// Whether this call created the row (rather than finding it).
    pub fn is_new(&self) -> bool {
        matches!(self, Registration::Created(_))
    }
}

impl Catalog {
    /// Register a source file, idempotently on its `source_sha256`.
    ///
    /// If the hash is already known the existing row is returned as
    /// [`Registration::AlreadyPresent`] and nothing is written;
    /// otherwise a new row is created with status
    /// [`IntakeStatus::Pending`] and returned as
    /// [`Registration::Created`].
    pub fn register_intake(&mut self, new: &NewIntake) -> Result<Registration> {
        let tx = self.conn.transaction()?;
        let existing = tx
            .query_row(
                &format!("SELECT {INTAKE_COLUMNS} FROM intake WHERE source_sha256 = ?1"),
                [new.source_sha256.as_str()],
                Intake::from_row,
            )
            .optional()?;
        if let Some(intake) = existing {
            return Ok(Registration::AlreadyPresent(intake));
        }

        // The timestamp is generated by SQLite so the whole crate has
        // one timestamp source.
        let intake = tx.query_row(
            &format!(
                "INSERT INTO intake
                   (source_sha256, original_path, format, byte_size, intake_at, status)
                 VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), ?5)
                 RETURNING {INTAKE_COLUMNS}"
            ),
            rusqlite::params![
                new.source_sha256,
                new.original_path,
                new.format,
                new.byte_size,
                IntakeStatus::Pending.as_str(),
            ],
            Intake::from_row,
        )?;
        tx.commit()?;
        Ok(Registration::Created(intake))
    }

    /// Look up an intake by its whole-file hash.
    pub fn intake_by_sha(&self, source_sha256: &str) -> Result<Option<Intake>> {
        let intake = self
            .conn
            .query_row(
                &format!("SELECT {INTAKE_COLUMNS} FROM intake WHERE source_sha256 = ?1"),
                [source_sha256],
                Intake::from_row,
            )
            .optional()?;
        Ok(intake)
    }

    /// Look up an intake by its id.
    pub fn intake_by_id(&self, intake_id: i64) -> Result<Option<Intake>> {
        let intake = self
            .conn
            .query_row(
                &format!("SELECT {INTAKE_COLUMNS} FROM intake WHERE intake_id = ?1"),
                [intake_id],
                Intake::from_row,
            )
            .optional()?;
        Ok(intake)
    }

    /// Advance an intake's lifecycle state. Returns whether a row with
    /// that id existed.
    pub fn set_intake_status(&self, intake_id: i64, status: IntakeStatus) -> Result<bool> {
        let affected = self.conn.execute(
            "UPDATE intake SET status = ?1 WHERE intake_id = ?2",
            (status.as_str(), intake_id),
        )?;
        Ok(affected > 0)
    }

    /// Record where an intake's file was placed in the opaque store.
    /// Returns whether a row with that id existed.
    pub fn set_stored_path(&self, intake_id: i64, stored_path: &str) -> Result<bool> {
        let affected = self.conn.execute(
            "UPDATE intake SET stored_path = ?1 WHERE intake_id = ?2",
            (stored_path, intake_id),
        )?;
        Ok(affected > 0)
    }
}

/// Read a `status` cell and decode it to an [`IntakeStatus`]. An
/// unrecognized string means the database was written by something
/// other than this crate; surface it as a conversion failure.
fn status_from_row(row: &Row<'_>, idx: usize) -> rusqlite::Result<IntakeStatus> {
    let raw: String = row.get(idx)?;
    IntakeStatus::from_db_str(&raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown intake status {raw:?}"),
            )),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog::open_in_memory().expect("open")
    }

    #[test]
    fn a_new_file_registers_as_created() {
        let mut catalog = catalog();
        let reg = catalog
            .register_intake(&NewIntake::new("sha-abc"))
            .expect("register");
        assert!(reg.is_new());
        let intake = reg.intake();
        assert!(intake.intake_id > 0);
        assert_eq!(intake.source_sha256, "sha-abc");
        assert_eq!(intake.status, IntakeStatus::Pending);
        assert_eq!(intake.stored_path, None);
        assert!(!intake.intake_at.is_empty());
    }

    #[test]
    fn re_registering_the_same_file_returns_the_existing_row() {
        let mut catalog = catalog();
        let first = catalog
            .register_intake(&NewIntake::new("sha-dup"))
            .expect("register")
            .into_intake();
        let again = catalog
            .register_intake(&NewIntake::new("sha-dup"))
            .expect("re-register");
        assert!(!again.is_new(), "a known file must not create a second row");
        assert_eq!(again.intake().intake_id, first.intake_id);
    }

    #[test]
    fn optional_fields_round_trip() {
        let mut catalog = catalog();
        let new = NewIntake::new("sha-xyz")
            .original_path("incoming/book.epub")
            .format("epub")
            .byte_size(4096);
        let intake = catalog
            .register_intake(&new)
            .expect("register")
            .into_intake();
        assert_eq!(intake.original_path.as_deref(), Some("incoming/book.epub"));
        assert_eq!(intake.format.as_deref(), Some("epub"));
        assert_eq!(intake.byte_size, Some(4096));
    }

    #[test]
    fn intake_lookups_by_sha_and_id() {
        let mut catalog = catalog();
        let intake = catalog
            .register_intake(&NewIntake::new("sha-find"))
            .expect("register")
            .into_intake();

        let by_sha = catalog.intake_by_sha("sha-find").expect("lookup");
        assert_eq!(by_sha.map(|i| i.intake_id), Some(intake.intake_id));
        let by_id = catalog.intake_by_id(intake.intake_id).expect("lookup");
        assert_eq!(by_id, Some(intake));

        assert!(catalog.intake_by_sha("absent").expect("lookup").is_none());
        assert!(catalog.intake_by_id(9999).expect("lookup").is_none());
    }

    #[test]
    fn stored_path_and_status_can_be_set() {
        let mut catalog = catalog();
        let id = catalog
            .register_intake(&NewIntake::new("sha-set"))
            .expect("register")
            .intake()
            .intake_id;

        assert!(catalog.set_stored_path(id, "store/7").expect("set path"));
        assert!(
            catalog
                .set_intake_status(id, IntakeStatus::Extracted)
                .expect("set status")
        );

        let intake = catalog.intake_by_id(id).expect("lookup").expect("present");
        assert_eq!(intake.stored_path.as_deref(), Some("store/7"));
        assert_eq!(intake.status, IntakeStatus::Extracted);

        // No such intake: nothing updated.
        assert!(
            !catalog
                .set_intake_status(9999, IntakeStatus::Aborted)
                .expect("miss")
        );
        assert!(!catalog.set_stored_path(9999, "store/x").expect("miss"));
    }

    #[test]
    fn intake_status_db_strings_round_trip() {
        for status in IntakeStatus::ALL {
            assert_eq!(IntakeStatus::from_db_str(status.as_str()), Some(status));
        }
        assert_eq!(IntakeStatus::from_db_str("not_a_status"), None);
    }
}
