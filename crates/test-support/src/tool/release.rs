// SPDX-License-Identifier: Apache-2.0

//! A release golden: the `golden/` of a rich sandbox a released binary
//! built, frozen in place and imported into a new sandbox by a later
//! build.
//!
//! The golden carries the absolute path of the sandbox that built it,
//! in its registry and in its queue, so a copy elsewhere has to be
//! rewritten before a daemon can read it. The rewrite is a byte
//! replacement of one prefix and is exact only because the path was
//! refused at freeze time if it held a character TOML or JSON would
//! escape. Everything else about the import is a check: the record's
//! schema, the signature of what is copied, and a scan for the old
//! prefix anywhere the rewrite did not reach.
//!
//! Freezing takes the write permission off `golden/` and the record;
//! the frozen state is read from the marker before any lock is taken,
//! so a frozen sandbox refuses every command but a freeze that finishes
//! an interrupted one and the import that reads it.

use std::collections::BTreeSet;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_core::Problem;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::exec::{Capture, ExecOpts};
use super::snapshot::{self, Direction, copy_tree};
pub use super::tripwire::resolve_existing_prefix;
use super::{
    Call, Class, CreateHooks, InstanceKind, Marker, Outcome, STAGE_BINARY, STAGE_INSTANCE,
    STAGE_PATHS, STAGE_PRESET, Session, ToolPaths, ToolResult, now, read_json, sig, write_json,
};
use crate::child::{DaemonProcess, Output, SpawnOpts};
use crate::sandbox::Sandbox;

/// The schema of `release.json` this module writes and reads.
// setting: internal -- the release record's schema number, not a value to tune
pub const RELEASE_SCHEMA: u32 = 1;

/// How long the knob inventory probe of an incoming binary may take.
// setting: internal -- bounds a probe of the binary under test, not a value to tune
pub const KNOBS_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The name of the record a freeze writes beside `golden/`.
pub const RELEASE_RECORD: &str = "release.json";

/// The staging directory an import copies the golden into before it
/// takes the golden's place.
const IMPORT_DIR: &str = "golden.import";

/// What a freeze records about the golden it froze.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseRecord {
    pub schema: u32,
    /// What the binary under test printed for `--version`.
    pub version: String,
    pub binary_sha256: String,
    pub tool_version: String,
    /// The sandbox's absolute path at freeze time; the prefix the
    /// import rewrites.
    pub dir: String,
    /// `sig::signature` of `golden/` at freeze time, every line.
    pub golden_sig: Vec<String>,
    pub manifest_schema: u32,
    pub frozen_at: f64,
}

/// The problem every frozen refusal carries.
pub fn frozen_problem(dir: &Path) -> Problem {
    Problem::new("the sandbox is frozen")
        .detail(format!(
            "{} holds a release golden; its golden/ and release.json are read-only.",
            dir.display()
        ))
        .hint(
            "Import it with 'bookrack-sandbox new <dir> --preset release --from <this dir>' \
             instead of using it in place.",
        )
}

/// Whether `path` holds a character that TOML or JSON would escape:
/// a double quote, a backslash, or a control character. A byte-level
/// rewrite of the path is exact only when it does not.
pub fn needs_escaping(path: &Path) -> bool {
    path.to_string_lossy()
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control())
}

