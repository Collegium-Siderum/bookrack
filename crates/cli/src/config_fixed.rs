// SPDX-License-Identifier: Apache-2.0

//! `bookrack config fixed`: every value compiled into this build that
//! no layer can move.
//!
//! The third surface beside `config effective` and `config knobs`, and
//! the one for a value an operator cannot set but still has to be able
//! to quote: a cap a response stopped at, a timeout a call died on, a
//! retry count a log implies. Those answers were reachable only by
//! reading the source, which put them out of reach of everyone the
//! answer was for.
//!
//! Reads nothing: no data root, no daemon, no `.env`. Like the knob
//! inventory, the output is a property of the binary — the same build
//! reports the same table on every machine.
//!
//! Registration is per crate and rolls out crate by crate; the gate in
//! `tests/fixed_inventory_gate.rs` names the crates it already covers
//! and holds each one's constants and registrations to each other in
//! both directions.

use bookrack_core::fixed::FixedSetting;
use eyre::Result;
use serde::Serialize;

use crate::render::table::RowTable;

/// Every crate that registers compiled-in values.
///
/// The counterpart of [`crate::config_effective::CONTRIBUTORS`]: a
/// crate absent from this list contributes nothing, and the gate reads
/// the same list so a registration that no surface collects fails
/// rather than going quiet.
pub const REGISTRIES: &[&[FixedSetting]] = &[
    crate::FIXED_SETTINGS,
    bookrack_catalog::FIXED_SETTINGS,
    bookrack_config::FIXED_SETTINGS,
    bookrack_control_client::FIXED_SETTINGS,
    bookrack_diagnose::FIXED_SETTINGS,
    bookrack_distill::FIXED_SETTINGS,
    bookrack_extract::FIXED_SETTINGS,
    bookrack_dbkit::FIXED_SETTINGS,
    bookrack_ingest::FIXED_SETTINGS,
    bookrack_embed::FIXED_SETTINGS,
    bookrack_mcp::FIXED_SETTINGS,
    bookrack_obs::stream::FIXED_SETTINGS,
    bookrack_query::dto::FIXED_SETTINGS,
    bookrack_rerank::FIXED_SETTINGS,
    bookrack_runtime::fixed::FIXED_SETTINGS,
];

/// One value as the JSON form carries it.
///
/// The field names are a contract. `value` is rendered from the
/// constant on every call rather than stored, so a reader can compare
/// it against a build without wondering whether the table was
/// refreshed.
#[derive(Serialize)]
struct FixedOut<'a> {
    key: &'a str,
    value: String,
    owner: &'a str,
    summary: &'a str,
    /// The surface whose behaviour changes with the value. Absent when
    /// the value only shapes a step nothing outside its crate sees.
    #[serde(skip_serializing_if = "Option::is_none")]
    acts_on: Option<&'a str>,
}

/// The whole inventory.
#[derive(Serialize)]
struct FixedInventoryOut<'a> {
    fixed: Vec<FixedOut<'a>>,
}

/// Print the compiled-in inventory.
pub fn run() -> Result<()> {
    let rows: Vec<FixedOut<'_>> = catalog().into_iter().map(fixed_out).collect();

    let ctx = crate::render::ctx();
    if !ctx.is_quiet() {
        if ctx.is_json() {
            println!(
                "{}",
                serde_json::to_string_pretty(&FixedInventoryOut { fixed: rows })?
            );
        } else {
            print_human(&rows);
        }
    }
    Ok(())
}

/// Every registered value, in key order.
///
/// Sorted here rather than trusted from the registrations: the keys
/// share one namespace across crates, so the order a reader scans has
/// to be the key's, not the order crates happen to be listed in.
pub fn catalog() -> Vec<&'static FixedSetting> {
    let mut rows: Vec<&'static FixedSetting> =
        REGISTRIES.iter().flat_map(|reg| reg.iter()).collect();
    rows.sort_by_key(|setting| setting.key);
    rows
}

/// Convert one registration to its wire shape.
fn fixed_out(setting: &'static FixedSetting) -> FixedOut<'static> {
    FixedOut {
        key: setting.key,
        value: (setting.value)(),
        owner: setting.owner,
        summary: setting.summary,
        acts_on: setting.surface,
    }
}

/// Render the human table.
fn print_human(rows: &[FixedOut<'_>]) {
    let mut table = RowTable::new(["key", "value", "acts on", "what it bounds"]);
    for row in rows {
        table.push_row([row.key, &row.value, row.acts_on.unwrap_or("-"), row.summary]);
    }
    println!("{}", table.render());
}
