// SPDX-License-Identifier: Apache-2.0

//! Rule intake: a rule file names a set of plain-text member files and
//! says how they read as one work.
//!
//! This module is the source half of the rule front end. It loads and
//! checks a rule file ([`RuleFile::load`]), finds the members the rule
//! names ([`RuleFile::members`]), and reads them into one canonical
//! text ([`RuleFile::read`]). Two values identify the result:
//!
//! - the **source hash** ([`RuleSource::source_sha256`]) covers the
//!   canonical text and nothing else, so the same text reached through
//!   a different encoding, line-ending convention, or rule is the same
//!   source;
//! - the **fingerprint** ([`RuleFile::fingerprint`]) covers the parsed
//!   rule, the member layout, and [`RULE_ENGINE_VERSION`], so a change
//!   to how an unchanged text is read is visible.
//!
//! Nothing here touches PDFium or any other format adapter.

mod book;
mod error;
mod glob;
mod source;
mod spec;

pub use error::RuleError;
pub use source::{CompletenessReport, Member, MemberFile, RuleSource, SourceUnit};
pub use spec::{
    BiblioSpec, BlockSplit, BlocksSpec, CompletenessSpec, Expect, Frontmatter, HeadingPattern,
    HeadingSource, MatchOn, MemberOrder, MembersSpec, ProvenanceSpec, RULE_SCHEMA, Rule, RuleFile,
    RuleKind, StructureSpec, TextLayer, UnitTitles, UnitsBy, UnitsSpec,
};

/// The adapter label of text read through a rule.
pub const ADAPTER: &str = "rule-text";

/// Version of the rule engine's behaviour, one of the inputs of a
/// rule's fingerprint.
///
/// Bump on any change that can alter what an unchanged rule produces
/// from unchanged members: the serialized form of [`Rule`], the member
/// pattern grammar or the member order, or the version of a crate in
/// [`FROZEN_RULE_DEPS`]. The companion test `tests/dep_hash.rs` fails
/// until the list is restated, forcing a deliberate bump.
// setting: internal -- a version stamp; docs/UPGRADE.md's runbook governs a bump
pub const RULE_ENGINE_VERSION: u32 = 1;

/// `name@version` of the regex engine and the crates it matches with,
/// as `Cargo.lock` holds them, sorted bytewise. Restated in lockstep
/// with [`RULE_ENGINE_VERSION`]. See `tests/dep_hash.rs` for the
/// selection rule.
pub const FROZEN_RULE_DEPS: &[&str] = &[
    "aho-corasick@1.1.4",
    "regex-automata@0.4.14",
    "regex-syntax@0.8.10",
    "regex@1.12.3",
];
