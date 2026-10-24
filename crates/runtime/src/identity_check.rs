// SPDX-License-Identifier: Apache-2.0

//! Does a registered library's data root carry the identity the
//! registry cached for it?
//!
//! One judgement for the two places a registry entry becomes a mount:
//! bring-up's eager loop in [`crate::daemon`] and the runtime
//! [`crate::mount::Mounter::mount`]. An entry that caches a `uuid` was
//! written against a root whose manifest records that uuid, so a root
//! that now holds no manifest, or a manifest with another uuid, is not
//! that library. The first is what an empty mount point looks like
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

use std::path::Path;

use bookrack_config::{ConfigError, LibraryEntry, MANIFEST_FILENAME, load_manifest};
use bookrack_core::Problem;

use crate::backend_probe::PreflightRefusal;

/// Which act a refused root was being mounted by, so the hint can
/// name the retry that fits: a bring-up is started again, a runtime
/// mount is repeated through `libraries mount`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountOccasion {
    /// The eager mount loop at daemon bring-up.
    BringUp,
    /// The `library.mount` control-plane method.
    Runtime,
}

impl MountOccasion {
    /// The closing clause of a hint: how to try again once the root
    /// is back.
    fn retry(self, name: &str) -> String {
        match self {
            MountOccasion::BringUp => "then start the daemon again".to_string(),
            MountOccasion::Runtime => format!("then run `bookrack libraries mount {name}`"),
        }
    }
}

/// Refuse `root` when `entry` caches a uuid the root's manifest does
/// not carry. `root` is the resolved form of the entry's data root;
/// the entry supplies the name and the cached uuid.
pub(crate) fn verify_registered_identity(
    entry: &LibraryEntry,
    root: &Path,
    occasion: MountOccasion,
) -> Result<(), PreflightRefusal> {
    let Some(cached) = entry.uuid.as_deref() else {
        return Ok(());
    };
    let problem = match load_manifest(root) {
        Ok(Some(manifest)) if manifest.uuid == cached => return Ok(()),
        Ok(Some(manifest)) => {
            identity_mismatch_problem(&entry.name, root, cached, &manifest.uuid, &manifest.name)
        }
        Ok(None) => missing_manifest_problem(&entry.name, root, cached, occasion),
        Err(_) => return Ok(()),
    };
    Err(PreflightRefusal {
        library: entry.name.clone(),
        problem,
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

/// The three-part refusal for a root with no manifest: the summary
/// names the library, the detail what the registry expected to find
/// there and what the directory looks like instead, the hint the two
/// ways out — the volume comes back, or the library is registered
/// where it now is.
fn missing_manifest_problem(
    name: &str,
    root: &Path,
    cached: &str,
    occasion: MountOccasion,
) -> Problem {
    Problem::new(format!(
        "cannot serve library '{name}': its data root carries no identity manifest"
    ))
    .detail(format!(
        "The registry records uuid {cached} for '{name}' at '{}', but no {MANIFEST_FILENAME} is \
         there. An empty mount point whose volume is not connected looks exactly like this.",
        root.display()
    ))
    .hint(format!(
        "Reconnect the volume that holds it, or register the library at its new location with \
         `bookrack libraries add {name} <path>`, {}.",
        occasion.retry(name)
    ))
}

/// The three-part refusal for a manifest recording another uuid: the
/// detail names both sides, the hint the two readings — the root is
/// this library under a refreshed identity, or another library's
/// volume is mounted where this one's belongs.
fn identity_mismatch_problem(
    name: &str,
    root: &Path,
    cached: &str,
    found: &str,
    found_name: &str,
) -> Problem {
    Problem::new(format!(
        "cannot serve library '{name}': the identity manifest at its data root records another \
         uuid"
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
        root = root.display()
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

    #[test]
    fn an_entry_without_a_cached_uuid_is_served_from_a_bare_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let e = entry("legacy", root.path(), None);
        assert!(verify_registered_identity(&e, root.path(), MountOccasion::BringUp).is_ok());
    }

    #[test]
    fn a_manifest_carrying_the_cached_uuid_passes() {
        let root = tempfile::tempdir().expect("tempdir");
        let manifest = new_manifest("alpha", LibraryKind::Test, None);
        write_manifest(root.path(), &manifest).expect("write manifest");
        let e = entry("alpha", root.path(), Some(&manifest.uuid));
        assert!(verify_registered_identity(&e, root.path(), MountOccasion::BringUp).is_ok());
    }

    #[test]
    fn a_bare_root_under_a_cached_uuid_is_refused_with_the_volume_first() {
        let root = tempfile::tempdir().expect("tempdir");
        let e = entry(
            "alpha",
            root.path(),
            Some("01890a5d-0000-7000-8000-000000000000"),
        );
        let refusal = verify_registered_identity(&e, root.path(), MountOccasion::BringUp)
            .expect_err("a cached uuid with no manifest is refused");
        assert_eq!(refusal.library, "alpha");
        assert!(
            refusal.problem.summary.contains("no identity manifest"),
            "{refusal:?}"
        );
        let hint = refusal.problem.data.hint.as_deref().expect("hint");
        assert!(hint.starts_with("Reconnect the volume"), "{hint}");
        assert!(hint.ends_with("then start the daemon again."), "{hint}");
    }

    #[test]
    fn the_runtime_occasion_names_the_mount_verb() {
        let root = tempfile::tempdir().expect("tempdir");
        let e = entry("beta", root.path(), Some("u"));
        let refusal = verify_registered_identity(&e, root.path(), MountOccasion::Runtime)
            .expect_err("a cached uuid with no manifest is refused");
        let hint = refusal.problem.data.hint.as_deref().expect("hint");
        assert!(hint.contains("`bookrack libraries mount beta`"), "{hint}");
    }

    #[test]
    fn a_manifest_with_another_uuid_names_both_sides() {
        let root = tempfile::tempdir().expect("tempdir");
        let present = new_manifest("other", LibraryKind::Test, None);
        write_manifest(root.path(), &present).expect("write manifest");
        let cached = "01890a5d-0000-7000-8000-000000000000";
        let e = entry("alpha", root.path(), Some(cached));
        let refusal = verify_registered_identity(&e, root.path(), MountOccasion::BringUp)
            .expect_err("a different uuid is refused");
        assert!(
            refusal.problem.summary.contains("another uuid"),
            "{refusal:?}"
        );
        let detail = refusal.problem.data.detail.as_deref().expect("detail");
        assert!(detail.contains(cached), "{detail}");
        assert!(detail.contains(&present.uuid), "{detail}");
        assert!(detail.contains("'other'"), "{detail}");
    }

    #[test]
    fn an_unreadable_manifest_is_left_to_doctor() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join(MANIFEST_FILENAME), "not = valid = toml")
            .expect("seed a malformed manifest");
        let e = entry("alpha", root.path(), Some("u"));
        assert!(verify_registered_identity(&e, root.path(), MountOccasion::BringUp).is_ok());
    }
}
