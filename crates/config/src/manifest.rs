// SPDX-License-Identifier: Apache-2.0

//! Self-describing identity manifest for a data root.
//!
//! Every data root carries a `bookrack-library.toml` naming the
//! library, its kind, a stable uuid, and the index profile its vectors
//! are built under. The registry is a regenerable cache over these
//! manifests: a lost registry can be rebuilt by scanning roots for their
//! manifests, so a library's identity and data contract live with its
//! data, not only in the registry. A root without a manifest stays valid
//! — the file is confirming evidence, never a gate on use.
//!
//! The manifest carries identity and the data contract — what this
//! library *is* and what its vectors *are* — and nothing else. Runtime
//! preferences and facility knobs (a `top_k`, an Ollama URL) belong to
//! `config.toml`: they describe how one machine runs the library, not
//! the library itself, and so must not travel with the data.

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
    /// Name of the index profile the library's vectors are built under.
    /// The authoritative copy of the reference: a registry entry's
    /// `index_profile` is a regenerable cache of this value. Absent when
    /// the library runs on field-level configuration alone.
    pub index_profile: Option<String>,
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
    #[serde(default)]
    index_profile: Option<String>,
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
        index_profile: file.index_profile,
    }))
}

/// Build a fresh manifest for a newly created library: a UUIDv7 (time
/// ordered, carrying a coarse creation instant) and an RFC 3339
/// `created_at`, at the current schema version. The profile reference
/// starts absent; [`set_manifest_index_profile`] is the way it is set.
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
        index_profile: None,
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
/// `format_version`, and only the optional fields that are set. Public
/// so a registration command can preview the exact file it is about to
/// write before an operator confirms.
pub fn render_manifest_toml(m: &LibraryManifest) -> String {
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
        ("index_profile", m.index_profile.as_ref()),
    ] {
        if let Some(v) = value {
            table.insert(key.to_string(), toml::Value::String(v.clone()));
        }
    }
    let body = toml::to_string(&table).expect("toml::Table is always serialisable");
    format!("# bookrack library identity. Written by `bookrack init`; do not edit.\n{body}")
}

/// Identity to stamp onto a manifest that has to be created on the
/// spot. Callers editing a library they resolved through the registry
/// pass its entry's identity, so a manifest minted for a pre-manifest
/// root agrees with the name the operator already knows it by.
#[derive(Debug, Clone, Copy)]
pub struct ManifestIdentitySeed<'a> {
    /// Registry name to record as the library's birth name.
    pub name: &'a str,
    /// Library kind.
    pub kind: LibraryKind,
    /// Free-form description; `None` leaves the field absent.
    pub description: Option<&'a str>,
}

