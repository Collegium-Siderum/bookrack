// SPDX-License-Identifier: Apache-2.0

//! The tripwires: the checks that run before a command touches a
//! sandbox, so a child whose environment, registry, tree, arguments or
//! session lock would reach outside the sandbox is refused rather than
//! run.
//!
//! T1 to T6, T8 and T9 are functions of the sandbox tree, the composed
//! environment and the argument vector. T7 asks the binary under test
//! for its own view of the configuration and judges the report; it is
//! the only tripwire that runs anything, and it runs last.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use bookrack_config::{DATA_DIR_ENV, MCP_ADDR_ENV, NO_DOTENV_ENV, OLLAMA_URL_ENV, REGISTRY_ENV};
use serde_json::Value;
use tokio::process::Command;

use super::NO_STUB_URL;
use super::argv::{self, Stripped};
use crate::child::{DaemonProcess, Output, SpawnOpts, endpoint_matches, endpoint_path_matches};
use crate::sandbox::Sandbox;
use crate::spawn::PASSTHROUGH_ENV;

/// File name of the control socket the daemon binds beside the session
/// lock.
const CONTROL_SOCKET_NAME: &str = "control.sock";

/// Capacity of a Unix socket address's path, NUL included; a path this
/// long or longer cannot be bound.
// setting: internal -- the kernel's sun_path capacity on this platform, not a value to tune
#[cfg(target_os = "macos")]
const SUN_PATH_CAP: usize = 104;
// setting: internal -- the kernel's sun_path capacity on this platform, not a value to tune
#[cfg(not(target_os = "macos"))]
const SUN_PATH_CAP: usize = 108;

/// How long the binary under test may take to print its configuration
/// report.
// setting: internal -- how long the harness waits for the binary's configuration report, not a value to tune
const BINARY_VIEW_TIMEOUT: Duration = Duration::from_secs(30);

/// The address a child must ask MCP to listen on inside a sandbox.
const KERNEL_ASSIGNED_MCP: &str = "127.0.0.1:0";

/// The configuration rows whose values name filesystem locations.
const LOCATION_ROWS: [&str; 5] = [
    "registry",
    "data_dir",
    "daemon_state_dir",
    "runtime_dir",
    "backup_dir",
];

/// The location rows a sandboxed child always has a value for: the
/// environment or the platform layer supplies each, so a null means the
/// isolation is broken.
const ALWAYS_SET_ROWS: [&str; 3] = ["registry", "daemon_state_dir", "runtime_dir"];

/// Which tripwire found something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// The composed environment.
    T1Environment,
    /// Every registry entry's root.
    T2Registry,
    /// Links inside the tree.
    T3Links,
    /// `.env` files the child could reach.
    T4Dotenv,
    /// The portable data layer beside the binary.
    T5PortableLayer,
    /// The control socket's path length.
    T6SocketPath,
    /// The binary's own view of its configuration.
    T7BinaryView,
    /// Path arguments and the `rpc` grammar.
    T8Arguments,
    /// The control endpoint the session lock names.
    T9ControlEndpoint,
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Rule::T1Environment => "T1 environment",
            Rule::T2Registry => "T2 registry",
            Rule::T3Links => "T3 links",
            Rule::T4Dotenv => "T4 dotenv",
            Rule::T5PortableLayer => "T5 portable layer",
            Rule::T6SocketPath => "T6 socket path",
            Rule::T7BinaryView => "T7 binary view",
            Rule::T8Arguments => "T8 arguments",
            Rule::T9ControlEndpoint => "T9 control endpoint",
        })
    }
}

/// One thing a tripwire found. Any finding refuses the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule: Rule,
    pub detail: String,
}

impl Finding {
    pub fn new(rule: Rule, detail: impl Into<String>) -> Finding {
        Finding {
            rule,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.rule, self.detail)
    }
}

/// The `.env` a dotenv run placed in the sandbox's working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedDotenv {
    pub path: PathBuf,
    pub content: Vec<u8>,
}

/// Everything the pure tripwires judge.
pub struct Input<'a> {
    /// The sandbox directory as a whole, which T3 walks.
    pub dir: &'a Path,
    /// The host tree the child is confined to.
    pub host: &'a Sandbox,
    /// The environment the child will see, composed from the plan.
    pub env: &'a [(OsString, OsString)],
    /// The child's arguments.
    pub argv: &'a [OsString],
    /// The URL of the one running stub, if any.
    pub stub_url: Option<&'a str>,
    /// The `.env` this run placed, when it is a dotenv run.
    pub dotenv: Option<&'a PlacedDotenv>,
}

/// Whether `path` lies inside `root`, judged on the filesystem rather
/// than on the text of the path.
///
/// `path` must be absolute. Its nearest existing ancestor is resolved
/// to its canonical form; what follows must be plain names, with any
/// `..` past the existing prefix counting as an escape rather than
/// being folded away; and the result must start with the canonical
/// `root`. A relative `path` is never within.
pub fn is_within(path: &Path, root: &Path) -> io::Result<bool> {
    if !path.is_absolute() {
        return Ok(false);
    }
    let root = root.canonicalize()?;
    let mut resolved = None;
    for ancestor in path.ancestors() {
        if let Ok(prefix) = ancestor.canonicalize() {
            let suffix = path
                .strip_prefix(ancestor)
                .expect("an ancestor is a prefix of the path");
            let mut joined = prefix;
            for part in suffix.components() {
                match part {
                    Component::Normal(name) => joined.push(name),
                    Component::CurDir => {}
                    Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                        return Ok(false);
                    }
                }
            }
            resolved = Some(joined);
            break;
        }
    }
    let Some(resolved) = resolved else {
        return Ok(false);
    };
    Ok(resolved.starts_with(&root))
}

/// `value` as the child would read it: an absolute path as is, anything
/// else relative to the child's working directory.
pub fn resolve(value: &Path, cwd: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        cwd.join(value)
    }
}

/// Rules T1 to T6, T8 and T9 over `input`: every finding, in rule
/// order. T7 is [`run_binary_view`], which a caller runs only when this
/// returns nothing.
pub fn check(input: &Input<'_>) -> Vec<Finding> {
    let mut findings = Vec::new();
    findings.extend(check_environment(input));
    findings.extend(check_registry(
        &input.host.registry_path(),
        input.host.path(),
    ));
    findings.extend(check_tree(input.dir, &mut |_| {}));
    findings.extend(check_dotenv(&input.host.cwd(), input.dotenv));
    findings.extend(check_portable_layer(input.dir));
    findings.extend(check_socket_path(input.host));
    findings.extend(argv::check(
        input.argv,
        &input.host.cwd(),
        input.host.path(),
    ));
    findings.extend(check_control_endpoint(input.host));
    findings
}

