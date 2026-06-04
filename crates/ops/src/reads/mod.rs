// SPDX-License-Identifier: Apache-2.0

//! Read ops over the bookrack library.
//!
//! Each function takes `&Ops<E>` and returns a DTO. Phase A wires the
//! search facade and the seven read methods on
//! [`bookrack_query::Library`]. Later phases add the metadata-audit,
//! pipeline-trail, and library-info reads.

pub mod books;
pub mod search;
