// SPDX-License-Identifier: Apache-2.0

//! The library registry: a TOML file mapping library names to data roots.
//!
//! Each library is an independent data root — its own intake store,
//! databases, and vector store. The registry gives those roots short
//! names so a caller can select one with `--library <name>` instead of
//! re-exporting the data-root variable. The file is named by the
//! `BOOKRACK_REGISTRY` environment variable; it is optional, and only
//! `--library` requires it.
//!
//! ```toml
//! default = "prod"
//! [libraries]
//! prod = "/abs/path/to/prod-root"
//! test = "/abs/path/to/test-root"
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

/// A parsed registry: named libraries and an optional default.
#[derive(Debug, Clone, Deserialize)]
pub struct Registry {
    /// Library used when no `--library`, `--data-dir`, or data-root
    /// variable selects one. Optional.
    #[serde(default)]
    pub default: Option<String>,
    /// Named libraries, each mapping to an absolute data root.
    #[serde(default)]
    pub libraries: HashMap<String, PathBuf>,
}

/// Parse a registry from TOML text. Pure: takes the text, touches no
/// filesystem, so the registry shape can be tested without temp files.
pub fn parse_registry(text: &str) -> Result<Registry, toml::de::Error> {
    toml::from_str(text)
}
