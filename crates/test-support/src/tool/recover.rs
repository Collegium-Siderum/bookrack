// SPDX-License-Identifier: Apache-2.0

//! Stopping a sandbox's background starts, and closing the ones whose
//! processes are gone.
//!
//! An unfinished instance is judged from its files and from what can be
//! observed of it. A supervisor that answers with its own nonce is asked
//! to stop; one that closes as it is asked leaves its `exit.json`. Every
//! other unfinished instance is never signaled: it is closed only once
//! every process it records is gone, the host's session lock is free,
//! and its spawn record does not leave a daemon unaccounted for. Pids
//! reused by an unrelated process, or orphans no one reaps, only make
//! the judgement lean toward "still there".

use std::time::Duration;

use bookrack_core::Problem;
use serde_json::{Value, json};

use super::detach::{self, Reason, Registration, read_state_opt, write_recovered};
use super::exec::{ExitRecord, Hosting, SpawnRecord, StartRecord};
use super::stub::{forget_hint, poll_exit};
use super::supervise::{CLIENT_MARGIN, STATUS_DEADLINE, ask};
use super::{
    Call, InstanceKind, Outcome, STAGE_INSTANCE, STAGE_SUPERVISOR, Session, ToolPaths, ToolResult,
};
use crate::child::TEARDOWN_BUDGET;

/// How long a judge waits for an `exit.json` from a background process
/// that may be closing.
// setting: internal -- how long the tool waits for a closing instance's record
pub(crate) const EXIT_POLL: Duration = Duration::from_secs(2);

/// How long `daemon stop` gives the daemon to leave after `SIGTERM`
/// unless told otherwise.
// setting: internal -- the tool's default stop deadline, overridden by --timeout
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// What a stopping command hands back: the call's outcome and the lines
/// it prints.
#[derive(Debug)]
pub struct Report {
    pub outcome: Outcome,
    pub lines: Vec<String>,
}

/// What a daemon instance's files say.
#[derive(Debug, Clone)]
pub struct DaemonFiles {
    pub hosting: Hosting,
    /// `supervisor.json` is there.
    pub registered: bool,
    pub spawn: Option<SpawnRecord>,
}

/// What could be observed of a daemon instance.
#[derive(Debug, Clone)]
pub struct DaemonProbes {
    /// The supervisor's phase, when it answered `status` with the
    /// registered nonce.
    pub answered: Option<String>,
    /// An `exit.json` appeared within [`EXIT_POLL`].
    pub exit_after_poll: bool,
    /// The recorded processes still alive, by role.
    pub alive: Vec<(&'static str, u32)>,
    /// Whether the host's session lock is held, or why that cannot be
    /// told.
    pub lock_held: Result<bool, String>,
}

/// Where a daemon instance is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonState {
    /// A supervisor answered as itself.
    Running { phase: String },
    /// The supervisor did not answer, but wrote its `exit.json`.
    Closing,
    /// The intent is recorded; no supervisor registered, so no daemon
    /// started.
    NeverHandedOff,
    /// Every recorded process is gone and the lock is free.
    Gone,
    /// A recorded process is alive or the lock is held.
    Held { detail: String },
    /// The spawn record says a daemon was being spawned and holds no
    /// pid: a daemon may be running that nothing recorded.
    Unknown { detail: String },
}

impl DaemonState {
    pub fn as_str(&self) -> &'static str {
        match self {
            DaemonState::Running { .. } => "running",
            DaemonState::Closing => "closing",
            DaemonState::NeverHandedOff => "never-handed-off",
            DaemonState::Gone => "gone",
            DaemonState::Held { .. } => "held",
            DaemonState::Unknown { .. } => "unknown",
        }
    }
}