/// Set or clear the manifest's `index_profile`, atomically, and return
/// the manifest as written.
///
/// This is the one write path for the authoritative profile reference;
/// a registry entry's copy is a cache refreshed after this returns.
///
/// A root with no manifest is handled by direction: setting a profile
/// mints one from `seed` (a fresh uuid, as `add`/`register` would),
/// because the reference has to live somewhere; clearing one is
/// `Ok(None)` and writes nothing, since there is no stored reference to
/// clear and minting an identity is not what `--unset` was asked to do.
///
/// A manifest that exists but does not load — corrupt, or from a future
/// schema — is an error rather than a root to mint over: overwriting it
/// would destroy the identity it was still carrying.
pub fn set_manifest_index_profile(
    data_dir: &Path,
    profile: Option<&str>,
    seed: ManifestIdentitySeed<'_>,
) -> Result<Option<LibraryManifest>, ManifestError> {
    let existing = load_manifest(data_dir)?;
    let mut manifest = match (existing, profile) {
        (Some(m), _) => m,
        (None, None) => return Ok(None),
        (None, Some(_)) => new_manifest(seed.name, seed.kind, seed.description.map(str::to_string)),
    };
    manifest.index_profile = profile.map(str::to_string);
    write_manifest(data_dir, &manifest)?;
    Ok(Some(manifest))
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
            index_profile: None,
        }
    }

    fn seed() -> ManifestIdentitySeed<'static> {
        ManifestIdentitySeed {
            name: "seeded",
            kind: LibraryKind::Test,
            description: Some("from the registry entry"),
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
    fn index_profile_round_trips_when_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let m = LibraryManifest {
            index_profile: Some("fast-local".to_string()),
            ..sample()
        };
        write_manifest(dir.path(), &m).expect("write");
        let text = std::fs::read_to_string(dir.path().join(MANIFEST_FILENAME)).expect("read");
        assert!(text.contains("index_profile = \"fast-local\""), "{text}");
        assert_eq!(
            load_manifest(dir.path()).expect("load").expect("present"),
            m
        );
    }

    #[test]
    fn manifest_without_index_profile_omits_the_key_and_reads_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_manifest(dir.path(), &sample()).expect("write");
        let text = std::fs::read_to_string(dir.path().join(MANIFEST_FILENAME)).expect("read");
        assert!(!text.contains("index_profile"), "{text}");
        let loaded = load_manifest(dir.path()).expect("load").expect("present");
        assert!(loaded.index_profile.is_none());
    }

    #[test]
    fn a_pre_field_manifest_reads_with_index_profile_none() {
        // A file written before the field existed must keep loading.
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
        assert_eq!(m.format_version, 1);
        assert!(m.index_profile.is_none());
    }

    #[test]
    fn set_index_profile_writes_the_reference_into_an_existing_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_manifest(dir.path(), &sample()).expect("write");
        let written = set_manifest_index_profile(dir.path(), Some("fast-local"), seed())
            .expect("set")
            .expect("manifest written");
        assert_eq!(written.index_profile.as_deref(), Some("fast-local"));
        // Identity is untouched: the seed is only for minting.
        assert_eq!(written.uuid, sample().uuid);
        assert_eq!(written.name, sample().name);
        let loaded = load_manifest(dir.path()).expect("load").expect("present");
        assert_eq!(loaded, written);
    }

    #[test]
    fn set_index_profile_to_none_clears_an_existing_reference() {
        let dir = tempfile::tempdir().expect("tempdir");
        let m = LibraryManifest {
            index_profile: Some("fast-local".to_string()),
            ..sample()
        };
        write_manifest(dir.path(), &m).expect("write");
        let written = set_manifest_index_profile(dir.path(), None, seed())
            .expect("clear")
            .expect("manifest written");
        assert!(written.index_profile.is_none());
        let text = std::fs::read_to_string(dir.path().join(MANIFEST_FILENAME)).expect("read");
        assert!(!text.contains("index_profile"), "{text}");
    }

    #[test]
    fn set_index_profile_mints_a_manifest_on_a_root_without_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let written = set_manifest_index_profile(dir.path(), Some("fast-local"), seed())
            .expect("set")
            .expect("manifest minted");
        assert_eq!(written.name, "seeded");
        assert_eq!(written.kind, LibraryKind::Test);
        assert_eq!(
            written.description.as_deref(),
            Some("from the registry entry")
        );
        assert_eq!(written.index_profile.as_deref(), Some("fast-local"));
        let parsed = uuid::Uuid::parse_str(&written.uuid).expect("valid uuid");
        assert_eq!(parsed.get_version_num(), 7);
        assert_eq!(
            load_manifest(dir.path()).expect("load").expect("present"),
            written
        );
    }

    #[test]
    fn clearing_index_profile_on_a_root_without_a_manifest_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            set_manifest_index_profile(dir.path(), None, seed())
                .expect("clear")
                .is_none()
        );
        assert!(
            !dir.path().join(MANIFEST_FILENAME).exists(),
            "clearing must not mint an identity"
        );
    }

    #[test]
    fn set_index_profile_refuses_to_mint_over_an_unreadable_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "format = \"bookrack-library\"\n\
             format_version = 99\n\
             uuid = \"u\"\n\
             name = \"n\"\n",
        )
        .expect("seed");
        assert!(matches!(
            set_manifest_index_profile(dir.path(), Some("fast-local"), seed()),
            Err(ManifestError::SchemaVersion { found: 99, .. })
        ));
        // The file it refused to understand is still intact.
        let text = std::fs::read_to_string(dir.path().join(MANIFEST_FILENAME)).expect("read");
        assert!(text.contains("format_version = 99"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn set_index_profile_fails_on_a_read_only_root() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("ro");
        std::fs::create_dir(&root).expect("create");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500)).expect("chmod");

        assert!(matches!(
            set_manifest_index_profile(&root, Some("fast-local"), seed()),
            Err(ManifestError::Io { .. })
        ));

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).expect("restore");
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
