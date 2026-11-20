// SPDX-License-Identifier: Apache-2.0

//! The configuration inventories of the center tier and everything
//! below it.
//!
//! Two lists: the crates that register compiled-in values, and the
//! crates that report knobs. A front end chains its own entries, and
//! those of the tiers this crate may not depend on, after these.

use bookrack_core::fixed::FixedSetting;
use bookrack_core::knob::{DotenvSupply, KnobOrigin};

/// Every crate at or below the center tier that registers compiled-in
/// values.
pub const FIXED_REGISTRIES: &[&[FixedSetting]] = &[
    bookrack_config::FIXED_SETTINGS,
    bookrack_control_client::FIXED_SETTINGS,
    bookrack_diagnose::FIXED_SETTINGS,
    bookrack_distill::FIXED_SETTINGS,
    bookrack_extract::FIXED_SETTINGS,
    bookrack_dbkit::FIXED_SETTINGS,
    crate::pipeline::common::embed::FIXED_SETTINGS,
    bookrack_embed::FIXED_SETTINGS,
    bookrack_obs::stream::FIXED_SETTINGS,
    bookrack_query::dto::FIXED_SETTINGS,
    bookrack_rerank::FIXED_SETTINGS,
];

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

/// Every crate at or below the center tier that reports knobs.
///
/// `bookrack-config` is absent by design: its report needs the resolved
/// `Config` that no other contributor takes, so it is called directly
/// by each surface.
pub const KNOB_CONTRIBUTORS: &[Contributor] = &[
    Contributor {
        report: bookrack_session::knob_origins,
        catalog: bookrack_session::knob_catalog,
    },
    Contributor {
        report: bookrack_extract::pdfium_gate::knob_origins,
        catalog: bookrack_extract::pdfium_gate::knob_catalog,
    },
];