/// Judge a daemon instance from its files and what was observed.
pub fn judge(files: &DaemonFiles, probes: &DaemonProbes) -> DaemonState {
    if files.hosting == Hosting::Detached {
        if !files.registered {
            return DaemonState::NeverHandedOff;
        }
        if let Some(phase) = &probes.answered {
            return DaemonState::Running {
                phase: phase.clone(),
            };
        }
        if probes.exit_after_poll {
            return DaemonState::Closing;
        }
    }
    let spawning = matches!(
        files.spawn,
        Some(SpawnRecord {
            spawning: true,
            pid: None,
            ..
        })
    );
    let mut found = Vec::new();
    if spawning {
        found.push("spawn.json records spawning with no pid".to_string());
    }
    for (role, pid) in &probes.alive {
        found.push(format!("the {role} pid {pid} is alive"));
    }
    match &probes.lock_held {
        Ok(false) => {}
        Ok(true) => found.push("the host's session lock is held".to_string()),
        Err(why) => found.push(format!(
            "whether the host's session lock is held cannot be told: {why}"
        )),
    }
    let detail = found.join("; ");
    if spawning {
        DaemonState::Unknown { detail }
    } else if !found.is_empty() {
        DaemonState::Held { detail }
    } else {
        DaemonState::Gone
    }
}

/// What a stub instance's files say.
#[derive(Debug, Clone)]
pub struct StubFiles {
    /// `serve.json` is there.
    pub registered: bool,
}

/// What could be observed of a stub instance.
#[derive(Debug, Clone)]
pub struct StubProbes {
    /// The stub answered at its URL with the registered nonce.
    pub answered: bool,
    /// An `exit.json` appeared within [`EXIT_POLL`].
    pub exit_after_poll: bool,
    /// The registered process is alive.
    pub serve_alive: bool,
    /// Why the stub did not answer, for a refusal's detail.
    pub detail: String,
}

/// Where a stub instance is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StubState {
    Running,
    Closing,
    NeverHandedOff,
    Gone,
    Held { detail: String },
}

impl StubState {
    pub fn as_str(&self) -> &'static str {
        match self {
            StubState::Running => "running",
            StubState::Closing => "closing",
            StubState::NeverHandedOff => "never-handed-off",
            StubState::Gone => "gone",
            StubState::Held { .. } => "held",
        }
    }
}

/// Judge a stub instance from its files and what was observed.
pub fn judge_stub(files: &StubFiles, probes: &StubProbes) -> StubState {
    if !files.registered {
        return StubState::NeverHandedOff;
    }
    if probes.answered {
        return StubState::Running;
    }
    if probes.exit_after_poll {
        return StubState::Closing;
    }
    if probes.serve_alive {
        return StubState::Held {
            detail: format!("{}; its process is alive", probes.detail),
        };
    }
    StubState::Gone
}

/// The highest-numbered ended instance of `kind` with its record, or
/// `null` when none has ended.
pub(crate) fn last_ended(paths: &ToolPaths, kind: InstanceKind) -> Value {
    let dir = paths.instances_dir(kind);
    if paths.check(&dir, super::Expect::DirOrAbsent).is_err() {
        return Value::Null;
    }
    let mut numbers: Vec<u32> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse().ok()))
        .collect();
    numbers.sort_unstable();
    for n in numbers.into_iter().rev() {
        let instance = paths.instance(kind, n);
        if !super::instance_ended(&instance) {
            continue;
        }
        let exit: Option<Value> = read_state_opt(paths, &instance.join("exit.json"))
            .ok()
            .flatten();
        let recovered: Option<Value> = read_state_opt(paths, &instance.join("recovered.json"))
            .ok()
            .flatten();
        return json!({ "n": n, "exit": exit, "recovered": recovered });
    }
    Value::Null
}

/// A daemon instance's files, read.
struct DaemonView {
    start: Option<StartRecord>,
    registration: Option<Registration>,
    spawn: Option<SpawnRecord>,
}

impl DaemonView {
    fn hosting(&self) -> Hosting {
        self.start.as_ref().map(|s| s.hosting).unwrap_or_default()
    }
}

