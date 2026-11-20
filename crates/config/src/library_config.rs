// SPDX-License-Identifier: Apache-2.0

//! The values that belong to one library.

use bookrack_core::knob::KnobOrigin;

use crate::{EmbedConfig, RootConfig, SearchConfig};

/// What one library decides for itself: its retrieval preferences, from
/// its own `config.toml`, and the embed model its index profile
/// declares.
///
/// [`LibraryConfig::resolve`] takes no environment. A library's values
/// travel with the library; what the process was started under cannot
/// move them.
#[derive(Debug, Clone)]
pub struct LibraryConfig {
    /// Retrieval preferences.
    pub search: SearchConfig,
    /// The embed model the library's vectors are built with.
    pub embed_model: String,
    origins: Vec<KnobOrigin>,
}

impl LibraryConfig {
    /// Resolve from the library's root config and the embed model its
    /// index profile declares, `None` when no profile is in effect.
    pub fn resolve(root: &RootConfig, profile_model: Option<&str>) -> LibraryConfig {
        let (embed, embed_rows) =
            EmbedConfig::resolve_with_origins_from(|_| None, None, profile_model);
        let (search, search_rows) = SearchConfig::resolve_with_origins_from(root);
        let mut origins: Vec<KnobOrigin> = embed_rows
            .into_iter()
            .filter(|row| row.key == "embed.model")
            .collect();
        origins.extend(search_rows);
        LibraryConfig {
            search,
            embed_model: embed.model,
            origins,
        }
    }

    /// One row per value, in a fixed order: where it came from and
    /// every layer that could have supplied it.
    pub fn origins(&self) -> &[KnobOrigin] {
        &self.origins
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_EMBED_MODEL, DEFAULT_SEARCH_TOP_K, RootSearchConfig};

    fn row<'a>(cfg: &'a LibraryConfig, key: &str) -> &'a KnobOrigin {
        cfg.origins()
            .iter()
            .find(|row| row.key == key)
            .unwrap_or_else(|| panic!("no {key} row"))
    }

    #[test]
    fn library_config_rows_and_fields_agree() {
        let root = RootConfig {
            search: Some(RootSearchConfig {
                top_k: Some(7),
                weak_threshold: Some(0.25),
            }),
        };
        let cfg = LibraryConfig::resolve(&root, Some("profile-model"));
        assert_eq!(
            row(&cfg, "search.top_k").value.as_deref(),
            Some(cfg.search.top_k.to_string().as_str())
        );
        assert_eq!(cfg.search.top_k, 7);
        assert_eq!(
            row(&cfg, "search.weak_threshold").value.as_deref(),
            Some(cfg.search.weak_distance_threshold.to_string().as_str())
        );
        assert_eq!(
            row(&cfg, "embed.model").value.as_deref(),
            Some(cfg.embed_model.as_str())
        );
        assert_eq!(cfg.embed_model, "profile-model");
        assert_eq!(cfg.origins().len(), 3);
    }

    #[test]
    fn an_empty_root_config_and_no_profile_resolve_the_defaults() {
        let cfg = LibraryConfig::resolve(&RootConfig::default(), None);
        assert_eq!(cfg.search.top_k, DEFAULT_SEARCH_TOP_K);
        assert_eq!(cfg.embed_model, DEFAULT_EMBED_MODEL);
    }
}
