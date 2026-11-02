// SPDX-License-Identifier: Apache-2.0

//! The argument grammar behind tripwire T8: which parts of a `bookrack`
//! command line name a filesystem location, and whether each one lands
//! inside the sandbox.
//!
//! Everything here is a pure function of the argument vector. The
//! global flags are peeled off first, so a `--json` placed before or
//! after `rpc call` cannot change how the rest is read; the first
//! remaining item is the subcommand, and `rpc` gets its own grammar
//! because its parameters carry paths inside JSON.

use std::ffi::OsString;
use std::path::Path;

use super::tripwire::{Finding, Rule, is_within, resolve};

/// The control-plane methods `rpc call` may name inside a sandbox: the
/// ones the daemon's own method table marks `read`.
pub const READ_ONLY_RPC: [&str; 10] = [
    "daemon.version",
    "daemon.status",
    "status",
    "daemon.methods",
    "queue.list",
    "library.list",
    "library.info",
    "library.stats",
    "library.vectors_status",
    "library.show_pipeline_trail",
];

/// The top-level flags `bookrack` accepts anywhere, with whether each
/// takes a value. None has a short form.
const GLOBAL_FLAGS: [(&str, bool); 6] = [
    ("--library", true),
    ("--data-dir", true),
    ("--audit-profile", true),
    ("--json", false),
    ("--quiet", false),
    ("--no-color", false),
];

/// A command line with its global flags taken out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stripped {
    pub library: Option<String>,
    pub data_dir: Option<String>,
    pub audit_profile: Option<String>,
    pub json: bool,
    pub quiet: bool,
    pub no_color: bool,
    /// Everything else, in order. A bare `--` is dropped, and nothing
    /// after it is read as a flag.
    pub rest: Vec<String>,
}

/// Take the six global flags out of `argv`.
///
/// A flag that takes a value accepts `--flag=value` or the next item;
/// one at the end of the line, or followed by a bare `--`, has no
/// value and is a finding. An item that is not UTF-8 is a finding too:
/// nothing below can judge it.
pub fn strip_globals(argv: &[OsString]) -> Result<Stripped, Finding> {
    let mut out = Stripped::default();
    let mut items = argv.iter();
    let mut positional_only = false;
    while let Some(raw) = items.next() {
        let Some(item) = raw.to_str() else {
            return Err(Finding::new(
                Rule::T8Arguments,
                format!("the argument {raw:?} is not UTF-8"),
            ));
        };
        if positional_only {
            out.rest.push(item.to_string());
            continue;
        }
        if item == "--" {
            positional_only = true;
            continue;
        }
        let Some((flag, takes_value)) = GLOBAL_FLAGS.iter().copied().find(|(flag, takes_value)| {
            item == *flag
                || (*takes_value && item.starts_with(flag) && item[flag.len()..].starts_with('='))
        }) else {
            out.rest.push(item.to_string());
            continue;
        };
        let value = if takes_value {
            if let Some(inline) = item.strip_prefix(flag).and_then(|s| s.strip_prefix('=')) {
                Some(inline.to_string())
            } else {
                match items.next().and_then(|v| v.to_str()) {
                    Some("--") | None => {
                        return Err(Finding::new(
                            Rule::T8Arguments,
                            format!("{flag} is missing its value"),
                        ));
                    }
                    Some(next) => Some(next.to_string()),
                }
            }
        } else {
            None
        };
        match flag {
            "--library" => out.library = value,
            "--data-dir" => out.data_dir = value,
            "--audit-profile" => out.audit_profile = value,
            "--json" => out.json = true,
            "--quiet" => out.quiet = true,
            "--no-color" => out.no_color = true,
            _ => unreachable!("GLOBAL_FLAGS lists every arm"),
        }
    }
    Ok(out)
}

/// What one argument value looks like to the path rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// Starts with `~`: the caller's shell did not expand it, and the
    /// product would read it as a directory named `~` under the cwd.
    Tilde,
    /// `http://` or `https://`: not a location on this machine.
    Url,
    /// Any other `scheme://`: refused outright rather than guessed at.
    ForeignScheme(String),
    /// Names a filesystem location: absolute, `.`, `..`, `./…`, `../…`,
    /// or anything with a `/` in it.
    Path,
    /// A bare word.
    Plain,
}

/// Classify `value` by shape alone.
pub fn shape(value: &str) -> Shape {
    if value.starts_with('~') {
        return Shape::Tilde;
    }
    if let Some(scheme) = url_scheme(value) {
        return if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
            Shape::Url
        } else {
            Shape::ForeignScheme(scheme.to_string())
        };
    }
    if Path::new(value).is_absolute()
        || value == "."
        || value == ".."
        || value.starts_with("./")
        || value.starts_with("../")
        || value.contains('/')
    {
        return Shape::Path;
    }
    Shape::Plain
}

