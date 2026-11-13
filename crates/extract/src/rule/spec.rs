// SPDX-License-Identifier: Apache-2.0

//! The rule file: its schema, and the checks a rule passes before any
//! member is read.

use std::path::{Component, Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};
use toml::de::{DeTable, DeValue};

use super::RuleError;
use super::glob;

/// The `schema` value this build reads.
// setting: internal -- the rule file format's revision
pub const RULE_SCHEMA: u32 = 1;

/// A rule as written: every table of the rule file, with defaults
/// filled in.
///
/// The serialized form of this value is part of a rule's fingerprint,
/// so the order of the fields, here and in every table below, is fixed
/// under [`RULE_ENGINE_VERSION`](super::RULE_ENGINE_VERSION).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// The rule format's revision; [`RULE_SCHEMA`] is the only one read.
    pub schema: u32,
    /// What the members are ingested as.
    #[serde(default)]
    pub kind: RuleKind,
    /// Which files the rule reads, and in what order.
    pub members: MembersSpec,
    /// How the text is cut into source units.
    pub units: UnitsSpec,
    /// How a unit's lines become blocks.
    #[serde(default)]
    pub blocks: BlocksSpec,
    /// Where headings come from.
    #[serde(default)]
    pub structure: StructureSpec,
    /// Bibliographic fields the rule states outright.
    #[serde(default)]
    pub biblio: BiblioSpec,
    /// What the rule says about where the text came from.
    #[serde(default)]
    pub provenance: ProvenanceSpec,
    /// How unit numbering is checked.
    #[serde(default)]
    pub completeness: CompletenessSpec,
}

/// What a rule's members are ingested as.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuleKind {
    /// A book.
    #[default]
    Book,
    /// A reference work.
    Reference,
}

/// The `[members]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MembersSpec {
    /// Patterns a member's path must match one of, relative to the
    /// rule file's directory.
    pub include: Vec<String>,
    /// Patterns that keep a matching file out.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// The order members are read in.
    #[serde(default)]
    pub order: MemberOrder,
    /// The members, in order, under [`MemberOrder::Explicit`].
    #[serde(default)]
    pub explicit: Vec<String>,
    /// Whether the first member opens with a front-matter block to drop.
    #[serde(default)]
    pub frontmatter: Frontmatter,
}

/// The order members are read in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemberOrder {
    /// By relative path, digit runs compared by value.
    #[default]
    Natural,
    /// By relative path, bytewise.
    Name,
    /// As `members.explicit` lists them.
    Explicit,
}

/// Whether members open with a YAML front-matter block (`---\n…\n---\n`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Frontmatter {
    /// The first member opens with one.
    First,
    /// Every member may open with one.
    Each,
    /// No member has one.
    #[default]
    None,
}

/// The `[units]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UnitsSpec {
    /// What marks the boundary between two units.
    pub by: UnitsBy,
    /// The line that opens a unit under [`UnitsBy::Marker`]; its named
    /// groups `label` and `sheet` carry the unit's page label and number.
    #[serde(default)]
    pub marker: Option<String>,
}

/// What marks the boundary between two source units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnitsBy {
    /// Each member is one unit.
    File,
    /// A line matching `units.marker` opens a unit.
    Marker,
    /// All members together are one unit.
    Whole,
    /// Units as a format adapter reported them. Reserved: a rule that
    /// names it is refused.
    Adapter,
}

/// The `[blocks]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct BlocksSpec {
    /// What separates two blocks.
    pub split: BlockSplit,
}

/// What separates two blocks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlockSplit {
    /// A blank line; the lines between two are joined.
    #[default]
    BlankLine,
    /// Every line break.
    Line,
}

/// The `[structure]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct StructureSpec {
    /// Where a unit's title comes from.
    pub unit_titles: UnitTitles,
    /// Depths for unit titles: the first pattern a title matches sets
    /// its depth, and its `label` group, when present, its text.
    pub unit_levels: Vec<HeadingPattern>,
    /// Where headings inside a unit come from.
    pub headings: HeadingSource,
    /// The patterns [`HeadingSource::Patterns`] matches lines against.
    pub patterns: Vec<HeadingPattern>,
    /// Which form of a line the heading patterns see.
    #[serde(rename = "match")]
    pub match_on: MatchOn,
}

