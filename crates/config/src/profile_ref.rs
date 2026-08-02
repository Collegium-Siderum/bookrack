// SPDX-License-Identifier: Apache-2.0

//! Which index profile a library references, and where that reference
//! came from.
//!
//! The reference can be recorded in two places, in descending order of
//! authority:
//!
//! 1. the data root's manifest — the truth, travelling with the data;
//! 2. the library's registry entry — a regenerable cache of the
//!    manifest, one machine's view of a library it may not even hold.
//!
//! Resolution takes the highest-priority source that names one and never
//! fails: a lower source naming something else is stale data, not an
//! irreconcilable conflict, so it is reported as drift for `doctor` and
//! `index-profile current` to surface and for `index-profile apply` or
//! `libraries scan` to repair.

use bookrack_core::knob::{Candidate, KnobReach, Layer, ReadAt, resolve_knob};
use serde::Serialize;

use crate::LibraryEntry;

use std::path::Path;

/// Where a library's effective profile reference was declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileRefOrigin {
    /// The data root's manifest — the authoritative copy.
    Manifest,
    /// The library's registry entry, a cache of the manifest.
    Registry,
}

impl ProfileRefOrigin {
    /// Stable label for human rendering, matching the serde
    /// `snake_case` token of the JSON form.
    pub fn as_str(self) -> &'static str {
        match self {
            ProfileRefOrigin::Manifest => "manifest",
            ProfileRefOrigin::Registry => "registry",
        }
    }

    /// The knob layer this source resolves at.
    fn layer(self) -> Layer {
        match self {
            ProfileRefOrigin::Manifest => Layer::Manifest,
            ProfileRefOrigin::Registry => Layer::Registry,
        }
    }

    /// The source a knob layer stands for, or `None` for a layer this
    /// knob never draws on.
    fn from_layer(layer: Layer) -> Option<ProfileRefOrigin> {
        match layer {
            Layer::Manifest => Some(ProfileRefOrigin::Manifest),
            Layer::Registry => Some(ProfileRefOrigin::Registry),
            _ => None,
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

/// Pick the effective profile reference from the two sources by fixed
/// priority: the manifest, then the registry entry. `None` when neither
/// names one — the library runs on the default embed model alone.
///
/// Never fails. Disagreement between sources is drift, not an error;
/// see [`profile_reference_drift`].
pub fn effective_profile_reference(
    manifest_ref: Option<&str>,
    registry_ref: Option<&str>,
) -> Option<(String, ProfileRefOrigin)> {
    let origin = resolve_profile_ref(manifest_ref, registry_ref);
    let value = origin.value?;
    ProfileRefOrigin::from_layer(origin.layer).map(|source| (value, source))
}

/// The two sources in descending priority. The **only** place this
/// module writes that order down: both public answers are read off the
/// one row it produces, so they cannot disagree about which source won.
fn profile_candidates(manifest_ref: Option<&str>, registry_ref: Option<&str>) -> Vec<Candidate> {
    [
        (ProfileRefOrigin::Manifest, manifest_ref),
        (ProfileRefOrigin::Registry, registry_ref),
    ]
    .into_iter()
    .map(|(source, value)| {
        Candidate::of(source.layer(), source.as_str(), value.map(str::to_string))
    })
    .collect()
}

/// The resolved row behind both public answers.
fn resolve_profile_ref(
    manifest_ref: Option<&str>,
    registry_ref: Option<&str>,
) -> bookrack_core::knob::KnobOrigin {
    resolve_knob(
        "index_profile",
        KnobReach::Library,
        ReadAt::AfterResolution,
        profile_candidates(manifest_ref, registry_ref),
    )
}

/// Report every source that names a profile other than the effective
/// one, in priority order.
///
/// Empty when the sources agree or only one names anything — including
/// the case where no source does. A source that names nothing is not
/// drift: absence is how a library that never declared a profile looks.
pub fn profile_reference_drift(
    manifest_ref: Option<&str>,
    registry_ref: Option<&str>,
) -> Vec<ProfileRefDrift> {
    let origin = resolve_profile_ref(manifest_ref, registry_ref);
    let Some(effective) = origin.value else {
        return Vec::new();
    };
    origin
        .shadowed
        .into_iter()
        .filter(|s| s.value != effective)
        .filter_map(|s| {
            ProfileRefOrigin::from_layer(s.layer).map(|source| ProfileRefDrift {
                source,
                stale_value: s.value,
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
        Option<(&'static str, ProfileRefOrigin)>,
    );

    /// Every combination of the two sources being absent, or naming
    /// `a`, or naming `b`, against the expected effective pick.
    #[test]
    fn effective_reference_follows_manifest_then_registry() {
        let cases: &[Case] = &[
            (None, None, None),
            (Some("a"), None, Some(("a", ProfileRefOrigin::Manifest))),
            (None, Some("a"), Some(("a", ProfileRefOrigin::Registry))),
            // The manifest wins over the cache below it, agreeing or not.
            (
                Some("a"),
                Some("a"),
                Some(("a", ProfileRefOrigin::Manifest)),
            ),
            (
                Some("a"),
                Some("b"),
                Some(("a", ProfileRefOrigin::Manifest)),
            ),
        ];
        for (manifest, registry, expected) in cases {
            let got = effective_profile_reference(*manifest, *registry);
            let expected = expected.map(|(name, origin)| (name.to_string(), origin));
            assert_eq!(got, expected, "manifest={manifest:?} registry={registry:?}");
        }
    }

    #[test]
    fn agreeing_sources_and_absent_sources_are_not_drift() {
        assert!(profile_reference_drift(None, None).is_empty());
        assert!(profile_reference_drift(Some("a"), Some("a")).is_empty());
        assert!(profile_reference_drift(Some("a"), None).is_empty());
        // A cache naming nothing is a cache that has not caught up to
        // the manifest, not a stale copy naming the wrong thing.
        assert!(profile_reference_drift(None, Some("a")).is_empty());
    }

    #[test]
    fn a_registry_cache_disagreeing_with_the_manifest_is_drift() {
        let drift = profile_reference_drift(Some("a"), Some("b"));
        assert_eq!(
            drift,
            vec![ProfileRefDrift {
                source: ProfileRefOrigin::Registry,
                stale_value: "b".to_string(),
            }]
        );
    }

    /// Both answers come off one resolved row, so the source that won
    /// can never also be reported as stale, and every lower source
    /// naming something else must be. Swept over all nine combinations
    /// of the two sources rather than the five spot cases above.
    ///
    /// This pins the relationship between the two answers, not the
    /// number of priority lists behind them: with only two sources
    /// there is at most one loser, so the order a second list would
    /// impose is unobservable from the outside. What rules a second
    /// list out is structural — `profile_candidates` is the only place
    /// the order is written down.
    ///
    /// The two assertions differ in what they can catch. The first
    /// holds by construction, since drift is filtered on the winning
    /// value itself, and stands as a guard against an implementation
    /// that stops filtering that way. The discriminating one is the
    /// second: it recomputes the expected sources straight from the
    /// inputs rather than from anything the module derives, so a
    /// dropped, extra, or mislabelled source fails it.
    #[test]
    fn the_winning_source_is_never_also_reported_as_stale() {
        let sources = [None, Some("a"), Some("b")];
        for manifest in sources {
            for registry in sources {
                let effective = effective_profile_reference(manifest, registry);
                let drift = profile_reference_drift(manifest, registry);
                let context = format!("manifest={manifest:?} registry={registry:?}");

                let Some((winning_value, winner)) = effective else {
                    assert!(
                        drift.is_empty(),
                        "no effective reference, yet drift reported: {context}"
                    );
                    continue;
                };

                assert!(
                    !drift.iter().any(|d| d.source == winner),
                    "the winning source {winner:?} is also reported as stale: {context}"
                );

                let expected: Vec<ProfileRefOrigin> = [
                    (ProfileRefOrigin::Manifest, manifest),
                    (ProfileRefOrigin::Registry, registry),
                ]
                .into_iter()
                .filter(|&(source, value)| {
                    source != winner && value.is_some_and(|v| v != winning_value)
                })
                .map(|(source, _)| source)
                .collect();
                assert_eq!(
                    drift.iter().map(|d| d.source).collect::<Vec<_>>(),
                    expected,
                    "{context}"
                );
            }
        }
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