/// Judge daemon instance `n`, collecting what [`judge`] needs.
async fn judge_daemon(
    paths: &ToolPaths,
    n: u32,
) -> ToolResult<(DaemonState, DaemonView, DaemonProbes)> {
    let instance = paths.daemon_instance(n);
    let refuse = |why: String| {
        Outcome::tool_error(
            STAGE_INSTANCE,
            Problem::new("cannot read the daemon instance").detail(why),
        )
    };
    let view = DaemonView {
        start: read_state_opt(paths, &instance.join("start.json")).map_err(refuse)?,
        registration: read_state_opt(paths, &instance.join("supervisor.json")).map_err(refuse)?,
        spawn: read_state_opt(paths, &instance.join("spawn.json")).map_err(refuse)?,
    };
    let hosting = view.hosting();
    let files = DaemonFiles {
        hosting,
        registered: view.registration.is_some(),
        spawn: view.spawn.clone(),
    };
    let mut probes = DaemonProbes {
        answered: None,
        exit_after_poll: false,
        alive: Vec::new(),
        lock_held: Ok(false),
    };
    if hosting == Hosting::Detached
        && let Some(reg) = &view.registration
    {
        let sock = instance.join("supervisor.sock");
        if let Ok(answer) = ask(&sock, &json!({ "op": "status" }), STATUS_DEADLINE).await
            && answer["nonce"] == reg.nonce.as_str()
        {
            probes.answered = answer["phase"].as_str().map(str::to_string);
        }
        if probes.answered.is_none() {
            probes.exit_after_poll = poll_exit(&instance).await;
        }
    }
    if let Some(reg) = &view.registration {
        probes.alive.push(("supervisor", reg.pid));
    }
    if let Some(pid) = view.spawn.as_ref().and_then(|s| s.pid) {
        probes.alive.push(("daemon", pid));
    }
    if hosting == Hosting::InProcess
        && let Some(start) = &view.start
    {
        probes.alive.push(("holder", start.parent_pid));
    }
    probes.alive.retain(|(_, pid)| detach::pid_alive(*pid));
    let lock = paths
        .host()
        .join("runtime")
        .join(bookrack_session::tty_lock_name());
    probes.lock_held = bookrack_session::lock_is_held(&lock).map_err(|e| format!("{e:#}"));
    Ok((judge(&files, &probes), view, probes))
}

impl Session {
    /// `daemon stop`: stop every unfinished daemon start of the sandbox,
    /// or close the ones whose processes are gone. With `forget`, close
    /// every unfinished instance without sending anything.
    pub async fn daemon_stop_instances(
        &mut self,
        within: Duration,
        forget: bool,
    ) -> ToolResult<Report> {
        let mut call = self
            .begin(
                "daemon stop",
                vec![format!("{within:?}"), format!("forget={forget}")],
                true,
            )
            .await?;
        let mut lines = Vec::new();
        let result = self
            .daemon_stop_detached(&mut call, within, forget, &mut lines)
            .await;
        let outcome = match self.end(call, result) {
            Ok(outcome) | Err(outcome) => outcome,
        };
        Ok(Report { outcome, lines })
    }

