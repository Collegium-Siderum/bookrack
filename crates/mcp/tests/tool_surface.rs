// SPDX-License-Identifier: Apache-2.0

//! Pins the published MCP tool surface.
//!
//! `bookrack_mcp::list_tools()` is what the daemon hands to the
//! control plane for `daemon.mcp_tools`, and what an agent client sees
//! over `tools/list`. Tool names are a compatibility surface: renaming
//! or removing one breaks every client keyed on the old name, and
//! adding one widens what an agent may do without an operator in the
//! loop. Nothing else in the workspace asserts the list, so this file
//! holds it as two hand-written literals — the read bucket and the
//! write bucket — and requires that their union be exactly the live
//! set. A new tool therefore fails this test until it is filed into
//! one of the two buckets, which is the moment the version discipline
//! (a tool-surface change is a minor bump) applies.
//!
//! The buckets are the ones `docs/control-plane.md` documents under
//! *MCP tool surface*.

use std::collections::BTreeSet;

/// Tools that do not mutate the library's own records. The four
/// search tools do append a `retrieval_calls` audit row per call, so
/// "read" here means "does not edit what it reports on", not "free of
/// side effects".
const READ_TOOLS: &[&str] = &[
    "library.find_books",
    "library.find_papers",
    "library.info",
    "library.list_metadata",
    "library.list_papers",
    "library.list_pending_reviews",
    "library.list_books",
    "library.read_context",
    "library.read_span",
    "library.search",
    "library.search_in_book",
    "library.search_in_paper",
    "library.show_audit_trail",
    "library.show_book",
    "library.show_metadata_audit",
    "library.show_metadata_report",
    "library.show_paper",
    "library.show_paper_toc",
    "library.show_pipeline_trail",
    "library.show_toc",
    "library.stats",
    "library.vectors_status",
    "papers.fetch_source",
    "reference.lookup",
    "session.info",
    "session.logs_tail",
    "session.queue_status",
];

/// Tools that mutate persistent state or the daemon's lifecycle.
const WRITE_TOOLS: &[&str] = &[
    "library.metadata.ack",
    "library.metadata.approve",
    "library.metadata.clear",
    "library.metadata.contributor_add",
    "library.metadata.contributor_remove",
    "library.metadata.reaudit",
    "library.metadata.reject",
    "library.metadata.set",
    "library.metadata.void",
    "reference.overlay_set",
    "session.shutdown",
];

fn published() -> BTreeSet<String> {
    bookrack_mcp::list_tools()
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

#[test]
fn the_published_tool_set_is_exactly_the_documented_read_and_write_buckets() {
    let read: BTreeSet<String> = READ_TOOLS.iter().map(|s| s.to_string()).collect();
    let write: BTreeSet<String> = WRITE_TOOLS.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        read.len(),
        READ_TOOLS.len(),
        "READ_TOOLS holds a duplicate name"
    );
    assert_eq!(
        write.len(),
        WRITE_TOOLS.len(),
        "WRITE_TOOLS holds a duplicate name"
    );
    let overlap: Vec<&String> = read.intersection(&write).collect();
    assert!(
        overlap.is_empty(),
        "a tool is filed as both read and write: {overlap:?}"
    );

    let expected: BTreeSet<String> = read.union(&write).cloned().collect();
    let actual = published();
    let added: Vec<&String> = actual.difference(&expected).collect();
    let removed: Vec<&String> = expected.difference(&actual).collect();
    assert!(
        added.is_empty() && removed.is_empty(),
        "MCP tool surface changed — this is a minor version bump. \
         File each new name into READ_TOOLS or WRITE_TOOLS here and \
         update the *MCP tool surface* section of docs/control-plane.md. \
         Newly published: {added:?}; no longer published: {removed:?}"
    );
}

#[test]
fn the_write_bucket_holds_eleven_tools() {
    // `docs/control-plane.md` states the count in prose; pin it so the
    // prose cannot drift silently past a bucket edit that happens to
    // keep the union test green.
    let actual = published();
    let write_count = WRITE_TOOLS
        .iter()
        .filter(|name| actual.contains(**name))
        .count();
    assert_eq!(
        write_count, 11,
        "docs/control-plane.md documents eleven MCP write tools"
    );
}

#[test]
fn every_published_tool_carries_a_description() {
    // The description is the only guidance an agent client gets before
    // deciding to call a tool; an empty one on a write tool means an
    // unannotated mutation.
    let missing: Vec<String> = bookrack_mcp::list_tools()
        .into_iter()
        .filter(|tool| tool.description.trim().is_empty())
        .map(|tool| tool.name)
        .collect();
    assert!(missing.is_empty(), "tools with no description: {missing:?}");
}

#[test]
fn the_logs_tail_description_states_the_bounds_the_server_applies() {
    // A description that quotes a number restates a constant, and a
    // restatement drifts. An agent client reads this text instead of
    // the inventory `bookrack config fixed` prints, so it is held to
    // the values the handler actually applies.
    let tool = bookrack_mcp::list_tools()
        .into_iter()
        .find(|tool| tool.name == "session.logs_tail")
        .expect("session.logs_tail is a published tool");

    for bound in [
        bookrack_obs::stream::TAIL_REQUEST_DEFAULT,
        bookrack_obs::stream::TAIL_REQUEST_MAX,
    ] {
        assert!(
            tool.description.contains(&bound.to_string()),
            "the description does not state {bound}, so an agent reads a bound the \
             server does not apply: {}",
            tool.description
        );
    }
}

#[test]
fn every_metadata_write_tool_names_its_reason_requirement() {
    // Each `library.metadata.*` write records an audit row carrying an
    // operator-supplied reason. A client picks the tool from the
    // description alone, so the requirement has to be stated there.
    let tools = bookrack_mcp::list_tools();
    for name in WRITE_TOOLS
        .iter()
        .filter(|n| n.starts_with("library.metadata."))
    {
        let tool = tools
            .iter()
            .find(|t| t.name == *name)
            .unwrap_or_else(|| panic!("{name} is not published"));
        assert!(
            tool.description.contains("reason") || tool.description.contains("audit"),
            "{name}'s description does not mention the recorded reason: {}",
            tool.description
        );
    }
}
