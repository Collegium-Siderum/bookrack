// SPDX-License-Identifier: Apache-2.0

//! Where one configuration knob's effective value came from.
//!
//! A knob is resolved by walking its layers from the highest authority
//! down and taking the first that offers a value. [`resolve_knob`] does
//! that walk and, in the same pass, records the lower layers that did
//! hold a value and lost — the part a caller cannot reconstruct after
//! the fact without re-running the priority chain.
//!
//! The layer sequence is the caller's, not this module's: knobs differ
//! in which layers can even speak for them, so a knob passes its own
//! candidates in descending priority and [`resolve_knob`] never
//! consults a built-in chain.

use serde::Serialize;

/// Which layer supplied a value.
///
/// Declared and ordered from the highest authority to the lowest, so
/// `Ord` is the priority order: a layer that compares less eclipses
/// every layer that compares greater. Not every knob draws on every
/// layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    /// A command-line flag on this invocation.
    Flag,
    /// A variable in the real process environment.
    Environment,
    /// A key the dotenv file supplied because the real environment
    /// carried none. Strictly below [`Layer::Environment`]: the loader
    /// only fills gaps.
    Dotenv,
    /// The data root's `config.toml`. Names that one file; a manifest
    /// is also a file but is its own layer.
    File,
    /// The data root's manifest, which travels with the data.
    Manifest,
    /// The machine's library registry, a regenerable cache.
    Registry,
    /// A platform convention, such as an XDG directory or the
    /// per-user cache directory.
    Platform,
    /// The value compiled in.
    Default,
}

impl Layer {
    /// Stable label for human rendering, matching the serde
    /// `snake_case` token of the JSON form.
    pub fn as_str(self) -> &'static str {
        match self {
            Layer::Flag => "flag",
            Layer::Environment => "environment",
            Layer::Dotenv => "dotenv",
            Layer::File => "file",
            Layer::Manifest => "manifest",
            Layer::Registry => "registry",
            Layer::Platform => "platform",
            Layer::Default => "default",
        }
    }
}

/// How far a knob's value reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KnobReach {
    /// A property of one library; a second library may hold another.
    Library,
    /// A property of this machine, shared by every library on it.
    Machine,
    /// A property of one process, fixed for its lifetime.
    Process,
    /// Re-read per operation; two calls in one process may differ.
    PerCall,
}

impl KnobReach {
    /// Stable label for human rendering, matching the serde
    /// `snake_case` token of the JSON form.
    pub fn as_str(self) -> &'static str {
        match self {
            KnobReach::Library => "library",
            KnobReach::Machine => "machine",
            KnobReach::Process => "process",
            KnobReach::PerCall => "per_call",
        }
    }
}

/// When in a process's life the value is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadAt {
    /// Before the data root is resolved, so it cannot depend on one.
    BeforeResolution,
    /// While the data root is being resolved.
    DuringResolution,
    /// After the data root is known, from the resolved configuration.
    AfterResolution,
    /// On each operation that needs it.
    PerCall,
}

impl ReadAt {
    /// Stable label for human rendering, matching the serde
    /// `snake_case` token of the JSON form.
    pub fn as_str(self) -> &'static str {
        match self {
            ReadAt::BeforeResolution => "before_resolution",
            ReadAt::DuringResolution => "during_resolution",
            ReadAt::AfterResolution => "after_resolution",
            ReadAt::PerCall => "per_call",
        }
    }
}

/// One layer's offer for a knob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    /// The layer making the offer.
    pub layer: Layer,
    /// Where the layer holds it: a variable name, a file path, a flag.
    pub site: String,
    /// The rendered value, or `None` when this layer says nothing.
    ///
    /// A layer whose raw text is present but unusable — blank, or
    /// malformed for the knob's type — offers `None`, matching the
    /// resolvers that treat both as unset. Such a layer is therefore
    /// not a [`Shadowed`] one either: it did not lose, it abstained.
    pub value: Option<String>,
}

impl Candidate {
    /// One layer's offer, with the site it is held at.
    pub fn of(layer: Layer, site: impl Into<String>, value: Option<String>) -> Candidate {
        Candidate {
            layer,
            site: site.into(),
            value,
        }
    }
}

/// A lower layer that held a value and lost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Shadowed {
    /// The layer holding the losing value.
    pub layer: Layer,
    /// Where that layer holds it.
    pub site: String,
    /// The value it offered.
    pub value: String,
}

/// One row of the effective-configuration table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KnobOrigin {
    /// The knob's dotted key, e.g. `search.top_k`.
    pub key: String,
    /// The effective value, or `None` when no layer offered one.
    pub value: Option<String>,
    /// The layer the value came from. With no layer offering one, the
    /// lowest candidate layer — the one that was meant to backstop.
    pub layer: Layer,
    /// Where that layer holds it.
    pub site: String,
    /// Every lower layer that offered a value and lost, in priority
    /// order.
    pub shadowed: Vec<Shadowed>,
    /// How far this knob's value reaches.
    pub reach: KnobReach,
    /// When in a process's life it is read.
    pub read_at: ReadAt,
}