/// T1: the composed environment points every location into the host
/// tree and carries no other `BOOKRACK_*` value.
fn check_environment(input: &Input<'_>) -> Vec<Finding> {
    let env: BTreeMap<&OsStr, &OsStr> = input
        .env
        .iter()
        .map(|(k, v)| (k.as_os_str(), v.as_os_str()))
        .collect();
    let host_root = input.host.path();
    let mut findings = Vec::new();
    let mut finding = |detail: String| findings.push(Finding::new(Rule::T1Environment, detail));

    let mut allowed: Vec<&str> = Vec::new();
    let mut location_keys: Vec<&'static str> =
        input.host.host_vars().into_iter().map(|(k, _)| k).collect();
    location_keys.push(REGISTRY_ENV);
    for key in &location_keys {
        allowed.push(key);
        match env.get(OsStr::new(key)) {
            None => finding(format!("{key} is not set on the child")),
            Some(value) => match is_within(Path::new(value), host_root) {
                Ok(true) => {}
                Ok(false) => finding(format!(
                    "{key} = {} points outside {}",
                    Path::new(value).display(),
                    host_root.display()
                )),
                Err(err) => finding(format!("{key} could not be judged: {err}")),
            },
        }
    }
    if env.contains_key(OsStr::new(DATA_DIR_ENV)) {
        finding(format!(
            "{DATA_DIR_ENV} is set; the tool routes through the registry"
        ));
    }
    allowed.extend([MCP_ADDR_ENV, OLLAMA_URL_ENV, NO_DOTENV_ENV]);
    match env.get(OsStr::new(MCP_ADDR_ENV)) {
        Some(v) if *v == OsStr::new(KERNEL_ASSIGNED_MCP) => {}
        other => finding(format!(
            "{MCP_ADDR_ENV} is {other:?}, not {KERNEL_ASSIGNED_MCP:?}"
        )),
    }
    let expected_url = input.stub_url.unwrap_or(NO_STUB_URL);
    match env.get(OsStr::new(OLLAMA_URL_ENV)) {
        Some(v) if *v == OsStr::new(expected_url) => {}
        other => finding(format!(
            "{OLLAMA_URL_ENV} is {other:?}, not {expected_url:?}"
        )),
    }
    match (input.dotenv.is_some(), env.get(OsStr::new(NO_DOTENV_ENV))) {
        (false, Some(v)) if *v == OsStr::new("1") => {}
        (false, other) => finding(format!(
            "{NO_DOTENV_ENV} is {other:?}, not \"1\", outside a dotenv run"
        )),
        (true, None) => {}
        (true, Some(v)) => finding(format!(
            "{NO_DOTENV_ENV} = {v:?} is set in a dotenv run; the child would ignore the .env"
        )),
    }
    for key in env.keys() {
        let Some(name) = key.to_str() else { continue };
        if name.starts_with("BOOKRACK_")
            && !allowed.contains(&name)
            && !PASSTHROUGH_ENV.contains(&name)
        {
            finding(format!(
                "{name} reaches the child but is not part of the plan"
            ));
        }
    }
    findings
}

/// T2: `registry` is a regular file whose every entry's root lies inside
/// `host_root`. The two are separate because a snapshot judges the copy
/// of the registry in its temporary tree against the host root the copy
/// will be restored to.
pub fn check_registry(registry: &Path, host_root: &Path) -> Vec<Finding> {
    let shown = registry.display();
    match std::fs::symlink_metadata(registry) {
        Err(err) => {
            return vec![Finding::new(
                Rule::T2Registry,
                format!("the registry {shown} cannot be inspected: {err}"),
            )];
        }
        Ok(meta) if meta.file_type().is_symlink() => {
            return vec![Finding::new(
                Rule::T2Registry,
                format!("the registry {shown} is a symbolic link"),
            )];
        }
        Ok(meta) if !meta.is_file() => {
            return vec![Finding::new(
                Rule::T2Registry,
                format!("the registry {shown} is not a regular file"),
            )];
        }
        Ok(_) => {}
    }
    let entries = match bookrack_config::list_libraries_at(registry) {
        Ok(entries) => entries,
        Err(err) => {
            return vec![Finding::new(
                Rule::T2Registry,
                format!("the registry {shown} does not parse: {err}"),
            )];
        }
    };
    let mut findings = Vec::new();
    for entry in entries {
        match is_within(&entry.data_dir, host_root) {
            Ok(true) => {}
            Ok(false) => findings.push(Finding::new(
                Rule::T2Registry,
                format!(
                    "library {:?} has its root at {}, outside {}",
                    entry.name,
                    entry.data_dir.display(),
                    host_root.display()
                ),
            )),
            Err(err) => findings.push(Finding::new(
                Rule::T2Registry,
                format!("library {:?} could not be judged: {err}", entry.name),
            )),
        }
    }
    findings
}

/// T3: no symbolic link anywhere under `root`, and no regular file with
/// more than one link; sockets and FIFOs are noted, not opened. An entry
/// that vanishes between being listed and being inspected is skipped: a
/// live daemon removes its own socket. `before_stat` is called with each
/// path just before it is inspected, so a test can make one vanish.
pub fn check_tree(root: &Path, before_stat: &mut dyn FnMut(&Path)) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        before_stat(&path);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                findings.push(Finding::new(
                    Rule::T3Links,
                    format!("{} cannot be inspected: {err}", path.display()),
                ));
                continue;
            }
        };
        let kind = meta.file_type();
        if kind.is_symlink() {
            findings.push(Finding::new(
                Rule::T3Links,
                format!("{} is a symbolic link", path.display()),
            ));
        } else if kind.is_file() && meta.nlink() > 1 {
            findings.push(Finding::new(
                Rule::T3Links,
                format!(
                    "{} has {} links; a file in the tree must have one",
                    path.display(),
                    meta.nlink()
                ),
            ));
        } else if kind.is_dir() {
            match std::fs::read_dir(&path) {
                Ok(entries) => {
                    for entry in entries {
                        match entry {
                            Ok(entry) => pending.push(entry.path()),
                            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                            Err(err) => findings.push(Finding::new(
                                Rule::T3Links,
                                format!("listing {} failed: {err}", path.display()),
                            )),
                        }
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => findings.push(Finding::new(
                    Rule::T3Links,
                    format!("listing {} failed: {err}", path.display()),
                )),
            }
        }
    }
    findings
}

