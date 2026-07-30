// SPDX-License-Identifier: Apache-2.0

//! Static checks over the command surface, consumed only by the three
//! gate tests: the whole binary from `crates/cli`, this crate's own
//! subcommand enums from a test shell below, and `runtime::cmd`'s two
//! enums from a shell there. The rules live here because `crates/cli`
//! and `crates/runtime` both depend on this crate, so one
//! implementation can be driven from all three; the module has no
//! runtime caller.
//!
//! Paths are relative to the binary: `queue list`, not `bookrack queue
//! list`. The whole-binary walk starts below `bookrack` and a mirrored
//! walk starts below a nameless shell, so the two agree without either
//! side stripping a prefix.

use std::collections::BTreeSet;
use std::fmt;

/// Upper bound on a command summary and on a rendered example line, in
/// characters. clap wraps neither: a summary prints in the parent's
/// `Commands` column, an example prints verbatim in the long-help
/// trailer, and an over-long one runs off the terminal instead of
/// folding.
pub const SUMMARY_LIMIT: usize = 100;

/// Lower bound on a leaf's examples: one typical invocation and one
/// that is not.
pub const MIN_EXAMPLES: usize = 2;

/// The prefix `examples!` renders in front of every example line.
pub const EXAMPLE_PREFIX: &str = "  $ bookrack ";

/// The argument ids clap marks `global` on the binary's root. Held as
/// a constant rather than read off the walked command: a mirrored test
/// shell declares none of them, so reading them off its root would
/// leave the shadowing rule with nothing to compare against and no way
/// to fail — on the two crates that own most of the shadowing
/// arguments. `declared_global_ids` keeps this honest from the one
/// point that can see the real thing.
pub const GLOBAL_ARG_IDS: &[&str] = &[
    "audit_profile",
    "data_dir",
    "json",
    "library",
    "no_color",
    "quiet",
];

/// Leaves that carry no examples yet and are in reach: nothing has to
/// land before they can be written. The list only shrinks. A leaf that
/// is not on it must carry at least `MIN_EXAMPLES`, and a leaf that is
/// on it must still carry none — filling one in without striking its
/// line out fails the gate just as loudly as leaving it empty.
pub const EXAMPLES_OWED: &[&str] = &[
    "audit-profile diff",
    "audit-profile list",
    "audit-profile show",
    "diagnose",
    "doctor",
    "dryrun",
    "index-profile apply",
    "index-profile current",
    "index-profile diff",
    "index-profile list",
    "index-profile show",
    "index-profile validate",
    "ingest",
    "init",
    "libraries add",
    "libraries config",
    "libraries default",
    "libraries detect",
    "libraries fork",
    "libraries info",
    "libraries list",
    "libraries register",
    "libraries remove",
    "libraries scan",
    "logs",
    "quit",
    "remove",
    "run",
    "status",
    "verify",
];

/// Leaves whose examples are deliberately deferred behind a rename
/// that is already decided: `exec` is being folded into a typed
/// surface, and the four book-side write namespaces move under a
/// `books` namespace. Writing examples against a form that is on its
/// way out buys nothing. Same discipline as `EXAMPLES_OWED`: the list
/// only shrinks, and the rename that empties it strikes out its own
/// lines.
pub const EXAMPLES_DEFERRED: &[&str] = &[
    "corpus rebuild",
    "exec",
    "metadata ack",
    "metadata advance",
    "metadata approve",
    "metadata clear",
    "metadata contributor-add",
    "metadata contributor-remove",
    "metadata reaudit",
    "metadata reject",
    "metadata set",
    "metadata void",
    "stamps reconcile",
    "vectors drop",
    "vectors rebuild",
    "vectors reembed",
    "vectors reset",
];

/// Commands whose grammar accepts any token sequence, so parsing an
/// example proves nothing about it. `exec` forwards a trailing var-arg
/// to the control plane; an `external_subcommand` fallback would join
/// it. The list only shrinks, and a command on it says so in the
/// failure message rather than silently passing.
pub const PARSE_EXEMPT: &[&str] = &["exec"];

/// Every double-quoted value an example is allowed to carry. Examples
/// are the largest batch of new literals in the repository and a real
/// title or contributor name is exactly what must not enter through
/// one; the local secret-scanning denylist does not run in CI, so this
/// table is the only constraint on them that CI can see.
pub const EXAMPLE_QUOTED_VALUES: &[&str] = &["Doe, Jane", "Sample Title"];

