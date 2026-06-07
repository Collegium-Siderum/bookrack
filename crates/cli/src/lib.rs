// SPDX-License-Identifier: Apache-2.0

//! Library face of the `bookrack` binary. The CLI ships as a bin
//! crate; this lib surface exists so internal modules with their own
//! contract — the persistent ingest queue, in this initial form — can
//! be unit-tested through `cargo test` and consumed cross-module
//! without funnelling through `main.rs`.

pub mod queue;