/// Where a unit's title comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnitTitles {
    /// The unit's first non-blank line.
    #[default]
    FirstLine,
    /// The member's file name, without its extension.
    Filename,
    /// Units have no title.
    None,
}

/// Where headings inside a unit come from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeadingSource {
    /// The built-in chapter and volume marker families.
    #[default]
    Builtin,
    /// `structure.patterns`.
    Patterns,
    /// No line is a heading.
    None,
}

/// Which form of a line the heading patterns see.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MatchOn {
    /// The line with surrounding whitespace removed.
    #[default]
    Trimmed,
    /// The line with its leading whitespace kept.
    Raw,
}

/// One heading pattern and the depth of the headings it finds.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HeadingPattern {
    /// The pattern; a `label` group, when present, is the heading text.
    pub regex: String,
    /// The heading's depth, 0 for the outermost.
    pub depth: u8,
}

/// The `[biblio]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct BiblioSpec {
    /// The work's title.
    pub title: Option<String>,
    /// The work's subtitle.
    pub subtitle: Option<String>,
    /// The work's authors, in order.
    pub authors: Vec<String>,
    /// The publisher.
    pub publisher: Option<String>,
    /// The year of publication.
    pub year: Option<i32>,
    /// The ISBN.
    pub isbn: Option<String>,
    /// The series the work belongs to.
    pub series: Option<String>,
    /// The language of the text, as a language tag.
    pub language: Option<String>,
}

/// The `[provenance]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProvenanceSpec {
    /// How far the text can be trusted as a transcription.
    pub text_layer: TextLayer,
}

/// How far a rule's text can be trusted as a transcription.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TextLayer {
    /// Typed or exported text.
    #[default]
    BornDigital,
    /// Recognised text that may carry recognition errors.
    Doubtful,
}

/// The `[completeness]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct CompletenessSpec {
    /// The pattern whose `n` group is a unit's number.
    pub number: String,
    /// What the unit numbers are expected to form.
    pub expect: Expect,
    /// Whether a gap, a repeat, or a step backwards fails the intake.
    pub fail_on_gap: bool,
}

impl Default for CompletenessSpec {
    fn default() -> CompletenessSpec {
        CompletenessSpec {
            number: String::from(r"^(?P<n>\d+)"),
            expect: Expect::Contiguous,
            fail_on_gap: false,
        }
    }
}

/// What a rule's unit numbers are expected to form.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Expect {
    /// Nothing; the numbers are not checked.
    None,
    /// One unbroken ascending run.
    #[default]
    Contiguous,
}

/// A rule file that parsed and passed every check made before a member
/// is read: its schema revision, every pattern compiled, and the keys
/// that constrain one another agreeing.
#[derive(Debug)]
pub struct RuleFile {
    pub(super) path: PathBuf,
    pub(super) dir: PathBuf,
    pub(super) rule: Rule,
    pub(super) include: Vec<Regex>,
    pub(super) exclude: Vec<Regex>,
    pub(super) marker: Option<Regex>,
    pub(super) unit_levels: Vec<(Regex, u8)>,
    pub(super) patterns: Vec<(Regex, u8)>,
    pub(super) number: Regex,
}

