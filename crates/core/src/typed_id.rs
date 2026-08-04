// SPDX-License-Identifier: Apache-2.0

//! The CLI-surface identifier for one catalog item.
//!
//! Book and paper intakes live in separate catalogs whose `intake_id`
//! columns increment independently, so `101` names one item in each and
//! nothing in the number itself tells them apart. A [`TypedItemId`]
//! carries the pipeline alongside the id — `book:12`, `paper:101`,
//! `reference:name_alpha/smith` — which is what makes an id a
//! resolvable input at a surface that does not already fix the
//! namespace.
//!
//! The kind vocabulary is [`ItemKind`]'s own scope strings, single
//! sourced through [`ItemKind::as_scope_str`]: adding a pipeline
//! extends the accepted prefixes without a second table to remember.
//!
//! This type is a parsing and rendering shape, not a wire shape. It
//! deliberately implements neither `Serialize` nor `Deserialize` — that
//! omission is the mechanism that keeps the `<kind>:<payload>` string
//! out of control-plane DTOs, where the established form is an
//! `intake_id` beside a `kind` field. Do not add a derive to make a
//! struct compile; project the id into those fields instead.

use std::fmt;
use std::str::FromStr;

use crate::item_kind::ItemKind;
use crate::problem::{Explain, Problem};

/// An identifier for one catalog item, naming the pipeline alongside
/// the id.
///
/// [`fmt::Display`] renders the accepted input syntax, so the output of
/// a listing can be pasted back into a command: `FromStr ∘ Display` is
/// the identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedItemId {
    /// A book intake, addressed by its catalog intake id.
    Book(i64),
    /// A paper intake, addressed by its catalog intake id.
    Paper(i64),
    /// A reference book, optionally narrowed to one of its entries.
    Reference {
        /// The reference book's slug, matching `[a-z][a-z0-9_]*`.
        book_slug: String,
        /// One entry within the book, taken verbatim from everything
        /// after the first `/`. `None` addresses the book itself.
        entry_key: Option<String>,
    },
}

impl TypedItemId {
    /// The pipeline this id addresses.
    pub const fn kind(&self) -> ItemKind {
        match self {
            TypedItemId::Book(_) => ItemKind::Book,
            TypedItemId::Paper(_) => ItemKind::Paper,
            TypedItemId::Reference { .. } => ItemKind::Reference,
        }
    }
}

impl fmt::Display for TypedItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = self.kind().as_scope_str();
        match self {
            TypedItemId::Book(id) | TypedItemId::Paper(id) => write!(f, "{kind}:{id}"),
            TypedItemId::Reference {
                book_slug,
                entry_key: None,
            } => write!(f, "{kind}:{book_slug}"),
            TypedItemId::Reference {
                book_slug,
                entry_key: Some(key),
            } => write!(f, "{kind}:{book_slug}/{key}"),
        }
    }
}

impl FromStr for TypedItemId {
    type Err = TypedIdParseError;

    fn from_str(s: &str) -> Result<TypedItemId, TypedIdParseError> {
        let Some((prefix, payload)) = s.split_once(':') else {
            // Without a prefix there is no kind to read. A number is
            // the case worth naming on its own — it is what a listing
            // from before this syntax, or another tool, hands over.
            return Err(if s.parse::<i64>().is_ok() {
                TypedIdParseError::BareId(s.to_string())
            } else {
                unknown_kind(s, "")
            });
        };

        let kind = parse_kind(prefix).ok_or_else(|| unknown_kind(prefix, payload))?;
        if payload.is_empty() {
            return Err(TypedIdParseError::EmptyPayload {
                kind,
                input: s.to_string(),
            });
        }

        match kind {
            ItemKind::Book => Ok(TypedItemId::Book(intake_id(kind, payload)?)),
            ItemKind::Paper => Ok(TypedItemId::Paper(intake_id(kind, payload)?)),
            ItemKind::Reference => {
                // The first `/` divides; everything after it is the
                // entry key verbatim. A slug cannot contain `/`, so the
                // split point is unambiguous, and refs stores entry
                // keys unescaped — any escaping here would have to be
                // undone before a lookup could match.
                let (book_slug, entry_key) = match payload.split_once('/') {
                    Some((slug, key)) => (slug, Some(key.to_string())),
                    None => (payload, None),
                };
                validate_slug(book_slug)?;
                Ok(TypedItemId::Reference {
                    book_slug: book_slug.to_string(),
                    entry_key,
                })
            }
        }
    }
}