/// Which tree the walk was handed.
#[derive(Clone, Copy, Debug)]
pub enum Scope {
    /// The binary's own tree, root included.
    WholeBinary,
    /// A test shell that mounts subcommand enums under the names the
    /// binary gives them. Only depth 2 and below is the surface: depth
    /// 1 is the shell's own mount variants, which carry no doc comment
    /// and would be reported as missing a summary. A subcommand enum
    /// cannot produce anything shallower — a depth-1 leaf could only
    /// come from a `clap::Args` tuple variant, and those live in
    /// `crates/cli`, where the whole-binary walk covers them.
    MirroredEnums,
}

impl Scope {
    fn min_depth(self) -> usize {
        match self {
            Scope::WholeBinary => 1,
            Scope::MirroredEnums => 2,
        }
    }
}

/// One rule broken by one command.
#[derive(Debug)]
pub struct Violation {
    /// Invocation path with the binary name left off, e.g. `queue list`.
    /// Empty for the root itself.
    pub path: String,
    pub kind: ViolationKind,
}

#[derive(Debug)]
pub enum ViolationKind {
    SummaryMissing,
    SummaryTooLong(usize),
    SummaryHasRustdocLink,
    FlagSummaryTooLong { flag: String, len: usize },
    ExamplesMissing,
    ExamplesTooFew(usize),
    ExamplesAlreadyWritten,
    ExampleLineTooLong { line: String, len: usize },
    ExampleOffPath { line: String },
    ExampleValueOffTable { value: String },
    AfterHelpBelowRoot,
    ExamplesOnNode,
    RootTrailerMisplaced,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "`bookrack`: ")?;
        } else {
            write!(f, "`bookrack {}`: ", self.path)?;
        }
        match &self.kind {
            ViolationKind::SummaryMissing => write!(
                f,
                "documents nothing in the parent's Commands column. Give it a \
                 doc comment."
            ),
            ViolationKind::SummaryTooLong(len) => write!(
                f,
                "summary is {len} characters, over the {SUMMARY_LIMIT} limit; \
                 clap does not wrap the Commands column. Put a blank line after \
                 the first sentence of the doc comment so the rest becomes long \
                 help."
            ),
            ViolationKind::SummaryHasRustdocLink => write!(
                f,
                "summary carries a rustdoc intra-doc link, which clap renders \
                 literally. Name the command instead of linking the type."
            ),
            ViolationKind::FlagSummaryTooLong { flag, len } => write!(
                f,
                "`{flag}` carries {len} characters of short help on a page an \
                 operator cannot avoid. Put a blank line after the first \
                 sentence."
            ),
            ViolationKind::ExamplesMissing => write!(
                f,
                "carries no examples. Add \
                 `#[command(after_long_help = examples![...])]` with at least \
                 {MIN_EXAMPLES}, or file the path in EXAMPLES_OWED."
            ),
            ViolationKind::ExamplesTooFew(count) => write!(
                f,
                "carries {count} example(s); the rule asks for at least \
                 {MIN_EXAMPLES}: one typical invocation and one that is not."
            ),
            ViolationKind::ExamplesAlreadyWritten => write!(
                f,
                "is filed as owing examples but carries them. Strike its line \
                 out of EXAMPLES_OWED / EXAMPLES_DEFERRED — those lists only \
                 shrink."
            ),
            ViolationKind::ExampleLineTooLong { line, len } => write!(
                f,
                "example line is {len} characters, over the {SUMMARY_LIMIT} \
                 limit; clap does not wrap the long-help trailer: {line}"
            ),
            ViolationKind::ExampleOffPath { line } => write!(
                f,
                "example does not open with this command's own invocation \
                 path, so it documents some other command: {line}"
            ),
            ViolationKind::ExampleValueOffTable { value } => write!(
                f,
                "example quotes {value:?}, which is not in \
                 EXAMPLE_QUOTED_VALUES. Examples take their values from that \
                 table so a real title or name cannot enter through one."
            ),
            ViolationKind::AfterHelpBelowRoot => write!(
                f,
                "sets `after_help`. Short help reads that field directly and \
                 long help falls back to it, so the block lands on `-h` too; \
                 the reflection layer does not fall back, so this gate sees no \
                 examples at all. Move it to `after_long_help`."
            ),
            ViolationKind::ExamplesOnNode => write!(
                f,
                "is a namespace, not a leaf, and sets `after_long_help`. \
                 Examples belong on the commands that run, where the length, \
                 path, and value rules reach them. Move them down."
            ),
            ViolationKind::RootTrailerMisplaced => write!(
                f,
                "must keep its trailing block in `after_help` and leave \
                 `after_long_help` unset, so `-h` and `--help` both show it. \
                 In `after_long_help` it renders on `--help` alone and \
                 disappears from `-h` without any other test noticing."
            ),
        }
    }
}

