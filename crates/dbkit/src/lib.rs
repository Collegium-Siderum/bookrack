// SPDX-License-Identifier: Apache-2.0

//! `dbkit` — the SQLite plumbing shared by the database-owning crates.
//!
//! This crate holds the parts of the database access layer that carry no
//! knowledge of any particular table: the [`TableSpec`] descriptor types,
//! the DDL renderer, the schema-conformance check, and the key/value
//! meta-table helpers. Each database-owning crate (`corpus`, `catalog`)
//! declares its own table specs and keeps all of its SQL; `dbkit` only
//! provides the machinery they share.
//!
//! The design it serves: a table's structure is declared once, as a
//! [`TableSpec`]; its `CREATE TABLE` text is rendered from that spec, so
//! the schema and the code reading it cannot drift apart.

mod ddl;
mod meta;
mod row;
mod spec;
mod timing;
mod verify;

pub use ddl::render_ddl;
pub use meta::{apply_schema, meta_get, meta_set};
pub use row::decode;
pub use spec::{ColumnSpec, ForeignKey, IndexSpec, OnDelete, PkRole, SqlType, TableSpec};
pub use timing::{DEFAULT_SLOW_QUERY_THRESHOLD, TimedConnection};
pub use verify::{SchemaMismatch, VerifyError, verify_all, verify_table};
