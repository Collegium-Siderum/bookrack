// SPDX-License-Identifier: Apache-2.0

//! DTOs for [`Ops`](crate::Ops).
//!
//! Read-side shapes are re-exported from `bookrack-query::dto` so a single
//! contract serves CLI `--json` output and MCP tool responses. Write-side
//! shapes and audit-trail / pipeline-trail entries live in this crate.

pub mod audit;
pub mod categories;
pub mod info;
pub mod metadata_report;
pub mod vectors_status;
pub mod writes;

pub use bookrack_query::dto::{
    AwaitingText, BookDetail, BookFilter, BookSummary, ContextWindow, ContributorEntry,
    DEFAULT_LIST_LIMIT, LibraryStats, ListBooksResult, ListPapersResult, MAX_CONTEXT_RADIUS,
    MAX_LIST_LIMIT, MAX_READ_CHARS, MAX_SPAN_LEAVES, MAX_TOC_NODES, MetadataFilter, PaperAuditInfo,
    PaperDetail, PaperFilter, PaperSource, PaperSummary, PapersStats, Passage, ShowTocArgs,
    SpanText, Toc, TocNode, TocNodes, TocSlimEntry, TocStats, UnknownFilterValue,
    checked_vocabulary, clamp_limit, parse_statuses,
};

#[cfg(test)]
mod tests {
    use serde_json::Value;

    /// The JSON form of `T`'s schema.
    fn schema<T: schemars::JsonSchema>() -> Value {
        serde_json::to_value(schemars::schema_for!(T)).expect("a schema serializes")
    }

    /// The schema of property `name` on `root`, with a `$ref` into the
    /// root's definitions followed.
    fn property<'a>(root: &'a Value, name: &str) -> &'a Value {
        let found = &root["properties"][name];
        assert!(!found.is_null(), "no property {name:?} in {root}");
        match found["$ref"].as_str() {
            Some(reference) => {
                let key = reference
                    .strip_prefix("#/$defs/")
                    .unwrap_or_else(|| panic!("a local reference: {reference}"));
                &root["$defs"][key]
            }
            None => found,
        }
    }

    /// Every result type the method table will publish derives an
    /// object schema, the lower crates' types inside it included.
    #[test]
    fn result_types_derive_object_schemas() {
        for (label, root) in [
            ("BookDetail", schema::<super::BookDetail>()),
            ("Toc", schema::<super::Toc>()),
            (
                "VectorsStatus",
                schema::<super::vectors_status::VectorsStatus>(),
            ),
            ("LibraryInfo", schema::<super::info::LibraryInfo>()),
            ("LookupResult", schema::<bookrack_refs::LookupResult>()),
            ("Citation", schema::<crate::Citation>()),
            ("AuditTrailEntry", schema::<super::audit::AuditTrailEntry>()),
            ("WriteOutcome", schema::<super::writes::WriteOutcome>()),
            ("LogEvent", schema::<bookrack_obs::LogEvent>()),
        ] {
            assert_eq!(root["type"], "object", "{label}: {root}");
            assert!(
                root["properties"]
                    .as_object()
                    .is_some_and(|p| !p.is_empty()),
                "{label}: {root}"
            );
        }
    }

    /// The table of contents serializes its nodes untagged, as either a
    /// full or a slim array; the schema says the same rather than
    /// describing a tagged enum no reply carries.
    #[test]
    fn toc_nodes_keep_their_untagged_shape() {
        let root = schema::<super::Toc>();
        let nodes = property(&root, "nodes");
        let branches = nodes["anyOf"]
            .as_array()
            .unwrap_or_else(|| panic!("an untagged union: {nodes}"));
        assert_eq!(branches.len(), 2, "{nodes}");
        for branch in branches {
            assert_eq!(branch["type"], "array", "{branch}");
        }
    }

    /// The string values a unit-variant enum's schema admits: an `enum`
    /// array, or one `const` per documented variant under `oneOf`.
    fn enum_names(schema: &Value) -> Vec<&str> {
        if let Some(names) = schema["enum"].as_array() {
            return names.iter().filter_map(Value::as_str).collect();
        }
        schema["oneOf"]
            .as_array()
            .unwrap_or_else(|| panic!("an enum of names: {schema}"))
            .iter()
            .map(|variant| {
                variant["const"]
                    .as_str()
                    .unwrap_or_else(|| panic!("a named variant: {variant}"))
            })
            .collect()
    }

    /// A citation's kind is the lowercase name the catalog writes, and
    /// its node ids are the bare integers the reply carries.
    #[test]
    fn citation_fields_keep_their_wire_shape() {
        let root = schema::<crate::Citation>();
        let kind = property(&root, "kind");
        assert_eq!(enum_names(kind), ["book", "paper", "reference"], "{kind}");
        for field in ["start_node_id", "end_node_id"] {
            assert_eq!(property(&root, field)["type"], "integer", "{field}");
        }
    }

    /// A log event's timestamp is an RFC 3339 string on the wire.
    #[test]
    fn a_log_event_timestamp_is_a_date_time_string() {
        let root = schema::<bookrack_obs::LogEvent>();
        let ts = property(&root, "ts");
        assert_eq!(ts["type"], "string", "{ts}");
        assert_eq!(ts["format"], "date-time", "{ts}");
    }
}
