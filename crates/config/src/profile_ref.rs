// SPDX-License-Identifier: Apache-2.0

//! Which index profile a library references, and where that reference
//! came from.
//!
//! The reference can be recorded in three places, in descending order of
//! authority:
//!
//! 1. the data root's manifest — the truth, travelling with the data;
//! 2. the data root's `config.toml` — a per-root declaration written by
//!    hand or by a binary that predates the manifest field;
//! 3. the library's registry entry — a regenerable cache of the
//!    manifest, one machine's view of a library it may not even hold.
//!
//! Resolution takes the highest-priority source that names one and never
//! fails: a lower source naming something else is stale data, not an
//! irreconcilable conflict, so it is reported as drift for `doctor` and
//! `index-profile current` to surface and for `index-profile apply` or
//! `libraries scan` to repair.

use serde::Serialize;

use crate::LibraryEntry;

use std::path::Path;

/// Where a library's effective profile reference was declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileRefOrigin {
    /// The data root's manifest — the authoritative copy.
    Manifest,
    /// The data root's `config.toml`.
    ConfigToml,
    /// The library's registry entry, a cache of the manifest.
    Registry,
}

impl ProfileRefOrigin {
    /// Stable label for human rendering. The JSON form is the serde
    /// `snake_case` token, which differs for `ConfigToml`: the file's
    /// real name reads better in a terminal than the token does.
    pub fn as_str(self) -> &'static str {
        match self {
            ProfileRefOrigin::Manifest => "manifest",
            ProfileRefOrigin::ConfigToml => "config.toml",
            ProfileRefOrigin::Registry => "registry",
        }
    }
}

/// A lower-priority source naming a profile other than the effective
/// one: a stale copy left by an older write path or an edit that did
/// not go through `index-profile apply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileRefDrift {
    /// The source holding the stale value.
    pub source: ProfileRefOrigin,
    /// The profile name that source names.
    pub stale_value: String,
}

/// Pick the effective profile reference from the three sources by fixed
/// priority: manifest, then `config.toml`, then the registry entry.
/// `None` when no source names one — the library runs on field-level
/// configuration alone.
///
/// Never fails. Disagreement between sources is drift, not an error;
/// see [`profile_reference_drift`].
pub fn effective_profile_reference(
    manifest_ref: Option<&str>,
    config_ref: Option<&str>,
    registry_ref: Option<&str>,
) -> Option<(String, ProfileRefOrigin)> {
    [
        (manifest_ref, ProfileRefOrigin::Manifest),
        (config_ref, ProfileRefOrigin::ConfigToml),
        (registry_ref, ProfileRefOrigin::Registry),
    ]
    .into_iter()
    .find_map(|(value, origin)| value.map(|v| (v.to_string(), origin)))
}

/// Report every source that names a profile other than the effective
/// one, in priority order.
///
/// Empty when the sources agree or only one names anything — including
/// the case where no source does. A source that names nothing is not
/// drift: absence is how a library that never declared a profile looks.
pub fn profile_reference_drift(
    manifest_ref: Option<&str>,
    config_ref: Option<&str>,
    registry_ref: Option<&str>,
) -> Vec<ProfileRefDrift> {
    let Some((effective, _)) = effective_profile_reference(manifest_ref, config_ref, registry_ref)
    else {
        return Vec::new();
    };
    [
        (manifest_ref, ProfileRefOrigin::Manifest),
        (config_ref, ProfileRefOrigin::ConfigToml),
        (registry_ref, ProfileRefOrigin::Registry),
    ]
    .into_iter()
    .filter_map(|(value, source)| {
        value.filter(|v| *v != effective).map(|v| ProfileRefDrift {
            source,
            stale_value: v.to_string(),
        })
    })
    .collect()
}

/// The `index_profile` a registry entry list records for a library:
/// matched by registry name when the selection carried one, otherwise by
/// data root. `None` when no entry matches or none records a profile.
///
/// Pure, so a test drives the name-match and path-fallback branches
/// without a registry on disk.
pub fn registry_profile_ref_in(
    entries: &[LibraryEntry],
    library: Option<&str>,
    data_dir: &Path,
) -> Option<String> {
    let entry = match library {
        Some(name) => entries.iter().find(|e| e.name == name),
        None => entries.iter().find(|e| same_dir(&e.data_dir, data_dir)),
    }?;
    entry.index_profile.clone()
}

