// SPDX-License-Identifier: Apache-2.0

//! `bookrack config effective`: what the configuration resolves to,
//! and which layer supplied each value.
//!
//! Needs no daemon. The report is assembled from the resolvers
//! themselves, so it answers on exactly the machines where nothing
//! else will — a data root that does not resolve produces a report
//! with the failure at its head, not an error instead of a report.
//!
//! When a daemon is running, its own snapshot is what is in use, and
//! the report says so: the table shows the daemon's values and marks
//! each row this shell would resolve differently after a restart.

use std::collections::BTreeSet;

use bookrack_config::{Config, ForeignStatus, LibrarySelection};
use bookrack_core::knob::{KnobOrigin, KnobReach};
use bookrack_core::{DaemonIdentity, Problem};
use bookrack_ops::settings::{Contributor, KNOB_CONTRIBUTORS};
use eyre::Result;
use serde::{Deserialize, Serialize};

use crate::error::BookrackCliError;
use crate::render::table::RowTable;

/// What a probe for the running daemon's configuration found.
#[derive(Debug, Clone)]
pub enum DaemonView {
    /// No daemon answered.
    NoDaemon,
    /// A daemon answered and does not serve `daemon.config`.
    NoSnapshot,
    /// The daemon's own account of its configuration.
    Snapshot(Box<DaemonSnapshot>),
}

/// The parts of a `daemon.config` reply this report reads.
#[derive(Debug, Clone, Deserialize)]
pub struct DaemonSnapshot {
    /// Which daemon answered.
    pub identity: DaemonIdentity,
    /// The process- and machine-scoped rows.
    pub rows: Vec<KnobOrigin>,
    /// Each mounted library's rows.
    pub libraries: Vec<LibrarySnapshot>,
    /// The libraries mounted when the daemon entered service.
    pub mounted_at_start: Vec<String>,
    /// Whether the queue worker is paused.
    pub queue_paused: bool,
}

/// One mounted library in a [`DaemonSnapshot`].
#[derive(Debug, Clone, Deserialize)]
pub struct LibrarySnapshot {
    /// The name it is served under.
    pub name: String,
    /// Its data root.
    pub data_dir: String,
    /// Its library-scoped rows, as resolved when it was mounted.
    pub rows: Vec<KnobOrigin>,
}

/// One knob the daemon and this shell resolve to different values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Difference {
    /// The knob.
    pub key: String,
    /// What the daemon uses; `None` when unset there.
    pub daemon: Option<String>,
    /// What a start from this shell would resolve.
    pub shell: Option<String>,
}

/// The knobs both sides carry whose values differ, in `shell` order. A
/// key only one side carries is not a difference: nothing can be said
/// about a value the other side did not report.
pub fn differences(shell: &[KnobOrigin], daemon: &[KnobOrigin]) -> Vec<Difference> {
    shell
        .iter()
        .filter_map(|ours| {
            let theirs = daemon.iter().find(|row| row.key == ours.key)?;
            (theirs.value != ours.value).then(|| Difference {
                key: ours.key.clone(),
                daemon: theirs.value.clone(),
                shell: ours.value.clone(),
            })
        })
        .collect()
}

/// The `daemon` node of the JSON report.
#[derive(Serialize)]
struct DaemonOut<'a> {
    /// Whether a daemon answered at all.
    reachable: bool,
    /// Whether it reported its configuration; `false` for a daemon
    /// that predates `daemon.config`. Absent when none answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity: Option<&'a DaemonIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rows: Option<Vec<RowOut<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    libraries: Option<Vec<LibraryOut<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    differences: Option<&'a [Difference]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mounted_at_start: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mounted_now: Option<Vec<&'a str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queue_paused: Option<bool>,
}

/// One mounted library in the `daemon` node.
#[derive(Serialize)]
struct LibraryOut<'a> {
    name: &'a str,
    data_dir: &'a str,
    rows: Vec<RowOut<'a>>,
}