    async fn daemon_stop_detached(
        &self,
        call: &mut Call,
        within: Duration,
        forget: bool,
        lines: &mut Vec<String>,
    ) -> ToolResult<Outcome> {
        let paths = self.paths().clone();
        let unfinished = self
            .unfinished_instances(InstanceKind::Daemon)
            .map_err(|e| Outcome::io(STAGE_INSTANCE, "list the daemon instances", e))?;
        if unfinished.is_empty() {
            lines.push("nothing to stop".to_string());
            lines.push(last_ended(&paths, InstanceKind::Daemon).to_string());
            return Ok(Outcome::ok());
        }
        let mut result = Ok(Outcome::ok());
        for n in unfinished {
            call.set_instance(n);
            let instance = paths.daemon_instance(n);
            let io_err = |e| Outcome::io(STAGE_INSTANCE, "write recovered.json", e);
            let recovered = |reason, detail| {
                write_recovered(&instance, reason, detail)
                    .map(|record| json!({ "n": n, "recovered": record }).to_string())
                    .map_err(io_err)
            };
            if forget {
                lines.push(recovered(Reason::Forgotten, None)?);
                continue;
            }
            let (state, view, _) = judge_daemon(&paths, n).await?;
            let this = match state {
                DaemonState::Running { .. } => {
                    let sock = instance.join("supervisor.sock");
                    let request = json!({ "op": "stop", "within_ms": within.as_millis() as u64 });
                    match ask(&sock, &request, within + TEARDOWN_BUDGET + CLIENT_MARGIN).await {
                        Ok(answer) => {
                            let exit: Option<ExitRecord> =
                                serde_json::from_value(answer["exit"].clone()).ok();
                            let phase = answer["phase"].as_str().unwrap_or("").to_string();
                            call.note(format!("the supervisor stopped in phase {phase}"));
                            lines.push(json!({ "n": n, "exit": exit }).to_string());
                            match (phase.as_str(), exit) {
                                ("running", Some(exit)) => Ok(exit.outcome()),
                                (_, Some(_)) => Ok(Outcome::ok()),
                                (_, None) => Err(Outcome::tool_error(
                                    STAGE_SUPERVISOR,
                                    Problem::new("the daemon's supervisor answered the stop oddly")
                                        .detail(answer.to_string()),
                                )),
                            }
                        }
                        Err(why) => {
                            let exit: Option<ExitRecord> = if poll_exit(&instance).await {
                                read_state_opt(&paths, &instance.join("exit.json"))
                                    .ok()
                                    .flatten()
                            } else {
                                None
                            };
                            match exit {
                                Some(exit) => {
                                    call.note(format!("the stop went unanswered ({why}); the supervisor wrote exit.json"));
                                    lines.push(json!({ "n": n, "exit": exit }).to_string());
                                    Ok(Outcome::ok())
                                }
                                None => Err(Outcome::tool_error(
                                    STAGE_SUPERVISOR,
                                    Problem::new("the daemon's supervisor did not answer the stop")
                                        .detail(why)
                                        .hint("run 'bookrack-sandbox daemon stop <dir>' again to judge the instance"),
                                )),
                            }
                        }
                    }
                }
                DaemonState::Closing => {
                    let exit: Option<ExitRecord> =
                        read_state_opt(&paths, &instance.join("exit.json"))
                            .ok()
                            .flatten();
                    call.note(format!("run/daemon/{n} was closing"));
                    lines.push(json!({ "n": n, "exit": exit }).to_string());
                    Ok(Outcome::ok())
                }
                DaemonState::NeverHandedOff => {
                    lines.push(recovered(Reason::NeverHandedOff, None)?);
                    Ok(Outcome::ok())
                }
                DaemonState::Gone => {
                    let reason = match view.hosting() {
                        Hosting::Detached => Reason::LostSupervisor,
                        Hosting::InProcess => Reason::LostHolder,
                    };
                    lines.push(recovered(reason, None)?);
                    Ok(Outcome::ok())
                }
                DaemonState::Held { detail } | DaemonState::Unknown { detail } => {
                    Err(Outcome::refused(
                        STAGE_INSTANCE,
                        Problem::new("cannot tell whether the daemon is gone")
                            .detail(format!("run/daemon/{n}: {detail}"))
                            .hint(forget_hint("daemon")),
                    ))
                }
            };
            result = match (result, this) {
                (Err(first), _) => Err(first),
                (Ok(_), Err(next)) => Err(next),
                (Ok(prev), Ok(next)) if prev.class == super::Class::Ok => Ok(next),
                (Ok(prev), Ok(_)) => Ok(prev),
            };
        }
        result
    }

