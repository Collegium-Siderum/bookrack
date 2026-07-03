// SPDX-License-Identifier: Apache-2.0

//! `bookrack retrieval` — operator-facing surface for the
//! `retrieval_calls` sidecar and its per-hit detail.
//!
//! * `retrieval list [--last N] [--corpus-fingerprint HEX]` reads
//!   recent retrieval calls joined with their `mcp_tool_calls` log
//!   rows and prints a compact table.
//! * `retrieval show <call-id>` reads one call and prints its metadata
//!   followed by the hits in rank order.
//!
//! Every sidecar row lands in the book-side `catalog.db` — the
//! recorder logs paper-side searches there too — so both commands open
//! that one catalog directly and never touch the daemon: the retrieval
//! surface is local-only and read-only.

use bookrack_catalog::{Catalog, RetrievalCallHit, RetrievalCallListing};
use bookrack_cli_grammar::RetrievalAction;
use bookrack_config::Config;
use eyre::{Context as _, Result};

/// Dispatch the requested `bookrack retrieval` action.
pub fn run(selection: &bookrack_config::LibrarySelection, action: RetrievalAction) -> Result<()> {
    let cfg = Config::resolve(selection).context("resolve configuration")?;
    let catalog_db = cfg.catalog_db();
    let catalog =
        Catalog::open(&catalog_db).with_context(|| format!("open {}", catalog_db.display()))?;
    match action {
        RetrievalAction::List {
            last,
            corpus_fingerprint,
        } => {
            let rows = catalog
                .list_retrieval_calls(corpus_fingerprint.as_deref(), last)
                .context("list retrieval_calls")?;
            println!("{}", render_retrieval_list(&rows));
        }
        RetrievalAction::Show { call_id } => {
            let Some(call) = catalog
                .retrieval_call_listing(call_id)
                .context("read retrieval call")?
            else {
                return Err(eyre::eyre!(
                    "no retrieval detail recorded for call id {call_id}"
                ));
            };
            let hits = catalog
                .retrieval_hits(call_id)
                .context("read retrieval hits")?;
            println!("{}", render_retrieval_show(&call, &hits));
        }
    }
    Ok(())
}

/// Build the `retrieval list` text block. Empty result prints a single
/// `No retrieval calls.` line so the operator sees an explicit zero
/// rather than blank output. Public to the crate so tests can assert
/// on the rendered shape without spawning the binary.
pub(crate) fn render_retrieval_list(rows: &[RetrievalCallListing]) -> String {
    if rows.is_empty() {
        return "No retrieval calls.".to_string();
    }
    let mut out = String::new();
    out.push_str(
        "call_id  ts                    tool                      query                             top_k  n_hits  fingerprint\n",
    );
    for row in rows {
        let query = row.query_text.as_deref().unwrap_or("-");
        let query = if query.chars().count() > 32 {
            let head: String = query.chars().take(31).collect();
            format!("{head}\u{2026}")
        } else {
            query.to_string()
        };
        out.push_str(&format!(
            "{call_id:>7}  {ts:<21} {tool:<25} {query:<33} {top_k:>5}  {n_hits:>6}  {fingerprint}\n",
            call_id = row.call_id,
            ts = row.ts,
            tool = row.tool,
            query = query,
            top_k = row.top_k_requested,
            n_hits = row.n_hits,
            fingerprint = row.corpus_fingerprint,
        ));
    }
    out.trim_end().to_string()
}