/// T4: no `.env` on the way up from `cwd` to the root, except, in a
/// dotenv run, the one this run placed in `cwd` itself, byte for byte.
fn check_dotenv(cwd: &Path, placed: Option<&PlacedDotenv>) -> Vec<Finding> {
    let mut findings = Vec::new();
    for dir in cwd.ancestors() {
        let candidate = dir.join(".env");
        let present = std::fs::symlink_metadata(&candidate).is_ok();
        match placed {
            Some(placed) if dir == cwd => {
                if !present {
                    findings.push(Finding::new(
                        Rule::T4Dotenv,
                        format!("the placed .env at {} is missing", candidate.display()),
                    ));
                    continue;
                }
                match std::fs::read(&candidate) {
                    Ok(bytes) if bytes == placed.content => {}
                    Ok(_) => findings.push(Finding::new(
                        Rule::T4Dotenv,
                        format!(
                            "the .env at {} is not the one this run placed",
                            candidate.display()
                        ),
                    )),
                    Err(err) => findings.push(Finding::new(
                        Rule::T4Dotenv,
                        format!("reading {} failed: {err}", candidate.display()),
                    )),
                }
            }
            _ => {
                if present {
                    findings.push(Finding::new(
                        Rule::T4Dotenv,
                        format!(
                            "a .env at {} is reachable from the child's working directory",
                            candidate.display()
                        ),
                    ));
                }
            }
        }
    }
    findings
}

/// T5: no portable data layer beside the copied binary.
fn check_portable_layer(dir: &Path) -> Vec<Finding> {
    let layer = dir.join("run").join("bin").join("bookrack-data");
    if std::fs::symlink_metadata(&layer).is_ok() {
        vec![Finding::new(
            Rule::T5PortableLayer,
            format!(
                "{} exists; a portable layer would outrank the sandbox's configuration",
                layer.display()
            ),
        )]
    } else {
        Vec::new()
    }
}

/// T6: the control socket's path fits a Unix socket address.
fn check_socket_path(host: &Sandbox) -> Vec<Finding> {
    let sock = host.runtime_dir().join(CONTROL_SOCKET_NAME);
    let len = sock.as_os_str().as_encoded_bytes().len();
    if len >= SUN_PATH_CAP {
        vec![Finding::new(
            Rule::T6SocketPath,
            format!(
                "{} is {len} bytes; a socket path must be shorter than {SUN_PATH_CAP}",
                sock.display()
            ),
        )]
    } else {
        Vec::new()
    }
}

/// T9: a session lock in the host's runtime directory names this
/// sandbox's own control socket.
///
/// A lock somebody holds belongs to a live daemon, so the socket must be
/// there and match on all four counts; a lock nobody holds is what a
/// cleanly stopped daemon leaves behind, with its socket already
/// removed, so only the path's shape is judged. A client discovering a
/// daemon through the lock dials whatever the lock says either way.
fn check_control_endpoint(host: &Sandbox) -> Vec<Finding> {
    let lock = host.tty_lock_path();
    let shown = lock.display();
    if matches!(std::fs::symlink_metadata(&lock), Err(err) if err.kind() == io::ErrorKind::NotFound)
    {
        return Vec::new();
    }
    let info = match bookrack_session::peek_lock(&lock) {
        Ok(Some(info)) => info,
        Ok(None) => return Vec::new(),
        Err(err) => {
            return vec![Finding::new(
                Rule::T9ControlEndpoint,
                format!("the session lock at {shown} cannot be read: {err:#}"),
            )];
        }
    };
    let Some(control_sock) = info.control_sock else {
        return vec![Finding::new(
            Rule::T9ControlEndpoint,
            format!("the session lock at {shown} names no control socket"),
        )];
    };
    let held = match bookrack_session::lock_is_held(&lock) {
        Ok(held) => held,
        Err(err) => {
            return vec![Finding::new(
                Rule::T9ControlEndpoint,
                format!("whether the session lock at {shown} is held cannot be told: {err:#}"),
            )];
        }
    };
    let verdict = if held {
        endpoint_matches(&lock, &control_sock)
    } else {
        endpoint_path_matches(&lock, &control_sock)
    };
    match verdict {
        Ok(()) => Vec::new(),
        Err(why) => vec![Finding::new(
            Rule::T9ControlEndpoint,
            format!(
                "the {} session lock at {shown} names a control socket this sandbox did not bind: {why}",
                if held { "held" } else { "stale" }
            ),
        )],
    }
}

/// What T7 judges the binary's report against.
pub struct BinaryView<'a> {
    /// The host tree every location must lie inside.
    pub host_root: &'a Path,
    /// The child's working directory, which relative values resolve
    /// against.
    pub cwd: &'a Path,
    /// The `.env` this run placed, when it is a dotenv run.
    pub dotenv: Option<&'a Path>,
}

/// The command T7 runs: `config effective --json`, carrying the global
/// `--library` the caller's command line carried and nothing else.
/// `--data-dir` is never passed on: T8 judges its value, and the
/// subcommand-level flag of the same name on `init` and `libraries
/// fork` would clash with it.
pub fn t7_command(stripped: &Stripped) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["config", "effective", "--json"]
        .into_iter()
        .map(OsString::from)
        .collect();
    if let Some(library) = &stripped.library {
        args.push("--library".into());
        args.push(library.into());
    }
    args
}

/// T7, the running half: start `cmd`, which a caller built from the
/// plan with [`t7_command`]'s arguments, wait for it to exit, and judge
/// what it printed. A child that does not exit, or whose streams do not
/// close, is a finding too.
pub async fn run_binary_view(cmd: Command, view: &BinaryView<'_>) -> Vec<Finding> {
    let child = match DaemonProcess::spawn_with(cmd, SpawnOpts::default()) {
        Ok(child) => child,
        Err(err) => {
            return vec![Finding::new(
                Rule::T7BinaryView,
                format!("the binary under test could not be started: {err}"),
            )];
        }
    };
    match child.exit_within(BINARY_VIEW_TIMEOUT).await {
        Ok(exit) => match exit.output {
            Output::Memory { stdout, .. } => judge_report(&stdout, view),
            Output::Streamed { .. } => vec![Finding::new(
                Rule::T7BinaryView,
                "the report was streamed away instead of kept".to_string(),
            )],
        },
        Err(failure) => vec![Finding::new(
            Rule::T7BinaryView,
            format!("config effective --json did not complete: {}", failure.why),
        )],
    }
}

