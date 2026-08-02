// SPDX-License-Identifier: Apache-2.0

//! Every knob this crate resolves, with where each value came from.
//!
//! The rows are a by-product of resolution, not a second pass over it:
//! each resolver builds its candidate layers, hands them to
//! [`resolve_knob`], and reads its own return value back off the row it
//! got. A report that re-walked the priority chain could drift from
//! what the resolvers actually do, which is the failure this module
//! exists to make impossible.

use bookrack_core::knob::KnobOrigin;

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

/// The data-root row: the rung the resolution recorded, or — with no
/// resolution to report — what the environment asked for, so a failed
/// resolution still says what it was pointed at.
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
    use bookrack_core::knob::Layer;

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