/// Map a kind prefix onto its pipeline. Private: the one caller is the
/// parser below, and a public `FromStr for ItemKind` would be a surface
/// designed for a single use.
fn parse_kind(prefix: &str) -> Option<ItemKind> {
    ItemKind::ALL
        .into_iter()
        .find(|kind| kind.as_scope_str() == prefix)
}

/// Classify a prefix that named no pipeline. A plural is a near miss
/// worth its own wording: the command namespaces are plural
/// (`bookrack papers export-csl`) and the id kinds are singular, so
/// `papers:101` is the mistake an operator actually makes.
fn unknown_kind(prefix: &str, payload: &str) -> TypedIdParseError {
    match prefix.strip_suffix('s').and_then(parse_kind) {
        Some(singular) => TypedIdParseError::PluralKind {
            prefix: prefix.to_string(),
            singular,
            payload: payload.to_string(),
        },
        None => TypedIdParseError::UnknownKind {
            prefix: prefix.to_string(),
        },
    }
}

fn intake_id(kind: ItemKind, payload: &str) -> Result<i64, TypedIdParseError> {
    payload
        .parse::<i64>()
        .map_err(|_| TypedIdParseError::NonNumericId {
            kind,
            payload: payload.to_string(),
        })
}

/// The slug rule `refs` applies to a reference book's name, restated
/// rather than called: `core` cannot depend on `refs`, which depends on
/// it. The two must agree, so the wording below quotes the same
/// `[a-z][a-z0-9_]*` the storage layer reports.
fn validate_slug(slug: &str) -> Result<(), TypedIdParseError> {
    let mut chars = slug.chars();
    let well_formed = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if well_formed {
        Ok(())
    } else {
        Err(TypedIdParseError::BadBookSlug {
            slug: slug.to_string(),
        })
    }
}

/// Why a string is not a usable item id.
///
/// Two renderings, one wording. [`Explain`] gives the three-part form
/// every other operator-facing error takes; [`fmt::Display`] flattens
/// it to the single line a `clap` `value_parser` can return, which is
/// the only shape available before dispatch exists to render a
/// [`Problem`]. The flat form is assembled from the three-part one, so
/// the two cannot drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedIdParseError {
    /// A catalog id with no kind prefix.
    BareId(String),

    /// A prefix naming no pipeline.
    UnknownKind {
        /// The prefix as written.
        prefix: String,
    },

    /// A prefix that is a pipeline name in the plural.
    PluralKind {
        /// The prefix as written.
        prefix: String,
        /// The pipeline whose singular name it misspells.
        singular: ItemKind,
        /// The payload it carried, empty when there was no `:`.
        payload: String,
    },

    /// A book or paper prefix over a payload that is not a number.
    NonNumericId {
        /// The pipeline the prefix named.
        kind: ItemKind,
        /// The payload as written.
        payload: String,
    },

    /// A reference book name outside `[a-z][a-z0-9_]*`.
    BadBookSlug {
        /// The name as written.
        slug: String,
    },

    /// A kind with nothing after the `:`.
    EmptyPayload {
        /// The pipeline the prefix named.
        kind: ItemKind,
        /// The id as written.
        input: String,
    },

    /// A well-formed id whose kind is not the one the command namespace
    /// addresses. Raised where a namespace already fixes the kind, not
    /// by [`FromStr`], which has no namespace to compare against.
    NamespaceMismatch {
        /// The pipeline the id names.
        kind: ItemKind,
        /// The payload it carried.
        payload: String,
        /// The pipeline the namespace addresses.
        expected: ItemKind,
        /// The namespace as the operator types it, in the plural.
        namespace: &'static str,
    },
}

