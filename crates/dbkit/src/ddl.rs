// SPDX-License-Identifier: Apache-2.0

//! Rendering a [`TableSpec`] to `CREATE TABLE` / `CREATE INDEX` text.
//!
//! This is the function that makes the spec the single source of truth:
//! the DDL is produced from the spec, never hand-written beside it, so
//! the two cannot disagree.

use crate::spec::{ColumnSpec, IndexSpec, OnDelete, PkRole, TableSpec};

/// Render the full DDL for one table: a `CREATE TABLE IF NOT EXISTS`
/// statement followed by one `CREATE INDEX IF NOT EXISTS` per index.
///
/// Every statement is idempotent, so applying the output to a database
/// that already has the table is a no-op. The result ends with a
/// newline, so several rendered tables concatenate into a valid batch.
pub fn render_ddl(spec: &TableSpec) -> String {
    let mut out = String::new();

    if let Some(comment) = spec.comment {
        out.push_str("-- ");
        out.push_str(comment);
        out.push('\n');
    }
    out.push_str("CREATE TABLE IF NOT EXISTS ");
    out.push_str(spec.name);
    out.push_str(" (\n");

    // Each entry pairs a definition line with an optional trailing
    // comment. Columns may carry a comment; table-level clauses never do.
    let mut items: Vec<(String, Option<&'static str>)> = Vec::new();
    for col in spec.columns {
        items.push((render_column(col), col.comment));
    }
    if let Some(pk) = spec.composite_pk {
        items.push((format!("PRIMARY KEY ({})", pk.join(", ")), None));
    }
    for unique in spec.uniques {
        items.push((format!("UNIQUE ({})", unique.join(", ")), None));
    }
    for check in spec.table_checks {
        items.push((format!("CHECK ({check})"), None));
    }

    let last = items.len().saturating_sub(1);
    for (i, (def, comment)) in items.iter().enumerate() {
        out.push_str("  ");
        out.push_str(def);
        // The separating comma must come before the comment: a `--`
        // comment runs to the end of the line and would otherwise
        // swallow the comma and break the column list.
        if i != last {
            out.push(',');
        }
        if let Some(text) = comment {
            out.push_str("  -- ");
            out.push_str(text);
        }
        out.push('\n');
    }
    out.push_str(");\n");

    for index in spec.indexes {
        render_index(&mut out, spec.name, index);
    }
    out
}

/// Render one column definition (without the trailing comma or comment).
fn render_column(col: &ColumnSpec) -> String {
    let mut s = String::new();
    s.push_str(col.name);
    s.push(' ');
    s.push_str(col.sql_type.as_sql());

    match col.pk {
        PkRole::None => {}
        PkRole::Primary => s.push_str(" PRIMARY KEY"),
        PkRole::PrimaryAutoinc => s.push_str(" PRIMARY KEY AUTOINCREMENT"),
    }
    if !col.nullable {
        s.push_str(" NOT NULL");
    }
    if col.unique {
        s.push_str(" UNIQUE");
    }
    if let Some(default) = col.default {
        s.push_str(" DEFAULT ");
        s.push_str(default);
    }
    if let Some(check) = col.check {
        s.push_str(" CHECK (");
        s.push_str(check);
        s.push(')');
    }
    if let Some(fk) = col.references {
        s.push_str(" REFERENCES ");
        s.push_str(fk.table);
        s.push('(');
        s.push_str(fk.column);
        s.push(')');
        match fk.on_delete {
            OnDelete::NoAction => {}
            OnDelete::Cascade => s.push_str(" ON DELETE CASCADE"),
            OnDelete::SetNull => s.push_str(" ON DELETE SET NULL"),
        }
    }
    s
}

/// Append one `CREATE INDEX` statement to `out`.
fn render_index(out: &mut String, table: &str, index: &IndexSpec) {
    out.push_str("CREATE ");
    if index.unique {
        out.push_str("UNIQUE ");
    }
    out.push_str("INDEX IF NOT EXISTS ");
    out.push_str(index.name);
    out.push_str(" ON ");
    out.push_str(table);
    out.push('(');
    out.push_str(&index.columns.join(", "));
    out.push(')');
    if let Some(predicate) = index.where_clause {
        out.push_str(" WHERE ");
        out.push_str(predicate);
    }
    out.push_str(";\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{ColumnSpec, ForeignKey, IndexSpec, OnDelete, TableSpec};

    #[test]
    fn renders_a_plain_table_with_an_index() {
        const SPEC: TableSpec = TableSpec {
            name: "widget",
            comment: Some("A widget."),
            columns: &[
                ColumnSpec::int("widget_id").pk_autoinc(),
                ColumnSpec::text("label").not_null(),
                ColumnSpec::int("size"),
            ],
            composite_pk: None,
            uniques: &[],
            table_checks: &[],
            indexes: &[IndexSpec::on("idx_widget_label", &["label"])],
        };
        let expected = "\
-- A widget.
CREATE TABLE IF NOT EXISTS widget (
  widget_id INTEGER PRIMARY KEY AUTOINCREMENT,
  label TEXT NOT NULL,
  size INTEGER
);
CREATE INDEX IF NOT EXISTS idx_widget_label ON widget(label);
";
        assert_eq!(render_ddl(&SPEC), expected);
    }

    #[test]
    fn renders_composite_key_check_foreign_key_and_partial_index() {
        const SPEC: TableSpec = TableSpec {
            name: "edit",
            comment: None,
            columns: &[
                ColumnSpec::int("node_id")
                    .not_null()
                    .references(ForeignKey::new("nodes", "node_id", OnDelete::Cascade)),
                ColumnSpec::text("field")
                    .not_null()
                    .comment("the edited field"),
                ColumnSpec::text("kind")
                    .not_null()
                    .check("kind IN ('a', 'b')"),
                ColumnSpec::int("flag").not_null().default("0"),
            ],
            composite_pk: Some(&["node_id", "field"]),
            uniques: &[],
            table_checks: &[],
            indexes: &[IndexSpec::on("idx_edit_kind", &["kind"]).partial("kind = 'a'")],
        };
        let expected = "\
CREATE TABLE IF NOT EXISTS edit (
  node_id INTEGER NOT NULL REFERENCES nodes(node_id) ON DELETE CASCADE,
  field TEXT NOT NULL,  -- the edited field
  kind TEXT NOT NULL CHECK (kind IN ('a', 'b')),
  flag INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (node_id, field)
);
CREATE INDEX IF NOT EXISTS idx_edit_kind ON edit(kind) WHERE kind = 'a';
";
        assert_eq!(render_ddl(&SPEC), expected);
    }

    #[test]
    fn renders_a_unique_index() {
        const SPEC: TableSpec = TableSpec {
            name: "tag",
            comment: None,
            columns: &[ColumnSpec::text("name").not_null()],
            composite_pk: None,
            uniques: &[],
            table_checks: &[],
            indexes: &[IndexSpec::on("idx_tag_name", &["name"]).unique()],
        };
        assert!(render_ddl(&SPEC).contains("CREATE UNIQUE INDEX IF NOT EXISTS idx_tag_name"));
    }
}