/// What a dotenv load supplied, as much of it as a knob row needs.
///
/// The loader writes the file's keys into the process environment, so
/// by the time anything reads one the two are indistinguishable. This
/// is the record that tells them apart, borrowed rather than owned so
/// every crate reporting a knob can consult one load.
#[derive(Debug, Clone, Copy)]
pub struct DotenvSupply<'a> {
    /// The file that was read.
    pub path: &'a str,
    /// The keys it supplied, in whatever order the caller holds them.
    pub supplied: &'a [String],
}

impl DotenvSupply<'_> {
    /// Whether this load is what put `name` in the environment.
    pub fn supplied(&self, name: &str) -> bool {
        self.supplied.iter().any(|key| key == name)
    }
}

/// The environment and dotenv layers for one variable, in that order.
///
/// At most one carries a value: the loader only fills gaps, so a key
/// the real environment already held was never written by the file.
///
/// With no load to consult the result is a single environment
/// candidate rather than two with the second empty — "no dotenv layer
/// in this process" and "the file supplied nothing" are different
/// claims, and only the first is true of a process that never loaded
/// one.
pub fn env_layers(
    dotenv: Option<DotenvSupply<'_>>,
    name: &str,
    raw: Option<String>,
) -> Vec<Candidate> {
    match dotenv.filter(|load| load.supplied(name)) {
        Some(load) => vec![
            Candidate::of(Layer::Environment, name, None),
            Candidate::of(Layer::Dotenv, load.path, raw),
        ],
        None => vec![Candidate::of(Layer::Environment, name, raw)],
    }
}

/// An environment variable's layers followed by the layers below it.
pub fn env_over(
    dotenv: Option<DotenvSupply<'_>>,
    name: &str,
    raw: Option<String>,
    lower: Vec<Candidate>,
) -> Vec<Candidate> {
    let mut candidates = env_layers(dotenv, name, raw);
    candidates.extend(lower);
    candidates
}