/// Build the `retrieval show <call-id>` text block: call metadata on
/// top, the hits in rank order below.
pub(crate) fn render_retrieval_show(
    call: &RetrievalCallListing,
    hits: &[RetrievalCallHit],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("call_id:      {}\n", call.call_id));
    out.push_str(&format!("ts:           {}\n", call.ts));
    out.push_str(&format!("tool:         {}\n", call.tool));
    out.push_str(&format!(
        "query:        {}\n",
        call.query_text.as_deref().unwrap_or("-")
    ));
    out.push_str(&format!("top_k:        {}\n", call.top_k_requested));
    out.push_str(&format!("n_hits:       {}\n", call.n_hits));
    out.push_str(&format!("fingerprint:  {}\n", call.corpus_fingerprint));
    if hits.is_empty() {
        out.push_str("\nno hits recorded for this call.");
        return out.trim_end().to_string();
    }
    out.push_str(
        "\nord  passage_id                                                        distance\n",
    );
    for hit in hits {
        out.push_str(&format!(
            "{ord:>3}  {passage_id:<65} {distance:>8.4}\n",
            ord = hit.ord,
            passage_id = hit.passage_id,
            distance = hit.distance,
        ));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_catalog::{NewMcpToolCall, NewRetrievalCall};

    fn seed_call(catalog: &Catalog, query: &str, hits: &[(&str, f32)]) -> i64 {
        catalog
            .record_tool_call_with_retrieval(
                &NewMcpToolCall::new("mcp", "library.search", "ok"),
                Some(&NewRetrievalCall {
                    fingerprint: "deadbeefcafef00d".to_string(),
                    top_k: 10,
                    query_text: Some(query.to_string()),
                    hits: hits
                        .iter()
                        .map(|(passage_id, distance)| (passage_id.to_string(), *distance))
                        .collect(),
                }),
            )
            .expect("record call with retrieval")
    }

    #[test]
    fn retrieval_list_renders_with_zero_calls() {
        assert_eq!(render_retrieval_list(&[]), "No retrieval calls.");
    }

    #[test]
    fn retrieval_list_renders_one_row_per_call() {
        let catalog = Catalog::open_in_memory().expect("open");
        seed_call(&catalog, "what is a monad", &[("p-alpha", 0.12)]);
        seed_call(&catalog, "types of functors", &[("p-beta", 0.34)]);

        let rows = catalog.list_retrieval_calls(None, None).expect("list");
        let out = render_retrieval_list(&rows);
        let header = out.lines().next().expect("header");
        assert!(header.starts_with("call_id"));
        assert_eq!(out.lines().count(), 3);
        // Newest first: the second seeded call renders on top.
        let first_data = out.lines().nth(1).expect("data row");
        assert!(first_data.contains("types of functors"));
        assert!(first_data.contains("deadbeefcafef00d"));
        assert!(first_data.contains("library.search"));
    }

    #[test]
    fn retrieval_show_renders_three_hits_in_order() {
        let catalog = Catalog::open_in_memory().expect("open");
        let call_id = seed_call(
            &catalog,
            "what is a monad",
            &[("p-alpha", 0.12), ("p-beta", 0.34), ("p-gamma", 0.56)],
        );

        let call = catalog
            .retrieval_call_listing(call_id)
            .expect("read")
            .expect("present");
        let hits = catalog.retrieval_hits(call_id).expect("read hits");
        let out = render_retrieval_show(&call, &hits);

        assert!(out.contains(&format!("call_id:      {call_id}")));
        assert!(out.contains("query:        what is a monad"));
        assert!(out.contains("fingerprint:  deadbeefcafef00d"));
        let hit_rows: Vec<&str> = out
            .lines()
            .skip_while(|l| !l.starts_with("ord"))
            .skip(1)
            .collect();
        assert_eq!(hit_rows.len(), 3);
        assert!(hit_rows[0].trim_start().starts_with("0  p-alpha"));
        assert!(hit_rows[1].trim_start().starts_with("1  p-beta"));
        assert!(hit_rows[2].trim_start().starts_with("2  p-gamma"));
    }

    #[test]
    fn retrieval_show_with_zero_hits_prints_header_only() {
        let catalog = Catalog::open_in_memory().expect("open");
        let call_id = seed_call(&catalog, "unmatched query", &[]);

        let call = catalog
            .retrieval_call_listing(call_id)
            .expect("read")
            .expect("present");
        let out = render_retrieval_show(&call, &[]);
        assert!(out.contains("n_hits:       0"));
        assert!(out.contains("no hits recorded for this call."));
        assert!(!out.contains("passage_id"));
    }
}