/// One row as the JSON form carries it.
///
/// The field names are a contract: a caller reads this to learn what
/// resolved and where it can be changed, and renaming one silently
/// breaks that caller. `chain` is what makes the report answerable on
/// a machine with nothing configured, where every `layer` is the
/// built-in default and no variable name appears anywhere else.
#[derive(Serialize)]
struct RowOut<'a> {
    key: &'a str,
    value: Option<&'a str>,
    layer: &'a str,
    site: &'a str,
    shadowed: Vec<ShadowedOut<'a>>,
    chain: Vec<SiteOut<'a>>,
    reach: &'a str,
    /// Which library's root the value was read from, for the rows whose
    /// value is a property of one library. Absent on the rows that are
    /// not: they would be the same whichever library was selected.
    #[serde(skip_serializing_if = "Option::is_none")]
    scope_instance: Option<String>,
    read_at: &'a str,
    /// Why a layer of this row could not be read, when one could not.
    /// The row then reports what the remaining layers resolve.
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'a str>,
}

#[derive(Serialize)]
struct ShadowedOut<'a> {
    layer: &'a str,
    site: &'a str,
    value: &'a str,
}

#[derive(Serialize)]
struct SiteOut<'a> {
    layer: &'a str,
    site: &'a str,
}

/// One variable `.env` reached that no row above can account for.
///
/// `key` and `status` are a contract in the same way the row fields
/// are. `status` carries the token, not the sentence: the human table
/// words it for its own column, and a caller reading JSON wants the
/// two cases distinguishable without matching prose.
#[derive(Serialize)]
struct ForeignOut<'a> {
    key: &'a str,
    status: ForeignStatus,
}

/// One `BOOKRACK_…` variable this process carries that no knob of this
/// build reads.
///
/// `key` and `source` are a contract like the row fields. The value is
/// left out for the reason the foreign section leaves it out: nothing
/// in this build knows what the variable holds, so nothing can say it
/// is safe to print.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct UnclaimedOut {
    key: String,
    source: UnclaimedSource,
}

/// Where the process got an unclaimed variable, serialized as its token.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum UnclaimedSource {
    /// The process started with it.
    Environment,
    /// This process's `.env` supplied it.
    Dotenv,
}

/// One retired variable this process carries.
///
/// The fields are a contract like the row fields. The value is left out
/// for the reason the unclaimed section leaves it out.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct RetiredOut {
    key: &'static str,
    source: UnclaimedSource,
    help: &'static str,
}

/// The whole report.
#[derive(Serialize)]
struct ReportOut<'a> {
    /// The three-part diagnostic for a data root that did not resolve.
    /// Absent when one did — its presence is what tells a reader the
    /// library-scoped rows below are unanswered rather than empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    problem: Option<Problem>,
    rows: Vec<RowOut<'a>>,
    /// The `.env` this process loaded, absent when it loaded none. A
    /// row credits the file as a `site` only when the file won that
    /// row, so a file whose whole effect was outside the prefix would
    /// otherwise be unnamed.
    #[serde(skip_serializing_if = "Option::is_none")]
    dotenv_path: Option<&'a str>,
    /// What `.env` did outside this workspace's own prefix. Always
    /// present, empty included: an absent field would read as "the
    /// report does not know", and on a run with no dotenv layer at all
    /// the report knows there was nothing.
    dotenv_foreign: Vec<ForeignOut<'a>>,
    /// The prefixed variables no knob of this build reads, sorted by
    /// name. Always present, empty included, for the reason
    /// `dotenv_foreign` is.
    unclaimed_variables: Vec<UnclaimedOut>,
    /// What the running daemon reports, beside `rows`: those stay this
    /// shell's own resolution whether or not a daemon answers.
    daemon: DaemonOut<'a>,
    /// The variables an earlier release read that this process still
    /// carries, in the order the retirement table lists them. Each is
    /// ignored. Always present, empty included.
    retired_variables: Vec<RetiredOut>,
    /// The keys the resolved library's `config.toml` carries that this
    /// build ignores, each as `kind`, `key`, and `help`. Always present,
    /// empty included — also when no root resolved.
    config_notices: &'a [bookrack_config::ConfigNotice],
    native_dependencies: Vec<bookrack_config::NativeDependencyOrigin>,
}

