// SPDX-License-Identifier: Apache-2.0

//! The library registry: a TOML file mapping library names to data roots.
//!
//! Each library is an independent data root — its own intake store,
//! databases, and vector store. The registry gives those roots short
//! names so a caller can select one with `--library <name>` instead of
//! re-exporting the data-root variable. The file is named by the
//! `BOOKRACK_REGISTRY` environment variable; it is optional, and only
//! `--library` requires it.
//!
//! An entry is written in one of two forms. A bare string is the legacy
//! format and stays permanently readable; a table carries the metadata
//! a plain path cannot (kind, description, index profile, timestamps,
//! uuid). A bare string is exactly equivalent to a table that sets only
//! `data_dir` and leaves `kind` at its default.
//!
//! ```toml
//! default = "prod"
//!
//! [libraries.prod]
//! data_dir = "/abs/path/to/prod-root"
//! kind     = "prod"
//!
//! # Legacy form, still accepted:
//! # test = "/abs/path/to/test-root"
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A parsed registry: named libraries and an optional default.
#[derive(Debug, Clone, Deserialize)]
pub struct Registry {
    /// Library used when no `--library`, `--data-dir`, or data-root
    /// variable selects one. Optional.
    #[serde(default)]
    pub default: Option<String>,
    /// Named libraries, each mapping to an absolute data root in one of
    /// the two supported entry forms.
    #[serde(default)]
    pub libraries: HashMap<String, RawRegistryEntry>,
}

/// One registry entry as written on disk, in either supported form.
///
/// Deserialized untagged: a string value always matches
/// [`RawRegistryEntry::Path`], a table value always matches
/// [`RawRegistryEntry::Table`]. There is no ambiguity between the two
/// arms because a TOML string and a TOML table are distinct value
/// types.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawRegistryEntry {
    /// Legacy bare-path entry, equivalent to a table that sets only
    /// `data_dir` and leaves every other field at its default.
    Path(PathBuf),
    /// Metadata-bearing entry.
    Table(RegistryEntryTable),
}

/// The table form of a registry entry: a data root plus the metadata
/// the registry caches for it. Every field but `data_dir` is optional,
/// so an entry can be filled in incrementally as later commands learn
/// more about the library.
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryEntryTable {
    /// Absolute data root the name maps to.
    pub data_dir: PathBuf,
    /// Library kind; defaults to [`LibraryKind::Prod`] when absent.
    #[serde(default)]
    pub kind: LibraryKind,
    /// Free-form description, when the operator set one.
    #[serde(default)]
    pub description: Option<String>,
    /// Index-profile name the library is built under. Stored here from
    /// this format onward; validated against the root config later.
    #[serde(default)]
    pub index_profile: Option<String>,
    /// RFC 3339 creation timestamp, cached from the data root's
    /// identity manifest.
    #[serde(default)]
    pub created_at: Option<String>,
    /// Stable library uuid, cached from the identity manifest.
    #[serde(default)]
    pub uuid: Option<String>,
}

/// What a library is for. Advisory today; carried so tooling can
/// distinguish a production library from a throwaway test or scratch
/// root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryKind {
    /// A primary, long-lived library.
    #[default]
    Prod,
    /// A library used for testing or evaluation.
    Test,
    /// A disposable scratch library.
    Scratch,
}

impl LibraryKind {
    /// The wire token, matching the serde `snake_case` representation,
    /// for writing the value back into a registry table.
    pub fn as_str(self) -> &'static str {
        match self {
            LibraryKind::Prod => "prod",
            LibraryKind::Test => "test",
            LibraryKind::Scratch => "scratch",
        }
    }
}

impl RawRegistryEntry {
    /// The data root, regardless of entry form.
    pub fn data_dir(&self) -> &Path {
        match self {
            RawRegistryEntry::Path(p) => p,
            RawRegistryEntry::Table(t) => &t.data_dir,
        }
    }

    /// The library kind. A legacy bare-path entry reports the default
    /// [`LibraryKind::Prod`].
    pub fn kind(&self) -> LibraryKind {
        match self {
            RawRegistryEntry::Path(_) => LibraryKind::default(),
            RawRegistryEntry::Table(t) => t.kind,
        }
    }

    /// The description, if the table form carries one.
    pub fn description(&self) -> Option<&str> {
        match self {
            RawRegistryEntry::Path(_) => None,
            RawRegistryEntry::Table(t) => t.description.as_deref(),
        }
    }