/// Whether two paths name the same directory, comparing canonicalized
/// forms and falling back to a raw comparison when canonicalization
/// fails.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::LibraryKind;
    use std::path::PathBuf;

    /// One row of the resolution table: the three sources, then the
    /// reference they should resolve to.
    type Case = (
        Option<&'static str>,
        Option<&'static str>,
        Option<&'static str>,
        Option<(&'static str, ProfileRefOrigin)>,
    );

    /// Every combination of the three sources being absent, or naming
    /// `a`, or naming `b`, against the expected effective pick.
    #[test]
    fn effective_reference_follows_manifest_then_config_then_registry() {
        let cases: &[Case] = &[
            (None, None, None, None),
            (
                Some("a"),
                None,
                None,
                Some(("a", ProfileRefOrigin::Manifest)),
            ),
            (
                None,
                Some("a"),
                None,
                Some(("a", ProfileRefOrigin::ConfigToml)),
            ),
            (
                None,
                None,
                Some("a"),
                Some(("a", ProfileRefOrigin::Registry)),
            ),
            // The manifest wins over anything below it, agreeing or not.
            (
                Some("a"),
                Some("a"),
                Some("a"),
                Some(("a", ProfileRefOrigin::Manifest)),
            ),
            (
                Some("a"),
                Some("b"),
                Some("b"),
                Some(("a", ProfileRefOrigin::Manifest)),
            ),
            (
                Some("a"),
                None,
                Some("b"),
                Some(("a", ProfileRefOrigin::Manifest)),
            ),
            // config.toml wins over the registry cache below it.
            (
                None,
                Some("a"),
                Some("b"),
                Some(("a", ProfileRefOrigin::ConfigToml)),
            ),
            (
                None,
                Some("a"),
                Some("a"),
                Some(("a", ProfileRefOrigin::ConfigToml)),
            ),
        ];
        for (manifest, config, registry, expected) in cases {
            let got = effective_profile_reference(*manifest, *config, *registry);
            let expected = expected.map(|(name, origin)| (name.to_string(), origin));
            assert_eq!(
                got, expected,
                "manifest={manifest:?} config={config:?} registry={registry:?}"
            );
        }
    }

    #[test]
    fn agreeing_sources_and_absent_sources_are_not_drift() {
        assert!(profile_reference_drift(None, None, None).is_empty());
        assert!(profile_reference_drift(Some("a"), Some("a"), Some("a")).is_empty());
        assert!(profile_reference_drift(Some("a"), None, None).is_empty());
        // Absence below the effective source is how an undeclared
        // library looks, not a stale copy.
        assert!(profile_reference_drift(Some("a"), None, Some("a")).is_empty());
    }

    #[test]
    fn every_disagreeing_source_is_reported_in_priority_order() {
        let drift = profile_reference_drift(Some("a"), Some("b"), Some("c"));
        assert_eq!(
            drift,
            vec![
                ProfileRefDrift {
                    source: ProfileRefOrigin::ConfigToml,
                    stale_value: "b".to_string(),
                },
                ProfileRefDrift {
                    source: ProfileRefOrigin::Registry,
                    stale_value: "c".to_string(),
                },
            ]
        );
    }

    #[test]
    fn a_stale_registry_cache_under_a_config_declaration_is_drift() {
        let drift = profile_reference_drift(None, Some("a"), Some("b"));
        assert_eq!(
            drift,
            vec![ProfileRefDrift {
                source: ProfileRefOrigin::Registry,
                stale_value: "b".to_string(),
            }]
        );
    }

    fn entry(name: &str, data_dir: &str, profile: Option<&str>) -> LibraryEntry {
        LibraryEntry {
            name: name.to_string(),
            data_dir: PathBuf::from(data_dir),
            kind: LibraryKind::Prod,
            description: None,
            index_profile: profile.map(str::to_string),
            created_at: None,
            uuid: None,
            is_default: false,
        }
    }

    #[test]
    fn registry_profile_ref_matches_by_name_when_one_was_selected() {
        let entries = vec![
            entry("main", "/data/main", Some("a")),
            entry("other", "/data/other", Some("b")),
        ];
        assert_eq!(
            registry_profile_ref_in(&entries, Some("other"), Path::new("/data/main")),
            Some("b".to_string()),
            "the name selects the entry, not the path"
        );
    }

    #[test]
    fn registry_profile_ref_falls_back_to_the_data_root() {
        let entries = vec![entry("main", "/data/main", Some("a"))];
        assert_eq!(
            registry_profile_ref_in(&entries, None, Path::new("/data/main")),
            Some("a".to_string())
        );
        assert_eq!(
            registry_profile_ref_in(&entries, None, Path::new("/data/elsewhere")),
            None
        );
    }

    #[test]
    fn registry_profile_ref_is_none_when_the_entry_records_no_profile() {
        let entries = vec![entry("main", "/data/main", None)];
        assert_eq!(
            registry_profile_ref_in(&entries, Some("main"), Path::new("/data/main")),
            None
        );
    }

    #[test]
    fn registry_profile_ref_finds_a_root_spelled_non_canonically() {
        // Real directories, so canonicalization actually resolves the
        // `..` hop rather than falling through to the raw comparison.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("a");
        std::fs::create_dir_all(&root).expect("create");
        let entries = vec![entry(
            "a",
            root.to_str().expect("utf-8 tempdir"),
            Some("profile-a"),
        )];

        let dotted = root.join("..").join("a");
        assert_eq!(
            registry_profile_ref_in(&entries, None, &dotted),
            Some("profile-a".to_string())
        );
    }

    #[test]
    fn an_unknown_name_matches_nothing_rather_than_falling_back_to_the_path() {
        let entries = vec![entry("main", "/data/main", Some("a"))];
        assert_eq!(
            registry_profile_ref_in(&entries, Some("ghost"), Path::new("/data/main")),
            None
        );
    }
}