/// The accepted prefixes, rendered from the vocabulary itself so a new
/// pipeline cannot leave a stale list behind in a hint.
fn accepted_kinds() -> String {
    ItemKind::ALL
        .into_iter()
        .map(|kind| format!("`{}`", kind.as_scope_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Explain for TypedIdParseError {
    fn explain(&self) -> Problem {
        // `retryable` stays false throughout: the same string parsed
        // again parses the same way.
        match self {
            TypedIdParseError::BareId(id) => {
                Problem::new("cannot resolve a bare id at the top level").hint(format!(
                    "Prefix the id with its kind, e.g. `book:{id}` or `paper:{id}`. \
                     The two catalogs number their items independently, so `{id}` \
                     alone names two different items."
                ))
            }

            TypedIdParseError::UnknownKind { prefix } => {
                Problem::new(format!("unknown item kind {prefix:?}"))
                    .hint(format!("Use one of {}.", accepted_kinds()))
            }

            TypedIdParseError::PluralKind {
                prefix,
                singular,
                payload,
            } => {
                let singular = singular.as_scope_str();
                let example = if payload.is_empty() {
                    format!("{singular}:<id>")
                } else {
                    format!("{singular}:{payload}")
                };
                Problem::new(format!("unknown item kind {prefix:?}")).hint(format!(
                    "Use one of {}. `{prefix}` is the command namespace; the id kind \
                     is singular, so write `{example}`.",
                    accepted_kinds()
                ))
            }

            TypedIdParseError::NonNumericId { kind, payload } => Problem::new(format!(
                "{} id must be a number, got {payload:?}",
                kind.as_scope_str()
            ))
            .hint(
                "Book and paper ids are catalog intake ids. `reference` is the kind \
                 that takes a name.",
            ),

            TypedIdParseError::BadBookSlug { slug } => {
                Problem::new(format!("{slug:?} is not a reference book name"))
                    .detail("A reference book name matches `[a-z][a-z0-9_]*`.")
                    .hint(
                        "A reference book name is lowercase letters, digits and \
                         underscores, starting with a letter.",
                    )
            }

            TypedIdParseError::EmptyPayload { kind, input } => {
                let example = match kind {
                    ItemKind::Book => "book:12",
                    ItemKind::Paper => "paper:101",
                    ItemKind::Reference => "reference:name_alpha/smith",
                };
                Problem::new(format!("item id {input:?} names a kind but no item")).hint(format!(
                    "Write the kind and the id together, e.g. `{example}`."
                ))
            }

            TypedIdParseError::NamespaceMismatch {
                kind,
                payload,
                expected,
                namespace,
            } => {
                let written = format!("{}:{payload}", kind.as_scope_str());
                Problem::new(format!(
                    "{written:?} does not apply to the {namespace} namespace"
                ))
                .hint(format!(
                    "Pass it as `{}:{payload}`, or drop the prefix.",
                    expected.as_scope_str()
                ))
            }
        }
    }
}

impl fmt::Display for TypedIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let problem = self.explain();
        match problem.data.hint {
            Some(hint) => write!(f, "{}. {hint}", problem.summary),
            None => f.write_str(&problem.summary),
        }
    }
}

impl std::error::Error for TypedIdParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// One of each variant, so a test that walks the set cannot go
    /// stale by omission: adding a variant without extending this makes
    /// the `match` below fail to compile.
    fn one_of_each() -> Vec<TypedIdParseError> {
        let sample = TypedIdParseError::BareId(String::new());
        match sample {
            TypedIdParseError::BareId(_)
            | TypedIdParseError::UnknownKind { .. }
            | TypedIdParseError::PluralKind { .. }
            | TypedIdParseError::NonNumericId { .. }
            | TypedIdParseError::BadBookSlug { .. }
            | TypedIdParseError::EmptyPayload { .. }
            | TypedIdParseError::NamespaceMismatch { .. } => {}
        }
        vec![
            TypedIdParseError::BareId("12".into()),
            TypedIdParseError::UnknownKind {
                prefix: "chapter".into(),
            },
            TypedIdParseError::PluralKind {
                prefix: "papers".into(),
                singular: ItemKind::Paper,
                payload: "101".into(),
            },
            TypedIdParseError::NonNumericId {
                kind: ItemKind::Book,
                payload: "name_alpha".into(),
            },
            TypedIdParseError::BadBookSlug { slug: "12".into() },
            TypedIdParseError::EmptyPayload {
                kind: ItemKind::Book,
                input: "book:".into(),
            },
            TypedIdParseError::NamespaceMismatch {
                kind: ItemKind::Book,
                payload: "12".into(),
                expected: ItemKind::Paper,
                namespace: "papers",
            },
        ]
    }

    /// A rendered id is an accepted input: that is what lets a listing
    /// column be copied back into a command. The reference cases cover
    /// what an entry key may hold — a space, a non-Latin script, and
    /// the `:` that also separates the kind.
    #[test]
    fn round_trips_through_its_text_form() {
        for id in [
            TypedItemId::Book(12),
            TypedItemId::Paper(101),
            TypedItemId::Reference {
                book_slug: "name_alpha".into(),
                entry_key: None,
            },
            TypedItemId::Reference {
                book_slug: "name_alpha".into(),
                entry_key: Some("smith".into()),
            },
            TypedItemId::Reference {
                book_slug: "name_alpha".into(),
                entry_key: Some("two words".into()),
            },
            TypedItemId::Reference {
                book_slug: "name_alpha".into(),
                entry_key: Some("\u{4e00}\u{4e8c}".into()),
            },
            TypedItemId::Reference {
                book_slug: "name_alpha".into(),
                entry_key: Some("a:b".into()),
            },
        ] {
            let text = id.to_string();
            assert_eq!(
                text.parse::<TypedItemId>(),
                Ok(id.clone()),
                "{text:?} did not parse back to {id:?}"
            );
        }
    }

    /// The first `/` divides the book from the entry; the rest is the
    /// key verbatim, not a path to split further.
    #[test]
    fn the_first_slash_ends_the_book_name() {
        assert_eq!(
            "reference:a_b/x/y".parse::<TypedItemId>(),
            Ok(TypedItemId::Reference {
                book_slug: "a_b".into(),
                entry_key: Some("x/y".into()),
            })
        );
    }

    /// Rejecting a bare id is only half of it: the message has to say
    /// what to write instead, because the number the operator holds is
    /// genuinely ambiguous rather than malformed.
    #[test]
    fn a_bare_id_is_rejected_with_a_message_that_points_at_a_kind() {
        let err = "12".parse::<TypedItemId>().expect_err("bare id");
        assert_eq!(err, TypedIdParseError::BareId("12".into()));
        let problem = err.explain();
        assert_eq!(problem.summary, "cannot resolve a bare id at the top level");
        let hint = problem.data.hint.expect("a bare id has a next step");
        assert!(hint.contains("book:12"), "{hint}");
        assert!(hint.contains("paper:12"), "{hint}");
    }

    /// `papers` is a command namespace and `paper` is an id kind. The
    /// plural is not an alias for the singular; the message says which
    /// is which.
    #[test]
    fn a_plural_kind_is_rejected_and_names_the_singular() {
        let err = "papers:101".parse::<TypedItemId>().expect_err("plural");
        let problem = err.explain();
        assert!(problem.summary.contains("papers"), "{}", problem.summary);
        let hint = problem.data.hint.expect("a plural kind has a next step");
        assert!(hint.contains("namespace"), "{hint}");
        assert!(hint.contains("`paper:101`"), "{hint}");
    }

    /// The accepted prefixes are `ItemKind`'s own scope strings. The
    /// `match` is exhaustive on purpose: adding a pipeline breaks the
    /// build here rather than silently leaving its ids unaddressable.
    #[test]
    fn the_kind_vocabulary_is_the_item_kind_vocabulary() {
        for kind in ItemKind::ALL {
            let prefix = match kind {
                ItemKind::Book => "book",
                ItemKind::Paper => "paper",
                ItemKind::Reference => "reference",
            };
            assert_eq!(prefix, kind.as_scope_str());
            assert_eq!(parse_kind(prefix), Some(kind));
            assert!(
                accepted_kinds().contains(prefix),
                "the hint lists {prefix:?}"
            );
        }
    }

    /// Each kind fixes the shape of its payload, so a name under a
    /// numeric kind and a number under a naming kind both fail — and
    /// fail for their own stated reason, not a shared "bad id".
    #[test]
    fn payload_shapes_do_not_cross_kinds() {
        assert_eq!(
            "book:name_alpha".parse::<TypedItemId>(),
            Err(TypedIdParseError::NonNumericId {
                kind: ItemKind::Book,
                payload: "name_alpha".into(),
            })
        );
        assert_eq!(
            "reference:12".parse::<TypedItemId>(),
            Err(TypedIdParseError::BadBookSlug { slug: "12".into() })
        );
    }

    /// A kind with nothing after it is its own mistake: the operator
    /// knows the vocabulary and dropped the id.
    #[test]
    fn a_kind_without_a_payload_is_rejected() {
        let err = "book:".parse::<TypedItemId>().expect_err("empty payload");
        let problem = err.explain();
        assert!(problem.summary.contains("\"book:\""), "{}", problem.summary);
        assert!(
            problem.data.hint.expect("hint").contains("book:12"),
            "the example matches the kind that was written"
        );
    }

    /// The flat form a `value_parser` returns is built from the
    /// three-part form, so a reworded hint cannot leave the one-line
    /// rendering behind.
    #[test]
    fn the_flat_rendering_carries_both_the_summary_and_the_hint() {
        for err in one_of_each() {
            let problem = err.explain();
            let flat = err.to_string();
            assert!(
                flat.contains(&problem.summary),
                "{flat:?} drops the summary"
            );
            let hint = problem.data.hint.expect("every variant states a next step");
            assert!(flat.contains(&hint), "{flat:?} drops the hint");
        }
    }

    /// The summary rules from the error discipline, checked
    /// mechanically: one line, lowercase opening, no trailing period.
    #[test]
    fn every_summary_obeys_the_shape_rules() {
        for err in one_of_each() {
            let summary = err.explain().summary;
            assert!(
                !summary.contains('\n'),
                "a summary is one line: {summary:?}"
            );
            assert!(
                !summary.ends_with('.'),
                "a summary carries no trailing period: {summary:?}"
            );
            let first = summary.chars().next().expect("non-empty summary");
            assert!(
                !first.is_uppercase(),
                "a summary opens lowercase: {summary:?}"
            );
        }
    }

    /// Resending the same string cannot make it parse.
    #[test]
    fn no_variant_is_retryable() {
        for err in one_of_each() {
            assert!(
                !err.explain().data.retryable,
                "{err:?} claims a second attempt may succeed"
            );
        }
    }

    /// The mismatch a command namespace raises names both sides and
    /// rewrites the id into the namespace it was typed under.
    #[test]
    fn a_namespace_mismatch_names_both_kinds() {
        let problem = TypedIdParseError::NamespaceMismatch {
            kind: ItemKind::Book,
            payload: "12".into(),
            expected: ItemKind::Paper,
            namespace: "papers",
        }
        .explain();
        assert_eq!(
            problem.summary,
            "\"book:12\" does not apply to the papers namespace"
        );
        assert!(
            problem.data.hint.expect("hint").contains("`paper:12`"),
            "the hint rewrites the id for the namespace it was typed under"
        );
    }
}
