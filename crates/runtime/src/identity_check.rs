// SPDX-License-Identifier: Apache-2.0

//! Does a registered library's data root carry the identity the
//! registry cached for it?
//!
//! One judgement for the places a registry entry's root is about to be
//! used: bring-up's eager loop in [`crate::daemon`] and the runtime
//! [`crate::mount::Mounter::mount`], which serve the root, and the
//! commands that write to it from the CLI process with no daemon in
//! between ([`refuse_offline_write`]). An entry that caches a `uuid`
//! was written against a root whose manifest records that uuid, so a
//! root that now holds no manifest, or a manifest with another uuid, is
//! not that library. The first is what an empty mount point looks like
//! while its volume is not connected: the directory exists and holds
//! nothing, so an existence check passes it, and serving it would
//! answer searches with nothing and write an ingest into the mount
//! point, where the volume covers it the moment it comes back. The
//! check runs before the root is locked or anything is opened, so a
//! refused root is left exactly as it was found.
//!
//! An entry with no cached uuid — a read-only root registered without
//! a manifest — has nothing to compare and is served as before;
//! `doctor` reports the gap. A manifest that cannot be read is not
//! this check's business either: `doctor` reports it as unreadable,
//! and refusing on it would block a whole bring-up on one damaged
//! file of a root that is otherwise present.
//!
//! The check returns an [`IdentityFault`], not a finished message. Its
//! reason names the repair and no retry: a daemon closes the same
//! fault once for a mount it goes on serving and once for a bring-up
//! that refuses, and only the caller knows which
//! ([`crate::unmounted::MountOccasion`]). An offline write is closed
//! here, by [`OfflineWriteRefusal`], with running the command again.

use std::path::{Path, PathBuf};

use bookrack_config::{
    Config, ConfigError, LibraryEntry, MANIFEST_FILENAME, list_libraries, load_manifest, same_root,
};
use bookrack_core::{Explain, Problem};

use crate::backend_probe::PreflightRefusal;

/// A registered library whose data root does not carry the identity
/// the registry cached for it.
#[derive(Debug, Clone)]
pub(crate) struct IdentityFault {
    library: String,
    root: PathBuf,
    cached: String,
    kind: FaultKind,
}

#[derive(Debug, Clone)]
enum FaultKind {
    /// The root carries no manifest.
    MissingManifest,
    /// The root's manifest records another uuid, under the name it
    /// gives.
    Mismatch { found: String, found_name: String },
}

impl IdentityFault {
    /// The three-part reason a daemon gives, its hint naming the repair
    /// and no retry.
    pub(crate) fn problem(&self) -> Problem {
        self.worded("serve")
    }

    /// The three-part reason with `verb` as what cannot be done to the
    /// library.
    fn worded(&self, verb: &str) -> Problem {
        match &self.kind {
            FaultKind::MissingManifest => {
                missing_manifest_problem(verb, &self.library, &self.root, &self.cached)
            }
            FaultKind::Mismatch { found, found_name } => identity_mismatch_problem(
                verb,
                &self.library,
                &self.root,
                &self.cached,
                found,
                found_name,
            ),
        }
    }

    /// The fault as the preflight refusal a runtime mount returns, its
    /// reason as [`IdentityFault::problem`] words it.
    pub(crate) fn refusal(&self) -> PreflightRefusal {
        PreflightRefusal {
            library: self.library.clone(),
            problem: self.problem(),
        }
    }
}

/// Fault `root` when `entry` caches a uuid the root's manifest does
/// not carry. `root` is the resolved form of the entry's data root;
/// the entry supplies the name and the cached uuid.
pub(crate) fn verify_registered_identity(
    entry: &LibraryEntry,
    root: &Path,
) -> Result<(), IdentityFault> {
    let Some(cached) = entry.uuid.as_deref() else {
        return Ok(());
    };
    let kind = match load_manifest(root) {
        Ok(Some(manifest)) if manifest.uuid == cached => return Ok(()),
        Ok(Some(manifest)) => FaultKind::Mismatch {
            found: manifest.uuid,
            found_name: manifest.name,
        },
        Ok(None) => FaultKind::MissingManifest,
        Err(_) => return Ok(()),
    };
    Err(IdentityFault {
        library: entry.name.clone(),
        root: root.to_path_buf(),
        cached: cached.to_string(),
        kind,
    })
}

/// The registry entry for `name`, read from the registry the
/// environment names. `Ok(None)` when no registry exists or none
/// carries the name — a resolution that succeeded a moment ago and an
/// entry that is gone now is a registry rewritten underneath the
/// daemon, which is not this check's fault to report.
pub(crate) fn registry_entry(name: &str) -> Result<Option<LibraryEntry>, ConfigError> {
    Ok(bookrack_config::list_libraries()?
        .and_then(|entries| entries.into_iter().find(|entry| entry.name == name)))
}

