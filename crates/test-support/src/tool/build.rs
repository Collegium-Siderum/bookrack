// SPDX-License-Identifier: Apache-2.0

//! The binary under test: built or copied into the sandbox, recorded,
//! and checked against its record before every use.

use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use bookrack_core::Problem;
use serde::{Deserialize, Serialize};

use super::{
    Outcome, STAGE_BINARY, ToolPaths, ToolResult, problems, read_json, release, sig, write_json,
};
use crate::sandbox::Sandbox;

/// The name an incoming binary is staged under beside `bookrack` until
/// the variable-claim gate passes.
pub const INCOMING: &str = "bookrack.incoming";

/// A hook run once between two steps.
pub type StepHook = Box<dyn FnOnce() -> io::Result<()>>;

/// Points in an install at which a test may step in.
#[derive(Default)]
pub struct BuildHooks {
    /// Runs after the incoming binary has taken `bookrack`'s place and
    /// before its record is written.
    pub after_rename: Option<StepHook>,
}

/// Up to this size the copied binary is re-hashed before every use; a
/// larger one is checked by length, mtime and inode alone.
// setting: internal -- the size up to which the copied binary is re-hashed on every use, not a value to tune
pub const SMALL_BINARY: u64 = 1024 * 1024;

/// How the binary under test was obtained and what it looked like when
/// it was copied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryRecord {
    /// `cargo`, `prebuilt` or `stand-in`.
    pub kind: String,
    /// Where the copy came from.
    pub origin: String,
    pub sha256: String,
    pub len: u64,
    pub mtime_secs: i64,
    pub mtime_nanos: i64,
    pub inode: u64,
    pub head: Option<String>,
    pub source_state: Option<String>,
    pub profile: Option<String>,
    pub features: Vec<String>,
}

/// The parts of a file's metadata a later look compares.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stat {
    len: u64,
    mtime_secs: i64,
    mtime_nanos: i64,
    inode: u64,
}

fn stat(path: &Path) -> io::Result<Stat> {
    let meta = std::fs::symlink_metadata(path)?;
    Ok(Stat {
        len: meta.len(),
        mtime_secs: meta.mtime(),
        mtime_nanos: meta.mtime_nsec(),
        inode: meta.ino(),
    })
}

/// The one `bookrack` artifact a cargo build reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub executable: PathBuf,
    pub fresh: bool,
    pub profile: Option<String>,
    pub features: Vec<String>,
}

/// Pick the `bookrack` artifact out of cargo's JSON message stream:
/// exactly one `compiler-artifact` message for target `bookrack` with
/// an executable.
pub fn pick_artifact(stdout: &[u8]) -> Result<Artifact, String> {
    let mut found = Vec::new();
    for line in stdout.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if message.get("reason").and_then(|v| v.as_str()) != Some("compiler-artifact") {
            continue;
        }
        if message
            .get("target")
            .and_then(|t| t.get("name"))
            .and_then(|v| v.as_str())
            != Some("bookrack")
        {
            continue;
        }
        let Some(executable) = message.get("executable").and_then(|v| v.as_str()) else {
            continue;
        };
        let profile = message.get("profile").map(|p| {
            let opt = p.get("opt_level").and_then(|v| v.as_str()).unwrap_or("?");
            let debug = p
                .get("debuginfo")
                .map(|v| v.to_string())
                .unwrap_or_default();
            format!("opt-level={opt} debuginfo={debug}")
        });
        let features = message
            .get("features")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        found.push(Artifact {
            executable: PathBuf::from(executable),
            fresh: message
                .get("fresh")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            profile,
            features,
        });
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err("cargo reported no bookrack executable".to_string()),
        n => Err(format!("cargo reported {n} bookrack executables")),
    }
}

/// One digest over the source tree's state: the commit, the porcelain
/// status, the diff against HEAD, and every untracked file's path and
/// digest.
pub fn digest_source_state(
    head: &str,
    status: &[u8],
    diff: &[u8],
    untracked: &[(String, String)],
) -> String {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(head.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(status);
    bytes.push(0);
    bytes.extend_from_slice(diff);
    bytes.push(0);
    for (path, sha) in untracked {
        bytes.extend_from_slice(path.as_bytes());
        bytes.push(b'=');
        bytes.extend_from_slice(sha.as_bytes());
        bytes.push(0);
    }
    sig::sha256_bytes(&bytes)
}

async fn git(workspace: &Path, args: &[&str]) -> ToolResult<Vec<u8>> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .await
        .map_err(|e| Outcome::io(STAGE_BINARY, &format!("run git {}", args.join(" ")), e))?;
    if !output.status.success() {
        return Err(Outcome::tool_error(
            STAGE_BINARY,
            Problem::new(format!("git {} failed", args.join(" ")))
                .detail(String::from_utf8_lossy(&output.stderr).into_owned()),
        ));
    }
    Ok(output.stdout)
}

