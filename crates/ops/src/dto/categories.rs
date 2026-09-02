// SPDX-License-Identifier: Apache-2.0

//! The library-wide category distribution.
//!
//! [`CategoryCounts`] is what `library.categories` returns: the browse
//! entry to the `categories` filter of `library.find_books`, which
//! otherwise asks the caller to guess a tag the library may not carry.

use serde::Serialize;

/// One line of the distribution: a category and the books carrying it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CategoryBooks {
    /// The category, as `find_books` expects it in `categories`.
    pub category: String,
    /// Books carrying it. A book carrying several categories is counted
    /// under each.
    pub books: u64,
}

/// Every category in the library with its book count, plus the books
/// no category reaches. `uncategorised` and the categorised books
/// together account for every book once: `total` is the registry
/// count, and a library nobody has tagged reports it whole under
/// `uncategorised`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CategoryCounts {
    /// Categories carried by at least one book, most-used first and by
    /// name within a count.
    pub categories: Vec<CategoryBooks>,
    /// Books carrying no category.
    pub uncategorised: u64,
    /// Books in the registry.
    pub total: u64,
}
