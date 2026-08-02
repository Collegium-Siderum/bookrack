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
    let root_config = root.map_or_else(RootConfig::default, |c| c.root_config.clone());
    knob_origins_from(|key| std::env::var(key).ok(), &root_config)
}

/// Pure form of [`knob_origins`], factored out so a test can drive the
/// environment layer without mutating process-global state.
fn knob_origins_from(
    get: impl Fn(&str) -> Option<String>,
    root_config: &RootConfig,
) -> Vec<KnobOrigin> {
    let mut rows = Vec::new();
    rows.extend(EmbedConfig::resolve_with_origins_from(&get, None).1);
    rows.extend(SearchConfig::resolve_with_origins_from(&get, root_config).1);
    rows.extend(RerankerConfig::resolve_with_origins_from(&get, root_config).1);
    rows.extend(McpConfig::resolve_with_origins_from(&get).1);
    rows.extend(LogConfig::resolve_with_origins_from(&get).1);
    rows
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
        for name in crate::RESOLVER_ENV_CONSTANTS {
            let rows = knob_origins_from(only(name, "7"), &RootConfig::default());
            let winner = rows
                .iter()
                .find(|r| r.layer == Layer::Environment)
                .unwrap_or_else(|| panic!("setting {name} moved no row to the environment layer"));
            assert_eq!(
                winner.site, *name,
                "setting {name} moved a row sited at {} instead",
                winner.site
            );
        }
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