/// A write from the CLI process refused before it touched the root: the
/// registry caches a uuid for the library, and the root carries no
/// manifest or one recording another uuid.
#[derive(Debug, Clone)]
pub struct OfflineWriteRefusal(IdentityFault);

impl std::fmt::Display for OfflineWriteRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.worded("write to").summary)
    }
}

impl std::error::Error for OfflineWriteRefusal {}

impl Explain for OfflineWriteRefusal {
    fn explain(&self) -> Problem {
        let mut problem = self.0.worded("write to");
        if let Some(hint) = problem.data.hint.take() {
            problem.data.hint = Some(format!("{hint} Then run this command again."));
        }
        problem
    }
}

/// Refuse a write to the root `config` resolved, when the registry
/// entry that root belongs to caches a uuid the root does not carry.
/// The entry is the one the resolution named, or, for a path-class root
/// it left unnamed, the one whose data root is that path. A root no
/// entry claims either way has nothing to compare and passes.
///
/// A registry that cannot be read is an error, not a pass: the
/// resolution that produced `config` read the same file a moment ago.
pub fn refuse_offline_write(config: &Config) -> eyre::Result<()> {
    let root = config.data_dir();
    let entries = list_libraries()?.unwrap_or_default();
    let entry = match config.library() {
        Some(name) => entries.into_iter().find(|entry| entry.name == name),
        None => entries
            .into_iter()
            .find(|entry| same_root(&entry.data_dir, root)),
    };
    match entry {
        Some(entry) => verify_registered_identity(&entry, root)
            .map_err(|fault| OfflineWriteRefusal(fault).into()),
        None => Ok(()),
    }
}

/// [`refuse_offline_write`] for a caller that took the root straight
/// from a registry entry: the entry's own data root is judged.
pub fn refuse_offline_write_to(entry: &LibraryEntry) -> Result<(), OfflineWriteRefusal> {
    verify_registered_identity(entry, &entry.data_dir).map_err(OfflineWriteRefusal)
}

/// The three-part refusal for a root with no manifest: the summary
/// names the library, the detail what the registry expected to find
/// there and what the directory looks like instead, the hint the two
/// ways out — the volume comes back, or the library is registered
/// where it now is.
fn missing_manifest_problem(verb: &str, name: &str, root: &Path, cached: &str) -> Problem {
    Problem::new(format!(
        "cannot {verb} library '{name}': its data root carries no identity manifest"
    ))
    .detail(format!(
        "The registry records uuid {cached} for '{name}' at '{}', but no {MANIFEST_FILENAME} is \
         there. An empty mount point whose volume is not connected looks exactly like this.",
        root.display()
    ))
    .hint(format!(
        "Reconnect the volume that holds it, or register the library at its new location with \
         `bookrack libraries add {name} <path>`."
    ))
}