    /// The index-profile name, if the table form carries one.
    pub fn index_profile(&self) -> Option<&str> {
        match self {
            RawRegistryEntry::Path(_) => None,
            RawRegistryEntry::Table(t) => t.index_profile.as_deref(),
        }
    }

    /// The cached creation timestamp, if the table form carries one.
    pub fn created_at(&self) -> Option<&str> {
        match self {
            RawRegistryEntry::Path(_) => None,
            RawRegistryEntry::Table(t) => t.created_at.as_deref(),
        }
    }

    /// The cached library uuid, if the table form carries one.
    pub fn uuid(&self) -> Option<&str> {
        match self {
            RawRegistryEntry::Path(_) => None,
            RawRegistryEntry::Table(t) => t.uuid.as_deref(),
        }
    }
}

/// Parse a registry from TOML text. Pure: takes the text, touches no
/// filesystem, so the registry shape can be tested without temp files.
pub fn parse_registry(text: &str) -> Result<Registry, toml::de::Error> {
    toml::from_str(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_table_entry_form() {
        let registry = parse_registry(
            "default = \"prod\"\n\
             [libraries.prod]\n\
             data_dir = \"/roots/prod\"\n\
             kind = \"prod\"\n\
             description = \"primary\"\n\
             index_profile = \"qwen3-0.6b-default\"\n\
             created_at = \"2026-06-30T12:00:00Z\"\n\
             uuid = \"01890a5d-0000-7000-8000-000000000000\"\n",
        )
        .expect("table form parses");
        let entry = &registry.libraries["prod"];
        assert_eq!(entry.data_dir(), Path::new("/roots/prod"));
        assert_eq!(entry.kind(), LibraryKind::Prod);
        assert_eq!(entry.description(), Some("primary"));
        assert_eq!(entry.index_profile(), Some("qwen3-0.6b-default"));
        assert_eq!(entry.created_at(), Some("2026-06-30T12:00:00Z"));
        assert_eq!(entry.uuid(), Some("01890a5d-0000-7000-8000-000000000000"));
    }

    #[test]
    fn parses_the_legacy_bare_path_form() {
        let registry = parse_registry("[libraries]\nlegacy = \"/roots/legacy\"\n")
            .expect("legacy form parses");
        let entry = &registry.libraries["legacy"];
        // A bare string always lands in the `Path` arm and reports the
        // default kind with no metadata.
        assert!(matches!(entry, RawRegistryEntry::Path(_)));
        assert_eq!(entry.data_dir(), Path::new("/roots/legacy"));
        assert_eq!(entry.kind(), LibraryKind::Prod);
        assert_eq!(entry.description(), None);
        assert_eq!(entry.uuid(), None);
    }

    #[test]
    fn parses_a_file_mixing_both_forms() {
        let registry = parse_registry(
            "default = \"new\"\n\
             [libraries]\n\
             old = \"/roots/old\"\n\
             [libraries.new]\n\
             data_dir = \"/roots/new\"\n\
             kind = \"test\"\n",
        )
        .expect("mixed file parses");
        assert!(matches!(
            registry.libraries["old"],
            RawRegistryEntry::Path(_)
        ));
        assert_eq!(registry.libraries["old"].kind(), LibraryKind::Prod);
        assert!(matches!(
            registry.libraries["new"],
            RawRegistryEntry::Table(_)
        ));
        assert_eq!(registry.libraries["new"].kind(), LibraryKind::Test);
    }

    #[test]
    fn every_kind_token_round_trips_through_serde() {
        for (token, kind) in [
            ("prod", LibraryKind::Prod),
            ("test", LibraryKind::Test),
            ("scratch", LibraryKind::Scratch),
        ] {
            let text = format!("[libraries.x]\ndata_dir = \"/x\"\nkind = \"{token}\"\n");
            let registry = parse_registry(&text).expect("parses");
            assert_eq!(registry.libraries["x"].kind(), kind);
            assert_eq!(kind.as_str(), token);
        }
    }

    #[test]
    fn an_unknown_kind_token_is_rejected() {
        let err = parse_registry("[libraries.x]\ndata_dir = \"/x\"\nkind = \"archive\"\n");
        assert!(err.is_err(), "an unknown kind token must not parse");
    }
}