/// T7, the judging half: the report's shape, and every location it
/// names, winner and shadowed alike, lies inside the host tree.
pub fn judge_report(stdout: &[u8], view: &BinaryView<'_>) -> Vec<Finding> {
    let finding = |detail: String| Finding::new(Rule::T7BinaryView, detail);
    let report: Value = match serde_json::from_slice(stdout) {
        Ok(report) => report,
        Err(err) => return vec![finding(format!("the report is not JSON: {err}"))],
    };
    let Some(rows) = report.get("rows").and_then(Value::as_array) else {
        return vec![finding("the report has no `rows` array".to_string())];
    };
    let mut findings = Vec::new();
    let mut by_key: BTreeMap<&str, &Value> = BTreeMap::new();
    for row in rows {
        let Some(key) = row.get("key").and_then(Value::as_str) else {
            findings.push(finding(format!("a row has no string `key`: {row}")));
            continue;
        };
        match row.get("value") {
            Some(Value::String(_)) | Some(Value::Null) | None => {}
            Some(other) => findings.push(finding(format!(
                "row {key:?} has a value that is neither a string nor null: {other}"
            ))),
        }
        match row.get("shadowed") {
            Some(Value::Array(_)) | None => {}
            Some(other) => findings.push(finding(format!(
                "row {key:?} has a `shadowed` that is not an array: {other}"
            ))),
        }
        by_key.insert(key, row);
    }
    for key in LOCATION_ROWS.iter().chain(["no_dotenv"].iter()) {
        if !by_key.contains_key(key) {
            findings.push(finding(format!("the report has no {key:?} row")));
        }
    }
    for key in LOCATION_ROWS {
        let Some(row) = by_key.get(key) else { continue };
        let winner = row.get("value");
        if ALWAYS_SET_ROWS.contains(&key) && !matches!(winner, Some(Value::String(_))) {
            findings.push(finding(format!(
                "row {key:?} has no value; a sandboxed child always resolves it"
            )));
        }
        if let Some(Value::String(value)) = winner {
            findings.extend(judge_location(key, "the winning value", value, view));
        }
        if let Some(Value::Array(shadowed)) = row.get("shadowed") {
            for entry in shadowed {
                match entry.get("value") {
                    Some(Value::String(value)) => {
                        findings.extend(judge_location(key, "a shadowed value", value, view));
                    }
                    other => findings.push(finding(format!(
                        "row {key:?} has a shadowed entry without a string value: {other:?}"
                    ))),
                }
            }
        }
    }
    if let Some(row) = by_key.get("no_dotenv") {
        let expected = if view.dotenv.is_some() { "0" } else { "1" };
        match row.get("value") {
            Some(Value::String(value)) if value == expected => {}
            other => findings.push(finding(format!(
                "row \"no_dotenv\" is {other:?}, not {expected:?}"
            ))),
        }
    }
    match (view.dotenv, report.get("dotenv_path")) {
        (None, None) | (None, Some(Value::Null)) => {}
        (None, Some(path)) => findings.push(finding(format!(
            "the child read a .env at {path} outside a dotenv run"
        ))),
        (Some(placed), Some(Value::String(path))) if Path::new(path) == placed => {}
        (Some(placed), other) => findings.push(finding(format!(
            "the child read {other:?} as its .env, not the placed {}",
            placed.display()
        ))),
    }
    findings
}