impl RuleFile {
    /// Read and check the rule file at `path`.
    ///
    /// Member patterns resolve against the directory holding the file.
    /// Every failure is a [`RuleError::Parse`] whose detail names the
    /// key at fault and, when the key is written in the file, its line.
    pub fn load(path: &Path) -> Result<RuleFile, RuleError> {
        let refuse = |detail: String| RuleError::Parse {
            path: path.to_path_buf(),
            detail,
        };
        let bytes =
            std::fs::read(path).map_err(|e| refuse(format!("it could not be read: {e}")))?;
        let text = String::from_utf8(bytes).map_err(|e| refuse(format!("it is not UTF-8: {e}")))?;
        let rule: Rule = toml::from_str(&text).map_err(|e| {
            let message = one_line(e.message());
            refuse(match e.span() {
                Some(span) => format!("line {}: {message}", line_at(&text, span.start)),
                None => message,
            })
        })?;
        let checked = check(&rule).map_err(|fault| {
            let message = one_line(&fault.message);
            refuse(match line_of_key(&text, &fault.key) {
                Some(line) => format!("{} (line {line}): {message}", fault.key),
                None => format!("{}: {message}", fault.key),
            })
        })?;
        let dir = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        Ok(RuleFile {
            path: path.to_path_buf(),
            dir,
            rule,
            include: checked.include,
            exclude: checked.exclude,
            marker: checked.marker,
            unit_levels: checked.unit_levels,
            patterns: checked.patterns,
            number: checked.number,
        })
    }

    /// The rule as written, defaults filled in.
    pub fn rule(&self) -> &Rule {
        &self.rule
    }

    /// The path the rule file was loaded from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Parse the `n` capture group of `completeness.number` out of
    /// `text` as a `u32`.
    pub(super) fn number_u32(&self, text: &str) -> Option<u32> {
        self.number
            .captures(text)
            .and_then(|c| c.name("n"))
            .and_then(|m| m.as_str().parse::<u32>().ok())
    }
}

/// What [`check`] compiled on the way.
struct Checked {
    include: Vec<Regex>,
    exclude: Vec<Regex>,
    marker: Option<Regex>,
    unit_levels: Vec<(Regex, u8)>,
    patterns: Vec<(Regex, u8)>,
    number: Regex,
}

/// A key a rule sets to a value the engine refuses.
struct Fault {
    /// The key, as a dotted path with `[n]` for an array element.
    key: String,
    message: String,
}

fn fault(key: impl Into<String>, message: impl Into<String>) -> Fault {
    Fault {
        key: key.into(),
        message: message.into(),
    }
}

/// Check a parsed rule: its revision, the keys that constrain one
/// another, and every pattern, compiled here so that a rule that loads
/// cannot fail on one later.
fn check(rule: &Rule) -> Result<Checked, Fault> {
    if rule.schema != RULE_SCHEMA {
        return Err(fault(
            "schema",
            format!(
                "revision {} is not one this version reads; it reads {RULE_SCHEMA}",
                rule.schema
            ),
        ));
    }

    let members = &rule.members;
    if members.include.is_empty() {
        return Err(fault("members.include", "the list is empty"));
    }
    let include = compile_patterns("members.include", &members.include)?;
    let exclude = compile_patterns("members.exclude", &members.exclude)?;
    if members.order == MemberOrder::Explicit && members.explicit.is_empty() {
        return Err(fault(
            "members.explicit",
            "the list is empty, and members.order = \"explicit\" reads members from it",
        ));
    }
    for (i, listed) in members.explicit.iter().enumerate() {
        let key = format!("members.explicit[{i}]");
        let plain = !listed.is_empty()
            && !listed.contains('\\')
            && Path::new(listed)
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
            && !listed.split('/').any(|part| part.is_empty() || part == ".");
        if !plain {
            return Err(fault(
                key,
                "a listed member is a `/`-separated path below the rule file's directory",
            ));
        }
        if members.explicit[..i].contains(listed) {
            return Err(fault(key, "the member is listed twice"));
        }
    }

    let units = &rule.units;
    match (units.by, &units.marker) {
        (UnitsBy::Adapter, _) => {
            return Err(fault(
                "units.by",
                "\"adapter\" is not supported in this version",
            ));
        }
        (UnitsBy::Marker, None) => {
            return Err(fault(
                "units.marker",
                "the key is missing, and units.by = \"marker\" cuts units at it",
            ));
        }
        _ => {}
    }
    let marker = match &units.marker {
        Some(pattern) => Some(compile_regex("units.marker", pattern)?),
        None => None,
    };

    let structure = &rule.structure;
    if structure.unit_titles == UnitTitles::Filename && units.by != UnitsBy::File {
        return Err(fault(
            "structure.unit_titles",
            "\"filename\" needs units.by = \"file\": only then is a unit one file",
        ));
    }
    let mut unit_levels = Vec::with_capacity(structure.unit_levels.len());
    for (i, level) in structure.unit_levels.iter().enumerate() {
        let re = compile_regex(format!("structure.unit_levels[{i}].regex"), &level.regex)?;
        unit_levels.push((re, level.depth));
    }
    let mut patterns = Vec::with_capacity(structure.patterns.len());
    for (i, pattern) in structure.patterns.iter().enumerate() {
        let re = compile_regex(format!("structure.patterns[{i}].regex"), &pattern.regex)?;
        patterns.push((re, pattern.depth));
    }

    let number = compile_regex("completeness.number", &rule.completeness.number)?;
    if !number.capture_names().any(|name| name == Some("n")) {
        return Err(fault(
            "completeness.number",
            "the pattern has no group named `n` to read the number from",
        ));
    }

    Ok(Checked {
        include,
        exclude,
        marker,
        unit_levels,
        patterns,
        number,
    })
}

