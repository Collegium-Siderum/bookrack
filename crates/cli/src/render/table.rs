// SPDX-License-Identifier: Apache-2.0

//! Table renderers built on [`comfy_table`].
//!
//! Two flavours: [`KvTable`] for two-column key/value cards (used by
//! `show` style commands), and [`RowTable`] for headered row layouts
//! (used by `list` / `find` commands). The wrappers stay opinionated
//! so subcommand modules do not learn the underlying crate's API.

use anstyle::Style;
use comfy_table::{Cell, ContentArrangement, Table, presets::UTF8_BORDERS_ONLY};

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
}