/// The commit and the source-state digest of `workspace`.
pub async fn source_state(workspace: &Path) -> ToolResult<(String, String)> {
    let head = String::from_utf8_lossy(&git(workspace, &["rev-parse", "HEAD"]).await?)
        .trim()
        .to_string();
    let status = git(workspace, &["status", "--porcelain=v1", "-z"]).await?;
    let diff = git(workspace, &["diff", "HEAD"]).await?;
    let listed = git(
        workspace,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .await?;
    let mut untracked = Vec::new();
    for raw in listed.split(|&b| b == 0).filter(|p| !p.is_empty()) {
        let rel = String::from_utf8_lossy(raw).into_owned();
        let sha = sig::sha256_file(&workspace.join(&rel))
            .map_err(|e| Outcome::io(STAGE_BINARY, &format!("hash the untracked file {rel}"), e))?;
        untracked.push((rel, sha));
    }
    Ok((
        head.clone(),
        digest_source_state(&head, &status, &diff, &untracked),
    ))
}

async fn cargo_build(workspace: &Path) -> ToolResult<Artifact> {
    let output = tokio::process::Command::new("cargo")
        .args([
            "build",
            "-p",
            "bookrack-cli",
            "--bin",
            "bookrack",
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(workspace)
        .output()
        .await
        .map_err(|e| Outcome::io(STAGE_BINARY, "run cargo build", e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr
            .lines()
            .rev()
            .take(30)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Err(Outcome::tool_error(
            STAGE_BINARY,
            Problem::new("cargo build failed")
                .detail(tail.join("\n"))
                .hint("fix the build in the workspace, then run 'bookrack-sandbox build <dir>'"),
        ));
    }
    pick_artifact(&output.stdout).map_err(|why| {
        Outcome::tool_error(
            STAGE_BINARY,
            Problem::new("cargo build reported no usable artifact").detail(why),
        )
    })
}

/// Build `bookrack` in `workspace` and copy it into the sandbox,
/// checking that neither the source tree nor the artifact moved
/// underneath the copy. With `host`, the copy passes the variable-claim
/// gate before it replaces the binary in place.
pub async fn build_cargo(
    paths: &ToolPaths,
    host: Option<&Sandbox>,
    workspace: &Path,
    hooks: &mut BuildHooks,
) -> ToolResult<BinaryRecord> {
    let (head, state_before) = source_state(workspace).await?;
    let artifact = cargo_build(workspace).await?;
    let before = stat(&artifact.executable)
        .map_err(|e| Outcome::io(STAGE_BINARY, "inspect the built binary", e))?;
    let (sha256, incoming) = stage(paths, &artifact.executable)?;
    // A fresh second build re-links the artifact into place, so its inode
    // moves even when nothing was rebuilt; the digest is what has to hold.
    let again = cargo_build(workspace).await?;
    let after = sig::sha256_file(&again.executable)
        .map_err(|e| Outcome::io(STAGE_BINARY, "hash the built binary again", e))?;
    if !again.fresh || after != sha256 || again.executable != artifact.executable {
        return Err(Outcome::tool_error(
            STAGE_BINARY,
            Problem::new("the build output changed while it was copied")
                .detail(format!(
                    "first build: {} {before:?} sha256 {sha256}; second build: fresh={} {} sha256 {after}",
                    artifact.executable.display(),
                    again.fresh,
                    again.executable.display()
                ))
                .hint("another cargo invocation is running against the same target directory; let it finish and run build again"),
        ));
    }
    let (_, state_after) = source_state(workspace).await?;
    if state_after != state_before {
        let _ = std::fs::remove_file(&incoming);
        return Err(Outcome::tool_error(
            STAGE_BINARY,
            Problem::new("the source tree changed during the build")
                .hint("stop editing, then run 'bookrack-sandbox build <dir>' again"),
        ));
    }
    let partial = BinaryRecord {
        kind: "cargo".to_string(),
        origin: artifact.executable.display().to_string(),
        sha256,
        len: 0,
        mtime_secs: 0,
        mtime_nanos: 0,
        inode: 0,
        head: Some(head),
        source_state: Some(state_before),
        profile: artifact.profile,
        features: artifact.features,
    };
    install(paths, host, &incoming, partial, hooks).await
}

/// Copy a binary, a prebuilt one or a stand-in script, from `origin`
/// into the sandbox and record it. With `host`, the copy passes the
/// variable-claim gate before it replaces the binary in place; a
/// stand-in passes `None`.
pub async fn install_copy(
    paths: &ToolPaths,
    host: Option<&Sandbox>,
    origin: &Path,
    kind: &str,
    hooks: &mut BuildHooks,
) -> ToolResult<BinaryRecord> {
    let origin = origin
        .canonicalize()
        .map_err(|e| Outcome::io(STAGE_BINARY, &format!("resolve {}", origin.display()), e))?;
    let (sha256, incoming) = stage(paths, &origin)?;
    let partial = BinaryRecord {
        kind: kind.to_string(),
        origin: origin.display().to_string(),
        sha256,
        len: 0,
        mtime_secs: 0,
        mtime_nanos: 0,
        inode: 0,
        head: None,
        source_state: None,
        profile: None,
        features: Vec::new(),
    };
    install(paths, host, &incoming, partial, hooks).await
}

/// The second half of an install: the gate, when there is a host to
/// judge it against; the rename into place; the record. A gate that
/// does not pass removes the incoming copy and leaves the installed
/// binary and its record as they were.
async fn install(
    paths: &ToolPaths,
    host: Option<&Sandbox>,
    incoming: &Path,
    mut record: BinaryRecord,
    hooks: &mut BuildHooks,
) -> ToolResult<BinaryRecord> {
    if let Some(host) = host
        && let Err(outcome) = release::gate(paths, host, incoming).await
    {
        let _ = std::fs::remove_file(incoming);
        return Err(outcome);
    }
    let io_err = |what: &str, err: io::Error| Outcome::io(STAGE_BINARY, what, err);
    std::fs::rename(incoming, paths.bin())
        .map_err(|e| io_err("move the copied binary into place", e))?;
    if let Some(after_rename) = hooks.after_rename.take() {
        after_rename().map_err(|e| io_err("continue the install", e))?;
    }
    let copied = stat(&paths.bin()).map_err(|e| io_err("inspect the copied binary", e))?;
    record.len = copied.len;
    record.mtime_secs = copied.mtime_secs;
    record.mtime_nanos = copied.mtime_nanos;
    record.inode = copied.inode;
    write_json(&paths.binary_record(), &record)
        .map_err(|e| io_err("write the binary record", e))?;
    Ok(record)
}

/// Copy `origin` to `run/bin/bookrack.incoming`, hashing on the way; a
/// stale incoming copy is removed first.
fn stage(paths: &ToolPaths, origin: &Path) -> ToolResult<(String, PathBuf)> {
    let io_err = |what: &str, err: io::Error| Outcome::io(STAGE_BINARY, what, err);
    let incoming = paths.bin_dir().join(INCOMING);
    match std::fs::remove_file(&incoming) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err("remove a stale incoming binary", e)),
    }
    let sha256 = copy_into(origin, &incoming)?;
    Ok((sha256, incoming))
}

/// Copy `origin` to `tmp`, created exclusively, hashing on the way.
fn copy_into(origin: &Path, tmp: &Path) -> ToolResult<String> {
    let io_err = |what: &str, err: io::Error| Outcome::io(STAGE_BINARY, what, err);
    let mut src = std::fs::File::open(origin).map_err(|e| io_err("open the binary to copy", e))?;
    let mode = src
        .metadata()
        .map_err(|e| io_err("inspect the binary to copy", e))?
        .permissions()
        .mode();
    let mut dst = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)
        .map_err(|e| io_err("create the copied binary", e))?;
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = src
            .read(&mut buf)
            .map_err(|e| io_err("read the binary to copy", e))?;
        if n == 0 {
            break;
        }
        sha2::Digest::update(&mut hasher, &buf[..n]);
        dst.write_all(&buf[..n])
            .map_err(|e| io_err("write the copied binary", e))?;
    }
    dst.flush()
        .map_err(|e| io_err("flush the copied binary", e))?;
    dst.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(|e| io_err("set the copied binary's mode", e))?;
    drop(dst);
    let digest = sha2::Digest::finalize(hasher);
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// The record `run/binary.json` holds.
pub fn read_record(paths: &ToolPaths) -> ToolResult<BinaryRecord> {
    read_json(&paths.binary_record()).map_err(|e| {
        Outcome::refused(
            STAGE_BINARY,
            Problem::new("the sandbox has no binary record")
                .detail(e.to_string())
                .hint("run 'bookrack-sandbox build <dir>'"),
        )
    })
}

/// Check the copied binary against its record: length, mtime and inode
/// always, and the digest too when the copy is small.
pub fn verify(paths: &ToolPaths) -> ToolResult<BinaryRecord> {
    let record = read_record(paths)?;
    let now = stat(&paths.bin())
        .map_err(|e| Outcome::io(STAGE_BINARY, "inspect the copied binary", e))?;
    let recorded = Stat {
        len: record.len,
        mtime_secs: record.mtime_secs,
        mtime_nanos: record.mtime_nanos,
        inode: record.inode,
    };
    if now != recorded {
        return Err(Outcome::refused(
            STAGE_BINARY,
            problems::binary_changed(&format!("recorded {recorded:?}, found {now:?}")),
        ));
    }
    if record.len <= SMALL_BINARY {
        let sha = sig::sha256_file(&paths.bin())
            .map_err(|e| Outcome::io(STAGE_BINARY, "hash the copied binary", e))?;
        if sha != record.sha256 {
            return Err(Outcome::refused(
                STAGE_BINARY,
                problems::binary_changed(&format!(
                    "recorded sha256 {}, found {sha}",
                    record.sha256
                )),
            ));
        }
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTIFACT: &str = r#"{"reason":"compiler-artifact","target":{"name":"bookrack","kind":["bin"]},"profile":{"opt_level":"0","debuginfo":2},"features":[],"executable":"/t/debug/bookrack","fresh":true}"#;
    const OTHER: &str = r#"{"reason":"compiler-artifact","target":{"name":"bookrack-cli","kind":["lib"]},"profile":{"opt_level":"0"},"features":[],"executable":null,"fresh":true}"#;
    const MESSAGE: &str = r#"{"reason":"build-script-executed","package_id":"x"}"#;
    const OTHER_BIN: &str = r#"{"reason":"compiler-artifact","target":{"name":"bookrack-sandbox","kind":["bin"]},"profile":{"opt_level":"0"},"features":[],"executable":"/t/debug/bookrack-sandbox","fresh":true}"#;

    /// Only the `bookrack` binary's artifact message counts, and there
    /// has to be exactly one.
    #[test]
    fn the_artifact_message_is_picked_from_canned_output() {
        let stdout = format!("{MESSAGE}\n{OTHER}\n{OTHER_BIN}\n{ARTIFACT}\n");
        let artifact = pick_artifact(stdout.as_bytes()).expect("one artifact");
        assert_eq!(artifact.executable, PathBuf::from("/t/debug/bookrack"));
        assert!(artifact.fresh);
        assert_eq!(artifact.profile.as_deref(), Some("opt-level=0 debuginfo=2"));

        let none = format!("{MESSAGE}\n{OTHER}\n");
        assert!(
            pick_artifact(none.as_bytes())
                .unwrap_err()
                .contains("no bookrack")
        );
        let two = format!("{ARTIFACT}\n{ARTIFACT}\n");
        assert!(
            pick_artifact(two.as_bytes())
                .unwrap_err()
                .contains("2 bookrack")
        );
    }

    /// The digest moves with every part of the state, untracked files
    /// included.
    #[test]
    fn the_source_state_changes_when_an_untracked_file_does() {
        let base = digest_source_state(
            "abc",
            b" M x.rs\0",
            b"diff",
            &[("new.rs".into(), "aa".into())],
        );
        let other_sha = digest_source_state(
            "abc",
            b" M x.rs\0",
            b"diff",
            &[("new.rs".into(), "bb".into())],
        );
        let other_head = digest_source_state(
            "abd",
            b" M x.rs\0",
            b"diff",
            &[("new.rs".into(), "aa".into())],
        );
        let other_diff = digest_source_state(
            "abc",
            b" M x.rs\0",
            b"diff2",
            &[("new.rs".into(), "aa".into())],
        );
        let same = digest_source_state(
            "abc",
            b" M x.rs\0",
            b"diff",
            &[("new.rs".into(), "aa".into())],
        );
        assert_eq!(base, same);
        for other in [other_sha, other_head, other_diff] {
            assert_ne!(base, other);
        }
    }
}