/// Whether two resolved paths are the same or one lies inside the
/// other.
pub fn overlaps(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

/// Replace every occurrence of `old` in `bytes` with `new`; the count
/// comes back with the result.
pub fn rewrite_prefix(bytes: &[u8], old: &[u8], new: &[u8]) -> (Vec<u8>, usize) {
    if old.is_empty() {
        return (bytes.to_vec(), 0);
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(old) {
            out.extend_from_slice(new);
            i += old.len();
            count += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    (out, count)
}

/// Whether a file under a golden tree is outside the scan for the old
/// prefix: a database and its journals, whose paths are product data,
/// a lance directory's contents, and the daemon's and the libraries'
/// logs, which are history.
pub fn scan_exempt(relative: &Path) -> bool {
    let name = relative
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.ends_with(".db") || name.ends_with(".db-wal") || name.ends_with(".db-shm") {
        return true;
    }
    relative.components().any(|c| {
        let part = c.as_os_str().to_string_lossy();
        part == "logs" || part.ends_with(".lance")
    })
}

/// The files under `root`, relative to it, that are not exempt from
/// the scan and hold `needle`.
pub fn scan_for(root: &Path, needle: &[u8]) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|_| path.clone());
                if scan_exempt(&relative) {
                    continue;
                }
                let bytes = std::fs::read(&path)?;
                if rewrite_prefix(&bytes, needle, b"").1 > 0 {
                    found.push(relative);
                }
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The variables of `plan_keys` that the knob inventory `inventory`
/// (the body of `config knobs --json`) does not claim under `variable`,
/// in knobs or in native dependencies.
pub fn judge_knobs(plan_keys: &[String], inventory: &Value) -> Vec<String> {
    let mut claimed: BTreeSet<&str> = BTreeSet::new();
    for section in ["knobs", "native_dependencies"] {
        if let Some(rows) = inventory.get(section).and_then(Value::as_array) {
            for row in rows {
                if let Some(variable) = row.get("variable").and_then(Value::as_str) {
                    claimed.insert(variable);
                }
            }
        }
    }
    let mut unclaimed: Vec<String> = plan_keys
        .iter()
        .filter(|key| !claimed.contains(key.as_str()))
        .cloned()
        .collect();
    unclaimed.sort();
    unclaimed.dedup();
    unclaimed
}

/// Recursively set or clear the write bits under `root`, `root`
/// itself included, directories after their contents on clearing.
pub fn set_writable(root: &Path, writable: bool) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(root)?;
    if meta.file_type().is_dir() {
        if writable {
            chmod(root, &meta, true)?;
        }
        for entry in std::fs::read_dir(root)? {
            set_writable(&entry?.path(), writable)?;
        }
        if !writable {
            chmod(root, &meta, false)?;
        }
        return Ok(());
    }
    chmod(root, &meta, writable)
}

fn chmod(path: &Path, meta: &std::fs::Metadata, writable: bool) -> io::Result<()> {
    let mode = meta.permissions().mode();
    let next = if writable {
        mode | 0o200
    } else {
        mode & !0o222
    };
    if next != mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(next))?;
    }
    Ok(())
}

/// How the knob probe of an incoming binary failed, for the refusal's
/// detail.
fn probe_problem(incoming: &Path, why: String) -> Problem {
    Problem::new("the binary under test does not answer its knob inventory")
        .detail(format!("{} --json config knobs: {why}", incoming.display()))
        .hint(
            "The gate needs a bookrack build that has 'config knobs'; a binary without it \
             would write outside the sandbox for every variable it does not know.",
        )
}

/// The variable-claim gate: run `incoming --json config knobs` under
/// the isolation plan `host` gets, and refuse unless every `BOOKRACK_*`
/// variable the plan sets is claimed. A probe that does not exit 0,
/// does not finish, holds its streams open or prints something other
/// than the inventory fails the gate too.
pub async fn gate(paths: &ToolPaths, host: &Sandbox, incoming: &Path) -> ToolResult<()> {
    let plan = super::plan_for(incoming, host, None, false, std::env::vars_os());
    let plan_keys: Vec<String> = plan
        .set
        .iter()
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .filter(|key| key.starts_with("BOOKRACK_"))
        .collect();
    let mut cmd = std::process::Command::new(incoming);
    cmd.args(["--json", "config", "knobs"]);
    plan.apply(&mut cmd);
    let refused = |why: String| Outcome::refused(STAGE_BINARY, probe_problem(incoming, why));
    let child = DaemonProcess::spawn_with(tokio::process::Command::from(cmd), SpawnOpts::default())
        .map_err(|e| refused(format!("could not be started: {e}")))?;
    let exit = child
        .exit_within(KNOBS_PROBE_TIMEOUT)
        .await
        .map_err(|failure| refused(format!("did not complete: {}", failure.why)))?;
    if !exit.status.success() {
        return Err(refused(format!(
            "exited {}: {}",
            exit.status,
            exit.stderr.trim_end()
        )));
    }
    let stdout = match exit.output {
        Output::Memory { stdout, .. } => stdout,
        Output::Streamed { .. } => return Err(refused("the output was streamed away".to_string())),
    };
    let inventory: Value = serde_json::from_slice(&stdout)
        .map_err(|e| refused(format!("the output is not JSON: {e}")))?;
    if !inventory.get("knobs").is_some_and(Value::is_array) {
        return Err(refused("the output has no `knobs` array".to_string()));
    }
    let unclaimed = judge_knobs(&plan_keys, &inventory);
    if !unclaimed.is_empty() {
        return Err(Outcome::refused(
            STAGE_BINARY,
            Problem::new("the binary under test does not know every variable the sandbox sets")
                .detail(format!(
                    "Its knob inventory does not claim: {}.",
                    unclaimed.join(", ")
                ))
                .hint(
                    "A variable the binary does not read is one it cannot honour, so that part \
                     of the isolation would land in the real tree. Use a build that knows the \
                     variable.",
                ),
        ));
    }
    let _ = paths;
    Ok(())
}