/// Print the effective-configuration report.
///
/// The resolution error is carried, not propagated: a root that will
/// not resolve is the case this command exists for, and every knob
/// that never depended on one still has an answer to give.
///
/// `daemon` is what a probe for the running daemon found. With a
/// snapshot the human table shows the daemon's values; the JSON `rows`
/// are this shell's either way, with the daemon's under `daemon`.
pub fn run(selection: &LibrarySelection, daemon: &DaemonView) -> Result<()> {
    // root identity: read-only; the report reads the resolution and writes nothing
    let resolved = Config::resolve(selection);
    let cfg = resolved.as_ref().ok();
    let problem = resolved.as_ref().err().map(resolution_problem);

    let profile = cfg.map(profile_model).unwrap_or(Ok(None));
    let rows = collect_rows(
        cfg,
        profile.as_ref().ok().and_then(|model| model.as_deref()),
    );
    let profile_note = profile.as_ref().err().map(String::as_str);
    let scope_instance = cfg.map(|c| c.data_dir().display().to_string());
    let out: Vec<RowOut<'_>> = rows
        .iter()
        .map(|r| {
            let note = profile_note.filter(|_| r.key == "embed.model");
            row_out(r, &scope_instance, note)
        })
        .collect();
    let foreign = foreign_out();
    let unclaimed = unclaimed_out(
        std::env::vars_os().filter_map(|(key, _)| key.into_string().ok()),
        &crate::config_knobs::claimed_variables(),
        bookrack_config::dotenv_load().map_or(&[], |load| load.supplied.as_slice()),
    );
    let retired = retired_out(
        bookrack_config::retired_seen_in_process(),
        bookrack_config::dotenv_load().map_or(&[], |load| load.supplied.as_slice()),
    );
    let dotenv_path = bookrack_config::dotenv_load().map(|load| load.supply().path);
    let notices = cfg.map_or(&[][..], Config::config_notices);
    let native = bookrack_config::native_dependency_origins(
        bookrack_ops::PDFIUM_RELEASE_TAG,
        reranker_tag(cfg),
    );

    // The daemon's rows this report can set against its own: the
    // process rows, and the rows of the library on the root this shell
    // resolved, when the daemon has it mounted.
    let snapshot = match daemon {
        DaemonView::Snapshot(snapshot) => Some(snapshot.as_ref()),
        DaemonView::NoDaemon | DaemonView::NoSnapshot => None,
    };
    let mounted_here = snapshot.and_then(|snapshot| {
        let root = cfg?.data_dir();
        snapshot.libraries.iter().find(|library| {
            bookrack_config::same_root(std::path::Path::new(&library.data_dir), root)
        })
    });
    let in_use: Vec<KnobOrigin> = snapshot
        .map(|snapshot| {
            snapshot
                .rows
                .iter()
                .chain(mounted_here.iter().flat_map(|library| library.rows.iter()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let differing = differences(&rows, &in_use);
    let no_root = None;
    let daemon_out = match daemon {
        DaemonView::NoDaemon => DaemonOut {
            reachable: false,
            snapshot: None,
            identity: None,
            rows: None,
            libraries: None,
            differences: None,
            mounted_at_start: None,
            mounted_now: None,
            queue_paused: None,
        },
        DaemonView::NoSnapshot => DaemonOut {
            reachable: true,
            snapshot: Some(false),
            identity: None,
            rows: None,
            libraries: None,
            differences: None,
            mounted_at_start: None,
            mounted_now: None,
            queue_paused: None,
        },
        DaemonView::Snapshot(snapshot) => DaemonOut {
            reachable: true,
            snapshot: Some(true),
            identity: Some(&snapshot.identity),
            rows: Some(
                snapshot
                    .rows
                    .iter()
                    .map(|row| row_out(row, &no_root, None))
                    .collect(),
            ),
            libraries: Some(
                snapshot
                    .libraries
                    .iter()
                    .map(|library| LibraryOut {
                        name: &library.name,
                        data_dir: &library.data_dir,
                        rows: library
                            .rows
                            .iter()
                            .map(|row| row_out(row, &no_root, None))
                            .collect(),
                    })
                    .collect(),
            ),
            differences: Some(&differing),
            mounted_at_start: Some(&snapshot.mounted_at_start),
            mounted_now: Some(
                snapshot
                    .libraries
                    .iter()
                    .map(|library| library.name.as_str())
                    .collect(),
            ),
            queue_paused: Some(snapshot.queue_paused),
        },
    };

    let ctx = crate::render::ctx();
    if !ctx.is_quiet() {
        let report = ReportOut {
            problem: problem.clone(),
            rows: out,
            daemon: daemon_out,
            dotenv_path,
            dotenv_foreign: foreign,
            unclaimed_variables: unclaimed,
            retired_variables: retired,
            config_notices: notices,
            native_dependencies: native,
        };
        if ctx.is_json() {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            let shown: Vec<ShownRow<'_>> = report
                .rows
                .iter()
                .map(|ours| match in_use.iter().find(|row| row.key == ours.key) {
                    Some(theirs) => ShownRow {
                        row: row_out(theirs, &scope_instance, ours.note),
                        from_this_shell: false,
                        restart: differing.iter().any(|d| d.key == ours.key).then_some(ours),
                    },
                    None => ShownRow {
                        row: row_out_copy(ours),
                        from_this_shell: snapshot.is_some(),
                        restart: None,
                    },
                })
                .collect();
            print_human(&report, daemon, &shown);
        }
    }

    match problem {
        None => Ok(()),
        Some(problem) => Err(BookrackCliError::LocalUserError {
            message: problem.summary,
        }
        .into()),
    }
}

/// The three-part diagnostic for a data root that would not resolve.
///
/// `ConfigError` does not implement `Explain`, and this command is the
/// wrong place to teach it to: that would put the wording of every
/// resolution failure behind one caller's needs. The flattened chain
/// carries the whole cause, and the hint says what this report can do
/// about it — which is the one thing this caller knows that the error
/// type does not.
fn resolution_problem(err: &bookrack_config::ConfigError) -> Problem {
    Problem::from_error_chain(err).hint(
        "The rows below still report every knob that did not need a data root, \
         and each row's chain names the layers it can be set at.",
    )
}

/// The knob reporters above the center tier.
///
/// [`collect_rows`] and the inventory chain these after
/// [`bookrack_ops::settings::KNOB_CONTRIBUTORS`].
/// `every_knob_reporting_crate_is_named_by_a_collector` holds the
/// workspace to the two lists together.
pub const CONTRIBUTORS: &[Contributor] = &[Contributor {
    report: crate::render::confirm::knob_origins,
    catalog: crate::render::confirm::knob_catalog,
}];

/// Every knob every crate resolves, in one list.
///
/// Each crate reports its own, because each is where the priority
/// chain actually runs; assembling them here rather than restating
/// them is what keeps this command from becoming a second answer that
/// can disagree with the first.
fn collect_rows(cfg: Option<&Config>, profile_model: Option<&str>) -> Vec<KnobOrigin> {
    let dotenv = bookrack_config::dotenv_load().map(|load| load.supply());

    let mut rows = bookrack_config::knob_origins(cfg, profile_model);
    for contributor in KNOB_CONTRIBUTORS.iter().chain(CONTRIBUTORS) {
        rows.extend((contributor.report)(dotenv));
    }
    rows
}

/// The embed model the library's effective index profile declares:
/// `Ok(None)` when no profile is in effect, and the flattened cause
/// when the reference does not resolve. A failure is carried rather
/// than raised, so the report still answers for every other row.
fn profile_model(cfg: &Config) -> Result<Option<String>, String> {
    bookrack_runtime::profile::effective_index_profile(cfg)
        .map(|effective| effective.map(|e| e.profile.embed.model))
        .map_err(|err| format!("{err:#}"))
}

/// What this process's `.env` did outside the workspace's own prefix.
///
/// Empty in a process with no dotenv layer, which is the same answer as
/// a file that named nothing foreign: neither leaves a variable whose
/// origin the rows above fail to explain.
fn foreign_out() -> Vec<ForeignOut<'static>> {
    bookrack_config::dotenv_load()
        .map(|load| {
            load.foreign()
                .into_iter()
                .map(|var| ForeignOut {
                    key: var.key,
                    status: var.status,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The prefixed variables among `present` that neither a name in
/// `claimed` nor the retirement table accounts for, sorted by name, each credited to `.env` when the load
/// supplied it and to the environment otherwise.
///
/// A name the file also wrote but the environment already carried is
/// the environment's: the load discarded the file's line, so the value
/// the process holds is the one it started with.
fn unclaimed_out(
    present: impl Iterator<Item = String>,
    claimed: &BTreeSet<String>,
    supplied: &[String],
) -> Vec<UnclaimedOut> {
    let mut out: Vec<UnclaimedOut> = present
        .filter(|key| key.starts_with(bookrack_config::ENV_PREFIX) && !claimed.contains(key))
        .filter(|key| {
            !bookrack_config::RETIRED_ENV_VARS
                .iter()
                .any(|var| var.name == key)
        })
        .map(|key| {
            let source = if supplied.contains(&key) {
                UnclaimedSource::Dotenv
            } else {
                UnclaimedSource::Environment
            };
            UnclaimedOut { key, source }
        })
        .collect();
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

/// The retired variables `seen`, each credited to `.env` when the load
/// supplied it and to the environment otherwise.
fn retired_out(
    seen: Vec<bookrack_config::RetiredVariable>,
    supplied: &[String],
) -> Vec<RetiredOut> {
    seen.into_iter()
        .map(|var| RetiredOut {
            key: var.name,
            source: if supplied.iter().any(|name| name == var.name) {
                UnclaimedSource::Dotenv
            } else {
                UnclaimedSource::Environment
            },
            help: var.help,
        })
        .collect()
}

/// The reranker model tag the native-dependency search looks for.
fn reranker_tag(_cfg: Option<&Config>) -> &'static str {
    bookrack_config::reranker_model_pin::RERANKER_MODEL_PINS
        .first()
        .map_or("", |pin| pin.tag)
}

/// One row of the human table: whose value it shows, and what this
/// shell would resolve instead when that differs.
struct ShownRow<'a> {
    /// The row as drawn.
    row: RowOut<'a>,
    /// The daemon reported no such row, so this is the shell's own.
    from_this_shell: bool,
    /// This shell's row, when its value differs from the daemon's.
    restart: Option<&'a RowOut<'a>>,
}

/// A second view of a row already in wire shape.
fn row_out_copy<'a>(row: &'a RowOut<'a>) -> RowOut<'a> {
    RowOut {
        key: row.key,
        value: row.value,
        layer: row.layer,
        site: row.site,
        shadowed: row
            .shadowed
            .iter()
            .map(|s| ShadowedOut {
                layer: s.layer,
                site: s.site,
                value: s.value,
            })
            .collect(),
        chain: row
            .chain
            .iter()
            .map(|s| SiteOut {
                layer: s.layer,
                site: s.site,
            })
            .collect(),
        reach: row.reach,
        scope_instance: row.scope_instance.clone(),
        read_at: row.read_at,
        note: row.note,
    }
}

/// Convert one row to its wire shape.
fn row_out<'a>(row: &'a KnobOrigin, root: &Option<String>, note: Option<&'a str>) -> RowOut<'a> {
    RowOut {
        key: &row.key,
        value: row.value.as_deref(),
        layer: row.layer.as_str(),
        site: &row.site,
        shadowed: row
            .shadowed
            .iter()
            .map(|s| ShadowedOut {
                layer: s.layer.as_str(),
                site: &s.site,
                value: &s.value,
            })
            .collect(),
        chain: row
            .chain
            .iter()
            .map(|s| SiteOut {
                layer: s.layer.as_str(),
                site: &s.site,
            })
            .collect(),
        reach: row.reach.as_str(),
        scope_instance: match row.reach {
            KnobReach::Library => root.clone(),
            _ => None,
        },
        read_at: row.read_at.as_str(),
        note,
    }
}

/// Render the human table.
///
/// The diagnostic goes at the head rather than into every row: it is
/// one fact about the run, and repeating it per row would bury the
/// rows that are unaffected by it.
// Parallel implementation: problem-rendering.
fn print_human(report: &ReportOut<'_>, daemon: &DaemonView, shown: &[ShownRow<'_>]) {
    let ReportOut {
        problem,
        rows: _,
        daemon: _,
        dotenv_path,
        dotenv_foreign: foreign,
        unclaimed_variables: unclaimed,
        retired_variables: retired,
        config_notices: notices,
        native_dependencies: native,
    } = report;
    let dotenv_path = *dotenv_path;
    if let Some(problem) = problem {
        println!("{}", problem.summary);
        if let Some(detail) = &problem.data.detail {
            println!("  {detail}");
        }
        if let Some(hint) = &problem.data.hint {
            println!("  {hint}");
        }
        println!();
    }

    // Whose values the table holds is said once, above it.
    match daemon {
        DaemonView::Snapshot(snapshot) => println!(
            "values in use by the running daemon (pid {}, version {})\n",
            snapshot.identity.pid, snapshot.identity.version
        ),
        DaemonView::NoSnapshot => println!(
            "a daemon is running and does not report its configuration; these are the values \
             a start from this shell would resolve\n"
        ),
        DaemonView::NoDaemon => println!(
            "no daemon is running; these are the values a start from this shell would resolve\n"
        ),
    }

    // The scope column carries the reach, not the root path: the path
    // is one value repeated down every library-scoped row, and it is
    // already stated once by the `data_dir` row above them. `--json`
    // carries it per row, where a consumer needs it without reading
    // the rest of the table.
    let mut table = RowTable::new(["key", "value", "from", "scope"]);
    for shown in shown {
        let row = &shown.row;
        let from = if shown.from_this_shell {
            format!("{} (this shell)", row.layer)
        } else {
            row.layer.to_string()
        };
        table.push_row([
            row.key,
            row.value.unwrap_or("(unset)"),
            from.as_str(),
            row.reach,
        ]);
    }
    println!("{}", table.render());

    // The continuation lines carry what a four-column table cannot:
    // which layers lost, which could speak but did not, and which
    // could not be read. The three are drawn differently on purpose —
    // `<-` is a value that was overridden, `.` is a place a value could
    // be put, `!` is a layer that failed.
    for shown in shown {
        let row = &shown.row;
        if let Some(ours) = shown.restart {
            println!(
                "  {key}  from this shell a restart would resolve {value} ({layer}, {site})",
                key = row.key,
                value = ours.value.unwrap_or("(unset)"),
                layer = ours.layer,
                site = ours.site
            );
        }
        for shadowed in &row.shadowed {
            println!(
                "  {key}  <- {layer} {site} = {value}",
                key = row.key,
                layer = shadowed.layer,
                site = shadowed.site,
                value = shadowed.value
            );
        }
        if let Some(note) = row.note {
            println!("  {key}  !  {note}", key = row.key);
        }
        for site in &row.chain {
            let is_winner = site.layer == row.layer && site.site == row.site;
            let is_shadowed = row
                .shadowed
                .iter()
                .any(|s| s.layer == site.layer && s.site == site.site);
            if !is_winner && !is_shadowed {
                println!(
                    "  {key}  .  {layer} {site}",
                    key = row.key,
                    layer = site.layer,
                    site = site.site
                );
            }
        }
    }

    // Keys of the library's own file that no row reports, because this
    // build resolves nothing from them.
    if !notices.is_empty() {
        println!();
        println!("config.toml carries these keys, which this build ignores:");
        let mut table = RowTable::new(["key", "why", "what to do"]);
        for notice in notices.iter() {
            table.push_row([notice.key(), notice.kind(), notice.help()]);
        }
        println!("{}", table.render());
    }

    // A section rather than rows: these have no layer to sit in and no
    // value to report, and the one thing they need said — that a file
    // set them — is the thing the table above cannot say about a
    // variable it does not own.
    if !foreign.is_empty() {
        println!();
        println!(
            "{} also names these variables, which no row above owns:",
            dotenv_path.unwrap_or(".env")
        );
        let mut table = RowTable::new(["variable", "what the file did"]);
        for var in foreign {
            table.push_row([
                var.key,
                match var.status {
                    ForeignStatus::Set => "set it in this process",
                    ForeignStatus::Eclipsed => {
                        "read and discarded; the environment already had one"
                    }
                    ForeignStatus::Rejected => "dropped; .env may not set this name",
                },
            ]);
        }
        println!("{}", table.render());
    }

    // A section for the same reason as the one above: no knob owns these
    // names, so no row can carry them, and without the list a misspelt
    // variable or one only another build reads looks exactly like one
    // that took effect.
    if !unclaimed.is_empty() {
        println!();
        println!("No knob in this build reads these variables, so they change nothing:");
        let mut table = RowTable::new(["variable", "set by"]);
        for var in unclaimed {
            table.push_row([
                var.key.as_str(),
                match var.source {
                    UnclaimedSource::Dotenv => dotenv_path.unwrap_or(".env"),
                    UnclaimedSource::Environment => "the environment",
                },
            ]);
        }
        println!("{}", table.render());
        println!("`bookrack config knobs` lists the variables this build reads.");
    }

    // Apart from the unclaimed list: these names were knobs once, so
    // the report can say what became of each.
    if !retired.is_empty() {
        println!();
        println!("These variables are retired and ignored:");
        let mut table = RowTable::new(["variable", "set by", "what to do"]);
        for var in retired {
            table.push_row([
                var.key,
                match var.source {
                    UnclaimedSource::Dotenv => dotenv_path.unwrap_or(".env"),
                    UnclaimedSource::Environment => "the environment",
                },
                var.help,
            ]);
        }
        println!("{}", table.render());
    }

    // What changed while the daemon ran, which no knob row carries.
    if let DaemonView::Snapshot(snapshot) = daemon {
        let now: Vec<&str> = snapshot
            .libraries
            .iter()
            .map(|library| library.name.as_str())
            .collect();
        println!();
        println!("Runtime state of the daemon:");
        let mut table = RowTable::new(["state", "at start", "now"]);
        let at_start = snapshot.mounted_at_start.join(", ");
        let mounted_now = now.join(", ");
        table.push_row(["mounted libraries", at_start.as_str(), mounted_now.as_str()]);
        table.push_row([
            "queue paused",
            "-",
            if snapshot.queue_paused { "yes" } else { "no" },
        ]);
        println!("{}", table.render());
    }

    println!();
    let mut deps = RowTable::new(["dependency", "resolved", "from", "override with"]);
    for dep in native {
        deps.push_row([
            dep.name.as_str(),
            dep.path.as_deref().unwrap_or("(not found)"),
            dep.site.as_deref().unwrap_or("-"),
            dep.override_site.as_str(),
        ]);
    }
    println!("{}", deps.render());
}

#[cfg(test)]
mod tests {
    use bookrack_core::knob::{Candidate, KnobReach, Layer, ReadAt, resolve_knob};

    use super::*;

    fn row(key: &str, value: Option<&str>) -> KnobOrigin {
        resolve_knob(
            key,
            KnobReach::Process,
            ReadAt::BeforeResolution,
            vec![Candidate::of(
                Layer::Default,
                "built-in",
                value.map(str::to_string),
            )],
        )
    }

    #[test]
    fn differences_lists_only_keys_whose_values_differ() {
        let shell = [row("a", Some("1")), row("b", Some("2")), row("c", None)];
        let daemon = [
            row("a", Some("1")),
            row("b", Some("7")),
            row("c", Some("x")),
        ];
        assert_eq!(
            differences(&shell, &daemon),
            [
                Difference {
                    key: "b".to_string(),
                    daemon: Some("7".to_string()),
                    shell: Some("2".to_string()),
                },
                Difference {
                    key: "c".to_string(),
                    daemon: Some("x".to_string()),
                    shell: None,
                },
            ]
        );
    }

    #[test]
    fn a_key_only_one_side_carries_is_not_a_difference() {
        let shell = [row("only.shell", Some("1")), row("both", Some("1"))];
        let daemon = [row("both", Some("1")), row("only.daemon", Some("9"))];
        assert_eq!(differences(&shell, &daemon), []);
    }
}
