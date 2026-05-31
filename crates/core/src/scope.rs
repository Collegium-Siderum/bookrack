// SPDX-License-Identifier: Apache-2.0

//! Content-stable logical addresses within a book's partition.
//!
//! A [`Scope`] names a position inside one book without binding to a
//! physical node id. Paired with a book's `intake_id`, it survives a
//! re-extraction that renumbers physical nodes: the book root is pure
//! arithmetic, an organizing node is keyed by its subtree content
//! signature, and a prose leaf by its normalized-text hash.
//!
//! [`Scope`] encodes to and from TEXT through [`fmt::Display`] and
//! [`FromStr`], mirroring the bare-`i64` `Display` style of
//! [`NodeId`](crate::NodeId).

use std::fmt;
use std::str::FromStr;

/// A logical address inside one book's partition, paired with an
/// `intake_id` to name the book. The address is content-stable: it
/// survives a re-extraction that renumbers physical node ids.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    /// The book itself — its partition root. Pure arithmetic, immune to
    /// re-extraction.
    Book,
    /// An organizing node, keyed by its subtree content signature.
    Work(String),
    /// A prose leaf, keyed by its normalized-text hash.
    Node(String),
}

/// Why a scope string could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeParseError {
    /// The string matched no known prefix (`book` / `work:` / `node:`).
    UnknownForm(String),
    /// A `work:` / `node:` payload was not a 64-char lowercase hex hash.
    BadHash(String),
}

impl fmt::Display for ScopeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScopeParseError::UnknownForm(s) => {
                write!(f, "unknown scope form: {s:?}")
            }
            ScopeParseError::BadHash(s) => {
                write!(
                    f,
                    "scope payload is not a 64-char lowercase hex hash: {s:?}"
                )
            }
        }
    }
}

impl std::error::Error for ScopeParseError {}

const HASH_LEN: usize = 64;

fn is_lower_hex64(s: &str) -> bool {
    s.len() == HASH_LEN
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Book => f.write_str("book"),
            Scope::Work(sig) => write!(f, "work:{sig}"),
            Scope::Node(sig) => write!(f, "node:{sig}"),
        }
    }
}

impl FromStr for Scope {
    type Err = ScopeParseError;

    fn from_str(s: &str) -> Result<Scope, ScopeParseError> {
        if s == "book" {
            return Ok(Scope::Book);
        }
        if let Some(sig) = s.strip_prefix("work:") {
            return is_lower_hex64(sig)
                .then(|| Scope::Work(sig.to_string()))
                .ok_or_else(|| ScopeParseError::BadHash(s.to_string()));
        }
        if let Some(sig) = s.strip_prefix("node:") {
            return is_lower_hex64(sig)
                .then(|| Scope::Node(sig.to_string()))
                .ok_or_else(|| ScopeParseError::BadHash(s.to_string()));
        }
        Err(ScopeParseError::UnknownForm(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn round_trips_through_text() {
        for scope in [
            Scope::Book,
            Scope::Work(H.to_string()),
            Scope::Node(H.to_string()),
        ] {
            let text = scope.to_string();
            assert_eq!(text.parse::<Scope>(), Ok(scope));
        }
    }

    #[test]
    fn book_renders_without_payload() {
        assert_eq!(Scope::Book.to_string(), "book");
        assert_eq!("book".parse(), Ok(Scope::Book));
    }

    #[test]
    fn unknown_prefix_is_unknown_form() {
        let bad = "chapter:x";
        assert_eq!(
            bad.parse::<Scope>(),
            Err(ScopeParseError::UnknownForm(bad.to_string()))
        );
    }

    #[test]
    fn malformed_hash_is_bad_hash() {
        for bad in [
            "work:abc",                                                            // too short
            &format!("node:{}", &H[..63]),                                         // 63 chars
            &format!("work:{}", H.to_uppercase()),                                 // uppercase hex
            "node:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcg", // non-hex
        ] {
            assert_eq!(
                bad.parse::<Scope>(),
                Err(ScopeParseError::BadHash(bad.to_string())),
                "expected BadHash for {bad:?}"
            );
        }
    }
}
