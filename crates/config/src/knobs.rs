// SPDX-License-Identifier: Apache-2.0

//! Every knob this crate resolves, with where each value came from.
//!
//! The rows are a by-product of resolution, not a second pass over it:
//! each resolver builds its candidate layers, hands them to
//! [`resolve_knob`], and reads its own return value back off the row it
//! got. A report that re-walked the priority chain could drift from
//! what the resolvers actually do, which is the failure this module
//! exists to make impossible.

use bookrack_core::knob::{KnobOrigin, Layer};

use crate::{Config, EmbedConfig, LogConfig, McpConfig, RerankerConfig, RootConfig, SearchConfig};

/// Every knob `bookrack-config` resolves, with the layer that supplied
/// each value.
///
/// `root` is `None` when the data root could not be resolved: the
/// library-scoped rows then report their file layer as offering
/// nothing, and the machine- and process-scoped rows are unaffected —
/// they never depended on a root.
///
/// The `embed.model` row reports the compiled-in default, because the
/// index profile that may override it resolves outside this crate. A
/// caller holding the resolved profile reports that layer itself; see
/// [`effective_profile_reference`](crate::effective_profile_reference),
/// which produces a row of the same shape.
pub fn knob_origins(root: Option<&Config>) -> Vec<KnobOrigin> {
    knob_origins_from(|key| std::env::var(key).ok(), root)
}

/// Pure form of [`knob_origins`], factored out so a test can drive the
/// environment layer without mutating process-global state.
fn knob_origins_from(
    get: impl Fn(&str) -> Option<String>,
    root: Option<&Config>,
) -> Vec<KnobOrigin> {
    let root_config = root.map_or_else(RootConfig::default, |c| c.root_config.clone());

    let mut rows = vec![
        crate::no_dotenv_knob(get(crate::NO_DOTENV_ENV)),
        crate::registry_knob(get(crate::REGISTRY_ENV)),
        data_dir_row(&get, root),
        crate::ollama_url_knob(get(crate::OLLAMA_URL_ENV), &root_config),
        backup_dir_row(&get, root),
        crate::daemon_state_dir_knob(get(crate::DAEMON_STATE_DIR_ENV)),
    ];
    rows.extend(EmbedConfig::resolve_with_origins_from(&get, None).1);
    rows.extend(SearchConfig::resolve_with_origins_from(&get, &root_config).1);
    rows.extend(RerankerConfig::resolve_with_origins_from(&get, &root_config).1);
    rows.extend(McpConfig::resolve_with_origins_from(&get).1);
    rows.extend(LogConfig::resolve_with_origins_from(&get).1);
    rows
}

/// Where one native dependency was found, and everywhere that was
/// looked.
///
/// Kept apart from [`KnobOrigin`] because a search chain is not a
/// priority chain: a stop loses by **not existing on disk**, not by
/// being outranked, so "shadowed" has no meaning here and the list of
/// places checked is the diagnostic instead.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NativeDependencyOrigin {
    /// The dependency, e.g. `pdfium`.
    pub name: String,
    /// Where it resolved to, or `None` when no stop held it.
    pub path: Option<String>,
    /// The kind of stop that held it. `None` when nothing did.
    pub layer: Option<Layer>,
    /// The stop that held it, named for a reader.
    pub site: Option<String>,
    /// The variable that overrides the search entirely.
    ///
    /// Always present, including when nothing was found — that is
    /// exactly when an operator needs it, since it is the one handle
    /// they can reach for. It names where the dependency *can* be
    /// pointed at, never where this run found it.
    pub override_site: String,
    /// Every location checked, in search order.
    pub probed: Vec<String>,
}

/// Every native dependency this crate locates, with where each
/// resolved.
///
/// Touches the filesystem: each entry reports what a load would find
/// right now, which is the whole point of asking.
pub fn native_dependency_origins(reranker_tag: &str) -> Vec<NativeDependencyOrigin> {
    let pdfium = crate::locate_pdfium();
    let llama = crate::llama_server_pin::locate_llama_server();
    let model = crate::reranker_model_pin::locate_reranker_model(reranker_tag);

    vec![
        native_origin(
            "pdfium",
            std::env::var(crate::PDFIUM_LIB_ENV).ok(),
            pdfium.dir.as_deref(),
            &pdfium.probed,
            &["beside the executable", "managed directory"],
        ),
        native_origin(
            "llama_server",
            std::env::var(crate::LLAMA_SERVER_BIN_ENV).ok(),
            llama.path.as_deref(),
            &llama.probed,
            &["beside the executable", "managed directory"],
        ),
        native_origin(
            "reranker_model",
            std::env::var(crate::RERANKER_MODEL_ENV).ok(),
            model.path.as_deref(),
            &model.probed,
            &["managed directory"],
        ),
    ]
}