/// One location value from the report, resolved against the child's
/// working directory and judged against the host root.
fn judge_location(key: &str, which: &str, value: &str, view: &BinaryView<'_>) -> Vec<Finding> {
    let resolved = resolve(Path::new(value), view.cwd);
    match is_within(&resolved, view.host_root) {
        Ok(true) => Vec::new(),
        Ok(false) => vec![Finding::new(
            Rule::T7BinaryView,
            format!(
                "row {key:?}: {which} {value:?} resolves to {} outside {}",
                resolved.display(),
                view.host_root.display()
            ),
        )],
        Err(err) => vec![Finding::new(
            Rule::T7BinaryView,
            format!("row {key:?}: {which} {value:?} could not be judged: {err}"),
        )],
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    use bookrack_config::DAEMON_STATE_DIR_ENV;
    use bookrack_session::RUNTIME_DIR_ENV;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::tool::plan_for;

    /// A parent process carrying the hostile shape the plan sweeps.
    fn hostile_host() -> Vec<(OsString, OsString)> {
        [
            ("HOME", "/host/home"),
            ("XDG_CONFIG_HOME", "/host/.config"),
            ("XDG_RUNTIME_DIR", "/run/user/501"),
            ("PATH", "/usr/bin"),
            (DATA_DIR_ENV, "/host/library"),
            (REGISTRY_ENV, "/host/registry.toml"),
            (RUNTIME_DIR_ENV, "/host/runtime"),
            (DAEMON_STATE_DIR_ENV, "/host/state"),
            (OLLAMA_URL_ENV, "http://host:11434"),
            ("BOOKRACK_BACKUP_DIR", "/host/backup"),
            ("BOOKRACK_NO_DOTENV", "1"),
            ("BOOKRACK_PDFIUM_LIB", "/host/pdfium"),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect()
    }

    /// A sandbox directory with its host tree, the way `new` lays it
    /// out, plus the composed environment the tool's plan gives a child.
    struct Fixture {
        dir: TempDir,
        host: Sandbox,
        env: Vec<(OsString, OsString)>,
        argv: Vec<OsString>,
        stub_url: Option<String>,
        dotenv: Option<PlacedDotenv>,
    }

    impl Fixture {
        fn new() -> Fixture {
            let dir = tempfile::tempdir().expect("tempdir");
            let host = Sandbox::create_at(&dir.path().join("host")).expect("host tree");
            let env = composed(&host, None, false);
            Fixture {
                dir,
                host,
                env,
                argv: Vec::new(),
                stub_url: None,
                dotenv: None,
            }
        }

        /// A fixture whose run places `content` as the child's `.env`.
        fn with_dotenv(content: &[u8]) -> Fixture {
            let mut fixture = Fixture::new();
            let path = fixture.host.cwd().join(".env");
            std::fs::write(&path, content).expect("write .env");
            fixture.env = composed(&fixture.host, None, true);
            fixture.dotenv = Some(PlacedDotenv {
                path,
                content: content.to_vec(),
            });
            fixture
        }

        fn argv(mut self, items: &[&str]) -> Fixture {
            self.argv = items.iter().map(OsString::from).collect();
            self
        }

        fn check(&self) -> Vec<Finding> {
            check(&Input {
                dir: self.dir.path(),
                host: &self.host,
                env: &self.env,
                argv: &self.argv,
                stub_url: self.stub_url.as_deref(),
                dotenv: self.dotenv.as_ref(),
            })
        }

        fn host_root(&self) -> PathBuf {
            self.host.path().to_path_buf()
        }
    }

    fn composed(host: &Sandbox, stub_url: Option<&str>, dotenv: bool) -> Vec<(OsString, OsString)> {
        plan_for(
            Path::new("/nonexistent/bookrack"),
            host,
            stub_url,
            dotenv,
            hostile_host().into_iter(),
        )
        .composed(hostile_host().into_iter())
    }

    fn set(env: &mut Vec<(OsString, OsString)>, key: &str, value: &str) {
        env.retain(|(k, _)| k != key);
        env.push((key.into(), value.into()));
    }

    fn unset(env: &mut Vec<(OsString, OsString)>, key: &str) {
        env.retain(|(k, _)| k != key);
    }

    fn only(findings: &[Finding], rule: Rule) -> Vec<&Finding> {
        findings.iter().filter(|f| f.rule == rule).collect()
    }

    fn assert_trips(findings: &[Finding], rule: Rule, mentions: &str) {
        let hits = only(findings, rule);
        assert!(
            hits.iter().any(|f| f.detail.contains(mentions)),
            "{rule} must report {mentions:?}; findings were {findings:#?}",
        );
    }

    fn assert_clean(findings: &[Finding], what: &str) {
        assert!(
            findings.is_empty(),
            "{what}: expected no findings, got {findings:#?}"
        );
    }

    /// A compliant report the way `config effective --json` prints one
    /// inside a sandbox, with every location inside `host`.
    fn report(host: &Path, dotenv: Option<&Path>) -> Value {
        let row = |key: &str, value: Option<String>, shadowed: Vec<Value>| {
            json!({ "key": key, "value": value, "layer": "environment", "site": "x",
                    "shadowed": shadowed, "chain": [], "reach": "process", "read_at": "x" })
        };
        let alpha = host.join("roots").join("alpha");
        let mut out = json!({
            "rows": [
                row("registry", Some(host.join("registry.toml").display().to_string()), vec![]),
                row("data_dir", Some(alpha.display().to_string()), vec![]),
                row("daemon_state_dir", Some(host.join("daemon-state").display().to_string()),
                    vec![json!({ "layer": "platform", "site": "x",
                                 "value": host.join("home/.local/share/bookrack").display().to_string() })]),
                row("runtime_dir", Some(host.join("runtime").display().to_string()),
                    vec![json!({ "layer": "platform", "site": "x",
                                 "value": host.join("home/.cache/bookrack").display().to_string() })]),
                row("backup_dir", Some(alpha.join("backup").display().to_string()), vec![]),
                row("no_dotenv", Some(if dotenv.is_some() { "0" } else { "1" }.to_string()), vec![]),
                row("search.top_k", Some("8".to_string()), vec![]),
            ],
            "dotenv_foreign": [],
            "native_dependencies": [],
        });
        if let Some(path) = dotenv {
            out["dotenv_path"] = json!(path.display().to_string());
        }
        out
    }

    fn judge(report: &Value, host: &Path, dotenv: Option<&Path>) -> Vec<Finding> {
        let cwd = host.join("cwd");
        judge_report(
            report.to_string().as_bytes(),
            &BinaryView {
                host_root: host,
                cwd: &cwd,
                dotenv,
            },
        )
    }

    fn set_row(report: &mut Value, key: &str, value: Value) {
        for row in report["rows"].as_array_mut().expect("rows") {
            if row["key"] == key {
                row["value"] = value;
                return;
            }
        }
        panic!("no row {key}");
    }

    fn held_lock(fixture: &Fixture, control_sock: &Path) -> File {
        let lock = fixture.host.tty_lock_path();
        std::fs::write(
            &lock,
            format!(
                "pid=1\nmcp=127.0.0.1:1\ncontrol_sock={}\n",
                control_sock.display()
            ),
        )
        .expect("write lock");
        let file = File::open(&lock).expect("open lock");
        file.lock().expect("hold the lock");
        file
    }

    fn stale_lock(fixture: &Fixture, control_sock: &Path) {
        std::fs::write(
            fixture.host.tty_lock_path(),
            format!(
                "pid=1\nmcp=127.0.0.1:1\ncontrol_sock={}\n",
                control_sock.display()
            ),
        )
        .expect("write lock");
    }

    // T2

    /// The registry is judged entry by entry, not by the one library
    /// the current selection would resolve to.
    #[test]
    fn a_sibling_library_outside_the_sandbox_trips() {
        let fixture = Fixture::new();
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let alpha = fixture.host.data_root("alpha");
        fixture.host.write_registry_entries(
            Some("alpha"),
            &[("alpha", alpha.as_path()), ("beta", elsewhere.path())],
        );
        assert_trips(&fixture.check(), Rule::T2Registry, "\"beta\"");
    }

    /// `list_libraries_at` reads a missing file as an empty registry;
    /// the tripwire does not.
    #[test]
    fn a_missing_registry_trips() {
        let fixture = Fixture::new();
        std::fs::remove_file(fixture.host.registry_path()).expect("remove registry");
        assert_trips(&fixture.check(), Rule::T2Registry, "cannot be inspected");
    }

    /// A snapshot's temporary copy of the registry names roots under the
    /// host tree, because that is where the copy will be restored to;
    /// judging it against its own location would fail every entry.
    #[test]
    fn a_copied_registry_is_judged_against_the_host_root() {
        let fixture = Fixture::new();
        let alpha = fixture.host.data_root("alpha");
        fixture
            .host
            .write_registry_entries(Some("alpha"), &[("alpha", alpha.as_path())]);
        let copy = tempfile::tempdir().expect("tempdir");
        let copied = copy.path().join("registry.toml");
        std::fs::copy(fixture.host.registry_path(), &copied).expect("copy registry");
        assert_clean(
            &check_registry(&copied, fixture.host.path()),
            "a copied registry judged against the host root",
        );
        assert_trips(
            &check_registry(&copied, copy.path()),
            Rule::T2Registry,
            "\"alpha\"",
        );
    }

    // T3

    /// A compliant root can still hold a store that is a link to the
    /// outside; the code opens whatever path it assembles.
    #[test]
    fn a_store_linked_out_of_a_compliant_root_trips() {
        let fixture = Fixture::new();
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let target = elsewhere.path().join("catalog.db");
        std::fs::write(&target, b"outside").expect("write target");
        let alpha = fixture.host.data_root("alpha");
        symlink(&target, alpha.join("catalog.db")).expect("symlink");
        assert_trips(&fixture.check(), Rule::T3Links, "is a symbolic link");
    }

    /// A hard link reaches outside as surely as a symbolic one.
    #[test]
    fn a_hard_linked_file_trips() {
        let fixture = Fixture::new();
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let target = elsewhere.path().join("shared");
        std::fs::write(&target, b"outside").expect("write target");
        let alpha = fixture.host.data_root("alpha");
        std::fs::hard_link(&target, alpha.join("shared")).expect("hard link");
        assert_trips(&fixture.check(), Rule::T3Links, "has 2 links");
    }

    /// An entry that disappears between listing and inspection is a
    /// daemon removing its socket, not a finding and not an error.
    #[test]
    fn a_vanishing_entry_is_not_a_finding() {
        let fixture = Fixture::new();
        let doomed = fixture.host.cwd().join("gone-soon");
        std::fs::write(&doomed, b"x").expect("write");
        let findings = check_tree(fixture.dir.path(), &mut |path| {
            if path.file_name() == doomed.file_name() {
                std::fs::remove_file(path).expect("remove under the walker");
            }
        });
        assert_clean(&findings, "a vanishing entry");
    }

    // T1

    /// Every variable the plan sets is checked on the composed
    /// environment, by a list the test writes out: a leak of any one of
    /// them, and any unknown `BOOKRACK_*` that slipped through, trips.
    #[test]
    fn every_isolation_variable_left_to_the_host_trips() {
        let fixture = Fixture::new();
        assert_clean(&fixture.check(), "premise: the fresh fixture");
        let expected = [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_RUNTIME_DIR",
            REGISTRY_ENV,
            RUNTIME_DIR_ENV,
            DAEMON_STATE_DIR_ENV,
            MCP_ADDR_ENV,
            NO_DOTENV_ENV,
            OLLAMA_URL_ENV,
        ];
        for key in expected {
            let mut gone = Fixture::new();
            unset(&mut gone.env, key);
            assert_trips(&gone.check(), Rule::T1Environment, key);

            let mut hostile = Fixture::new();
            set(&mut hostile.env, key, "/host/elsewhere");
            assert_trips(&hostile.check(), Rule::T1Environment, key);
        }
        let mut data_dir = Fixture::new();
        set(&mut data_dir.env, DATA_DIR_ENV, "/host/library");
        assert_trips(&data_dir.check(), Rule::T1Environment, DATA_DIR_ENV);

        let mut unknown = Fixture::new();
        set(&mut unknown.env, "BOOKRACK_SOMETHING_NEW", "1");
        assert_trips(
            &unknown.check(),
            Rule::T1Environment,
            "BOOKRACK_SOMETHING_NEW",
        );
    }

    /// In a dotenv run the switch must be absent: a `1` that leaked in
    /// from the parent would make the child ignore the placed `.env`.
    #[test]
    fn a_no_dotenv_switch_in_a_dotenv_scenario_trips() {
        let mut fixture = Fixture::with_dotenv(b"BOOKRACK_LOG=info\n");
        assert_clean(&fixture.check(), "premise: the dotenv fixture");
        set(&mut fixture.env, NO_DOTENV_ENV, "1");
        assert_trips(&fixture.check(), Rule::T1Environment, NO_DOTENV_ENV);
    }

    // The containment rule

    /// A `..` past the existing prefix is an escape, whatever it would
    /// fold to lexically; one inside the existing prefix is resolved by
    /// the filesystem.
    #[test]
    fn a_parent_component_past_the_existing_prefix_trips() {
        let fixture = Fixture::new();
        let host = fixture.host_root();
        fixture.host.data_root("alpha");
        let escapes = host.join("nonexistent/../../../outside");
        assert!(!is_within(&escapes, &host).expect("judged"));
        let folds_inside = host.join("nonexistent/../roots/alpha");
        assert!(
            !is_within(&folds_inside, &host).expect("judged"),
            "a `..` after a component that does not exist must not be folded away",
        );
        let plain = host.join("roots/alpha/new/file");
        assert!(is_within(&plain, &host).expect("judged"));
        let existing_parent = host.join("roots/alpha/../alpha");
        assert!(is_within(&existing_parent, &host).expect("judged"));
        assert!(!is_within(Path::new("relative/path"), &host).expect("judged"));
    }

    // T4

    /// `dotenvy` searches upward, so a `.env` anywhere above the working
    /// directory reaches the child.
    #[test]
    fn a_dotenv_above_the_working_directory_trips() {
        let fixture = Fixture::new();
        std::fs::write(fixture.host_root().join(".env"), b"X=1\n").expect("write .env");
        assert_trips(&fixture.check(), Rule::T4Dotenv, "reachable");
    }

    /// In a dotenv run the file in the working directory must be the
    /// one this run placed, byte for byte, and must be there.
    #[test]
    fn a_dotenv_of_another_content_trips() {
        let fixture = Fixture::with_dotenv(b"BOOKRACK_LOG=info\n");
        std::fs::write(fixture.host.cwd().join(".env"), b"BOOKRACK_LOG=debug\n")
            .expect("overwrite .env");
        assert_trips(
            &fixture.check(),
            Rule::T4Dotenv,
            "not the one this run placed",
        );

        let missing = Fixture::with_dotenv(b"BOOKRACK_LOG=info\n");
        std::fs::remove_file(missing.host.cwd().join(".env")).expect("remove .env");
        assert_trips(&missing.check(), Rule::T4Dotenv, "is missing");
    }

    // T5

    /// A portable data layer beside the binary outranks every
    /// configuration source.
    #[test]
    fn a_portable_data_root_beside_the_binary_trips() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.dir.path().join("run/bin/bookrack-data"))
            .expect("portable layer");
        assert_trips(&fixture.check(), Rule::T5PortableLayer, "bookrack-data");
    }

    // T6

    /// A sandbox deep enough that its control socket's path does not fit
    /// a socket address is refused before a daemon fails to bind.
    #[test]
    fn a_runtime_path_past_the_socket_limit_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let deep = dir.path().join("a".repeat(120));
        let host = Sandbox::create_at(&deep.join("host")).expect("deep host");
        let env = composed(&host, None, false);
        let findings = check(&Input {
            dir: &deep,
            host: &host,
            env: &env,
            argv: &[],
            stub_url: None,
            dotenv: None,
        });
        assert_trips(&findings, Rule::T6SocketPath, "control.sock");
    }

    // T7

    #[test]
    fn a_row_outside_the_sandbox_trips() {
        let fixture = Fixture::new();
        let host = fixture.host_root();
        let mut r = report(&host, None);
        set_row(&mut r, "data_dir", json!("/host/library"));
        assert_trips(&judge(&r, &host, None), Rule::T7BinaryView, "\"data_dir\"");
    }

    /// A location the home directory derived shows up only as a shadowed
    /// value when the environment wins, and it still has to be inside.
    #[test]
    fn a_shadowed_home_path_trips() {
        let fixture = Fixture::new();
        let host = fixture.host_root();
        let mut r = report(&host, None);
        for row in r["rows"].as_array_mut().expect("rows") {
            if row["key"] == "runtime_dir" {
                row["shadowed"] = json!([{ "layer": "platform", "site": "x",
                                           "value": "/host/home/.cache/bookrack" }]);
            }
        }
        assert_trips(
            &judge(&r, &host, None),
            Rule::T7BinaryView,
            "a shadowed value",
        );
    }

    #[test]
    fn a_report_missing_a_required_row_trips() {
        let fixture = Fixture::new();
        let host = fixture.host_root();
        for key in LOCATION_ROWS.iter().chain(["no_dotenv"].iter()) {
            let mut r = report(&host, None);
            r["rows"]
                .as_array_mut()
                .expect("rows")
                .retain(|row| row["key"] != *key);
            assert_trips(&judge(&r, &host, None), Rule::T7BinaryView, key);
        }
        let findings = judge(&json!({ "problem": {} }), &host, None);
        assert_trips(&findings, Rule::T7BinaryView, "no `rows` array");
    }

    #[test]
    fn a_row_of_the_wrong_type_trips() {
        let fixture = Fixture::new();
        let host = fixture.host_root();
        let mut r = report(&host, None);
        set_row(&mut r, "registry", json!(42));
        assert_trips(
            &judge(&r, &host, None),
            Rule::T7BinaryView,
            "neither a string nor null",
        );

        let mut r = report(&host, None);
        for row in r["rows"].as_array_mut().expect("rows") {
            if row["key"] == "registry" {
                row["shadowed"] = json!("not an array");
            }
        }
        assert_trips(&judge(&r, &host, None), Rule::T7BinaryView, "not an array");

        let mut r = report(&host, None);
        r["rows"]
            .as_array_mut()
            .expect("rows")
            .push(json!({ "value": "x" }));
        assert_trips(
            &judge(&r, &host, None),
            Rule::T7BinaryView,
            "no string `key`",
        );

        assert_trips(
            &judge(&json!([]), &host, None),
            Rule::T7BinaryView,
            "no `rows` array",
        );
        let findings = judge_report(
            b"not json",
            &BinaryView {
                host_root: &host,
                cwd: &host.join("cwd"),
                dotenv: None,
            },
        );
        assert_trips(&findings, Rule::T7BinaryView, "not JSON");
    }

    /// Outside a dotenv run the child must not have read any `.env`.
    #[test]
    fn an_unexpected_dotenv_trips() {
        let fixture = Fixture::new();
        let host = fixture.host_root();
        let mut r = report(&host, None);
        r["dotenv_path"] = json!(host.join("cwd/.env").display().to_string());
        assert_trips(
            &judge(&r, &host, None),
            Rule::T7BinaryView,
            "outside a dotenv run",
        );

        let placed = host.join("cwd/.env");
        let r = report(&host, None);
        assert_trips(
            &judge(&r, &host, Some(&placed)),
            Rule::T7BinaryView,
            "not the placed",
        );
    }

    /// A relative value is read against the child's working directory,
    /// winner and shadowed alike.
    #[test]
    fn a_relative_backup_dir_that_climbs_out_trips() {
        let fixture = Fixture::new();
        let host = fixture.host_root();
        let mut r = report(&host, None);
        set_row(&mut r, "backup_dir", json!("../../../outside"));
        assert_trips(
            &judge(&r, &host, None),
            Rule::T7BinaryView,
            "\"backup_dir\"",
        );

        let mut r = report(&host, None);
        for row in r["rows"].as_array_mut().expect("rows") {
            if row["key"] == "backup_dir" {
                row["shadowed"] = json!([{ "layer": "default", "site": "x",
                                           "value": "../../../outside" }]);
            }
        }
        assert_trips(
            &judge(&r, &host, None),
            Rule::T7BinaryView,
            "a shadowed value",
        );

        let mut r = report(&host, None);
        set_row(&mut r, "backup_dir", json!("backups"));
        assert_clean(&judge(&r, &host, None), "a relative value inside the cwd");
    }

    /// The rows the sandbox always supplies cannot be null; the two
    /// that depend on a resolved library can.
    #[test]
    fn a_required_row_with_a_null_value_trips() {
        let fixture = Fixture::new();
        let host = fixture.host_root();
        for key in ALWAYS_SET_ROWS {
            let mut r = report(&host, None);
            set_row(&mut r, key, Value::Null);
            assert_trips(&judge(&r, &host, None), Rule::T7BinaryView, key);
        }
        let mut r = report(&host, None);
        set_row(&mut r, "data_dir", Value::Null);
        set_row(&mut r, "backup_dir", Value::Null);
        assert_clean(&judge(&r, &host, None), "nullable rows");
    }

    #[test]
    fn a_no_dotenv_row_with_the_wrong_value_trips() {
        let fixture = Fixture::new();
        let host = fixture.host_root();
        let mut r = report(&host, None);
        set_row(&mut r, "no_dotenv", json!("0"));
        assert_trips(&judge(&r, &host, None), Rule::T7BinaryView, "\"no_dotenv\"");

        let placed = host.join("cwd/.env");
        let mut r = report(&host, Some(&placed));
        set_row(&mut r, "no_dotenv", json!("1"));
        assert_trips(
            &judge(&r, &host, Some(&placed)),
            Rule::T7BinaryView,
            "\"no_dotenv\"",
        );
    }

    /// The judge reads the fields it needs and ignores the rest, so a
    /// report that grows does not start failing.
    #[test]
    fn a_report_with_extra_fields_passes() {
        let fixture = Fixture::new();
        let host = fixture.host_root();
        let mut r = report(&host, None);
        r["problem"] = json!({ "summary": "x", "retryable": false });
        r["dotenv_foreign"] = json!([{ "key": "CI", "status": "rejected" }]);
        r["something_new"] = json!(1);
        assert_clean(&judge(&r, &host, None), "a report with extra fields");
    }

    /// T7 carries exactly the global `--library` T8 stripped: not one
    /// after a bare `--`, and never `--data-dir`.
    #[test]
    fn the_t7_command_carries_only_the_stripped_library() {
        let args = |items: &[&str]| -> Vec<OsString> { items.iter().map(OsString::from).collect() };
        let with =
            t7_command(&argv::strip_globals(&args(&["--library", "beta", "status"])).unwrap());
        assert_eq!(
            with,
            args(&["config", "effective", "--json", "--library", "beta"])
        );
        for argv in [
            &["--data-dir=x", "status"][..],
            &["status", "--", "--library", "beta"][..],
        ] {
            let cmd = t7_command(&argv::strip_globals(&args(argv)).unwrap());
            assert_eq!(cmd, args(&["config", "effective", "--json"]), "{argv:?}");
        }
    }

    // T8

    #[test]
    fn a_path_argument_outside_the_sandbox_trips() {
        for argv in [
            &["ingest", "/elsewhere/x.txt"][..],
            &["--data-dir=/elsewhere", "status"][..],
            &["diagnose", "--out=/elsewhere"][..],
        ] {
            let fixture = Fixture::new().argv(argv);
            assert_trips(&fixture.check(), Rule::T8Arguments, "/elsewhere");
        }
    }

    /// Relative values climbing out of the host tree from the working
    /// directory, with and without an `=`.
    #[test]
    fn a_parent_past_the_host_root_trips() {
        for argv in [
            &["--data-dir", "../..", "status"][..],
            &["--data-dir=../../..", "status"][..],
            &["libraries", "add", "x", "../../run"][..],
            &["run", "--runtime-dir=../../.."][..],
            &["libraries", "config", "alpha", "ollama_url=../../x"][..],
        ] {
            let fixture = Fixture::new().argv(argv);
            assert_trips(&fixture.check(), Rule::T8Arguments, "outside");
        }
    }

    /// A relative value that stays inside the host tree passes: a rule
    /// refusing every relative path would pass the negatives too.
    #[test]
    fn a_parent_inside_the_host_passes() {
        for argv in [
            &["--data-dir=..", "status"][..],
            &["ingest", "../inputs/b1.txt"][..],
            &["diagnose", "--out=../inputs/o"][..],
        ] {
            let fixture = Fixture::new().argv(argv);
            assert_clean(
                &only(&fixture.check(), Rule::T8Arguments)
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>(),
                &format!("{argv:?}"),
            );
        }
    }

    #[test]
    fn a_tilde_argument_trips() {
        for argv in [
            &["ingest", "~/x.txt"][..],
            &["--data-dir=~", "status"][..],
            &["diagnose", "--out=~/x"][..],
            &["libraries", "config", "alpha", "ollama_url=~/x"][..],
        ] {
            let fixture = Fixture::new().argv(argv);
            assert_trips(&fixture.check(), Rule::T8Arguments, "literal `~`");
        }
    }

    #[test]
    fn a_foreign_scheme_trips() {
        for argv in [
            &["fetch", "file:///etc/passwd"][..],
            &["fetch", "ftp://x/y"][..],
            &["libraries", "config", "alpha", "ollama_url=file:///x"][..],
        ] {
            let fixture = Fixture::new().argv(argv);
            assert_trips(&fixture.check(), Rule::T8Arguments, "scheme");
        }
        for argv in [
            &["fetch", "https://x/y"][..],
            &["fetch", "http://127.0.0.1:9"][..],
            &[
                "libraries",
                "config",
                "alpha",
                "ollama_url=http://127.0.0.1:9",
            ][..],
        ] {
            let fixture = Fixture::new().argv(argv);
            assert_clean(
                &only(&fixture.check(), Rule::T8Arguments)
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>(),
                &format!("{argv:?}"),
            );
        }
    }

    /// A path inside JSON params is judged after decoding, so escapes
    /// cannot hide it.
    #[test]
    fn an_encoded_path_in_rpc_params_trips() {
        let fixture = Fixture::new().argv(&[
            "rpc",
            "call",
            "library.info",
            r#"{"library":"\/elsewhere"}"#,
        ]);
        assert_trips(&fixture.check(), Rule::T8Arguments, "/elsewhere");
    }

    #[test]
    fn a_write_method_through_rpc_trips() {
        for argv in [
            &[
                "rpc",
                "call",
                "library.fork",
                r#"{"new_name":"x","data_dir":"inside"}"#,
            ][..],
            &["rpc", "call", "queue.pause"][..],
        ] {
            let fixture = Fixture::new().argv(argv);
            assert_trips(
                &fixture.check(),
                Rule::T8Arguments,
                "not a read-only method",
            );
        }
    }

    /// Global flags come off before the `rpc` grammar looks at positions,
    /// so where `--json` sits changes nothing.
    #[test]
    fn global_flags_are_stripped_before_the_rpc_grammar() {
        for argv in [
            &["--json", "rpc", "call", "daemon.version"][..],
            &["rpc", "call", "daemon.version", "--json"][..],
            &["rpc", "--json", "call", "daemon.version"][..],
            &["--library", "beta", "rpc", "list"][..],
        ] {
            let fixture = Fixture::new().argv(argv);
            assert_clean(
                &only(&fixture.check(), Rule::T8Arguments)
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>(),
                &format!("{argv:?}"),
            );
        }
        let fixture = Fixture::new().argv(&["rpc", "--json", "call", "queue.pause"]);
        assert_trips(&fixture.check(), Rule::T8Arguments, "queue.pause");
    }

    #[test]
    fn an_rpc_of_unknown_shape_trips() {
        for argv in [
            &["rpc"][..],
            &["rpc", "foo"][..],
            &["rpc", "call"][..],
            &["rpc", "list", "extra"][..],
            &["rpc", "call", "daemon.version", "{}", "extra"][..],
        ] {
            let fixture = Fixture::new().argv(argv);
            assert!(
                !only(&fixture.check(), Rule::T8Arguments).is_empty(),
                "{argv:?} must be refused"
            );
        }
    }

    // T9

    /// A held lock belongs to a live daemon; its socket must be the
    /// sandbox's own, on all four counts.
    #[test]
    fn a_held_lock_pointing_at_a_foreign_socket_trips() {
        let fixture = Fixture::new();
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let foreign = elsewhere.path().join("control.sock");
        let _listener = UnixListener::bind(&foreign).expect("bind");
        let _held = held_lock(&fixture, &foreign);
        assert_trips(&fixture.check(), Rule::T9ControlEndpoint, "held");
    }

    /// A held lock whose socket is gone is a daemon that is not
    /// reachable: refused, not tolerated as stale.
    #[test]
    fn a_held_lock_whose_socket_is_gone_trips() {
        let fixture = Fixture::new();
        let own = fixture.host.runtime_dir().join("control.sock");
        let _held = held_lock(&fixture, &own);
        assert_trips(
            &fixture.check(),
            Rule::T9ControlEndpoint,
            "cannot be inspected",
        );
    }

    /// A stale lock is still a routing entry: `discover` dials whatever
    /// it names, so its shape is judged even with nobody holding it.
    #[test]
    fn a_stale_lock_naming_a_foreign_path_trips() {
        let fixture = Fixture::new();
        let elsewhere = tempfile::tempdir().expect("tempdir");
        stale_lock(&fixture, &elsewhere.path().join("control.sock"));
        assert_trips(&fixture.check(), Rule::T9ControlEndpoint, "stale");
    }

    /// What a cleanly stopped daemon leaves: the lock file with the
    /// sandbox's own socket named, nobody holding it, the socket
    /// removed. That passes; demanding the socket exist would refuse
    /// every sandbox from its second command on.
    #[test]
    fn a_stale_lock_naming_the_sandbox_socket_passes() {
        let fixture = Fixture::new();
        stale_lock(&fixture, &fixture.host.runtime_dir().join("control.sock"));
        assert_clean(&fixture.check(), "a stale lock naming the sandbox's socket");
    }

    // The control

    /// A fresh sandbox with the tool's own plan passes every pure rule,
    /// and a compliant report passes T7: a tripwire that refused
    /// everything would pass every test above.
    #[test]
    fn a_fresh_sandbox_passes() {
        let fixture = Fixture::new().argv(&["status", "--json"]);
        assert_clean(&fixture.check(), "a fresh sandbox");
        let host = fixture.host_root();
        assert_clean(
            &judge(&report(&host, None), &host, None),
            "a compliant report",
        );
        let placed = host.join("cwd/.env");
        assert_clean(
            &judge(&report(&host, Some(&placed)), &host, Some(&placed)),
            "a compliant dotenv report",
        );
        let with_stub = Fixture::new();
        let mut with_stub = Fixture {
            env: composed(&with_stub.host, Some("http://127.0.0.1:4242"), false),
            ..with_stub
        };
        with_stub.stub_url = Some("http://127.0.0.1:4242".to_string());
        assert_clean(&with_stub.check(), "a sandbox with a running stub");
    }
}
