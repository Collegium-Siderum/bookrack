// SPDX-License-Identifier: Apache-2.0

//! Table descriptors — the single source of truth for one SQLite table.
//!
//! A [`TableSpec`] declares a table's columns, keys, constraints, and
//! indexes as plain Rust data. The `CREATE TABLE` text is rendered from
//! it (see [`crate::render_ddl`]) rather than hand-written, so the DDL
//! and the code that reads it cannot drift apart: there is only one
//! description, and the SQL is one of its projections.
//!
//! Specs are built as `const` values. Column constructors and their
//! chained setters are all `const fn`, so a whole [`TableSpec`] is a
//! compile-time constant with no runtime construction cost.

/// A column's SQLite storage class, as written in the rendered DDL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlType {
    /// `INTEGER` affinity.
    Integer,
    /// `TEXT` affinity.
    Text,
    /// `REAL` affinity.
    Real,
    /// `BLOB` affinity (no affinity, stored as given).
    Blob,
}

impl SqlType {
    /// The keyword written into a `CREATE TABLE` column definition.
    pub const fn as_sql(self) -> &'static str {
        match self {
            SqlType::Integer => "INTEGER",
            SqlType::Text => "TEXT",
            SqlType::Real => "REAL",
            SqlType::Blob => "BLOB",
        }
    }
}

/// Whether and how a single column participates in the primary key.
/// A composite primary key is declared on the [`TableSpec`] instead and
/// leaves every column at [`PkRole::None`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkRole {
    /// Not a single-column primary key.
    None,
    /// `PRIMARY KEY`.
    Primary,
    /// `PRIMARY KEY AUTOINCREMENT` — a never-reused surrogate key.
    PrimaryAutoinc,
}

/// The `ON DELETE` action of a foreign key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDelete {
    /// No `ON DELETE` clause.
    NoAction,
    /// `ON DELETE CASCADE`.
    Cascade,
    /// `ON DELETE SET NULL`.
    SetNull,
}

impl OnDelete {
    /// The string SQLite's `foreign_key_list` pragma reports for this
    /// action, used when verifying a live schema.
    pub const fn pragma_str(self) -> &'static str {
        match self {
            OnDelete::NoAction => "NO ACTION",
            OnDelete::Cascade => "CASCADE",
            OnDelete::SetNull => "SET NULL",
        }
    }
}

/// An intra-database foreign key from one column to another table's
/// column. Cross-database links are bare integer soft references and are
/// never declared as foreign keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForeignKey {
    /// The referenced table.
    pub table: &'static str,
    /// The referenced column.
    pub column: &'static str,
    /// What happens to this row when the referenced row is deleted.
    pub on_delete: OnDelete,
}

impl ForeignKey {
    /// A foreign key to `table(column)` with the given delete action.
    pub const fn new(table: &'static str, column: &'static str, on_delete: OnDelete) -> ForeignKey {
        ForeignKey {
            table,
            column,
            on_delete,
        }
    }
}

/// One column of a table.
///
/// Start from a type constructor ([`ColumnSpec::int`], [`ColumnSpec::text`],
/// ...) and attach constraints with the chained `const fn` setters.
/// Columns default to nullable, matching SQLite.
#[derive(Debug, Clone, Copy)]
pub struct ColumnSpec {
    /// The column name.
    pub name: &'static str,
    /// The storage class.
    pub sql_type: SqlType,
    /// Whether `NULL` is allowed. Default `true`.
    pub nullable: bool,
    /// Single-column primary-key role.
    pub pk: PkRole,
    /// Whether the column carries a `UNIQUE` constraint.
    pub unique: bool,
    /// A `DEFAULT` expression, written verbatim (quote text yourself).
    pub default: Option<&'static str>,
    /// A column-level `CHECK` expression, written verbatim.
    pub check: Option<&'static str>,
    /// An intra-database foreign key.
    pub references: Option<ForeignKey>,
    /// A one-line comment rendered after the column definition.
    pub comment: Option<&'static str>,
}

impl ColumnSpec {
    /// Shared constructor: a nullable column of the given type with no
    /// constraints.
    const fn bare(name: &'static str, sql_type: SqlType) -> ColumnSpec {
        ColumnSpec {
            name,
            sql_type,
            nullable: true,
            pk: PkRole::None,
            unique: false,
            default: None,
            check: None,
            references: None,
            comment: None,
        }
    }

    /// An `INTEGER` column.
    pub const fn int(name: &'static str) -> ColumnSpec {
        ColumnSpec::bare(name, SqlType::Integer)
    }

    /// A `TEXT` column.
    pub const fn text(name: &'static str) -> ColumnSpec {
        ColumnSpec::bare(name, SqlType::Text)
    }