    /// `daemon status`: judge every unfinished daemon start without
    /// acting on any.
    pub async fn daemon_status(&self) -> ToolResult<Value> {
        let call = self.begin("daemon status", Vec::new(), false).await?;
        let result = async {
            let paths = self.paths().clone();
            let unfinished = self
                .unfinished_instances(InstanceKind::Daemon)
                .map_err(|e| Outcome::io(STAGE_INSTANCE, "list the daemon instances", e))?;
            let mut entries = Vec::new();
            for n in unfinished {
                let (state, view, probes) = judge_daemon(&paths, n).await?;
                let (phase, detail) = match &state {
                    DaemonState::Running { phase } => (Some(phase.clone()), None),
                    DaemonState::Held { detail } | DaemonState::Unknown { detail } => {
                        (None, Some(detail.clone()))
                    }
                    _ => (None, None),
                };
                let hosting = view.hosting();
                entries.push(json!({
                    "n": n,
                    "hosting": hosting,
                    "state": state.as_str(),
                    "phase": phase,
                    "pids": {
                        "supervisor": view.registration.as_ref().map(|r| r.pid),
                        "daemon": view.spawn.as_ref().and_then(|s| s.pid),
                        "parent": match hosting {
                            Hosting::InProcess => view.start.as_ref().map(|s| s.parent_pid),
                            Hosting::Detached => None,
                        },
                    },
                    "lock_held": probes.lock_held.ok(),
                    "detail": detail,
                }));
            }
            Ok(json!({
                "unfinished": entries,
                "last_ended": last_ended(&paths, InstanceKind::Daemon),
            }))
        }
        .await;
        match result {
            Ok(value) => {
                self.end(call, Ok(Outcome::ok()))?;
                Ok(value)
            }
            Err(outcome) => Err(self.end(call, Err(outcome)).unwrap_err()),
        }
    }

