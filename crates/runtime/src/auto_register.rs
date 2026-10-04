// SPDX-License-Identifier: Apache-2.0

//! Registration of a path-class root at bring-up.
//!
//! A root selected by `--data-dir`, the data-root variable, or the
//! portable layout that carries an identity manifest the registry does
//! not know is recorded under the manifest's name before the mount set
//! is decided, so the daemon serves the registry the way it would had
//! the operator run `libraries add` first. The `default` pointer is
//! not touched. Every other case leaves the registry alone and the
//! daemon serves the root by itself.

use std::path::{Path, PathBuf};

use bookrack_config::{
    Config, LibraryEntry, LibraryEntryFields, LibraryManifest, ResolutionSource, list_libraries_at,
    load_manifest, registry_target_path, same_root, upsert_library_entry,
};

/// What bring-up did about the selected root.
#[derive(Debug)]
pub enum AutoRegistration {
    /// The root is now the registry entry named `name`.
    Registered { name: String },
    /// The registry was not touched; the daemon serves the root alone.
    Skipped(Skip),
}

/// Why a root was left unregistered. Every variant is a fallback, not
/// a failure: bring-up continues in the single-library form.
#[derive(Debug, PartialEq, Eq)]
pub enum Skip {
    /// The library was selected through the registry, so it is an
    /// entry already.
    RegistrySelection,
    /// The resolver matched the root to an entry at this path.
    AlreadyRegistered { name: String },
    /// The root carries no identity manifest.
    NoManifest,
    /// The manifest exists but could not be read or parsed.
    UnreadableManifest { reason: String },
    /// Another root holds the manifest's name.
    KeyTaken { name: String, other_root: PathBuf },
    /// The registry could not be read, or there is no file to write.
    RegistryUnavailable { reason: String },
    /// The entry could not be written.
    WriteFailed { reason: String },
}

/// Register the resolved root when it is a path-class selection with
/// an identity manifest the registry does not know. Reads the
/// registry the write-side verbs target and writes the entry through
/// the plain writer, so the `default` pointer stays as it was.
pub fn auto_register(cfg: &Config) -> AutoRegistration {
    if !matches!(
        cfg.source(),
        ResolutionSource::DataDirFlag
            | ResolutionSource::EnvVar
            | ResolutionSource::PortableExeNeighbor
    ) {
        return AutoRegistration::Skipped(Skip::RegistrySelection);
    }
    if let Some(name) = cfg.library() {
        return AutoRegistration::Skipped(Skip::AlreadyRegistered {
            name: name.to_string(),
        });
    }
    if let Some(unusable) = cfg.unusable_registry() {
        return AutoRegistration::Skipped(Skip::RegistryUnavailable {
            reason: format!("{}: {}", unusable.path.display(), unusable.reason),
        });
    }
    let root = cfg.data_dir();
    let manifest = match load_manifest(root) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => return AutoRegistration::Skipped(Skip::NoManifest),
        Err(e) => {
            return AutoRegistration::Skipped(Skip::UnreadableManifest {
                reason: e.to_string(),
            });
        }
    };
    let Some(registry_path) = registry_target_path() else {
        return AutoRegistration::Skipped(Skip::RegistryUnavailable {
            reason: "no registry path resolves on this machine".to_string(),
        });
    };
    let entries = match list_libraries_at(&registry_path) {
        Ok(entries) => entries,
        Err(e) => {
            return AutoRegistration::Skipped(Skip::RegistryUnavailable {
                reason: e.to_string(),
            });
        }
    };
    let fields = match decide(&manifest, &entries, root) {
        Ok(fields) => fields,
        Err(skip) => return AutoRegistration::Skipped(skip),
    };
    match upsert_library_entry(&registry_path, &manifest.name, &fields) {
        Ok(()) => AutoRegistration::Registered {
            name: manifest.name,
        },
        Err(e) => AutoRegistration::Skipped(Skip::WriteFailed {
            reason: e.to_string(),
        }),
    }
}

