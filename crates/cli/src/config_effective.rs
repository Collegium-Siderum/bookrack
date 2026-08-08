// SPDX-License-Identifier: Apache-2.0

//! `bookrack config effective`: what the configuration resolves to,
//! and which layer supplied each value.
//!
//! Offline by construction. The report is assembled from the resolvers
//! themselves rather than from a daemon, so it answers on exactly the
//! machines where nothing else will — a data root that does not
//! resolve produces a report with the failure at its head, not an
//! error instead of a report.

use bookrack_config::{Config, ForeignStatus, LibrarySelection};
use bookrack_core::Problem;
use bookrack_core::knob::{DotenvSupply, KnobOrigin, KnobReach};
use eyre::Result;
use serde::Serialize;

use crate::error::BookrackCliError;
use crate::render::table::RowTable;

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
    native_dependencies: Vec<bookrack_config::NativeDependencyOrigin>,
}

/// Print the effective-configuration report.
///
/// The resolution error is carried, not propagated: a root that will
/// not resolve is the case this command exists for, and every knob
/// that never depended on one still has an answer to give.
pub fn run(selection: &LibrarySelection) -> Result<()> {
    let resolved = Config::resolve(selection);
    let cfg = resolved.as_ref().ok();
    let problem = resolved.as_ref().err().map(resolution_problem);

    let rows = collect_rows(cfg);
    let scope_instance = cfg.map(|c| c.data_dir().display().to_string());
    let out: Vec<RowOut<'_>> = rows.iter().map(|r| row_out(r, &scope_instance)).collect();
    let foreign = foreign_out();
    let dotenv_path = bookrack_config::dotenv_load().map(|load| load.supply().path);
    let native = bookrack_config::native_dependency_origins(reranker_tag(cfg));

    let ctx = crate::render::ctx();
    if !ctx.is_quiet() {
        if ctx.is_json() {
            let report = ReportOut {
                problem: problem.clone(),
                rows: out,
                dotenv_path,
                dotenv_foreign: foreign,
                native_dependencies: native,
            };
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print_human(problem.as_ref(), &out, dotenv_path, &foreign, &native);
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

/// One crate that reports knobs, as the pair of entry points it offers.
///
/// The two are kept together so adding a crate is a single edit: a
/// contributor reachable from the report but not the inventory would
/// make `config knobs` quietly narrower than `config effective`, which
/// is the drift the pair exists to prevent.
pub struct Contributor {
    /// What this crate resolves on this machine, given the dotenv
    /// record of the running process.
    pub report: fn(Option<DotenvSupply<'_>>) -> Vec<KnobOrigin>,
    /// What this crate would resolve with nothing set anywhere.
    pub catalog: fn() -> Vec<KnobOrigin>,
}

/// Every crate that reports knobs.
///
/// `bookrack-config` is absent by design: its report needs the resolved
/// [`Config`] that no other contributor takes, so it is called directly
/// by each surface. `every_knob_reporting_crate_is_a_contributor` holds
/// the rest of the workspace to this list.
pub const CONTRIBUTORS: &[Contributor] = &[
    Contributor {
        report: bookrack_search::knob_origins,
        catalog: bookrack_search::knob_catalog,
    },
    Contributor {
        report: bookrack_session::knob_origins,
        catalog: bookrack_session::knob_catalog,
    },
    Contributor {
        report: bookrack_extract::pdfium_gate::knob_origins,
        catalog: bookrack_extract::pdfium_gate::knob_catalog,
    },
    Contributor {
        report: crate::render::confirm::knob_origins,
        catalog: crate::render::confirm::knob_catalog,
    },
];

/// Every knob every crate resolves, in one list.
///
/// Each crate reports its own, because each is where the priority
/// chain actually runs; assembling them here rather than restating
/// them is what keeps this command from becoming a second answer that
/// can disagree with the first.
fn collect_rows(cfg: Option<&Config>) -> Vec<KnobOrigin> {
    let dotenv = bookrack_config::dotenv_load().map(|load| load.supply());

    let mut rows = bookrack_config::knob_origins(cfg);
    for contributor in CONTRIBUTORS {
        rows.extend((contributor.report)(dotenv));
    }
    rows
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

/// The reranker model tag the native-dependency search looks for.
fn reranker_tag(_cfg: Option<&Config>) -> &'static str {
    bookrack_config::reranker_model_pin::RERANKER_MODEL_PINS
        .first()
        .map_or("", |pin| pin.tag)
}

/// Convert one row to its wire shape.
fn row_out<'a>(row: &'a KnobOrigin, root: &Option<String>) -> RowOut<'a> {
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
    }
}

/// Render the human table.
///
/// The diagnostic goes at the head rather than into every row: it is
/// one fact about the run, and repeating it per row would bury the
/// rows that are unaffected by it.
fn print_human(
    problem: Option<&Problem>,
    rows: &[RowOut<'_>],
    dotenv_path: Option<&str>,
    foreign: &[ForeignOut<'_>],
    native: &[bookrack_config::NativeDependencyOrigin],
) {
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

    // The scope column carries the reach, not the root path: the path
    // is one value repeated down every library-scoped row, and it is
    // already stated once by the `data_dir` row above them. `--json`
    // carries it per row, where a consumer needs it without reading
    // the rest of the table.
    let mut table = RowTable::new(["key", "value", "from", "scope"]);
    for row in rows {
        table.push_row([
            row.key,
            row.value.unwrap_or("(unset)"),
            row.layer,
            row.reach,
        ]);
    }
    println!("{}", table.render());

    // The continuation lines carry what a four-column table cannot:
    // which layers lost, and which could speak but did not. The two
    // are drawn differently on purpose — `<-` is a value that was
    // overridden, `.` is a place a value could be put.
    for row in rows {
        for shadowed in &row.shadowed {
            println!(
                "  {key}  <- {layer} {site} = {value}",
                key = row.key,
                layer = shadowed.layer,
                site = shadowed.site,
                value = shadowed.value
            );
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
