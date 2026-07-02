// SPDX-License-Identifier: Apache-2.0

//! Stable fingerprinting of profile TOML sources and a boolean-toggle
//! summary of an [`AuditProfile`], both destined for audit rows.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

use crate::AuditProfile;

/// Number of hex characters kept from the SHA-256 digest.
const FINGERPRINT_HEX_LEN: usize = 16;

/// Reasons a fingerprint or toggle-summary computation can fail.
#[derive(Debug)]
pub enum FingerprintError {
    /// The input bytes were not valid UTF-8.
    Utf8(std::str::Utf8Error),
    /// The input was UTF-8 but did not parse as TOML.
    Parse(toml::de::Error),
    /// The canonical projection could not be serialized to JSON.
    Serialize(serde_json::Error),
}

impl std::fmt::Display for FingerprintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Utf8(error) => write!(f, "profile source is not valid UTF-8: {error}"),
            Self::Parse(error) => write!(f, "profile source does not parse as TOML: {error}"),
            Self::Serialize(error) => {
                write!(f, "failed to serialize canonical projection: {error}")
            }
        }
    }
}

impl std::error::Error for FingerprintError {}

/// Fingerprint v1: parse the bytes as TOML, project the parsed value
/// onto JSON with every table's keys sorted, hash the JSON text with
/// SHA-256, and keep the first 16 hex characters.
///
/// Hashing the sorted projection instead of the raw bytes makes the
/// fingerprint independent of key order, whitespace, and comments in
/// the source; it changes only when a parsed value changes.
pub fn stable_fingerprint(toml_bytes: &[u8]) -> Result<String, FingerprintError> {
    let text = std::str::from_utf8(toml_bytes).map_err(FingerprintError::Utf8)?;
    let value: toml::Value = toml::from_str(text).map_err(FingerprintError::Parse)?;
    let canonical =
        serde_json::to_string(&sorted_json(&value)).map_err(FingerprintError::Serialize)?;
    let digest = Sha256::digest(canonical.as_bytes());
    let mut hex = String::with_capacity(FINGERPRINT_HEX_LEN);
    for byte in digest.iter().take(FINGERPRINT_HEX_LEN / 2) {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(hex)
}

/// Summarize every boolean toggle of a profile as a JSON array of
/// `{"enabled": <bool>, "name": "<section>.<field>"}` objects, sorted
/// by name. Numeric thresholds and string lists are not part of the
/// summary; the fingerprint covers them.
pub fn profile_toggle_summary(profile: &AuditProfile) -> Result<String, FingerprintError> {
    let value = serde_json::to_value(profile).map_err(FingerprintError::Serialize)?;
    let mut toggles: Vec<(String, bool)> = Vec::new();
    collect_bool_leaves(&value, "", &mut toggles);
    toggles.sort();
    let entries: Vec<serde_json::Value> = toggles
        .into_iter()
        .map(|(name, enabled)| {
            // Keys are inserted in sorted order so the byte output is
            // identical whether the map preserves insertion order or
            // sorts on its own.
            let mut entry = serde_json::Map::new();
            entry.insert("enabled".to_string(), serde_json::Value::Bool(enabled));
            entry.insert("name".to_string(), serde_json::Value::String(name));
            serde_json::Value::Object(entry)
        })
        .collect();
    serde_json::to_string(&serde_json::Value::Array(entries)).map_err(FingerprintError::Serialize)
}

/// Project a TOML value onto JSON with every table's keys sorted.
fn sorted_json(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::from(*i),
        toml::Value::Float(f) => serde_json::Value::from(*f),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Datetime(dt) => serde_json::Value::String(dt.to_string()),
        toml::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sorted_json).collect())
        }
        toml::Value::Table(table) => {
            let mut pairs: Vec<(&String, &toml::Value)> = table.iter().collect();
            pairs.sort_by_key(|(key, _)| *key);
            let mut map = serde_json::Map::new();
            for (key, child) in pairs {
                map.insert(key.clone(), sorted_json(child));
            }
            serde_json::Value::Object(map)
        }
    }
}

/// Collect every boolean leaf of a JSON object tree as a dotted-name
/// entry. Arrays are not descended into: the profile keeps toggles in
/// named fields, and positional names would not survive reordering.
fn collect_bool_leaves(value: &serde_json::Value, prefix: &str, out: &mut Vec<(String, bool)>) {
    match value {
        serde_json::Value::Bool(b) => out.push((prefix.to_string(), *b)),
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let name = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                collect_bool_leaves(child, &name, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_fingerprint_is_order_independent() {
        let a = b"alpha = 1\n[year]\nrange_check = true\nmin = 1400\n[title]\nplaceholder_check = false\n";
        let b = b"# reordered copy\nalpha = 1\n\n[title]\nplaceholder_check = false\n\n[year]\nmin = 1400\nrange_check = true\n";
        let fp_a = stable_fingerprint(a).expect("fingerprint a");
        let fp_b = stable_fingerprint(b).expect("fingerprint b");
        assert_eq!(fp_a, fp_b);
        assert_eq!(fp_a.len(), FINGERPRINT_HEX_LEN);
        assert!(fp_a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn stable_fingerprint_changes_when_toggle_changes() {
        let base = b"[year]\nrange_check = true\n";
        let flipped = b"[year]\nrange_check = false\n";
        assert_ne!(
            stable_fingerprint(base).expect("fingerprint base"),
            stable_fingerprint(flipped).expect("fingerprint flipped"),
        );
    }

    #[test]
    fn stable_fingerprint_rejects_non_toml() {
        assert!(matches!(
            stable_fingerprint(b"not = = toml"),
            Err(FingerprintError::Parse(_))
        ));
        assert!(matches!(
            stable_fingerprint(&[0xff, 0xfe]),
            Err(FingerprintError::Utf8(_))
        ));
    }

    #[test]
    fn profile_toggle_summary_is_stable() {
        let profile = AuditProfile::default_profile();
        let first = profile_toggle_summary(&profile).expect("first summary");
        let second = profile_toggle_summary(&profile).expect("second summary");
        assert_eq!(first, second);

        let entries: Vec<serde_json::Value> =
            serde_json::from_str(&first).expect("summary parses as JSON array");
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().expect("name is a string"))
            .collect();
        assert!(names.contains(&"audit_enabled"));
        assert!(names.contains(&"year.range_check"));
        // Numeric thresholds stay out of the summary.
        assert!(!names.iter().any(|n| n.ends_with("min_entries")));
        // Sorted by name.
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }
}
