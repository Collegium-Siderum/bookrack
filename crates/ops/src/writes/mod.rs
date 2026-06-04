// SPDX-License-Identifier: Apache-2.0

//! Write ops over the bookrack library.
//!
//! Reserved for a later phase. Each write op will open the catalog
//! read-write, apply its change, and record one
//! [`bookrack_catalog::MetadataAudit`] row tagged with the
//! [`crate::Caller`] this [`crate::Ops`] was built with.
