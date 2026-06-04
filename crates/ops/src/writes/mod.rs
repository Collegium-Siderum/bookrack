// SPDX-License-Identifier: Apache-2.0

//! Write ops over the bookrack library.
//!
//! Each write op opens the catalog read-write, applies the change, and
//! records a [`bookrack_catalog::MetadataAudit`] row tagged with the
//! [`crate::Caller`] this [`crate::Ops`] was built with.

pub mod metadata;