/// The release record at `dir`, read and checked for its schema.
pub fn read_release(dir: &Path) -> Result<ReleaseRecord, String> {
    let record: ReleaseRecord = read_json(&dir.join(RELEASE_RECORD))
        .map_err(|e| format!("no readable release.json: {e}"))?;
    if record.schema != RELEASE_SCHEMA {
        return Err(format!(
            "release.json is schema {}, this tool reads schema {RELEASE_SCHEMA}",
            record.schema
        ));
    }
    Ok(record)
}

/// The checks an import runs before anything is created or cleared:
/// the target path can be written into TOML and JSON byte for byte, the
/// two paths do not overlap, the source carries a readable release
/// record, and its golden still matches that record.
pub fn preflight_import(target: &Path, from: &Path) -> ToolResult<ReleaseRecord> {
    if needs_escaping(target) {
        return Err(Outcome::refused(
            STAGE_PATHS,
            Problem::new("cannot import a release golden to this path")
                .detail(format!(
                    "{} holds a quote, a backslash or a control character, which TOML and \
                     JSON escape; the import writes the path byte for byte.",
                    target.display()
                ))
                .hint("Choose a sandbox path without those characters."),
        ));
    }
    let resolve = |path: &Path| {
        resolve_existing_prefix(path).map_err(|e| {
            Outcome::refused(
                STAGE_PATHS,
                Problem::new("cannot resolve a path of the import").detail(e.to_string()),
            )
        })
    };
    let target_resolved = resolve(target)?;
    let from_resolved = resolve(from)?;
    if overlaps(&target_resolved, &from_resolved) {
        return Err(Outcome::refused(
            STAGE_PATHS,
            Problem::new("the import's source and target overlap")
                .detail(format!(
                    "{} and {} are the same directory or one lies inside the other.",
                    from_resolved.display(),
                    target_resolved.display()
                ))
                .hint("Import into a directory outside the frozen sandbox."),
        ));
    }
    let record = read_release(&from_resolved).map_err(|why| {
        Outcome::refused(
            STAGE_PRESET,
            Problem::new(format!(
                "{} is not a frozen release golden",
                from_resolved.display()
            ))
            .detail(why)
            .hint("Point --from at a sandbox 'bookrack-sandbox freeze' has frozen."),
        )
    })?;
    let golden = from_resolved.join("golden");
    let now_sig = sig::signature(&golden)
        .map_err(|e| Outcome::io(STAGE_PRESET, "sign the release golden", e))?;
    if now_sig != record.golden_sig {
        return Err(Outcome::refused(
            STAGE_PRESET,
            Problem::new(format!(
                "the release golden at {} has changed since it was frozen",
                from_resolved.display()
            ))
            .detail(sig::compact_diff(&sig::diff(&record.golden_sig, &now_sig)))
            .hint(
                "Freeze the golden again from a sandbox that passes its checks, or import another.",
            ),
        ));
    }
    Ok(record)
}