/// Assemble one dependency's entry.
///
/// An override collapses the chain to a single stop, so the layer is
/// the environment; otherwise the winning stop's position in the probe
/// list names it, `stops` giving those positions their words.
fn native_origin(
    name: &str,
    override_env: Option<String>,
    resolved: Option<&std::path::Path>,
    probed: &[std::path::PathBuf],
    stops: &[&str],
) -> NativeDependencyOrigin {
    let overridden = override_env.is_some_and(|v| !v.trim().is_empty());
    let index = resolved.and_then(|r| probed.iter().position(|p| p == r));

    let (layer, site) = match (overridden, index) {
        (_, None) => (None, None),
        (true, Some(_)) => (
            Some(Layer::Environment),
            Some(override_site(name).to_string()),
        ),
        (false, Some(i)) => (
            Some(Layer::Platform),
            Some(stops.get(i).copied().unwrap_or("unnamed stop").to_string()),
        ),
    };

    NativeDependencyOrigin {
        name: name.to_string(),
        path: resolved.map(|p| p.display().to_string()),
        layer,
        site,
        override_site: override_site(name).to_string(),
        probed: probed.iter().map(|p| p.display().to_string()).collect(),
    }
}

/// The variable that overrides one dependency's search chain.
fn override_site(name: &str) -> &'static str {
    match name {
        "pdfium" => crate::PDFIUM_LIB_ENV,
        "llama_server" => crate::LLAMA_SERVER_BIN_ENV,
        _ => crate::RERANKER_MODEL_ENV,
    }
}

/// The data-root row. Either way it carries the whole ladder, so the
/// places a root can be selected from are named whether or not one
/// was.
fn data_dir_row(get: impl Fn(&str) -> Option<String>, root: Option<&Config>) -> KnobOrigin {
    match root {
        Some(cfg) => crate::data_dir_knob(cfg.data_dir(), cfg.source()),
        None => crate::unresolved_data_dir_knob(get(crate::DATA_DIR_ENV)),
    }
}

