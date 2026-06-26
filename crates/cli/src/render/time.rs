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
}