fn compile_regex(key: impl Into<String>, pattern: &str) -> Result<Regex, Fault> {
    Regex::new(pattern).map_err(|e| fault(key, e.to_string()))
}

fn compile_patterns(key: &str, patterns: &[String]) -> Result<Vec<Regex>, Fault> {
    patterns
        .iter()
        .enumerate()
        .map(|(i, pattern)| {
            glob::compile(pattern).map_err(|message| fault(format!("{key}[{i}]"), message))
        })
        .collect()
}

/// `text` with every run of whitespace, line breaks included, folded
/// into one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The 1-based line of byte `offset` in `text`.
fn line_at(text: &str, offset: usize) -> usize {
    1 + text.as_bytes()[..offset.min(text.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

/// The 1-based line the value of `key` starts on in the rule file
/// `text`, or `None` when the file does not write the key.
///
/// `key` is a dotted path in which `[n]` selects an array element.
fn line_of_key(text: &str, key: &str) -> Option<usize> {
    let root = DeTable::parse(text).ok()?;
    let mut segments = key.split('.').flat_map(|part| {
        let (name, indexes) = part.split_once('[').unwrap_or((part, ""));
        let indexes = indexes
            .split('[')
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.trim_end_matches(']').parse::<usize>().ok());
        std::iter::once(Segment::Key(name)).chain(indexes.map(Segment::Index))
    });
    let Some(Segment::Key(first)) = segments.next() else {
        return None;
    };
    let mut at = root.get_ref().get(first)?;
    for segment in segments {
        at = match segment {
            Segment::Key(name) => at.get_ref().get(name)?,
            Segment::Index(i) => at.get_ref().get(i)?,
        };
    }
    let _: &DeValue<'_> = at.get_ref();
    Some(line_at(text, at.span().start))
}

enum Segment<'a> {
    Key(&'a str),
    Index(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_located_through_tables_inline_tables_and_arrays() {
        let text = "schema = 1\n\n[members]\ninclude = [\n  \"a\",\n  \"b\",\n]\n\n\
                    [structure]\nunit_levels = [\n  { regex = \"x\", depth = 0 },\n  \
                    { regex = \"y\", depth = 1 },\n]\n";
        assert_eq!(line_of_key(text, "schema"), Some(1));
        assert_eq!(line_of_key(text, "members.include"), Some(4));
        assert_eq!(line_of_key(text, "members.include[1]"), Some(6));
        assert_eq!(
            line_of_key(text, "structure.unit_levels[1].regex"),
            Some(12)
        );
        assert_eq!(line_of_key(text, "structure.unit_levels[2].regex"), None);
        assert_eq!(line_of_key(text, "completeness.number"), None);
    }

    #[test]
    fn a_multi_line_message_folds_into_one_line() {
        assert_eq!(
            one_line("regex parse error:\n    (a\n    ^\nerror: x"),
            "regex parse error: (a ^ error: x"
        );
    }
}