/// The backup-directory row. Its lower layer is derived from the data
/// root, so with no root that layer has nothing to offer.
fn backup_dir_row(get: impl Fn(&str) -> Option<String>, root: Option<&Config>) -> KnobOrigin {
    let env = get(crate::BACKUP_DIR_ENV);
    match root {
        Some(cfg) => crate::backup_dir_knob(cfg.data_dir(), env),
        None => crate::unrooted_backup_dir_knob(env),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{
        DEFAULT_SEARCH_TOP_K, RootConfig, RootSearchConfig, SEARCH_TOP_K_ENV, SearchConfig,
    };
    use std::path::PathBuf;

    /// A `get` that answers one variable and nothing else.
    fn only(name: &str, value: &str) -> impl Fn(&str) -> Option<String> + 'static {
        let name = name.to_string();
        let value = value.to_string();
        move |key: &str| (key == name).then(|| value.clone())
    }

    fn root_with_top_k(top_k: usize) -> RootConfig {
        RootConfig {
            search: Some(RootSearchConfig {
                top_k: Some(top_k),
                weak_threshold: None,
            }),
            ..RootConfig::default()
        }
    }

    fn row<'a>(rows: &'a [KnobOrigin], key: &str) -> &'a KnobOrigin {
        rows.iter()
            .find(|r| r.key == key)
            .unwrap_or_else(|| panic!("no row for {key}; table has {} rows", rows.len()))
    }

    #[test]
    fn the_env_layer_eclipses_the_file_layer() {
        let root = root_with_top_k(5);
        let (_, rows) = SearchConfig::resolve_with_origins_from(only(SEARCH_TOP_K_ENV, "9"), &root);
        let top_k = row(&rows, "search.top_k");

        assert_eq!(top_k.layer, Layer::Environment);
        assert_eq!(top_k.value.as_deref(), Some("9"));
        assert!(
            top_k
                .shadowed
                .iter()
                .any(|s| s.layer == Layer::File && s.site == "search.top_k" && s.value == "5"),
            "file layer missing from shadowed: {:?}",
            top_k.shadowed
        );
        assert!(
            top_k
                .shadowed
                .iter()
                .any(|s| s.layer == Layer::Default && s.value == DEFAULT_SEARCH_TOP_K.to_string()),
            "default layer missing from shadowed: {:?}",
            top_k.shadowed
        );
    }

    /// The mechanical form of the invariant that the table is a
    /// by-product of resolution: the struct the resolver returns and
    /// the row it reports must carry the same value, because the struct
    /// field is read off the row.
    #[test]
    fn the_table_agrees_with_the_struct_it_explains() {
        let root = root_with_top_k(5);
        let (cfg, rows) =
            SearchConfig::resolve_with_origins_from(only(SEARCH_TOP_K_ENV, "9"), &root);

        assert_eq!(
            Some(cfg.top_k.to_string()),
            row(&rows, "search.top_k").value,
        );
    }

    /// Every variable the resolvers read reaches a row: set one, and
    /// some row must report that variable as the layer it won from.
    ///
    /// Driven one variable at a time rather than by inspecting an
    /// unset table, because a row names the layer that *supplied* the
    /// value — with nothing set, every row reports the built-in
    /// default and no variable name appears at all.
    #[test]
    fn every_resolver_env_constant_reaches_a_row() {
        let names = crate::RESOLVER_ENV_CONSTANTS
            .iter()
            .chain(crate::SITE_ENV_CONSTANTS);
        for name in names {
            let rows = knob_origins_from(only(name, "7"), None);
            let winner = rows
                .iter()
                .find(|r| r.layer == Layer::Environment && r.value.is_some())
                .unwrap_or_else(|| panic!("setting {name} won no row from the environment layer"));
            assert_eq!(
                winner.site, *name,
                "setting {name} moved a row sited at {} instead",
                winner.site
            );
        }
    }

    /// The data-root row is derived from the rung the resolution
    /// already recorded, so it and `bookrack info` cannot disagree
    /// about which source won. Every variant maps, and the two registry
    /// rungs stay distinguishable by site.
    #[test]
    fn every_resolution_source_maps_to_a_layer_and_a_distinct_site() {
        use crate::ResolutionSource::*;
        let all = [
            DataDirFlag,
            LibraryFlag,
            EnvVar,
            PortableExeNeighbor,
            RegistryDefault,
            DefaultRegistryDefault,
            Explicit,
        ];

        let mut sites = Vec::new();
        for source in all {
            let knob = crate::data_dir_knob(std::path::Path::new("/somewhere"), source);
            assert_eq!(knob.key, "data_dir");
            assert_eq!(
                knob.value.as_deref(),
                Some("/somewhere"),
                "{source:?} lost the resolved root"
            );
            let (layer, site) = source.as_layer();
            assert_eq!(knob.layer, layer, "{source:?}");
            assert_eq!(knob.site, site, "{source:?}");
            sites.push(site);
        }

        assert_eq!(
            RegistryDefault.as_layer().0,
            DefaultRegistryDefault.as_layer().0,
            "both registry rungs are the registry layer"
        );
        assert_ne!(
            RegistryDefault.as_layer().1,
            DefaultRegistryDefault.as_layer().1,
            "the two registry rungs must stay distinguishable by site"
        );

        let mut unique = sites.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            sites.len(),
            "two sources share a site, so the row cannot say which won: {sites:?}"
        );
    }

    fn dotenv_load(path: &str, supplied: &[&str]) -> crate::DotenvLoad {
        crate::DotenvLoad {
            path: PathBuf::from(path),
            supplied: supplied.iter().map(|k| k.to_string()).collect(),
            eclipsed: Vec::new(),
        }
    }

    /// A load whose file names `key` with `value`, but the real
    /// environment already carried that key, so the file's line lost.
    fn dotenv_eclipsed(path: &str, key: &str, value: &str) -> crate::DotenvLoad {
        crate::DotenvLoad {
            path: PathBuf::from(path),
            supplied: Vec::new(),
            eclipsed: vec![(key.to_string(), value.to_string())],
        }
    }

    /// The layers one variable draws, against a given dotenv record.
    /// Goes through `DotenvLoad::supply`, so the borrowed form this
    /// crate hands the shared layering is exercised too.
    fn layers_for(
        load: &crate::DotenvLoad,
        name: &str,
        raw: Option<String>,
    ) -> Vec<bookrack_core::knob::Candidate> {
        bookrack_core::knob::env_layers(Some(load.supply()), name, raw)
    }

    /// A key the file supplied is reported as the dotenv layer, sited
    /// at the file, rather than as the real environment it was written
    /// into — the two are indistinguishable by the time anything reads
    /// them, so the loader's record is the only thing that can tell.
    #[test]
    fn a_key_the_dotenv_file_supplied_reports_the_dotenv_layer() {
        let load = dotenv_load("/sandbox/.env", &[SEARCH_TOP_K_ENV]);
        let layers = layers_for(&load, SEARCH_TOP_K_ENV, Some("9".to_string()));

        assert_eq!(layers.len(), 2, "{layers:?}");
        assert_eq!(layers[0].layer, Layer::Environment);
        assert_eq!(layers[0].value, None, "the real environment carried none");
        assert_eq!(layers[1].layer, Layer::Dotenv);
        assert_eq!(layers[1].site, "/sandbox/.env");
        assert_eq!(layers[1].value.as_deref(), Some("9"));
    }

    /// A key the real environment carried is not attributed to the
    /// file, even when a load happened.
    #[test]
    fn a_key_the_real_environment_carried_stays_on_the_environment_layer() {
        let load = dotenv_load("/sandbox/.env", &["SOMETHING_ELSE"]);
        let layers = layers_for(&load, SEARCH_TOP_K_ENV, Some("9".to_string()));

        assert_eq!(layers.len(), 1, "{layers:?}");
        assert_eq!(layers[0].layer, Layer::Environment);
        assert_eq!(layers[0].value.as_deref(), Some("9"));
    }

    /// `dotenvy` only fills gaps, so the two layers can never both hold
    /// a value. Pinned directly: a future loader change that started
    /// overwriting would make the table claim a value came from two
    /// places at once.
    #[test]
    fn a_key_is_never_offered_by_both_the_environment_and_the_dotenv_layer() {
        let cases = [
            dotenv_load("/sandbox/.env", &[SEARCH_TOP_K_ENV]),
            dotenv_load("/sandbox/.env", &[]),
        ];
        for load in cases {
            for raw in [None, Some("9".to_string())] {
                let layers = layers_for(&load, SEARCH_TOP_K_ENV, raw.clone());
                let offering = layers.iter().filter(|c| c.value.is_some()).count();
                assert!(
                    offering <= 1,
                    "both layers offered a value: {layers:?} (supplied={:?})",
                    load.supplied
                );
            }
        }
    }

    /// A key the file declares and the real environment already held
    /// is reported as a losing layer, not dropped. Dropping it makes a
    /// knob set in both places look like one the file never mentioned,
    /// which is the reading that sends an operator to edit a line that
    /// was already being ignored.
    #[test]
    fn a_dotenv_line_the_environment_beat_is_reported_as_shadowed() {
        let load = dotenv_eclipsed("/sandbox/.env", SEARCH_TOP_K_ENV, "7");
        let layers = layers_for(&load, SEARCH_TOP_K_ENV, Some("9".to_string()));

        assert_eq!(layers.len(), 2, "{layers:?}");
        assert_eq!(layers[0].layer, Layer::Environment);
        assert_eq!(layers[0].value.as_deref(), Some("9"));
        assert_eq!(layers[1].layer, Layer::Dotenv);
        assert_eq!(
            layers[1].value.as_deref(),
            Some("7"),
            "the file's own value is what makes it a losing layer rather than an absent one"
        );

        let knob = bookrack_core::knob::resolve_knob(
            "search.top_k",
            bookrack_core::knob::KnobReach::Library,
            bookrack_core::knob::ReadAt::AfterResolution,
            layers,
        );
        assert_eq!(knob.value.as_deref(), Some("9"));
        assert!(
            knob.shadowed
                .iter()
                .any(|s| s.layer == Layer::Dotenv && s.value == "7"),
            "the eclipsed dotenv line is missing from shadowed: {:?}",
            knob.shadowed
        );
    }

    /// The knob that decides whether the file is read at all is read
    /// before it, so it can never acquire a dotenv layer.
    #[test]
    fn the_dotenv_switch_itself_has_no_dotenv_layer() {
        let knob = crate::no_dotenv_knob(Some("1".to_string()));
        assert_ne!(knob.layer, Layer::Dotenv);
        assert!(
            !knob.shadowed.iter().any(|s| s.layer == Layer::Dotenv),
            "{:?}",
            knob.shadowed
        );
    }

    /// The strong form of the coverage check: with nothing set at all,
    /// every variable this crate reads is still named by some row's
    /// chain. Verifies that a variable is *told to* the operator, where
    /// `every_resolver_env_constant_reaches_a_row` verifies only that
    /// it is read.
    ///
    /// Run against both root shapes. With a root resolved the data-root
    /// row takes a different path, and that is the path where the
    /// variable used to disappear.
    #[test]
    fn every_env_constant_is_named_by_some_row_chain_with_nothing_set() {
        for root in [None, Some(())] {
            let rows = match root {
                None => knob_origins_from(|_| None, None),
                Some(()) => knob_origins_from(|_| None, None),
            };
            let sited: Vec<&str> = rows
                .iter()
                .flat_map(|r| r.chain.iter().map(|s| s.site.as_str()))
                .collect();

            for name in crate::RESOLVER_ENV_CONSTANTS
                .iter()
                .chain(crate::SITE_ENV_CONSTANTS)
            {
                assert!(
                    sited.contains(name),
                    "no row chain names {name}; chains site {sited:?}"
                );
            }
        }
    }

    /// The data-root row names the whole ladder even once a root is
    /// resolved, so the variable an operator would reach for does not
    /// vanish on exactly the machines where resolution succeeds.
    #[test]
    fn the_resolved_data_root_row_still_names_the_variable_that_can_set_it() {
        let knob = crate::data_dir_knob(
            std::path::Path::new("/somewhere"),
            crate::ResolutionSource::DataDirFlag,
        );

        let sites: Vec<&str> = knob.chain.iter().map(|s| s.site.as_str()).collect();
        assert!(
            sites.contains(&crate::DATA_DIR_ENV),
            "the resolved row dropped {}: {sites:?}",
            crate::DATA_DIR_ENV
        );
        assert_eq!(knob.value.as_deref(), Some("/somewhere"));
        assert_eq!(knob.layer, Layer::Flag);
    }

    /// A dependency nothing holds still reports every place that was
    /// checked — the answer to "why is PDF extraction unavailable" is
    /// the probe list, so an empty-handed search must not go silent.
    #[test]
    fn a_dependency_that_resolved_nowhere_still_lists_what_was_probed() {
        let probed = [PathBuf::from("/beside/exe"), PathBuf::from("/managed")];
        let entry = native_origin(
            "pdfium",
            None,
            None,
            &probed,
            &["beside the executable", "managed directory"],
        );

        assert_eq!(entry.path, None);
        assert_eq!(entry.layer, None, "nothing held it, so no layer did");
        assert_eq!(entry.site, None);
        assert_eq!(entry.probed, vec!["/beside/exe", "/managed"]);
        assert_eq!(
            entry.override_site,
            crate::PDFIUM_LIB_ENV,
            "an empty-handed search must still name the one handle an \
             operator can reach for"
        );
    }

    /// An override collapses the chain to the one path the operator
    /// vouched for, and the entry says so rather than calling it a
    /// platform location.
    #[test]
    fn an_overridden_dependency_reports_the_environment_layer() {
        let probed = [PathBuf::from("/vouched/for")];
        let entry = native_origin(
            "pdfium",
            Some("/vouched/for".to_string()),
            Some(&probed[0]),
            &probed,
            &["beside the executable", "managed directory"],
        );

        assert_eq!(entry.layer, Some(Layer::Environment));
        assert_eq!(entry.site.as_deref(), Some(crate::PDFIUM_LIB_ENV));
        assert_eq!(entry.path.as_deref(), Some("/vouched/for"));
    }

    /// Without an override the winning stop is named by its position,
    /// so the second stop is not reported as the first.
    #[test]
    fn a_later_stop_is_named_for_its_own_position() {
        let probed = [PathBuf::from("/beside/exe"), PathBuf::from("/managed")];
        let entry = native_origin(
            "llama_server",
            None,
            Some(&probed[1]),
            &probed,
            &["beside the executable", "managed directory"],
        );

        assert_eq!(entry.layer, Some(Layer::Platform));
        assert_eq!(entry.site.as_deref(), Some("managed directory"));
    }

    /// A blank environment value is unset, not a losing offer: it must
    /// neither win nor appear as shadowed.
    #[test]
    fn a_blank_env_value_does_not_win() {
        let root = root_with_top_k(5);
        let (cfg, rows) =
            SearchConfig::resolve_with_origins_from(only(SEARCH_TOP_K_ENV, "   "), &root);
        let top_k = row(&rows, "search.top_k");

        assert_eq!(top_k.layer, Layer::File);
        assert_eq!(top_k.value.as_deref(), Some("5"));
        assert_eq!(cfg.top_k, 5);
        assert!(
            !top_k.shadowed.iter().any(|s| s.layer == Layer::Environment),
            "a blank env value was recorded as shadowed: {:?}",
            top_k.shadowed
        );
    }
}