/// Resolve one knob from its layers, recording what it eclipsed.
///
/// `candidates` are the layers that can speak for this knob, in
/// descending priority; the first offering a value wins and every
/// lower one that also offered a value is recorded as [`Shadowed`].
/// The order is the caller's because layer sequences differ per knob,
/// and a debug assertion holds callers to it.
///
/// One layer may appear more than once, at different sites: a data
/// root can be named by either of two flags, and PDFium's requirement
/// by either of two variables. Order within a layer is then the
/// caller's too, and the site is what tells the entries apart.
///
/// `candidates` must be non-empty: a knob with no layer at all has
/// nothing to report. An empty list yields a valueless
/// [`Layer::Default`] row rather than a panic in release.
pub fn resolve_knob(
    key: impl Into<String>,
    reach: KnobReach,
    read_at: ReadAt,
    candidates: Vec<Candidate>,
) -> KnobOrigin {
    debug_assert!(
        !candidates.is_empty(),
        "resolve_knob needs at least one candidate layer"
    );
    debug_assert!(
        candidates.windows(2).all(|w| w[0].layer <= w[1].layer),
        "candidates must descend in priority, highest layer first"
    );

    let backstop = candidates
        .last()
        .map(|c| (c.layer, c.site.clone()))
        .unwrap_or((Layer::Default, String::new()));

    let mut winner: Option<Candidate> = None;
    let mut shadowed = Vec::new();

    for candidate in candidates {
        match (&winner, candidate.value) {
            (_, None) => {}
            (None, Some(value)) => {
                winner = Some(Candidate {
                    layer: candidate.layer,
                    site: candidate.site,
                    value: Some(value),
                });
            }
            (Some(_), Some(value)) => shadowed.push(Shadowed {
                layer: candidate.layer,
                site: candidate.site,
                value,
            }),
        }
    }

    let key = key.into();
    match winner {
        Some(c) => KnobOrigin {
            key,
            value: c.value,
            layer: c.layer,
            site: c.site,
            shadowed,
            reach,
            read_at,
        },
        None => KnobOrigin {
            key,
            value: None,
            layer: backstop.0,
            site: backstop.1,
            shadowed,
            reach,
            read_at,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(layer: Layer, site: &str, value: Option<&str>) -> Candidate {
        Candidate::of(layer, site, value.map(str::to_string))
    }

    #[test]
    fn shadowed_lists_every_lower_layer_that_had_a_value() {
        let origin = resolve_knob(
            "search.top_k",
            KnobReach::Library,
            ReadAt::AfterResolution,
            vec![
                candidate(Layer::Environment, "BOOKRACK_SEARCH_TOP_K", Some("9")),
                candidate(Layer::File, "search.top_k", Some("5")),
                candidate(Layer::Default, "built-in", Some("5")),
            ],
        );

        assert_eq!(origin.layer, Layer::Environment);
        assert_eq!(origin.value.as_deref(), Some("9"));
        assert_eq!(
            origin.shadowed,
            vec![
                Shadowed {
                    layer: Layer::File,
                    site: "search.top_k".to_string(),
                    value: "5".to_string(),
                },
                Shadowed {
                    layer: Layer::Default,
                    site: "built-in".to_string(),
                    value: "5".to_string(),
                },
            ]
        );
    }

    #[test]
    fn a_layer_with_no_value_is_not_shadowed() {
        let origin = resolve_knob(
            "search.top_k",
            KnobReach::Library,
            ReadAt::AfterResolution,
            vec![
                candidate(Layer::Environment, "BOOKRACK_SEARCH_TOP_K", None),
                candidate(Layer::File, "search.top_k", Some("5")),
                candidate(Layer::Default, "built-in", Some("5")),
            ],
        );

        assert_eq!(origin.layer, Layer::File);
        assert_eq!(
            origin.shadowed,
            vec![Shadowed {
                layer: Layer::Default,
                site: "built-in".to_string(),
                value: "5".to_string(),
            }]
        );
        assert!(
            !origin
                .shadowed
                .iter()
                .any(|s| s.layer == Layer::Environment),
            "a layer that offered nothing must not be reported as shadowed"
        );
    }

    #[test]
    fn a_knob_no_layer_answers_reports_the_layer_that_was_meant_to_backstop() {
        let origin = resolve_knob(
            "session.runtime_dir",
            KnobReach::Process,
            ReadAt::BeforeResolution,
            vec![
                candidate(Layer::Flag, "run --runtime-dir", None),
                candidate(Layer::Environment, "BOOKRACK_RUNTIME_DIR", None),
                candidate(Layer::Platform, "XDG_RUNTIME_DIR", None),
            ],
        );

        assert_eq!(origin.value, None);
        assert_eq!(
            origin.layer,
            Layer::Platform,
            "a valueless row must name the knob's own lowest layer, not a \
             layer it never draws on"
        );
        assert_eq!(origin.site, "XDG_RUNTIME_DIR");
        assert!(origin.shadowed.is_empty());
    }

    #[test]
    #[should_panic(expected = "candidates must descend in priority")]
    fn candidates_given_out_of_priority_order_are_a_bug() {
        resolve_knob(
            "search.top_k",
            KnobReach::Library,
            ReadAt::AfterResolution,
            vec![
                candidate(Layer::File, "search.top_k", Some("5")),
                candidate(Layer::Environment, "BOOKRACK_SEARCH_TOP_K", Some("9")),
            ],
        );
    }

    #[test]
    fn every_label_matches_the_serde_token_it_renders_beside() {
        let layers = [
            Layer::Flag,
            Layer::Environment,
            Layer::Dotenv,
            Layer::File,
            Layer::Manifest,
            Layer::Registry,
            Layer::Platform,
            Layer::Default,
        ];
        for layer in layers {
            assert_eq!(
                serde_json::to_string(&layer).unwrap(),
                format!("\"{}\"", layer.as_str()),
                "{layer:?}"
            );
        }

        for reach in [
            KnobReach::Library,
            KnobReach::Machine,
            KnobReach::Process,
            KnobReach::PerCall,
        ] {
            assert_eq!(
                serde_json::to_string(&reach).unwrap(),
                format!("\"{}\"", reach.as_str()),
                "{reach:?}"
            );
        }

        for read_at in [
            ReadAt::BeforeResolution,
            ReadAt::DuringResolution,
            ReadAt::AfterResolution,
            ReadAt::PerCall,
        ] {
            assert_eq!(
                serde_json::to_string(&read_at).unwrap(),
                format!("\"{}\"", read_at.as_str()),
                "{read_at:?}"
            );
        }
    }

    #[test]
    fn the_layer_order_is_the_priority_order() {
        assert!(Layer::Flag < Layer::Environment);
        assert!(
            Layer::Environment < Layer::Dotenv,
            "the dotenv loader only fills gaps, so its layer is below the \
             real environment"
        );
        assert!(Layer::Dotenv < Layer::File);
        assert!(Layer::File < Layer::Manifest);
        assert!(Layer::Manifest < Layer::Registry);
        assert!(Layer::Registry < Layer::Platform);
        assert!(Layer::Platform < Layer::Default);
    }

    #[test]
    fn the_lowest_layer_can_win() {
        let origin = resolve_knob(
            "search.top_k",
            KnobReach::Library,
            ReadAt::AfterResolution,
            vec![
                candidate(Layer::Environment, "BOOKRACK_SEARCH_TOP_K", None),
                candidate(Layer::File, "search.top_k", None),
                candidate(Layer::Default, "built-in", Some("5")),
            ],
        );

        assert_eq!(origin.layer, Layer::Default);
        assert_eq!(origin.value.as_deref(), Some("5"));
        assert!(
            origin.shadowed.is_empty(),
            "the winning layer must not shadow itself"
        );
    }
}