/// Whether `manifest`, found at `root`, can be registered under its
/// own name given the entries the registry holds, and the entry to
/// write if so. Pure. A name already mapped to another root is left to
/// the operator, who picks an alias; it is never derived here.
pub fn decide(
    manifest: &LibraryManifest,
    entries: &[LibraryEntry],
    root: &Path,
) -> Result<LibraryEntryFields, Skip> {
    if let Some(other) = entries
        .iter()
        .find(|e| e.name == manifest.name && !same_root(&e.data_dir, root))
    {
        return Err(Skip::KeyTaken {
            name: manifest.name.clone(),
            other_root: other.data_dir.clone(),
        });
    }
    Ok(LibraryEntryFields {
        data_dir: root.to_path_buf(),
        kind: manifest.kind,
        description: manifest.description.clone(),
        index_profile: manifest.index_profile.clone(),
        created_at: manifest.created_at.clone(),
        uuid: Some(manifest.uuid.clone()),
    })
}

impl Skip {
    /// Record the decision in the daemon log at the level its cause
    /// deserves: nothing for a selection that had nothing to register,
    /// `info` for the expected manifestless root, `warn` where an
    /// operator may want to act.
    pub fn log(&self, root: &Path) {
        let root = root.display();
        match self {
            Skip::RegistrySelection | Skip::AlreadyRegistered { .. } => {}
            Skip::NoManifest => tracing::info!(
                %root,
                "root carries no manifest; serving it alone, not registering",
            ),
            Skip::UnreadableManifest { reason } => tracing::warn!(
                %root,
                %reason,
                "root manifest could not be read; serving the root alone, not registering",
            ),
            Skip::KeyTaken { name, other_root } => tracing::warn!(
                %root,
                %name,
                other_root = %other_root.display(),
                "name already maps to another root; serving this root alone — register it \
                 under an alias with `bookrack libraries add <alias> <root>`",
            ),
            Skip::RegistryUnavailable { reason } => tracing::warn!(
                %root,
                %reason,
                "registry unavailable; serving the root alone, not registering",
            ),
            Skip::WriteFailed { reason } => tracing::warn!(
                %root,
                %reason,
                "registry entry could not be written; serving the root alone",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use bookrack_config::{LibraryKind, new_manifest};

    use super::*;

    fn entry(name: &str, data_dir: &Path) -> LibraryEntry {
        LibraryEntry {
            name: name.to_string(),
            data_dir: data_dir.to_path_buf(),
            is_default: false,
            kind: LibraryKind::Prod,
            description: None,
            index_profile: None,
            created_at: None,
            uuid: None,
        }
    }

    /// A name the registry already maps to another root is not
    /// re-pointed and not aliased: the root stays unregistered.
    #[test]
    fn a_name_mapped_to_another_root_is_key_taken() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("gamma");
        std::fs::create_dir_all(&root).expect("root");
        let manifest = new_manifest("gamma", LibraryKind::Test, None);
        let entries = [entry("gamma", Path::new("/roots/elsewhere"))];
        let skip =
            decide(&manifest, &entries, &root).expect_err("a taken name must not be registered");
        assert_eq!(
            skip,
            Skip::KeyTaken {
                name: "gamma".to_string(),
                other_root: PathBuf::from("/roots/elsewhere"),
            }
        );
    }

    /// The entry written carries the manifest's identity fields and
    /// the root it was found at.
    #[test]
    fn a_free_name_yields_an_entry_from_the_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("gamma");
        std::fs::create_dir_all(&root).expect("root");
        let manifest = new_manifest("gamma", LibraryKind::Test, Some("shelf".to_string()));
        let entries = [entry("alpha", Path::new("/roots/alpha"))];
        let fields = decide(&manifest, &entries, &root).expect("registers");
        assert_eq!(fields.data_dir, root);
        assert_eq!(fields.kind, LibraryKind::Test);
        assert_eq!(fields.description.as_deref(), Some("shelf"));
        assert_eq!(fields.uuid.as_deref(), Some(manifest.uuid.as_str()));
        assert_eq!(fields.created_at, manifest.created_at);
    }
}