/// One indented line per violation, for a test's failure message.
pub fn report(violations: &[Violation]) -> String {
    violations.iter().map(|v| format!("\n  {v}")).collect()
}

/// Walks `root` and reports every rule the surface below it breaks.
pub fn audit_tree(root: &clap::Command, scope: Scope) -> Vec<Violation> {
    let mut out = Vec::new();
    if matches!(scope, Scope::WholeBinary) {
        if root.get_about().is_none() {
            out.push(Violation {
                path: String::new(),
                kind: ViolationKind::SummaryMissing,
            });
        }
        if root.get_after_help().is_none() || root.get_after_long_help().is_some() {
            out.push(Violation {
                path: String::new(),
                kind: ViolationKind::RootTrailerMisplaced,
            });
        }
        audit_flags(root, "", true, &mut out);
    }
    walk(root, "", 0, scope, &mut out);
    out
}

/// clap's generated `help` pseudo-subcommand is skipped: it is inserted
/// by `Command::build`, so whether it is present depends on how the
/// tree was obtained rather than on the surface. Hidden commands are
/// kept — hiding a command must not be a way past the gate.
fn children(cmd: &clap::Command) -> impl Iterator<Item = &clap::Command> {
    cmd.get_subcommands().filter(|sub| sub.get_name() != "help")
}

fn walk(cmd: &clap::Command, path: &str, depth: usize, scope: Scope, out: &mut Vec<Violation>) {
    for sub in children(cmd) {
        let sub_path = if path.is_empty() {
            sub.get_name().to_string()
        } else {
            format!("{path} {}", sub.get_name())
        };
        if depth + 1 >= scope.min_depth() {
            audit_summary(sub, &sub_path, out);
            audit_flags(sub, &sub_path, false, out);
            audit_trailer(sub, &sub_path, out);
            if children(sub).next().is_none() {
                audit_examples(sub, &sub_path, out);
            }
        }
        walk(sub, &sub_path, depth + 1, scope, out);
    }
}

/// Reports a trailing help block that sits where the gate cannot see
/// it. Short help reads `after_help` and long help falls back to it,
/// so a block written there prints on `-h` too — the page the examples
/// are kept off. The reflection layer does not fall back, so a leaf
/// that sets the wrong field reads as carrying no examples, and one
/// still on a debt list passes every rule while printing them.
///
/// A namespace is checked too: `audit_examples` runs on leaves only,
/// so a block hung on a node would escape the length, path, and value
/// rules while still reaching the parse check.
fn audit_trailer(cmd: &clap::Command, path: &str, out: &mut Vec<Violation>) {
    if cmd.get_after_help().is_some() {
        out.push(Violation {
            path: path.to_string(),
            kind: ViolationKind::AfterHelpBelowRoot,
        });
    }
    if children(cmd).next().is_some() && cmd.get_after_long_help().is_some() {
        out.push(Violation {
            path: path.to_string(),
            kind: ViolationKind::ExamplesOnNode,
        });
    }
}

fn audit_summary(cmd: &clap::Command, path: &str, out: &mut Vec<Violation>) {
    let Some(about) = cmd.get_about() else {
        out.push(Violation {
            path: path.to_string(),
            kind: ViolationKind::SummaryMissing,
        });
        return;
    };
    let about = about.to_string();
    let len = about.chars().count();
    if len > SUMMARY_LIMIT {
        out.push(Violation {
            path: path.to_string(),
            kind: ViolationKind::SummaryTooLong(len),
        });
    }
    if about.contains("[`") {
        out.push(Violation {
            path: path.to_string(),
            kind: ViolationKind::SummaryHasRustdocLink,
        });
    }
}

