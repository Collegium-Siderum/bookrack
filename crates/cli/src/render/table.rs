// SPDX-License-Identifier: Apache-2.0

//! Table renderers built on [`comfy_table`].
//!
//! Two flavours: [`KvTable`] for two-column key/value cards (used by
//! `show` style commands), and [`RowTable`] for headered row layouts
//! (used by `list` / `find` commands). The wrappers stay opinionated
//! so subcommand modules do not learn the underlying crate's API.

use anstyle::Style;
use comfy_table::{Cell, ContentArrangement, Table, presets::UTF8_BORDERS_ONLY};
use serde_json::Value;

use super::ctx;

/// Two-column key/value renderer for `show`-style commands.
pub struct KvTable {
    inner: Table,
}

impl KvTable {
    pub fn new() -> Self {
        let mut inner = Table::new();
        inner
            .load_preset(UTF8_BORDERS_ONLY)
            .set_content_arrangement(ContentArrangement::Dynamic);
        Self { inner }
    }

    /// Appends a `(key, value)` row.
    pub fn push(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        let key_cell = Cell::new(key.into());
        let value_cell = Cell::new(value.into());
        self.inner.add_row([key_cell, value_cell]);
        self
    }

    /// Renders the table to a `String`.
    pub fn render(&self) -> String {
        self.inner.to_string()
    }
}

impl Default for KvTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Multi-column row renderer for `list` / `find` style commands.
pub struct RowTable {
    inner: Table,
}

impl RowTable {
    pub fn new<H>(headers: H) -> Self
    where
        H: IntoIterator,
        H::Item: Into<String>,
    {
        let mut inner = Table::new();
        inner
            .load_preset(UTF8_BORDERS_ONLY)
            .set_content_arrangement(ContentArrangement::Dynamic);
        inner.set_header(headers.into_iter().map(|h| Cell::new(h.into())));
        Self { inner }
    }

    /// Appends one row. Cells are coerced to strings.
    pub fn push_row<R>(&mut self, row: R) -> &mut Self
    where
        R: IntoIterator,
        R::Item: Into<String>,
    {
        self.inner
            .add_row(row.into_iter().map(|c| Cell::new(c.into())));
        self
    }

    /// Renders the table to a `String`.
    pub fn render(&self) -> String {
        self.inner.to_string()
    }
}

/// Walk a JSON value into a [`KvTable`]. Scalars become rows with
/// dot-notation keys; arrays render as a compact JSON string so the
/// table stays narrow.
pub fn flatten_into_kv(table: &mut KvTable, prefix: &str, value: &Value) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let next = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_into_kv(table, &next, v);
            }
        }
        Value::Null => {
            table.push(prefix, "");
        }
        scalar @ (Value::Bool(_) | Value::Number(_) | Value::String(_)) => {
            let s = match scalar {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            table.push(prefix, s);
        }
        Value::Array(arr) => {
            let compact = serde_json::to_string(arr).unwrap_or_else(|_| "[…]".to_string());
            table.push(prefix, compact);
        }
    }
}

/// Returns the bold style if the active context allows color.
pub fn bold() -> Style {
    if ctx().color_enabled() {
        Style::new().bold()
    } else {
        Style::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_table_render_round_trip() {
        let mut t = KvTable::new();
        t.push("title", "x").push("year", "1999");
        let s = t.render();
        assert!(s.contains("title"));
        assert!(s.contains("1999"));
    }

    #[test]
    fn row_table_render_with_header() {
        let mut t = RowTable::new(["id8", "kind", "state"]);
        t.push_row(["abcd1234", "book", "Done"]);
        let s = t.render();
        assert!(s.contains("id8"));
        assert!(s.contains("abcd1234"));
        assert!(s.contains("Done"));
    }

    #[test]
    fn flatten_renders_a_sectioned_card_with_dotted_keys() {
        let card = serde_json::json!({
            "daemon": {
                "version": "0.1.0",
                "pid": 4242,
                "state": "idle",
                "control": null,
            },
            "library": {
                "name": "main",
                "chunks": 182430,
            },
            "queue": {
                "pending": 0,
                "worker": "enabled",
            },
        });
        let mut t = KvTable::new();
        flatten_into_kv(&mut t, "", &card);
        let s = t.render();
        for needle in [
            "daemon.version",
            "0.1.0",
            "daemon.pid",
            "4242",
            "daemon.state",
            "idle",
            "daemon.control",
            "library.name",
            "main",
            "library.chunks",
            "182430",
            "queue.pending",
            "queue.worker",
            "enabled",
        ] {
            assert!(s.contains(needle), "missing {needle:?} in:\n{s}");
        }
    }
}