/// The scheme of `value` when it has the form `scheme://…`, with a
/// scheme of a letter followed by letters, digits, `+`, `.` or `-`.
fn url_scheme(value: &str) -> Option<&str> {
    let (scheme, _) = value.split_once("://")?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')) {
        Some(scheme)
    } else {
        None
    }
}

/// The part of `item` after its first `=`, when it has one: the value
/// of `--out=/x` and of a bare `key=/x` alike.
pub fn assigned_value(item: &str) -> Option<&str> {
    item.split_once('=').map(|(_, value)| value)
}

/// Rule T8 over one command line: every finding, in argument order.
///
/// `cwd` is the directory the command runs in and `host` the tree it
/// must stay inside; both are the sandbox's own.
pub fn check(argv: &[OsString], cwd: &Path, host: &Path) -> Vec<Finding> {
    let stripped = match strip_globals(argv) {
        Ok(stripped) => stripped,
        Err(finding) => return vec![finding],
    };
    let mut findings = Vec::new();
    if let Some(value) = &stripped.data_dir {
        judge_value("--data-dir", value, true, cwd, host, &mut findings);
    }
    if let Some(value) = &stripped.library {
        judge_value("--library", value, false, cwd, host, &mut findings);
    }
    if let Some(value) = &stripped.audit_profile {
        judge_value("--audit-profile", value, false, cwd, host, &mut findings);
    }
    if stripped.rest.first().is_some_and(|sub| sub == "rpc") {
        check_rpc(&stripped.rest, cwd, host, &mut findings);
    } else {
        for item in &stripped.rest {
            let value = assigned_value(item).unwrap_or(item);
            judge_value(item, value, false, cwd, host, &mut findings);
        }
    }
    findings
}

/// The `rpc` grammar: `rpc list`, or `rpc call <method> [params]` with
/// a whitelisted method and JSON params whose every string is judged.
fn check_rpc(rest: &[String], cwd: &Path, host: &Path, findings: &mut Vec<Finding>) {
    let refuse = |detail: String| Finding::new(Rule::T8Arguments, detail);
    match rest.get(1).map(String::as_str) {
        Some("list") => {
            if rest.len() > 2 {
                findings.push(refuse(format!(
                    "rpc list takes no arguments, got {:?}",
                    &rest[2..]
                )));
            }
        }
        Some("call") => {
            let Some(method) = rest.get(2) else {
                findings.push(refuse("rpc call names no method".to_string()));
                return;
            };
            if !READ_ONLY_RPC.contains(&method.as_str()) {
                findings.push(refuse(format!(
                    "rpc call {method} is not a read-only method"
                )));
            }
            if rest.len() > 4 {
                findings.push(refuse(format!(
                    "rpc call takes one params argument, got {:?}",
                    &rest[3..]
                )));
            }
            if let Some(params) = rest.get(3) {
                match serde_json::from_str::<serde_json::Value>(params) {
                    Ok(value) => {
                        let mut strings = Vec::new();
                        collect_strings(&value, &mut strings);
                        for s in strings {
                            let what = format!("rpc params string {s:?}");
                            judge_value(&what, &s, false, cwd, host, findings);
                        }
                    }
                    Err(err) => {
                        findings.push(refuse(format!("rpc call params are not JSON: {err}")));
                    }
                }
            }
        }
        other => findings.push(refuse(format!(
            "rpc takes `list` or `call <method> [params]`, got {other:?}"
        ))),
    }
}

/// Every string value in `value`: object values and array items, not
/// object keys.
fn collect_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect_strings(v, out)),
        serde_json::Value::Object(map) => map.values().for_each(|v| collect_strings(v, out)),
        _ => {}
    }
}

