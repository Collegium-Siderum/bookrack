// SPDX-License-Identifier: Apache-2.0

//! Which index-profile remarks each library has already been shown.
//!
//! A write entrance that points a library at a profile prints the
//! registry's remarks on it once; this record is how "once" survives
//! the process. It sits beside the registry file, keyed by the uuid in
//! each library's manifest, and holds opaque string keys: the caller
//! decides what a key identifies.
//!
//! The record is a convenience, never a gate. A missing, unreadable, or
//! malformed file reads as "nothing shown", which costs at most one
//! repeated remark. Writes go through the registry's lock discipline —
//! an exclusive sibling lock over the read-modify-write window, then an
//! atomic rename — so two writers never drop each other's libraries; two
//! writers for the same library may each print the same remark.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::{RegistryWriteLock, registry_target_path, write_atomically};

/// Filename of the record, beside the registry file.
pub const ADVICE_RECORD_FILENAME: &str = "advice-seen.toml";

/// The record format this binary reads and writes. A file declaring a
/// higher version reads as empty and is never overwritten.
// setting: internal -- a version stamp; a bump means a new record layout
pub const ADVICE_RECORD_FORMAT_VERSION: u32 = 1;

/// The record's location: beside the registry file, `None` when no
/// registry location resolves.
pub fn advice_record_path() -> Option<PathBuf> {
    registry_target_path()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .map(|d| d.join(ADVICE_RECORD_FILENAME))
}

/// The keys recorded as shown for the library `uuid`. Empty when the
/// file is missing, unreadable, malformed, or of a newer format.
pub fn seen_keys(path: &Path, uuid: &str) -> BTreeSet<String> {
    let Some(doc) = read_current(path) else {
        return BTreeSet::new();
    };
    doc.get("libraries")
        .and_then(toml::Value::as_table)
        .and_then(|libraries| libraries.get(uuid))
        .and_then(|entry| entry.get("seen"))
        .and_then(toml::Value::as_array)
        .map(|keys| {
            keys.iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Replace the keys recorded for the library `uuid` with `keys`; an
/// empty set removes the library's entry. Other libraries' entries are
/// kept. A file of a newer format is left byte for byte.
pub fn record_seen(path: &Path, uuid: &str, keys: &BTreeSet<String>) -> std::io::Result<()> {
    let _lock = RegistryWriteLock::acquire(path).map_err(std::io::Error::other)?;
    let mut doc = match std::fs::read_to_string(path) {
        Ok(text) => match text.parse::<toml::Table>() {
            Ok(doc) => {
                if declared_version(&doc).is_some_and(|v| v > ADVICE_RECORD_FORMAT_VERSION) {
                    return Ok(());
                }
                doc
            }
            Err(_) => toml::Table::new(),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(e) => return Err(e),
    };
    doc.insert(
        "format_version".to_string(),
        toml::Value::Integer(ADVICE_RECORD_FORMAT_VERSION.into()),
    );
    let libraries = doc
        .entry("libraries")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !libraries.is_table() {
        *libraries = toml::Value::Table(toml::Table::new());
    }
    let libraries = libraries
        .as_table_mut()
        .expect("replaced with a table above");
    if keys.is_empty() {
        libraries.remove(uuid);
    } else {
        let mut entry = toml::Table::new();
        entry.insert(
            "seen".to_string(),
            toml::Value::Array(keys.iter().cloned().map(toml::Value::String).collect()),
        );
        libraries.insert(uuid.to_string(), toml::Value::Table(entry));
    }
    let text = toml::to_string(&doc).map_err(std::io::Error::other)?;
    write_atomically(path, &text)
}

/// The parsed record when it is one this binary reads.
fn read_current(path: &Path) -> Option<toml::Table> {
    let doc = std::fs::read_to_string(path)
        .ok()?
        .parse::<toml::Table>()
        .ok()?;
    match declared_version(&doc) {
        Some(v) if v > ADVICE_RECORD_FORMAT_VERSION => None,
        _ => Some(doc),
    }
}

fn declared_version(doc: &toml::Table) -> Option<u32> {
    doc.get("format_version")
        .and_then(toml::Value::as_integer)
        .and_then(|v| u32::try_from(v).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn record() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(ADVICE_RECORD_FILENAME);
        (dir, path)
    }

    #[test]
    fn a_missing_record_has_seen_nothing() {
        let (_dir, path) = record();
        assert!(seen_keys(&path, "u").is_empty());
    }

    #[test]
    fn a_recorded_set_reads_back_per_library() {
        let (_dir, path) = record();
        record_seen(&path, "a", &keys(&["k1", "k2"])).expect("record a");
        record_seen(&path, "b", &keys(&["k3"])).expect("record b");
        assert_eq!(seen_keys(&path, "a"), keys(&["k1", "k2"]));
        assert_eq!(seen_keys(&path, "b"), keys(&["k3"]));

        record_seen(&path, "a", &keys(&["k2"])).expect("replace a");
        assert_eq!(seen_keys(&path, "a"), keys(&["k2"]), "replaced, not merged");
        record_seen(&path, "a", &BTreeSet::new()).expect("clear a");
        assert!(seen_keys(&path, "a").is_empty());
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains("[libraries.a]"), "{text}");
        assert_eq!(seen_keys(&path, "b"), keys(&["k3"]), "b is untouched");
    }

    #[test]
    fn a_malformed_record_reads_empty_and_is_rewritten_whole() {
        for garbage in [
            "not toml at all [[[",
            "libraries = 3\n",
            "[libraries.u]\nseen = 7\n",
        ] {
            let (_dir, path) = record();
            std::fs::write(&path, garbage).expect("write garbage");
            assert!(seen_keys(&path, "u").is_empty(), "{garbage:?}");
            record_seen(&path, "u", &keys(&["k"])).expect("record over garbage");
            assert_eq!(seen_keys(&path, "u"), keys(&["k"]), "{garbage:?}");
        }
    }

    #[test]
    fn a_newer_record_reads_empty_and_is_never_overwritten() {
        let (_dir, path) = record();
        let newer = "format_version = 2\nunknown = \"field\"\n\n[libraries.u]\nseen = [\"k\"]\n";
        std::fs::write(&path, newer).expect("write newer");
        assert!(seen_keys(&path, "u").is_empty());
        record_seen(&path, "u", &keys(&["other"]))
            .expect("a newer record is skipped, not an error");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), newer);
    }

    #[test]
    fn a_record_upgraded_after_it_was_read_is_not_overwritten() {
        let (_dir, path) = record();
        record_seen(&path, "u", &keys(&["k"])).expect("record v1");
        assert_eq!(seen_keys(&path, "u"), keys(&["k"]));
        let newer = "format_version = 2\n";
        std::fs::write(&path, newer).expect("upgrade in between");
        record_seen(&path, "u", &keys(&["k", "k2"])).expect("skipped");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), newer);
    }

    #[test]
    fn concurrent_writers_keep_each_others_libraries() {
        let (_dir, path) = record();
        let writers: Vec<_> = ["a", "b", "c", "d"]
            .into_iter()
            .map(|uuid| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for round in 0..20 {
                        record_seen(&path, uuid, &keys(&[&format!("{uuid}-{round}")]))
                            .expect("record");
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("writer thread");
        }
        for uuid in ["a", "b", "c", "d"] {
            assert_eq!(
                seen_keys(&path, uuid),
                keys(&[&format!("{uuid}-19")]),
                "{uuid}"
            );
        }
    }
}
