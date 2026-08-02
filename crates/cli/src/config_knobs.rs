// SPDX-License-Identifier: Apache-2.0

//! `bookrack config knobs`: every knob this build has, with the places
//! each one can be set.
//!
//! The inventory beside `config effective`'s report. Both are assembled
//! from the same crates through [`crate::config_effective::CONTRIBUTORS`],
//! and each crate answers with the same resolution fed a different
//! environment — this one an empty environment, so every row reports the
//! layer that backstops it rather than what this machine happens to
//! carry.
//!
//! Reads nothing: no data root, no daemon, no `.env`. A knob appears
//! here because the binary can resolve it, which is what makes the
//! output a property of the build rather than of the host.

use bookrack_core::knob::{KnobOrigin, Layer};
use eyre::Result;
use serde::Serialize;

use crate::render::table::RowTable;

/// One knob as the JSON form carries it.
///
/// The field names are a contract. `variable` is lifted out of `chain`
/// rather than replacing it: it is the answer to the common question,
/// and `chain` remains the whole answer for a knob that can also be set
/// in a file, a manifest, or a flag.
#[derive(Serialize)]
struct KnobOut<'a> {
    key: &'a str,
    /// The environment variable that moves this knob, when one does.
    /// `None` for a knob only a file or a flag can set.
    #[serde(skip_serializing_if = "Option::is_none")]
    variable: Option<&'a str>,
    /// What the knob resolves to with nothing set, or `None` when no
    /// layer backstops it — meaning unset is itself a value, and some
    /// other artifact (a built index, the platform) decides.
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<&'a str>,
    /// The layer that supplied `default`.
    default_layer: &'a str,
    settable_at: Vec<SiteOut<'a>>,
    reach: &'a str,
    read_at: &'a str,
}

#[derive(Serialize)]
struct SiteOut<'a> {
    layer: &'a str,
    site: &'a str,
}

/// One native dependency: not a knob with a priority chain, but a thing
/// an operator configures, so an inventory that omitted it would send
/// them back to the source to find the variable.
#[derive(Serialize)]
struct DependencyOut<'a> {
    name: &'a str,
    variable: &'a str,
}

/// The whole inventory.
#[derive(Serialize)]
struct InventoryOut<'a> {
    knobs: Vec<KnobOut<'a>>,
    native_dependencies: Vec<DependencyOut<'a>>,
}

/// Print the knob inventory.
pub fn run() -> Result<()> {
    let rows = catalog();
    let knobs: Vec<KnobOut<'_>> = rows.iter().map(knob_out).collect();
    let deps: Vec<DependencyOut<'_>> = bookrack_config::NATIVE_DEPENDENCY_KNOBS
        .iter()
        .map(|d| DependencyOut {
            name: d.name,
            variable: d.override_site,
        })
        .collect();

    let ctx = crate::render::ctx();
    if !ctx.is_quiet() {
        if ctx.is_json() {
            let inventory = InventoryOut {
                knobs,
                native_dependencies: deps,
            };
            println!("{}", serde_json::to_string_pretty(&inventory)?);
        } else {
            print_human(&knobs, &deps);
        }
    }
    Ok(())
}

/// Every knob every crate has, in one list.
///
/// The inventory counterpart of `config effective`'s row collection,
/// assembled the same way and in the same order: `bookrack-config`
/// first, then each contributor. The two functions differ only in which
/// entry point of each crate they call.
pub fn catalog() -> Vec<KnobOrigin> {
    let mut rows = bookrack_config::knob_catalog();
    for contributor in crate::config_effective::CONTRIBUTORS {
        rows.extend((contributor.catalog)());
    }
    rows
}

/// Convert one row to its wire shape.
fn knob_out(row: &KnobOrigin) -> KnobOut<'_> {
    KnobOut {
        key: &row.key,
        variable: environment_site(row),
        default: row.value.as_deref(),
        default_layer: row.layer.as_str(),
        settable_at: row
            .chain
            .iter()
            .map(|s| SiteOut {
                layer: s.layer.as_str(),
                site: &s.site,
            })
            .collect(),
        reach: row.reach.as_str(),
        read_at: row.read_at.as_str(),
    }
}

/// The `BOOKRACK_…` variable a knob draws on, when it draws on one.
///
/// A knob may have two environment sites — `pdfium.required` reads its
/// own variable and falls back to `CI` — so the first is taken rather
/// than joining them: the first is the one the inventory is naming, and
/// the rest stay visible in `settable_at`.
fn environment_site(row: &KnobOrigin) -> Option<&str> {
    row.chain
        .iter()
        .find(|s| s.layer == Layer::Environment)
        .map(|s| s.site.as_str())
}

/// Render the human table.
fn print_human(knobs: &[KnobOut<'_>], deps: &[DependencyOut<'_>]) {
    let mut table = RowTable::new(["key", "variable", "default", "reach", "read at"]);
    for knob in knobs {
        table.push_row([
            knob.key,
            knob.variable.unwrap_or("-"),
            knob.default.unwrap_or("(none)"),
            knob.reach,
            knob.read_at,
        ]);
    }
    println!("{}", table.render());

    // A continuation line per site the table does not already name, so
    // a knob that also has a file key, a flag, or a second variable
    // says so. Only the one site the `variable` column carries is
    // dropped: filtering the whole environment layer would hide the
    // `CI` fallback that decides `pdfium.required`.
    for knob in knobs {
        for site in &knob.settable_at {
            if Some(site.site) != knob.variable {
                println!(
                    "  {key}  .  {layer} {site}",
                    key = knob.key,
                    layer = site.layer,
                    site = site.site
                );
            }
        }
    }

    println!();
    let mut table = RowTable::new(["dependency", "point at it with"]);
    for dep in deps {
        table.push_row([dep.name, dep.variable]);
    }
    println!("{}", table.render());
}