/// Judge one value: refuse a `~` or a foreign scheme, pass a URL or a
/// bare word, and resolve a path against `cwd` to see whether it stays
/// inside `host`. `always_path` is `--data-dir`, whose value is a
/// location whatever it looks like.
fn judge_value(
    what: &str,
    value: &str,
    always_path: bool,
    cwd: &Path,
    host: &Path,
    findings: &mut Vec<Finding>,
) {
    match shape(value) {
        Shape::Tilde => findings.push(Finding::new(
            Rule::T8Arguments,
            format!("{what}: {value:?} starts with a literal `~`"),
        )),
        Shape::ForeignScheme(scheme) => findings.push(Finding::new(
            Rule::T8Arguments,
            format!("{what}: {value:?} uses the {scheme}:// scheme"),
        )),
        Shape::Url => {}
        Shape::Plain if !always_path => {}
        Shape::Path | Shape::Plain => {
            let resolved = resolve(Path::new(value), cwd);
            match is_within(&resolved, host) {
                Ok(true) => {}
                Ok(false) => findings.push(Finding::new(
                    Rule::T8Arguments,
                    format!(
                        "{what}: {value:?} resolves to {} outside {}",
                        resolved.display(),
                        host.display()
                    ),
                )),
                Err(err) => findings.push(Finding::new(
                    Rule::T8Arguments,
                    format!("{what}: {value:?} could not be judged: {err}"),
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    /// Candidate recognition by shape alone, before any path is
    /// resolved: the rule that decides what T8 even looks at.
    #[test]
    fn candidates_are_recognised_by_shape() {
        for value in ["..", ".", "./x", "../x", "a/b", "/abs"] {
            assert_eq!(
                shape(value),
                Shape::Path,
                "{value:?} must be a path candidate"
            );
        }
        for item in ["--data-dir=v", "--out=a/b", "key=a/b"] {
            let value = assigned_value(item).expect("has a value");
            assert!(
                matches!(shape(value), Shape::Path | Shape::Plain),
                "{item:?} must split at its `=`"
            );
        }
        assert_eq!(shape(assigned_value("--out=a/b").unwrap()), Shape::Path);
        assert_eq!(shape(assigned_value("key=a/b").unwrap()), Shape::Path);
        for value in ["plain", "--json"] {
            assert_eq!(
                shape(value),
                Shape::Plain,
                "{value:?} must not be a candidate"
            );
        }
        for item in ["--out=plain", "key=plain"] {
            assert_eq!(shape(assigned_value(item).unwrap()), Shape::Plain);
        }
        assert_eq!(shape("http://x/y"), Shape::Url);
        assert_eq!(shape("https://x/y"), Shape::Url);
        assert_eq!(shape("~/x"), Shape::Tilde);
        assert_eq!(
            shape("file:///etc/passwd"),
            Shape::ForeignScheme("file".into())
        );
    }

    /// The six global flags come off wherever they sit, with their
    /// values, and nothing after a bare `--` is read as a flag.
    #[test]
    fn global_flags_are_stripped_wherever_they_sit() {
        let s = strip_globals(&args(&[
            "--json",
            "rpc",
            "--library",
            "beta",
            "call",
            "daemon.version",
            "--data-dir=/x",
            "--quiet",
        ]))
        .expect("strips");
        assert_eq!(s.library.as_deref(), Some("beta"));
        assert_eq!(s.data_dir.as_deref(), Some("/x"));
        assert!(s.json && s.quiet && !s.no_color);
        assert_eq!(s.rest, ["rpc", "call", "daemon.version"]);

        let s = strip_globals(&args(&["status", "--", "--library", "beta"])).expect("strips");
        assert_eq!(s.library, None);
        assert_eq!(s.rest, ["status", "--library", "beta"]);
    }

    /// `library.show_pipeline_trail` is on the whitelist; its sibling
    /// `library.show_audit_trail`, marked `read` in the daemon's table
    /// too, is not.
    #[test]
    fn the_pipeline_trail_is_the_one_trail_rpc_call_admits() {
        let here = Path::new(".");
        let findings = check(
            &args(&[
                "rpc",
                "call",
                "library.show_pipeline_trail",
                r#"{"intake_id":1}"#,
            ]),
            here,
            here,
        );
        assert!(
            findings.is_empty(),
            "the pipeline trail must be admitted: {:?}",
            findings.iter().map(|f| &f.detail).collect::<Vec<_>>()
        );

        let findings = check(
            &args(&[
                "rpc",
                "call",
                "library.show_audit_trail",
                r#"{"intake_id":1}"#,
            ]),
            here,
            here,
        );
        assert!(
            findings
                .iter()
                .any(|f| f.rule == Rule::T8Arguments
                    && f.detail.contains("is not a read-only method")),
            "the audit trail must stay refused: {:?}",
            findings.iter().map(|f| &f.detail).collect::<Vec<_>>()
        );
    }

    /// A value-taking global flag with nothing after it, or with a bare
    /// `--` after it, is a finding rather than a flag that ate the
    /// wrong item.
    #[test]
    fn a_global_flag_without_its_value_trips() {
        for argv in [
            &["status", "--library"][..],
            &["--data-dir", "--", "status"][..],
        ] {
            let err = strip_globals(&args(argv)).expect_err("a missing value is a finding");
            assert_eq!(err.rule, Rule::T8Arguments);
            assert!(
                err.detail.contains("missing its value"),
                "the finding must say which flag lacks a value: {}",
                err.detail
            );
        }
    }
}
