// SPDX-License-Identifier: Apache-2.0

//! Self-describing identity manifest for a data root.
//!
//! Every data root carries a `bookrack-library.toml` naming the
//! library, its kind, and a stable uuid. The registry is a regenerable
//! cache over these manifests: a lost registry can be rebuilt by
//! scanning roots for their manifests, so a library's identity lives
//! with its data, not only in the registry. A root without a manifest
//! stays valid — the file is confirming evidence, never a gate on use.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::LibraryKind;
use crate::write_atomically;

/// Filename of the identity manifest inside a data root.
pub const MANIFEST_FILENAME: &str = "bookrack-library.toml";

/// Value of the `format` key: the confirming evidence that a directory
/// is a bookrack data root, distinguishing it from an unrelated TOML.
pub const MANIFEST_FORMAT: &str = "bookrack-library";

/// Supported manifest schema version. A file declaring a higher version
/// is rejected rather than silently misread; a v1 reader tolerates
/// unknown keys within v1.x.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// A data root's identity, read from or written to its manifest. The
/// `format` magic is validated at the parse boundary and dropped; it is
/// not carried on the struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LibraryManifest {
    /// Schema version the file declared.
    pub format_version: u32,
    /// Stable library uuid, generated once at creation.
    pub uuid: String,
    /// Birth name of the library.
    pub name: String,
    /// Library kind.
    pub kind: LibraryKind,
    /// Free-form description; absent when never set.
    pub description: Option<String>,
    /// RFC 3339 creation timestamp; absent on manifests that omit it.
    pub created_at: Option<String>,
}