/// A global flag's short help is reprinted on every page in the tree,
/// so it is the most repeated text in the binary. A local argument that
/// shadows a global one wins on its own page and inherits that weight
/// while escaping a root-level survey, so both are measured. The name
/// in the message is the long form an operator would type, not clap's
/// internal id.
///
/// Below the root the comparison is against `GLOBAL_ARG_IDS`, not
/// against whatever the walked root happens to declare: a mirrored
/// shell declares nothing, and deriving the set from it would leave
/// this rule unable to fail on two of the three crates.
fn audit_flags(cmd: &clap::Command, path: &str, at_root: bool, out: &mut Vec<Violation>) {
    for arg in cmd.get_arguments() {
        let id = arg.get_id().to_string();
        let reprinted = if at_root {
            arg.is_global_set()
        } else {
            GLOBAL_ARG_IDS.contains(&id.as_str()) && !arg.is_global_set()
        };
        if !reprinted {
            continue;
        }
        let Some(help) = arg.get_help() else { continue };
        let len = help.to_string().chars().count();
        if len > SUMMARY_LIMIT {
            let flag = format!("--{}", arg.get_long().unwrap_or(id.as_str()));
            out.push(Violation {
                path: path.to_string(),
                kind: ViolationKind::FlagSummaryTooLong { flag, len },
            });
        }
    }
}

fn audit_examples(cmd: &clap::Command, path: &str, out: &mut Vec<Violation>) {
    let block = cmd
        .get_after_long_help()
        .map(|block| block.to_string())
        .unwrap_or_default();
    let lines = example_lines(&block);
    let owed = owes_examples(path);
    if lines.is_empty() {
        if !owed {
            out.push(Violation {
                path: path.to_string(),
                kind: ViolationKind::ExamplesMissing,
            });
        }
        return;
    }
    if owed {
        out.push(Violation {
            path: path.to_string(),
            kind: ViolationKind::ExamplesAlreadyWritten,
        });
    }
    if lines.len() < MIN_EXAMPLES {
        out.push(Violation {
            path: path.to_string(),
            kind: ViolationKind::ExamplesTooFew(lines.len()),
        });
    }
    for line in lines {
        let len = line.chars().count();
        if len > SUMMARY_LIMIT {
            out.push(Violation {
                path: path.to_string(),
                kind: ViolationKind::ExampleLineTooLong {
                    line: line.to_string(),
                    len,
                },
            });
        }
        let invocation = &line[EXAMPLE_PREFIX.len()..];
        if invocation != path && !invocation.starts_with(&format!("{path} ")) {
            out.push(Violation {
                path: path.to_string(),
                kind: ViolationKind::ExampleOffPath {
                    line: line.to_string(),
                },
            });
        }
        for value in quoted_values(invocation) {
            if !EXAMPLE_QUOTED_VALUES.contains(&value) {
                out.push(Violation {
                    path: path.to_string(),
                    kind: ViolationKind::ExampleValueOffTable {
                        value: value.to_string(),
                    },
                });
            }
        }
    }
}

/// The rendered example lines of a long-help block. A block written by
/// hand instead of through `examples!` carries no prefixed line and so
/// reads as no examples at all, which is the same failure as writing
/// none.
pub fn example_lines(block: &str) -> Vec<&str> {
    block
        .lines()
        .filter(|line| line.starts_with(EXAMPLE_PREFIX))
        .collect()
}

/// Every `(path, invocation)` pair in the tree, with the rendered
/// prefix stripped. The parse check in `crates/cli` walks this.
pub fn examples_in_tree(root: &clap::Command) -> Vec<(String, String)> {
    let mut out = Vec::new();
    collect_examples(root, "", &mut out);
    out
}

fn collect_examples(cmd: &clap::Command, path: &str, out: &mut Vec<(String, String)>) {
    for sub in children(cmd) {
        let sub_path = if path.is_empty() {
            sub.get_name().to_string()
        } else {
            format!("{path} {}", sub.get_name())
        };
        if let Some(block) = sub.get_after_long_help() {
            for line in example_lines(&block.to_string()) {
                out.push((sub_path.clone(), line[EXAMPLE_PREFIX.len()..].to_string()));
            }
        }
        collect_examples(sub, &sub_path, out);
    }
}

/// The double-quoted values of one invocation, in order.
fn quoted_values(invocation: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = invocation;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        out.push(&after[..close]);
        rest = &after[close + 1..];
    }
    out
}