impl Session {
    /// Import the frozen golden at `from`, whose record `release` was
    /// read by [`preflight_import`], into this sandbox: copy to a
    /// staging tree, check the copy, make it writable, rewrite the
    /// prefix, drop the locks, scan for what the rewrite missed, and
    /// put it in place as `golden/`. `release.json` of the source is
    /// left where it is.
    pub(crate) async fn import_release(
        &mut self,
        call: &mut Call,
        from: &Path,
        release: &ReleaseRecord,
        hooks: &mut CreateHooks,
    ) -> ToolResult<()> {
        let paths = self.paths().clone();
        let io_err = |what: &str, e| Outcome::io(STAGE_PRESET, what, e);
        let staging = paths.dir().join(IMPORT_DIR);
        if staging.exists() {
            std::fs::remove_dir_all(&staging).map_err(|e| io_err("clear the staging tree", e))?;
        }
        let from = resolve_existing_prefix(from).map_err(|e| io_err("resolve the source", e))?;
        let mut on_file = hooks.on_import_copy.take();
        let copied = copy_tree(
            &from.join("golden"),
            &staging,
            &mut |files| match &mut on_file {
                Some(hook) => hook(files),
                None => Ok(()),
            },
        );
        if let Err(e) = copied {
            let _ = release_staging(&staging);
            return Err(io_err("copy the release golden", e));
        }
        let staged_sig = sig::signature(&staging).map_err(|e| io_err("sign the copy", e))?;
        if staged_sig != release.golden_sig {
            let _ = release_staging(&staging);
            return Err(Outcome::refused(
                STAGE_PRESET,
                Problem::new(format!(
                    "the release golden at {} changed while it was copied",
                    from.display()
                ))
                .detail(sig::compact_diff(&sig::diff(
                    &release.golden_sig,
                    &staged_sig,
                )))
                .hint("Nothing else may write to a frozen sandbox; try the import again."),
            ));
        }
        set_writable(&staging, true).map_err(|e| io_err("make the copy writable", e))?;

        let old = format!("{}/host/", release.dir);
        let new = format!("{}/host/", paths.dir().display());
        let mut rewritten = 0;
        for relative in ["registry.toml", "daemon-state/queue.json"] {
            let path = staging.join(relative);
            if !path.is_file() {
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|e| io_err("read a file to rewrite", e))?;
            let (out, count) = rewrite_prefix(&bytes, old.as_bytes(), new.as_bytes());
            if count > 0 {
                std::fs::write(&path, out).map_err(|e| io_err("rewrite a file", e))?;
                rewritten += count;
            }
        }
        call.note(format!("import: rewrote {rewritten} paths"));
        let runtime = staging.join("runtime");
        if runtime.is_dir() {
            for entry in std::fs::read_dir(&runtime).map_err(|e| io_err("list runtime/", e))? {
                let entry = entry.map_err(|e| io_err("list runtime/", e))?;
                if entry
                    .file_type()
                    .map_err(|e| io_err("inspect runtime/", e))?
                    .is_file()
                {
                    std::fs::remove_file(entry.path()).map_err(|e| io_err("remove a lock", e))?;
                }
            }
        }
        let needle = format!("{}/", release.dir);
        let left = scan_for(&staging, needle.as_bytes()).map_err(|e| io_err("scan the copy", e))?;
        if !left.is_empty() {
            let _ = release_staging(&staging);
            return Err(Outcome::refused(
                STAGE_PRESET,
                Problem::new("the release golden names its old path outside the rewritten files")
                    .detail(
                        left.iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                    .hint(
                        "The import rewrites registry.toml and daemon-state/queue.json only; a \
                         path anywhere else would still point at the frozen sandbox.",
                    ),
            ));
        }
        let golden = paths.golden();
        if golden.exists() {
            std::fs::remove_dir_all(&golden).map_err(|e| io_err("clear golden/", e))?;
        }
        std::fs::rename(&staging, &golden).map_err(|e| io_err("move the copy into place", e))?;
        let imported = sig::signature(&golden).map_err(|e| io_err("sign golden/", e))?;
        call.note(format!("import: from {}", from.display()));
        call.note(format!("import: release {}", release.version));
        self.set_import_marker(&from, release, &imported)?;
        snapshot::switch(&paths, Direction::Restore, &mut |_| Ok(()))?;
        Ok(())
    }

    fn set_import_marker(
        &self,
        from: &Path,
        release: &ReleaseRecord,
        imported: &[String],
    ) -> ToolResult<()> {
        let marker_path = self.paths().marker();
        let mut marker: Marker = read_json(&marker_path)
            .map_err(|e| Outcome::io(STAGE_PATHS, "read the sandbox marker", e))?;
        marker.from = Some(from.display().to_string());
        marker.release_version = Some(release.version.clone());
        marker.imported_sig_sha256 = Some(sig::sha256_bytes(imported.join("\n").as_bytes()));
        write_json(&marker_path, &marker)
            .map_err(|e| Outcome::io(STAGE_PATHS, "write the sandbox marker", e))
    }

    /// `freeze`: run both checks on a ready rich sandbox with nothing
    /// unfinished, record what was frozen, mark the sandbox frozen, and
    /// take the write permission off `golden/` and the record. On a
    /// sandbox already frozen, confirm the golden still matches its
    /// record and finish the permission step.
    pub async fn freeze(&mut self) -> ToolResult<Outcome> {
        let mut call = self.begin_with("freeze", Vec::new(), true, true).await?;
        let marker = match Self::marker_prelock(self.paths().dir()) {
            Ok((_, marker)) => marker,
            Err(outcome) => return Err(self.end(call, Err(outcome)).unwrap_err()),
        };
        let dir = self.paths().dir().to_path_buf();
        let result = if marker.frozen {
            self.refreeze(&marker)
        } else {
            self.freeze_inner(&mut call, &marker).await
        };
        match result {
            Ok(()) => {
                self.end(call, Ok(Outcome::ok()))?;
                set_writable(&dir.join("golden"), false).map_err(|e| {
                    Outcome::io(STAGE_PATHS, "take the write permission off golden/", e)
                })?;
                set_writable(&dir.join(RELEASE_RECORD), false).map_err(|e| {
                    Outcome::io(STAGE_PATHS, "take the write permission off release.json", e)
                })?;
                Ok(Outcome::ok())
            }
            Err(outcome) => Err(self.end(call, Err(outcome)).unwrap_err()),
        }
    }

    fn refreeze(&self, marker: &Marker) -> ToolResult<()> {
        let dir = self.paths().dir();
        let record = read_release(dir).map_err(|why| {
            Outcome::refused(
                STAGE_PRESET,
                Problem::new("the frozen sandbox has no usable release record")
                    .detail(why)
                    .hint("The marker says frozen but release.json is missing or unreadable."),
            )
        })?;
        let now_sig = sig::signature(&self.paths().golden())
            .map_err(|e| Outcome::io(STAGE_PRESET, "sign golden/", e))?;
        if now_sig != record.golden_sig {
            return Err(Outcome::refused(
                STAGE_PRESET,
                Problem::new("the frozen golden does not match its record")
                    .detail(sig::compact_diff(&sig::diff(&record.golden_sig, &now_sig))),
            ));
        }
        let _ = marker;
        Ok(())
    }

    async fn freeze_inner(&mut self, call: &mut Call, marker: &Marker) -> ToolResult<()> {
        let paths = self.paths().clone();
        if needs_escaping(paths.dir()) {
            return Err(Outcome::refused(
                STAGE_PATHS,
                Problem::new("cannot freeze a sandbox at this path")
                    .detail(format!(
                        "{} holds a quote, a backslash or a control character, which TOML and \
                         JSON escape; an import rewrites the path byte for byte.",
                        paths.dir().display()
                    ))
                    .hint("Build the sandbox to freeze at a path without those characters."),
            ));
        }
        if marker.preset != "rich" {
            return Err(Outcome::refused(
                STAGE_PRESET,
                Problem::new(format!("cannot freeze a {} sandbox", marker.preset))
                    .detail("A release golden is the rich preset built by a released binary."),
            ));
        }
        for kind in [InstanceKind::Daemon, InstanceKind::Stub] {
            let unfinished = self
                .unfinished_instances(kind)
                .map_err(|e| Outcome::io(STAGE_INSTANCE, "list the instances", e))?;
            if !unfinished.is_empty() {
                return Err(Outcome::refused(
                    STAGE_INSTANCE,
                    match kind {
                        InstanceKind::Daemon => super::problems::unfinished_instances(&unfinished),
                        InstanceKind::Stub => super::problems::unfinished_stub(&unfinished),
                    },
                ));
            }
        }
        let findings = self.check_inner(call, true, &mut |_| Ok(())).await?;
        if !findings.is_empty() {
            return Err(Outcome::refused(
                STAGE_PRESET,
                Problem::new(format!(
                    "cannot freeze a golden that does not match its inputs: {} findings",
                    findings.len()
                ))
                .detail(
                    findings
                        .iter()
                        .map(|f| format!("{}: {}", f.key, f.detail))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            ));
        }
        let version = self.binary_version(call).await?;
        let binary = super::build::read_record(&paths)?;
        let golden_sig = sig::signature(&paths.golden())
            .map_err(|e| Outcome::io(STAGE_PRESET, "sign golden/", e))?;
        let manifest = super::rich::read_manifest(&paths.golden().join("inputs"))
            .map_err(|e| Outcome::io(STAGE_PRESET, "read the inputs' manifest", e))?;
        let record = ReleaseRecord {
            schema: RELEASE_SCHEMA,
            version,
            binary_sha256: binary.sha256,
            tool_version: super::TOOL_VERSION.to_string(),
            dir: paths.dir().display().to_string(),
            golden_sig,
            manifest_schema: manifest.schema,
            frozen_at: now(),
        };
        write_json(&paths.dir().join(RELEASE_RECORD), &record)
            .map_err(|e| Outcome::io(STAGE_PATHS, "write release.json", e))?;
        let mut marker = marker.clone();
        marker.frozen = true;
        write_json(&paths.marker(), &marker)
            .map_err(|e| Outcome::io(STAGE_PATHS, "write the sandbox marker", e))?;
        call.note(format!("frozen: {}", record.version));
        Ok(())
    }

    /// What the binary under test prints for `--version`, run inside
    /// the sandbox.
    async fn binary_version(&self, call: &mut Call) -> ToolResult<String> {
        let opts = ExecOpts {
            capture: Capture::Memory,
            ..ExecOpts::default()
        };
        let mut output = None;
        let args = [std::ffi::OsString::from("--version")];
        let outcome = self.exec_inner(call, &args, &opts, &mut output).await?;
        let stdout = match output {
            Some(Output::Memory { stdout, .. }) => stdout,
            _ => Vec::new(),
        };
        if (outcome.class, outcome.code) != (Class::Exited, Some(0)) {
            return Err(Outcome::tool_error(
                STAGE_BINARY,
                Problem::new("the binary under test does not report its version").detail(format!(
                    "--version ended as {:?} {:?}",
                    outcome.class, outcome.code
                )),
            ));
        }
        Ok(String::from_utf8_lossy(&stdout).trim().to_string())
    }
}

/// Remove a staging tree, restoring write permission first so a
/// read-only copy comes off too.
fn release_staging(staging: &Path) -> io::Result<()> {
    set_writable(staging, true)?;
    std::fs::remove_dir_all(staging)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn judge_knobs_reports_what_neither_section_claims() {
        let inventory = json!({
            "knobs": [
                { "key": "a", "variable": "BOOKRACK_A" },
                { "key": "b" },
            ],
            "native_dependencies": [ { "name": "x", "variable": "BOOKRACK_X" } ],
        });
        let plan = ["BOOKRACK_A", "BOOKRACK_X", "BOOKRACK_B", "BOOKRACK_B"]
            .map(String::from)
            .to_vec();
        assert_eq!(judge_knobs(&plan, &inventory), ["BOOKRACK_B"]);
        assert_eq!(judge_knobs(&plan[..2], &inventory), Vec::<String>::new());
        assert_eq!(judge_knobs(&plan, &json!({})), {
            let mut all = plan.clone();
            all.sort();
            all.dedup();
            all
        });
    }

    #[test]
    fn a_release_record_round_trips() {
        let record = ReleaseRecord {
            schema: RELEASE_SCHEMA,
            version: "bookrack 0.14.0".to_string(),
            binary_sha256: "ab".repeat(32),
            tool_version: super::super::TOOL_VERSION.to_string(),
            dir: "/s/k".to_string(),
            golden_sig: vec!["dir - - roots".to_string()],
            manifest_schema: 1,
            frozen_at: 1.5,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        write_json(&dir.path().join(RELEASE_RECORD), &record).expect("write");
        assert_eq!(read_release(dir.path()).expect("read"), record);
        let mut newer = record.clone();
        newer.schema = RELEASE_SCHEMA + 1;
        write_json(&dir.path().join(RELEASE_RECORD), &newer).expect("write");
        assert!(read_release(dir.path()).unwrap_err().contains("schema"));
    }

    #[test]
    fn an_existing_prefix_resolves_whether_or_not_the_tail_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().canonicalize().expect("canonical");
        std::fs::create_dir(real.join("a")).expect("a");
        assert_eq!(
            resolve_existing_prefix(&real.join("a")).unwrap(),
            real.join("a")
        );
        assert_eq!(
            resolve_existing_prefix(&real.join("a/b")).unwrap(),
            real.join("a/b")
        );
        assert_eq!(
            resolve_existing_prefix(&real.join("x/y/z")).unwrap(),
            real.join("x/y/z")
        );
        std::os::unix::fs::symlink(real.join("a"), real.join("link")).expect("link");
        assert_eq!(
            resolve_existing_prefix(&real.join("link/new")).unwrap(),
            real.join("a/new")
        );
        assert!(resolve_existing_prefix(&real.join("missing/../b")).is_err());
        assert!(resolve_existing_prefix(Path::new("relative/path")).is_err());
    }

    #[test]
    fn overlap_is_identity_or_containment() {
        let a = Path::new("/s/a");
        assert!(overlaps(a, a));
        assert!(overlaps(a, Path::new("/s/a/b")));
        assert!(overlaps(Path::new("/s/a/b"), a));
        assert!(!overlaps(a, Path::new("/s/ab")));
        assert!(!overlaps(a, Path::new("/t/a")));
    }

    #[test]
    fn a_prefix_is_rewritten_wherever_it_appears_and_nowhere_else() {
        let (out, n) = rewrite_prefix(b"x /s/a/host/r y", b"/s/a/host/", b"/t/b/host/");
        assert_eq!((out.as_slice(), n), (&b"x /t/b/host/r y"[..], 1));
        let (out, n) = rewrite_prefix(b"/s/a/host/1\n/s/a/host/2", b"/s/a/host/", b"/t/");
        assert_eq!((out.as_slice(), n), (&b"/t/1\n/t/2"[..], 2));
        let (out, n) = rewrite_prefix(b"/s/a2/host/r", b"/s/a/host/", b"/t/");
        assert_eq!((out.as_slice(), n), (&b"/s/a2/host/r"[..], 0));
    }

    #[test]
    fn the_scan_skips_databases_lance_and_logs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for (path, hit) in [
            ("notes.txt", true),
            ("roots/alpha/catalog.db", true),
            ("roots/alpha/catalog.db-wal", true),
            ("roots/alpha/lancedb/chunks.lance/data/x", true),
            ("daemon-state/logs/today.log", true),
            ("roots/alpha/logs/x.log", true),
            ("clean.txt", false),
        ] {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, if hit { "see /s/a/host/x" } else { "nothing" }).unwrap();
        }
        assert_eq!(
            scan_for(root, b"/s/a/").unwrap(),
            vec![PathBuf::from("notes.txt")]
        );
    }

    #[test]
    fn paths_that_toml_or_json_would_escape_are_recognised() {
        assert!(needs_escaping(Path::new("/s/a\"b")));
        assert!(needs_escaping(Path::new("/s/a\\b")));
        assert!(needs_escaping(Path::new("/s/a\u{1}b")));
        assert!(!needs_escaping(Path::new("/s/a b/c-d_e.f")));
    }

    #[test]
    fn write_permission_comes_off_and_back_on_recursively() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("g");
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::write(root.join("d/f"), "x").unwrap();
        set_writable(&root, false).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o222;
        assert_eq!(mode(&root), 0);
        assert_eq!(mode(&root.join("d/f")), 0);
        assert!(std::fs::write(root.join("d/f"), "y").is_err());
        set_writable(&root, true).unwrap();
        assert_ne!(mode(&root.join("d/f")), 0);
        std::fs::write(root.join("d/f"), "y").unwrap();
    }
}