/// Reasons loading a manifest can fail.
#[derive(Debug)]
pub enum ManifestError {
    /// The file existed but could not be read.
    Io {
        path: PathBuf,
        error: std::io::Error,
    },
    /// The file was read but did not parse as the expected schema.
    Parse {
        path: PathBuf,
        error: toml::de::Error,
    },
    /// The file's `format_version` was above the supported value.
    SchemaVersion { path: PathBuf, found: u32 },
    /// The file parsed but its `format` magic did not match, so the
    /// directory is not a bookrack data root.
    NotALibrary { path: PathBuf },
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, error } => {
                write!(f, "failed to read {}: {error}", path.display())
            }
            Self::Parse { path, error } => {
                write!(f, "failed to parse {}: {error}", path.display())
            }
            Self::SchemaVersion { path, found } => write!(
                f,
                "unsupported format_version {found} in {} (expected {MANIFEST_SCHEMA_VERSION})",
                path.display()
            ),
            Self::NotALibrary { path } => write!(
                f,
                "{} is not a bookrack library manifest (missing `format = \"{MANIFEST_FORMAT}\"`)",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

/// Wire form of the manifest. The `format` magic is checked on the raw
/// table before this is deserialized, so it is not a field here; unknown
/// keys are tolerated so a v1 reader survives a v1.x file.
#[derive(Deserialize)]
struct ManifestFile {
    format_version: u32,
    uuid: String,
    name: String,
    #[serde(default)]
    kind: LibraryKind,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

/// Load the identity manifest from `data_dir`.
///
/// A missing file resolves to `Ok(None)` — a root without a manifest is
/// permanently legal. A file whose `format` magic does not match yields
/// [`ManifestError::NotALibrary`]; a `format_version` above
/// [`MANIFEST_SCHEMA_VERSION`] yields [`ManifestError::SchemaVersion`].
pub fn load_manifest(data_dir: &Path) -> Result<Option<LibraryManifest>, ManifestError> {
    let path = data_dir.join(MANIFEST_FILENAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ManifestError::Io { path, error }),
    };
    // Check the magic before full deserialization so a foreign TOML that
    // merely lacks our required fields is reported as NotALibrary, not as
    // a missing-field parse error.
    let table: toml::Table = toml::from_str(&text).map_err(|error| ManifestError::Parse {
        path: path.clone(),
        error,
    })?;
    if table.get("format").and_then(toml::Value::as_str) != Some(MANIFEST_FORMAT) {
        return Err(ManifestError::NotALibrary { path });
    }
    let file: ManifestFile =
        toml::Value::Table(table)
            .try_into()
            .map_err(|error| ManifestError::Parse {
                path: path.clone(),
                error,
            })?;
    if file.format_version > MANIFEST_SCHEMA_VERSION {
        return Err(ManifestError::SchemaVersion {
            path,
            found: file.format_version,
        });
    }
    Ok(Some(LibraryManifest {
        format_version: file.format_version,
        uuid: file.uuid,
        name: file.name,
        kind: file.kind,
        description: file.description,
        created_at: file.created_at,
    }))
}

/// Build a fresh manifest for a newly created library: a UUIDv7 (time
/// ordered, carrying a coarse creation instant) and an RFC 3339
/// `created_at`, at the current schema version.
pub fn new_manifest(
    name: impl Into<String>,
    kind: LibraryKind,
    description: Option<String>,
) -> LibraryManifest {
    LibraryManifest {
        format_version: MANIFEST_SCHEMA_VERSION,
        uuid: uuid::Uuid::now_v7().to_string(),
        name: name.into(),
        kind,
        description,
        created_at: Some(chrono::Utc::now().to_rfc3339()),
    }
}

/// Write `m` to the manifest inside `data_dir`, atomically. The file
/// carries the `format` magic and a single leading comment line.
pub fn write_manifest(data_dir: &Path, m: &LibraryManifest) -> Result<(), ManifestError> {
    let path = data_dir.join(MANIFEST_FILENAME);
    let body = render_manifest_toml(m);
    write_atomically(&path, &body).map_err(|error| ManifestError::Io { path, error })
}

/// Render a manifest into TOML, including the `format` magic and
/// `format_version`, and only the optional fields that are set.
fn render_manifest_toml(m: &LibraryManifest) -> String {
    let mut table = toml::Table::new();
    table.insert(
        "format".to_string(),
        toml::Value::String(MANIFEST_FORMAT.to_string()),
    );
    table.insert(
        "format_version".to_string(),
        toml::Value::Integer(i64::from(m.format_version)),
    );
    table.insert("uuid".to_string(), toml::Value::String(m.uuid.clone()));
    table.insert("name".to_string(), toml::Value::String(m.name.clone()));
    table.insert(
        "kind".to_string(),
        toml::Value::String(m.kind.as_str().to_string()),
    );
    for (key, value) in [
        ("description", m.description.as_ref()),
        ("created_at", m.created_at.as_ref()),
    ] {
        if let Some(v) = value {
            table.insert(key.to_string(), toml::Value::String(v.clone()));
        }
    }
    let body = toml::to_string(&table).expect("toml::Table is always serialisable");
    format!("# bookrack library identity. Written by `bookrack init`; do not edit.\n{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> LibraryManifest {
        LibraryManifest {
            format_version: MANIFEST_SCHEMA_VERSION,
            uuid: "01890a5d-0000-7000-8000-000000000000".to_string(),
            name: "prod-main".to_string(),
            kind: LibraryKind::Prod,
            description: Some("primary production library".to_string()),
            created_at: Some("2026-06-30T12:00:00Z".to_string()),
        }
    }

    #[test]
    fn round_trips_through_the_filesystem() {
        let dir = tempfile::tempdir().expect("tempdir");
        let m = sample();
        write_manifest(dir.path(), &m).expect("write");
        let loaded = load_manifest(dir.path()).expect("load").expect("present");
        assert_eq!(loaded, m);
    }

    #[test]
    fn written_file_carries_the_magic_and_comment() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_manifest(dir.path(), &sample()).expect("write");
        let text = std::fs::read_to_string(dir.path().join(MANIFEST_FILENAME)).expect("read");
        assert!(text.starts_with("# bookrack library identity"));
        assert!(text.contains("format = \"bookrack-library\""));
    }

    #[test]
    fn missing_file_is_ok_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load_manifest(dir.path()).expect("load").is_none());
    }

    #[test]
    fn optional_fields_absent_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let m = LibraryManifest {
            description: None,
            created_at: None,
            ..sample()
        };
        write_manifest(dir.path(), &m).expect("write");
        let text = std::fs::read_to_string(dir.path().join(MANIFEST_FILENAME)).expect("read");
        assert!(!text.contains("description"));
        assert!(!text.contains("created_at"));
        assert_eq!(
            load_manifest(dir.path()).expect("load").expect("present"),
            m
        );
    }

    #[test]
    fn kind_defaults_to_prod_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "format = \"bookrack-library\"\n\
             format_version = 1\n\
             uuid = \"u\"\n\
             name = \"n\"\n",
        )
        .expect("seed");
        let m = load_manifest(dir.path()).expect("load").expect("present");
        assert_eq!(m.kind, LibraryKind::Prod);
    }

    #[test]
    fn wrong_magic_is_not_a_library() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "format = \"something-else\"\nkey = \"value\"\n",
        )
        .expect("seed");
        assert!(matches!(
            load_manifest(dir.path()),
            Err(ManifestError::NotALibrary { .. })
        ));
    }

    #[test]
    fn future_schema_version_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "format = \"bookrack-library\"\n\
             format_version = 2\n\
             uuid = \"u\"\n\
             name = \"n\"\n",
        )
        .expect("seed");
        assert!(matches!(
            load_manifest(dir.path()),
            Err(ManifestError::SchemaVersion { found: 2, .. })
        ));
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "format = \"bookrack-library\"\n\
             format_version = 1\n\
             uuid = \"u\"\n\
             name = \"n\"\n\
             future_key = \"ignored\"\n",
        )
        .expect("seed");
        let m = load_manifest(dir.path()).expect("load").expect("present");
        assert_eq!(m.name, "n");
    }

    #[test]
    fn new_manifest_generates_a_v7_uuid_and_timestamp() {
        let m = new_manifest("lib", LibraryKind::Test, None);
        assert_eq!(m.format_version, MANIFEST_SCHEMA_VERSION);
        assert_eq!(m.kind, LibraryKind::Test);
        assert!(m.created_at.is_some());
        let parsed = uuid::Uuid::parse_str(&m.uuid).expect("valid uuid");
        assert_eq!(parsed.get_version_num(), 7);
    }
}