/// Splits an example's invocation the way a shell would, honouring
/// double quotes and nothing else. The accepted input is closed by
/// `EXAMPLE_QUOTED_VALUES` and by the values the corpus table allows,
/// so single quotes, escapes, and backslashes are out of policy rather
/// than unimplemented.
pub fn split_example(invocation: &str) -> Result<Vec<String>, UnbalancedQuote> {
    let mut out = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut started = false;
    for ch in invocation.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    out.push(std::mem::take(&mut token));
                    started = false;
                }
            }
            c => {
                token.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return Err(UnbalancedQuote);
    }
    if started {
        out.push(token);
    }
    Ok(out)
}

/// An example whose double quotes do not close.
#[derive(Debug)]
pub struct UnbalancedQuote;

/// Whether the leaf is on either debt list.
pub fn owes_examples(path: &str) -> bool {
    EXAMPLES_OWED.contains(&path) || EXAMPLES_DEFERRED.contains(&path)
}

/// Whether the command's grammar makes a parse check vacuous.
pub fn is_parse_exempt(path: &str) -> bool {
    PARSE_EXEMPT.contains(&path)
}

/// Defects in the constants themselves: an unsorted or duplicated list
/// reads as complete when it is not, and a path on both debt lists
/// makes "deferred" and "in reach" the same state. Every entry point
/// calls this, so a bad edit fails on whichever crate was touched.
pub fn policy_defects() -> Vec<String> {
    let mut out = Vec::new();
    for (name, list) in [
        ("EXAMPLES_OWED", EXAMPLES_OWED),
        ("EXAMPLES_DEFERRED", EXAMPLES_DEFERRED),
        ("PARSE_EXEMPT", PARSE_EXEMPT),
        ("EXAMPLE_QUOTED_VALUES", EXAMPLE_QUOTED_VALUES),
        ("GLOBAL_ARG_IDS", GLOBAL_ARG_IDS),
    ] {
        if is_unsorted_or_duplicated(list) {
            out.push(format!("{name} is not sorted, or holds a duplicate"));
        }
    }
    for path in EXAMPLES_DEFERRED {
        if EXAMPLES_OWED.contains(path) {
            out.push(format!("`{path}` is filed on both debt lists"));
        }
    }
    out
}

/// Whether the list breaks the strictly-ascending order every policy
/// constant keeps. Equal neighbours are a duplicate, a descending pair
/// is a sort break; both make a lookup against the list unreliable.
fn is_unsorted_or_duplicated(list: &[&str]) -> bool {
    list.windows(2).any(|pair| pair[0] >= pair[1])
}

/// The ids clap actually marks `global` on `root`, sorted. Only the
/// whole-binary point can read this — a mirrored shell declares none of
/// them — and comparing it there against `GLOBAL_ARG_IDS` is what stops
/// the constant from drifting away from the surface it stands in for.
pub fn declared_global_ids(root: &clap::Command) -> Vec<String> {
    let mut ids: Vec<String> = root
        .get_arguments()
        .filter(|arg| arg.is_global_set())
        .map(|arg| arg.get_id().to_string())
        .collect();
    ids.sort();
    ids
}

/// Debt-list entries no command answers to. Only a walk of the whole
/// binary can tell: an outer point sees one crate's leaves and would
/// read every other crate's entries as stale.
pub fn unclaimed_debt(root: &clap::Command) -> Vec<&'static str> {
    let mut live = BTreeSet::new();
    collect_leaves(root, "", &mut live);
    EXAMPLES_OWED
        .iter()
        .chain(EXAMPLES_DEFERRED.iter())
        .copied()
        .filter(|path| !live.contains(*path))
        .collect()
}