/// The three-part refusal for a manifest recording another uuid: the
/// detail names both sides, the hint the two readings — the root is
/// this library under a refreshed identity, or another library's
/// volume is mounted where this one's belongs.
fn identity_mismatch_problem(
    verb: &str,
    name: &str,
    root: &Path,
    cached: &str,
    found: &str,
    found_name: &str,
) -> Problem {
    Problem::new(format!(
        "cannot {verb} library '{name}': the identity manifest at its data root records \
         another uuid"
    ))
    .detail(format!(
        "The registry records uuid {cached} for '{name}'; the manifest at '{}' records uuid \
         {found} under the name '{found_name}'.",
        root.display()
    ))
    .hint(format!(
        "If this root is '{name}', refresh the registry's cached identity with `bookrack \
         libraries add {name} {root}`; if another library's volume is mounted here, reconnect \
         the one that holds '{name}'.",
        root = root.display(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_config::{LibraryKind, new_manifest, write_manifest};

    fn entry(name: &str, root: &Path, uuid: Option<&str>) -> LibraryEntry {
        LibraryEntry {
            name: name.to_string(),
            data_dir: root.to_path_buf(),
            is_default: false,
            kind: LibraryKind::Test,
            description: None,
            index_profile: None,
            created_at: None,
            uuid: uuid.map(str::to_string),
        }
    }

    fn hint(problem: &Problem) -> &str {
        problem.data.hint.as_deref().expect("hint")
    }

    #[test]
    fn an_entry_without_a_cached_uuid_is_served_from_a_bare_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let e = entry("legacy", root.path(), None);
        assert!(verify_registered_identity(&e, root.path()).is_ok());
    }

    #[test]
    fn a_manifest_carrying_the_cached_uuid_passes() {
        let root = tempfile::tempdir().expect("tempdir");
        let manifest = new_manifest("alpha", LibraryKind::Test, None);
        write_manifest(root.path(), &manifest).expect("write manifest");
        let e = entry("alpha", root.path(), Some(&manifest.uuid));
        assert!(verify_registered_identity(&e, root.path()).is_ok());
    }

    #[test]
    fn a_bare_root_under_a_cached_uuid_is_refused_with_the_volume_first() {
        let root = tempfile::tempdir().expect("tempdir");
        let e = entry(
            "alpha",
            root.path(),
            Some("01890a5d-0000-7000-8000-000000000000"),
        );
        let fault = verify_registered_identity(&e, root.path())
            .expect_err("a cached uuid with no manifest is refused");
        let refusal = fault.refusal();
        assert_eq!(refusal.library, "alpha");
        assert!(
            refusal.problem.summary.contains("no identity manifest"),
            "{refusal:?}"
        );
        let hint = hint(&refusal.problem);
        assert!(hint.starts_with("Reconnect the volume"), "{hint}");
        assert!(
            hint.ends_with("`bookrack libraries add alpha <path>`."),
            "{hint}"
        );
    }

    /// The reason names the repair and stops there: the retry depends
    /// on whether a daemon is serving, which only the caller knows.
    #[test]
    fn a_bare_roots_hint_names_no_retry() {
        let root = tempfile::tempdir().expect("tempdir");
        let e = entry("beta", root.path(), Some("u"));
        let fault = verify_registered_identity(&e, root.path())
            .expect_err("a cached uuid with no manifest is refused");
        let problem = fault.problem();
        assert!(
            problem.summary.starts_with("cannot serve library 'beta'"),
            "{problem:?}"
        );
        assert!(!hint(&problem).contains("libraries mount"), "{problem:?}");
        assert!(!hint(&problem).contains("start the daemon"), "{problem:?}");
    }

    #[test]
    fn a_manifest_with_another_uuid_names_both_sides() {
        let root = tempfile::tempdir().expect("tempdir");
        let present = new_manifest("other", LibraryKind::Test, None);
        write_manifest(root.path(), &present).expect("write manifest");
        let cached = "01890a5d-0000-7000-8000-000000000000";
        let e = entry("alpha", root.path(), Some(cached));
        let fault =
            verify_registered_identity(&e, root.path()).expect_err("a different uuid is refused");
        let problem = fault.problem();
        assert!(problem.summary.contains("another uuid"), "{problem:?}");
        let detail = problem.data.detail.as_deref().expect("detail");
        assert!(detail.contains(cached), "{detail}");
        assert!(detail.contains(&present.uuid), "{detail}");
        assert!(detail.contains("'other'"), "{detail}");
    }

    #[test]
    fn a_mismatchs_hint_names_both_readings_and_no_retry() {
        let root = tempfile::tempdir().expect("tempdir");
        write_manifest(root.path(), &new_manifest("other", LibraryKind::Test, None))
            .expect("write manifest");
        let e = entry("alpha", root.path(), Some("u"));
        let fault =
            verify_registered_identity(&e, root.path()).expect_err("a different uuid is refused");
        let problem = fault.problem();
        assert!(
            problem.summary.starts_with("cannot serve library 'alpha'"),
            "{problem:?}"
        );
        let hint = hint(&problem);
        assert!(hint.starts_with("If this root is 'alpha'"), "{hint}");
        assert!(
            hint.ends_with("reconnect the one that holds 'alpha'."),
            "{hint}"
        );
    }

    /// An offline write says what it could not do, keeps the daemon's
    /// detail and repair, and closes with the one retry that fits a
    /// command run from the CLI process.
    #[test]
    fn an_offline_write_names_writing_and_closes_with_running_the_command_again() {
        let bare = tempfile::tempdir().expect("tempdir");
        let foreign = tempfile::tempdir().expect("tempdir");
        write_manifest(
            foreign.path(),
            &new_manifest("other", LibraryKind::Test, None),
        )
        .expect("write manifest");
        for (root, tail) in [
            (bare.path(), "its data root carries no identity manifest"),
            (
                foreign.path(),
                "the identity manifest at its data root records another uuid",
            ),
        ] {
            let e = entry("alpha", root, Some("u"));
            let refusal = refuse_offline_write_to(&e).expect_err("the root is refused");
            let served = verify_registered_identity(&e, root)
                .expect_err("the root is refused")
                .problem();
            let problem = refusal.explain();
            assert_eq!(
                problem.summary,
                format!("cannot write to library 'alpha': {tail}")
            );
            assert_eq!(refusal.to_string(), problem.summary);
            assert_eq!(problem.data.detail, served.data.detail);
            assert_eq!(
                hint(&problem),
                format!("{} Then run this command again.", hint(&served))
            );
        }
    }

    #[test]
    fn an_unreadable_manifest_is_left_to_doctor() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join(MANIFEST_FILENAME), "not = valid = toml")
            .expect("seed a malformed manifest");
        let e = entry("alpha", root.path(), Some("u"));
        assert!(verify_registered_identity(&e, root.path()).is_ok());
    }
}