    /// A `REAL` column.
    pub const fn real(name: &'static str) -> ColumnSpec {
        ColumnSpec::bare(name, SqlType::Real)
    }

    /// A `BLOB` column.
    pub const fn blob(name: &'static str) -> ColumnSpec {
        ColumnSpec::bare(name, SqlType::Blob)
    }

    /// Mark the column `PRIMARY KEY`.
    pub const fn primary_key(mut self) -> ColumnSpec {
        self.pk = PkRole::Primary;
        self
    }

    /// Mark the column `PRIMARY KEY AUTOINCREMENT`.
    pub const fn pk_autoinc(mut self) -> ColumnSpec {
        self.pk = PkRole::PrimaryAutoinc;
        self
    }

    /// Mark the column `NOT NULL`.
    pub const fn not_null(mut self) -> ColumnSpec {
        self.nullable = false;
        self
    }

    /// Give the column a `UNIQUE` constraint.
    pub const fn unique(mut self) -> ColumnSpec {
        self.unique = true;
        self
    }

    /// Attach a `DEFAULT` expression. Written verbatim — a text default
    /// must include its own quotes, e.g. `default("'open'")`.
    pub const fn default(mut self, expr: &'static str) -> ColumnSpec {
        self.default = Some(expr);
        self
    }

    /// Attach a column-level `CHECK` expression, written verbatim.
    pub const fn check(mut self, expr: &'static str) -> ColumnSpec {
        self.check = Some(expr);
        self
    }

    /// Attach an intra-database foreign key.
    pub const fn references(mut self, fk: ForeignKey) -> ColumnSpec {
        self.references = Some(fk);
        self
    }

    /// Attach a one-line comment, rendered into the DDL.
    pub const fn comment(mut self, text: &'static str) -> ColumnSpec {
        self.comment = Some(text);
        self
    }
}

/// A secondary index on a table.
#[derive(Debug, Clone, Copy)]
pub struct IndexSpec {
    /// The index name.
    pub name: &'static str,
    /// The indexed columns, in order.
    pub columns: &'static [&'static str],
    /// Whether the index is `UNIQUE`.
    pub unique: bool,
    /// A partial-index `WHERE` predicate, written verbatim.
    pub where_clause: Option<&'static str>,
}

impl IndexSpec {
    /// A plain index on `columns`.
    pub const fn on(name: &'static str, columns: &'static [&'static str]) -> IndexSpec {
        IndexSpec {
            name,
            columns,
            unique: false,
            where_clause: None,
        }
    }

    /// Make the index `UNIQUE`.
    pub const fn unique(mut self) -> IndexSpec {
        self.unique = true;
        self
    }

    /// Restrict the index to rows matching `predicate` (a partial index).
    pub const fn partial(mut self, predicate: &'static str) -> IndexSpec {
        self.where_clause = Some(predicate);
        self
    }
}

/// The complete description of one table: the single source of truth its
/// DDL, its column list, and its conformance check all derive from.
#[derive(Debug, Clone, Copy)]
pub struct TableSpec {
    /// The table name.
    pub name: &'static str,
    /// A one-line comment rendered above the `CREATE TABLE`.
    pub comment: Option<&'static str>,
    /// The columns, in declaration order.
    pub columns: &'static [ColumnSpec],
    /// A composite primary key, as an ordered column list. When set, no
    /// column may also carry a single-column [`PkRole`].
    pub composite_pk: Option<&'static [&'static str]>,
    /// Table-level composite `UNIQUE` constraints.
    pub uniques: &'static [&'static [&'static str]],
    /// Table-level `CHECK` expressions, written verbatim.
    pub table_checks: &'static [&'static str],
    /// Secondary indexes.
    pub indexes: &'static [IndexSpec],
}

impl TableSpec {
    /// The columns as a comma-separated list, for a `SELECT` or
    /// `RETURNING` clause. Derived from [`TableSpec::columns`], so it can
    /// never drift from the schema.
    pub fn select_list(&self) -> String {
        let mut list = String::new();
        for (i, col) in self.columns.iter().enumerate() {
            if i > 0 {
                list.push_str(", ");
            }
            list.push_str(col.name);
        }
        list
    }

    /// The primary-key columns in key order: the composite key if one is
    /// declared, otherwise the single column carrying a [`PkRole`].
    pub fn primary_key_columns(&self) -> Vec<&'static str> {
        if let Some(cols) = self.composite_pk {
            return cols.to_vec();
        }
        self.columns
            .iter()
            .filter(|c| !matches!(c.pk, PkRole::None))
            .map(|c| c.name)
            .collect()
    }
}
