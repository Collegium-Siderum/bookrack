// SPDX-License-Identifier: Apache-2.0

//! Human-friendly time formatting helpers shared by the read-side
//! renderers (`queue list`, `papers find/list`, ...).

use chrono::{DateTime, Utc};

/// Render an RFC 3339 timestamp as a coarse "5m ago" / "3d ago"
/// relative span. Returns the input verbatim when it cannot be
/// parsed, and `"now"` for the sub-second window.
pub fn relative_from_iso(iso: &str) -> String {
    let parsed = match DateTime::parse_from_rfc3339(iso) {
        Ok(dt) => dt.with_timezone(&Utc),
        Err(_) => return iso.to_string(),
    };
    relative_from(parsed, Utc::now())
}

/// Test-friendly core of [`relative_from_iso`].
pub fn relative_from(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(then);
    let secs = delta.num_seconds();
    let (n, unit) = if secs < 1 {
        return "now".to_string();
    } else if secs < 60 {
        (secs, "s")
    } else if secs < 3_600 {
        (secs / 60, "m")
    } else if secs < 86_400 {
        (secs / 3_600, "h")
    } else {
        (secs / 86_400, "d")
    };
    format!("{n}{unit} ago")
}

/// Render the span since an RFC 3339 timestamp as a compact duration
/// (`45s`, `5m`, `3h12m`, `2d3h`). Returns the input verbatim when it
/// cannot be parsed. Used for daemon uptime, where "how long" reads
/// better than the "5m ago" form of [`relative_from_iso`].
pub fn uptime_from_iso(started_at: &str) -> String {
    let parsed = match DateTime::parse_from_rfc3339(started_at) {
        Ok(dt) => dt.with_timezone(&Utc),
        Err(_) => return started_at.to_string(),
    };
    duration_human(Utc::now().signed_duration_since(parsed).num_seconds())
}

/// Render a second count as a compact duration: the largest unit plus
/// its immediate neighbour when non-zero (`3h12m`, not `3h0m`).
/// Negative input clamps to `0s`.
pub fn duration_human(secs: i64) -> String {
    let secs = secs.max(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hours, rem) = (rem / 3_600, rem % 3_600);
    let (minutes, seconds) = (rem / 60, rem % 60);
    match (days, hours, minutes, seconds) {
        (0, 0, 0, s) => format!("{s}s"),
        (0, 0, m, 0) => format!("{m}m"),
        (0, 0, m, s) => format!("{m}m{s}s"),
        (0, h, 0, _) => format!("{h}h"),
        (0, h, m, _) => format!("{h}h{m}m"),
        (d, 0, _, _) => format!("{d}d"),
        (d, h, _, _) => format!("{d}d{h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(sec: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(sec, 0).unwrap()
    }

    #[test]
    fn under_one_second_is_now() {
        assert_eq!(relative_from(t(100), t(100)), "now");
    }

    #[test]
    fn ranges_pick_the_right_unit() {
        assert_eq!(relative_from(t(0), t(45)), "45s ago");
        assert_eq!(relative_from(t(0), t(120)), "2m ago");
        assert_eq!(relative_from(t(0), t(7_200)), "2h ago");
        assert_eq!(relative_from(t(0), t(2 * 86_400)), "2d ago");
    }

    #[test]
    fn unparseable_input_passes_through() {
        assert_eq!(relative_from_iso("not a date"), "not a date");
    }

    #[test]
    fn duration_shows_at_most_two_adjacent_units() {
        assert_eq!(duration_human(0), "0s");
        assert_eq!(duration_human(45), "45s");
        assert_eq!(duration_human(300), "5m");
        assert_eq!(duration_human(312), "5m12s");
        assert_eq!(duration_human(3 * 3_600), "3h");
        assert_eq!(duration_human(3 * 3_600 + 12 * 60), "3h12m");
        assert_eq!(duration_human(3 * 3_600 + 12 * 60 + 59), "3h12m");
        assert_eq!(duration_human(2 * 86_400), "2d");
        assert_eq!(duration_human(2 * 86_400 + 3 * 3_600), "2d3h");
        assert_eq!(duration_human(2 * 86_400 + 59 * 60), "2d");
    }

    #[test]
    fn negative_duration_clamps_to_zero() {
        assert_eq!(duration_human(-5), "0s");
    }

    #[test]
    fn unparseable_uptime_passes_through() {
        assert_eq!(uptime_from_iso("not a date"), "not a date");
    }
}
