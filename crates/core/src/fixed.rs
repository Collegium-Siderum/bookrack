// SPDX-License-Identifier: Apache-2.0

//! Values compiled into the binary that no configuration layer moves.
//!
//! A knob has a priority chain and [`crate::knob`] reports where each
//! layer holds it. These have none: they are decided at build time, and
//! an operator cannot change one without a rebuild. What an operator
//! can do is *find* one — a cap, a retry count, a timeout — when a
//! value has to be explained while tuning or reading a failure. Each
//! crate registers its own with [`fixed_settings!`] and one inventory
//! collects them, so the answer is a command rather than a source
//! search.
//!
//! Discoverable is not the same as settable, and the two surfaces stay
//! apart on purpose: a value listed here is one an operator can quote,
//! not one they can move.
//!
//! A registration holds the constant itself rather than a copy of its
//! text. [`FixedSetting::value`] renders it on demand, so an entry
//! cannot fall out of step with the constant it describes — the
//! failure a hand-written table has by construction.

use std::time::Duration;

/// One compiled-in value, with what it bounds and where it acts.
///
/// The field names are a contract: the inventory serializes them, and
/// a caller reads the result to learn what a value is before deciding
/// whether it explains the behaviour in front of them.
#[derive(Clone, Copy)]
pub struct FixedSetting {
    /// Dotted key, in the same namespace a knob's key is drawn from.
    /// Unique across the whole workspace: two constants sharing one key
    /// are two homes for one value, which is the drift this inventory
    /// exists to prevent.
    pub key: &'static str,
    /// Renders the constant. A function rather than a stored string so
    /// the entry carries the value itself, not a transcription of it.
    pub value: fn() -> String,
    /// The crate that owns the constant.
    pub owner: &'static str,
    /// One line stating what the value bounds. Lowercase, no trailing
    /// period.
    pub summary: &'static str,
    /// The surface whose behaviour changes when this value changes, or
    /// `None` when it only shapes a step nothing outside the crate
    /// observes.
    pub surface: Option<&'static str>,
}

/// Rendering for the constant types a registration can hold.
///
/// Deliberately not a blanket `Display` impl: [`Duration`] has no
/// `Display`, and the inventory needs it to print the way an operator
/// writes a timeout rather than as a debug struct.
pub trait FixedValue {
    /// The value as the inventory prints it.
    fn render(&self) -> String;
}

macro_rules! render_via_display {
    ($($ty:ty),* $(,)?) => {
        $(
            impl FixedValue for $ty {
                fn render(&self) -> String {
                    self.to_string()
                }
            }
        )*
    };
}

render_via_display!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);

impl FixedValue for Duration {
    /// The largest unit that divides the duration exactly, so a
    /// quarter-second reads as `250ms` and a quarter-hour as `15m`
    /// rather than both as a raw count of one unit.
    fn render(&self) -> String {
        let secs = self.as_secs();
        if secs == 0 || self.subsec_millis() != 0 {
            return format!("{}ms", self.as_millis());
        }
        if secs.is_multiple_of(60) {
            return format!("{}m", secs / 60);
        }
        format!("{secs}s")
    }
}

/// Register a crate's compiled-in values as `pub const FIXED_SETTINGS`.
///
/// Each entry names the constant itself, so the rendered value comes
/// from the code rather than from a literal repeated in the table.
///
/// ```
/// use std::time::Duration;
///
/// const RETRY_BACKOFF: Duration = Duration::from_millis(250);
/// const MAX_RETRIES: u32 = 2;
///
/// bookrack_core::fixed_settings! {
///     owner = "example";
///     "example.retry_backoff" = RETRY_BACKOFF,
///         "pause between two attempts at the same request",
///         acts on "every outbound call this crate makes";
///     "example.retries_max" = MAX_RETRIES,
///         "attempts after the first before the call is given up on",
///         acts on nothing;
/// }
///
/// assert_eq!((FIXED_SETTINGS[0].value)(), "250ms");
/// assert_eq!(FIXED_SETTINGS[1].surface, None);
/// ```
#[macro_export]
macro_rules! fixed_settings {
    (
        owner = $owner:literal;
        $($key:literal = $konst:expr, $summary:literal, acts on $surface:tt;)*
    ) => {
        /// Every compiled-in value this crate registers, in key order.
        ///
        /// Collected into `bookrack config fixed`; the gate behind that
        /// command holds this list and the constants themselves to each
        /// other in both directions.
        pub const FIXED_SETTINGS: &[$crate::fixed::FixedSetting] = &[
            $(
                $crate::fixed::FixedSetting {
                    key: $key,
                    value: || $crate::fixed::FixedValue::render(&$konst),
                    owner: $owner,
                    summary: $summary,
                    surface: $crate::fixed_settings!(@surface $surface),
                },
            )*
        ];
    };
    (@surface nothing) => {
        None
    };
    (@surface $surface:literal) => {
        Some($surface)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CAP: usize = 30_000;
    const SAMPLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

    crate::fixed_settings! {
        owner = "core";
        "sample.cap" = SAMPLE_CAP, "characters one response may carry", acts on "the read tools";
        "sample.timeout" = SAMPLE_TIMEOUT, "how long a plan stays valid", acts on nothing;
    }

    /// The rendered value is read off the constant, so editing the
    /// constant alone moves the table. A registration that stored the
    /// text would pass every other test in this file and still be able
    /// to lie.
    #[test]
    fn an_entry_renders_the_constant_rather_than_a_copy_of_it() {
        let cap = FIXED_SETTINGS
            .iter()
            .find(|s| s.key == "sample.cap")
            .expect("the registration declares it");

        assert_eq!((cap.value)(), SAMPLE_CAP.to_string());
        assert_eq!((cap.value)(), "30000");
    }

    #[test]
    fn a_surface_is_optional_and_recorded_when_given() {
        let by_key = |key: &str| {
            *FIXED_SETTINGS
                .iter()
                .find(|s| s.key == key)
                .expect("the registration declares it")
        };

        assert_eq!(by_key("sample.cap").surface, Some("the read tools"));
        assert_eq!(by_key("sample.timeout").surface, None);
        assert_eq!(by_key("sample.cap").owner, "core");
    }

    /// A duration prints in the unit an operator would have written it
    /// in. Without this the table reports `900s` for a value the code
    /// spells `15 * 60` and the documentation calls fifteen minutes.
    #[test]
    fn a_duration_renders_in_its_largest_exact_unit() {
        assert_eq!(Duration::from_millis(250).render(), "250ms");
        assert_eq!(Duration::from_secs(30).render(), "30s");
        assert_eq!(Duration::from_secs(60).render(), "1m");
        assert_eq!(Duration::from_secs(15 * 60).render(), "15m");
        assert_eq!(Duration::from_millis(1_500).render(), "1500ms");
        assert_eq!(Duration::ZERO.render(), "0ms");
    }

    #[test]
    fn a_float_keeps_its_fractional_part() {
        assert_eq!(0.55_f64.render(), "0.55");
    }
}