    /// `teardown`: in one call, stop the daemon, then the stub. The exit
    /// code is the one of the two that is not zero, the daemon's when
    /// both are not.
    pub async fn teardown(&mut self) -> ToolResult<Report> {
        let mut call = self.begin("teardown", Vec::new(), true).await?;
        let mut lines = Vec::new();
        let daemon = match self
            .daemon_stop_detached(&mut call, DEFAULT_STOP_TIMEOUT, false, &mut lines)
            .await
        {
            Ok(outcome) | Err(outcome) => outcome,
        };
        let stub = match self.stub_stop_inner(&mut call, false, &mut lines).await {
            Ok(outcome) | Err(outcome) => outcome,
        };
        call.note(format!("daemon stop: exit {}", daemon.exit_code()));
        call.note(format!("stub stop: exit {}", stub.exit_code()));
        let outcome = if daemon.exit_code() != 0 || stub.exit_code() == 0 {
            daemon
        } else {
            stub
        };
        let failed = outcome.exit_code() != 0;
        let outcome = match self.end(call, if failed { Err(outcome) } else { Ok(outcome) }) {
            Ok(outcome) | Err(outcome) => outcome,
        };
        Ok(Report { outcome, lines })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(hosting: Hosting, registered: bool, spawn: Option<SpawnRecord>) -> DaemonFiles {
        DaemonFiles {
            hosting,
            registered,
            spawn,
        }
    }

    fn probes(
        answered: Option<&str>,
        exit_after_poll: bool,
        alive: &[(&'static str, u32)],
        lock_held: Result<bool, String>,
    ) -> DaemonProbes {
        DaemonProbes {
            answered: answered.map(str::to_string),
            exit_after_poll,
            alive: alive.to_vec(),
            lock_held,
        }
    }

    /// The full table: every row of the judgement, and the order in
    /// which its conditions are read.
    #[test]
    fn the_daemon_judgement_reads_its_rows_in_order() {
        use DaemonState::*;
        let spawned = Some(SpawnRecord::spawned(43));
        let spawning = Some(SpawnRecord::spawning());
        let cases: Vec<(DaemonFiles, DaemonProbes, &str)> = vec![
            // A detached start that never registered.
            (
                files(Hosting::Detached, false, None),
                probes(None, false, &[("daemon", 1)], Ok(true)),
                "never-handed-off",
            ),
            // A supervisor that answers is running, whatever else holds.
            (
                files(Hosting::Detached, true, spawned.clone()),
                probes(Some("running"), true, &[("supervisor", 2)], Ok(true)),
                "running",
            ),
            // One that wrote its exit.json while it was asked.
            (
                files(Hosting::Detached, true, spawned.clone()),
                probes(None, true, &[("supervisor", 2)], Ok(true)),
                "closing",
            ),
            // Lost: everything gone.
            (
                files(Hosting::Detached, true, spawned.clone()),
                probes(None, false, &[], Ok(false)),
                "gone",
            ),
            // Lost, with the daemon still alive.
            (
                files(Hosting::Detached, true, spawned.clone()),
                probes(None, false, &[("daemon", 43)], Ok(false)),
                "held",
            ),
            // Lost, with only the lock held.
            (
                files(Hosting::Detached, true, spawned.clone()),
                probes(None, false, &[], Ok(true)),
                "held",
            ),
            // Lost, with a lock that cannot be read.
            (
                files(Hosting::Detached, true, spawned.clone()),
                probes(None, false, &[], Err("io".into())),
                "held",
            ),
            // Spawning with no pid is unknown even with nothing alive.
            (
                files(Hosting::Detached, true, spawning.clone()),
                probes(None, false, &[], Ok(false)),
                "unknown",
            ),
            // In process: a supervisor's answer is never asked for.
            (
                files(Hosting::InProcess, false, spawned.clone()),
                probes(Some("running"), true, &[], Ok(false)),
                "gone",
            ),
            (
                files(Hosting::InProcess, false, spawned.clone()),
                probes(None, false, &[("holder", 7)], Ok(false)),
                "held",
            ),
            (
                files(Hosting::InProcess, false, spawning),
                probes(None, false, &[], Ok(false)),
                "unknown",
            ),
            (
                files(Hosting::InProcess, false, None),
                probes(None, false, &[], Ok(false)),
                "gone",
            ),
        ];
        for (i, (f, p, want)) in cases.iter().enumerate() {
            let state = judge(f, p);
            assert_eq!(state.as_str(), *want, "row {i}: {f:?} {p:?}");
            if let Running { phase } = &state {
                assert_eq!(phase, "running");
            }
        }
        let held = judge(
            &files(Hosting::Detached, true, Some(SpawnRecord::spawning())),
            &probes(None, false, &[("supervisor", 2)], Ok(true)),
        );
        let Unknown { detail } = held else {
            panic!("spawning with no pid is unknown: {held:?}")
        };
        assert!(
            detail.contains("spawning") && detail.contains("lock is held"),
            "the detail names the spawn record and the lock: {detail}"
        );
    }

    #[test]
    fn the_stub_judgement_reads_its_rows_in_order() {
        let p = |answered, exit_after_poll, serve_alive| StubProbes {
            answered,
            exit_after_poll,
            serve_alive,
            detail: "no url".to_string(),
        };
        let registered = StubFiles { registered: true };
        let unregistered = StubFiles { registered: false };
        let cases = [
            (&unregistered, p(true, true, true), "never-handed-off"),
            (&registered, p(true, false, true), "running"),
            (&registered, p(false, true, true), "closing"),
            (&registered, p(false, false, true), "held"),
            (&registered, p(false, false, false), "gone"),
        ];
        for (i, (f, p, want)) in cases.iter().enumerate() {
            assert_eq!(judge_stub(f, p).as_str(), *want, "row {i}");
        }
    }
}