fn collect_leaves(cmd: &clap::Command, path: &str, out: &mut BTreeSet<String>) {
    for sub in children(cmd) {
        let sub_path = if path.is_empty() {
            sub.get_name().to_string()
        } else {
            format!("{path} {}", sub.get_name())
        };
        if children(sub).next().is_none() {
            out.insert(sub_path.clone());
        }
        collect_leaves(sub, &sub_path, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command that documents nothing shows an empty cell in the
    /// parent's Commands column.
    #[test]
    fn a_command_without_a_summary_is_reported() {
        let cmd = clap::Command::new("probe").subcommand(clap::Command::new("undocumented"));
        let violations = audit_tree(&cmd, Scope::WholeBinary);
        assert!(
            violations.iter().any(
                |v| matches!(v.kind, ViolationKind::SummaryMissing) && v.path == "undocumented"
            ),
            "expected the walk to report the missing summary, got: {}",
            report(&violations)
        );
    }

    /// clap does not wrap the Commands column, so an over-long summary
    /// runs off the terminal.
    #[test]
    fn an_over_long_summary_is_reported_with_its_length() {
        let about = "a".repeat(SUMMARY_LIMIT + 1);
        let cmd = clap::Command::new("probe").subcommand(clap::Command::new("wordy").about(about));
        let violations = audit_tree(&cmd, Scope::WholeBinary);
        assert!(
            violations.iter().any(
                |v| matches!(v.kind, ViolationKind::SummaryTooLong(len) if len == SUMMARY_LIMIT + 1)
            ),
            "expected the walk to report the over-long summary, got: {}",
            report(&violations)
        );
    }

    /// clap renders a rustdoc intra-doc link literally, brackets and
    /// all.
    #[test]
    fn a_summary_carrying_a_rustdoc_link_is_reported() {
        let cmd = clap::Command::new("probe")
            .subcommand(clap::Command::new("linked").about("Peer of [`SomeAction`]."));
        let violations = audit_tree(&cmd, Scope::WholeBinary);
        assert!(
            violations
                .iter()
                .any(|v| matches!(v.kind, ViolationKind::SummaryHasRustdocLink)),
            "expected the walk to report the rustdoc link, got: {}",
            report(&violations)
        );
    }

    /// A local argument that shadows a global flag must be caught in
    /// both scopes. The mirrored shells declare no globals of their
    /// own, so the rule compares against `GLOBAL_ARG_IDS`; deriving the
    /// set from the walked root would leave the mirrored assertion
    /// below with nothing to fail on.
    #[test]
    fn an_over_long_shadowing_flag_is_reported_in_both_scopes() {
        let help = "a".repeat(SUMMARY_LIMIT + 1);
        let cmd = clap::Command::new("probe").subcommand(
            clap::Command::new("mount").about("Mount.").subcommand(
                clap::Command::new("leaf").about("Leaf.").arg(
                    clap::Arg::new("json")
                        .long("json")
                        .action(clap::ArgAction::SetTrue)
                        .help(help),
                ),
            ),
        );
        for scope in [Scope::WholeBinary, Scope::MirroredEnums] {
            let violations = audit_tree(&cmd, scope);
            assert!(
                violations.iter().any(|v| matches!(
                    &v.kind,
                    ViolationKind::FlagSummaryTooLong { flag, .. } if flag == "--json"
                )),
                "expected {scope:?} to report the shadowing flag, got: {}",
                report(&violations)
            );
        }
    }

    /// A leaf that is not on a debt list and carries no examples is the
    /// state the gate exists to end.
    #[test]
    fn a_leaf_off_the_debt_list_must_carry_examples() {
        let cmd = clap::Command::new("probe")
            .subcommand(clap::Command::new("nothing-owes-this").about("Probe."));
        let violations = audit_tree(&cmd, Scope::WholeBinary);
        assert!(
            violations
                .iter()
                .any(|v| matches!(v.kind, ViolationKind::ExamplesMissing)),
            "expected the walk to report the leaf as carrying no examples, got: {}",
            report(&violations)
        );
    }

    /// One example cannot show both the typical invocation and a
    /// non-trivial one.
    #[test]
    fn a_leaf_with_a_single_example_is_reported() {
        let block = format!("Examples:\n{EXAMPLE_PREFIX}probe");
        let cmd = clap::Command::new("root").subcommand(
            clap::Command::new("probe")
                .about("Probe.")
                .after_long_help(block),
        );
        let violations = audit_tree(&cmd, Scope::WholeBinary);
        assert!(
            violations
                .iter()
                .any(|v| matches!(v.kind, ViolationKind::ExamplesTooFew(1))),
            "expected the walk to report the single example, got: {}",
            report(&violations)
        );
    }

    /// Filling in a debt-listed leaf without striking its line out
    /// leaves the list claiming a state that is no longer true.
    #[test]
    fn a_debt_listed_leaf_that_carries_examples_is_reported() {
        let block =
            format!("Examples:\n{EXAMPLE_PREFIX}metadata set\n{EXAMPLE_PREFIX}metadata set");
        let cmd = clap::Command::new("probe").subcommand(
            clap::Command::new("metadata")
                .about("Namespace.")
                .subcommand(
                    clap::Command::new("set")
                        .about("Set.")
                        .after_long_help(block),
                ),
        );
        let violations = audit_tree(&cmd, Scope::WholeBinary);
        assert!(
            violations
                .iter()
                .any(|v| matches!(v.kind, ViolationKind::ExamplesAlreadyWritten)
                    && v.path == "metadata set"),
            "expected the walk to report the un-struck debt line, got: {}",
            report(&violations)
        );
    }

    /// clap does not wrap the long-help trailer, so an example longer
    /// than the limit runs off the terminal.
    #[test]
    fn an_over_long_example_line_is_reported_with_its_length() {
        let long = "a".repeat(SUMMARY_LIMIT);
        let block = format!("Examples:\n{EXAMPLE_PREFIX}probe {long}\n{EXAMPLE_PREFIX}probe");
        let cmd = clap::Command::new("root").subcommand(
            clap::Command::new("probe")
                .about("Probe.")
                .after_long_help(block),
        );
        let violations = audit_tree(&cmd, Scope::WholeBinary);
        assert!(
            violations
                .iter()
                .any(|v| matches!(v.kind, ViolationKind::ExampleLineTooLong { .. })),
            "expected the walk to report the over-long line, got: {}",
            report(&violations)
        );
    }

    /// An example whose first tokens are not this command's own path
    /// documents some other command, and mirror drift would never be
    /// caught.
    #[test]
    fn an_example_off_its_own_path_is_reported() {
        let block = format!("Examples:\n{EXAMPLE_PREFIX}other\n{EXAMPLE_PREFIX}probe");
        let cmd = clap::Command::new("root").subcommand(
            clap::Command::new("probe")
                .about("Probe.")
                .after_long_help(block),
        );
        let violations = audit_tree(&cmd, Scope::WholeBinary);
        assert!(
            violations
                .iter()
                .any(|v| matches!(&v.kind, ViolationKind::ExampleOffPath { line } if line.contains("other"))),
            "expected the walk to report the off-path example, got: {}",
            report(&violations)
        );
    }

    /// A quoted value outside the corpus table is the shape a real
    /// title or contributor name would arrive in.
    #[test]
    fn a_quoted_value_off_the_corpus_table_is_reported() {
        let block =
            format!("Examples:\n{EXAMPLE_PREFIX}probe \"Off The Table\"\n{EXAMPLE_PREFIX}probe");
        let cmd = clap::Command::new("root").subcommand(
            clap::Command::new("probe")
                .about("Probe.")
                .after_long_help(block),
        );
        let violations = audit_tree(&cmd, Scope::WholeBinary);
        assert!(
            violations
                .iter()
                .any(|v| matches!(&v.kind, ViolationKind::ExampleValueOffTable { value } if value == "Off The Table")),
            "expected the walk to report the off-table value, got: {}",
            report(&violations)
        );
    }

    /// A block written into `after_help` prints on `-h` and is
    /// invisible to the reflection layer this gate reads.
    #[test]
    fn a_leaf_setting_after_help_is_reported() {
        let cmd = clap::Command::new("root").subcommand(
            clap::Command::new("probe")
                .about("Probe.")
                .after_help("Examples: ..."),
        );
        let violations = audit_tree(&cmd, Scope::WholeBinary);
        assert!(
            violations
                .iter()
                .any(|v| matches!(v.kind, ViolationKind::AfterHelpBelowRoot)),
            "expected the walk to report the misplaced trailer, got: {}",
            report(&violations)
        );
    }

    /// A block hung on a namespace escapes the per-leaf length, path,
    /// and value rules.
    #[test]
    fn a_node_setting_after_long_help_is_reported() {
        let cmd = clap::Command::new("root").subcommand(
            clap::Command::new("mount")
                .about("Mount.")
                .after_long_help("Examples: ...")
                .subcommand(clap::Command::new("leaf").about("Leaf.")),
        );
        let violations = audit_tree(&cmd, Scope::WholeBinary);
        assert!(
            violations
                .iter()
                .any(|v| matches!(v.kind, ViolationKind::ExamplesOnNode) && v.path == "mount"),
            "expected the walk to report the node-level trailer, got: {}",
            report(&violations)
        );
    }

    /// The root's trailer must stay in `after_help`: moved to
    /// `after_long_help` it disappears from `-h` while `--help` keeps
    /// rendering it through the one-way fallback.
    #[test]
    fn a_root_without_its_after_help_trailer_is_reported() {
        let bare = clap::Command::new("probe").about("Probe.");
        let violations = audit_tree(&bare, Scope::WholeBinary);
        assert!(
            violations
                .iter()
                .any(|v| matches!(v.kind, ViolationKind::RootTrailerMisplaced) && v.path.is_empty()),
            "expected the walk to report the bare root, got: {}",
            report(&violations)
        );

        let moved = clap::Command::new("probe")
            .about("Probe.")
            .after_help("Environment: ...")
            .after_long_help("Environment: ...");
        let violations = audit_tree(&moved, Scope::WholeBinary);
        assert!(
            violations
                .iter()
                .any(|v| matches!(v.kind, ViolationKind::RootTrailerMisplaced)),
            "expected the walk to report the doubled root trailer, got: {}",
            report(&violations)
        );

        let kept = clap::Command::new("probe")
            .about("Probe.")
            .after_help("Environment: ...");
        let violations = audit_tree(&kept, Scope::WholeBinary);
        assert!(
            !violations
                .iter()
                .any(|v| matches!(v.kind, ViolationKind::RootTrailerMisplaced)),
            "expected the well-placed root trailer to pass, got: {}",
            report(&violations)
        );
    }

    /// The splitter honours double quotes and reports one that never
    /// closes.
    #[test]
    fn split_example_honours_double_quotes_and_rejects_unbalanced_ones() {
        let tokens = split_example("metadata set 12 title \"Sample Title\"").expect("balanced");
        assert_eq!(tokens, ["metadata", "set", "12", "title", "Sample Title"]);
        assert!(
            split_example("metadata set 12 title \"Sample").is_err(),
            "an unbalanced quote must be rejected"
        );
    }

    /// An unsorted or duplicated policy list reads as complete when it
    /// is not.
    #[test]
    fn an_unsorted_or_duplicated_policy_list_is_detected() {
        assert!(
            is_unsorted_or_duplicated(&["b", "a"]),
            "a descending pair must be detected"
        );
        assert!(
            is_unsorted_or_duplicated(&["a", "a"]),
            "a duplicate must be detected"
        );
        assert!(
            !is_unsorted_or_duplicated(&["a", "b"]),
            "an ascending list must pass"
        );
        assert!(
            policy_defects().is_empty(),
            "the shipped constants must be well-formed"
        );
    }

    /// A debt entry whose command exists is claimed; every other entry
    /// is unclaimed. The judgement is that mounting a listed path makes
    /// exactly that path disappear from the result.
    #[test]
    fn a_debt_entry_with_a_live_command_is_not_reported_unclaimed() {
        let cmd = clap::Command::new("probe").subcommand(
            clap::Command::new("metadata")
                .about("Namespace.")
                .subcommand(clap::Command::new("set").about("Set.")),
        );
        let unclaimed = unclaimed_debt(&cmd);
        assert!(
            !unclaimed.contains(&"metadata set"),
            "the mounted path must read as claimed"
        );
        assert!(
            unclaimed.contains(&"exec"),
            "an entry no command answers to must stay in the result"
        );
    }

    /// Depth 1 is the shell's own mount variants in a mirrored walk and
    /// the real surface in a whole-binary walk; one tree pins both
    /// sides of the cut.
    #[test]
    fn the_mirrored_scope_skips_depth_one_and_the_whole_binary_scope_does_not() {
        let cmd = clap::Command::new("probe").subcommand(
            clap::Command::new("mount").subcommand(clap::Command::new("leaf").about("Leaf.")),
        );
        let whole = audit_tree(&cmd, Scope::WholeBinary);
        assert!(
            whole
                .iter()
                .any(|v| matches!(v.kind, ViolationKind::SummaryMissing) && v.path == "mount"),
            "expected the whole-binary walk to audit depth 1, got: {}",
            report(&whole)
        );
        let mirrored = audit_tree(&cmd, Scope::MirroredEnums);
        assert!(
            !mirrored
                .iter()
                .any(|v| matches!(v.kind, ViolationKind::SummaryMissing) && v.path == "mount"),
            "expected the mirrored walk to skip depth 1, got: {}",
            report(&mirrored)
        );
    }

    /// A failure message that does not name the offending command sends
    /// the reader on a search the report already did.
    #[test]
    fn the_report_names_the_offending_command_in_full() {
        let cmd = clap::Command::new("probe").subcommand(
            clap::Command::new("queue")
                .about("Namespace.")
                .subcommand(clap::Command::new("list")),
        );
        let violations = audit_tree(&cmd, Scope::WholeBinary);
        assert!(
            report(&violations).contains("`bookrack queue list`"),
            "expected the report to name the command, got: {}",
            report(&violations)
        );
    }
}
