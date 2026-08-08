// SPDX-License-Identifier: Apache-2.0

//! `bookrack show <kind>:<id>` — one item, addressed by an id that
//! carries its own pipeline.
//!
//! The verb fixes no namespace, so the id is what decides which
//! catalog is read: `book:12` and `paper:101` reach two different
//! control-plane methods and render two different cards. That decision
//! is made once, in [`call_for`], which pairs the method with the
//! renderer for its response — a second table keyed by kind is how the
//! two would drift into disagreeing about which pipeline an id named.
//!
//! `reference:` parses and is refused: the reference read surface is
//! exposed as MCP tools and the control plane carries no method behind
//! it. The refusal happens before a connection is opened, so it reads
//! as a capability boundary rather than as a daemon that is not
//! running.

use std::path::PathBuf;
use std::str::FromStr;

use bookrack_cli::error::BookrackCliError;
use bookrack_cli::library_param;
use bookrack_cli::render::ctx;
use bookrack_cli::render::table::KvTable;
use bookrack_core::{Explain, Problem, TypedItemId};
use eyre::Result;
use serde_json::{Value, json};

use super::helpers;
use super::listing::card_library;
use super::papers::format_paper_detail;

pub async fn run(id: String, runtime_dir: Option<PathBuf>) -> Result<()> {
    let id = TypedItemId::from_str(&id).map_err(|err| BookrackCliError::ItemIdUnusable {
        problem: err.explain(),
    })?;
    let call = call_for(&id)?;
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    let response = helpers::dispatch(&client, call.method, call.params).await?;
    if ctx().is_json() {
        helpers::print_value(&response);
        return Ok(());
    }
    if ctx().is_quiet() {
        return Ok(());
    }
    println!("{}", (call.card)(&response));

    // Only the human branch asks: `--json` and `--quiet` return above,
    // and neither draws a card for the row to sit under.
    let selected = library_param::selected();
    let status = match selected {
        Some(_) => None,
        None => helpers::dispatch(&client, "status", Value::Null).await.ok(),
    };
    if let Some(name) = card_library(selected, status.as_ref()) {
        println!("library: {name}");
    }
    Ok(())
}

/// The control-plane call one typed id resolves to, together with the
/// renderer for its response.
struct Call {
    /// Method the id addresses.
    method: &'static str,
    /// Parameters for that method. The library selection is injected
    /// downstream by `library_param`, not here.
    params: Value,
    /// Card the human-readable branch draws from the response.
    card: fn(&Value) -> String,
}

/// Project a typed id onto the call that reads it.
///
/// Exhaustive by construction: a fourth pipeline fails to compile here
/// rather than falling through to whichever arm a `_` would have
/// picked. Every method name this verb sends is a literal in this
/// function and nowhere else, so the unit test below pins the routing
/// the verb actually performs.
fn call_for(id: &TypedItemId) -> Result<Call, BookrackCliError> {
    match id {
        TypedItemId::Book(intake_id) => Ok(Call {
            method: "library.show_book",
            params: json!({ "intake_id": intake_id }),
            card: format_book_detail,
        }),
        TypedItemId::Paper(intake_id) => Ok(Call {
            method: "library.show_paper",
            params: json!({ "intake_id": intake_id }),
            card: format_paper_detail,
        }),
        TypedItemId::Reference { .. } => Err(BookrackCliError::ItemIdUnusable {
            problem: reference_unreadable(),
        }),
    }
}

/// The refusal a `reference:` id earns at this surface.
///
/// The hint names no command: `bookrack rpc call` reaches the control
/// plane's method table, and no `reference.*` method is in it, so any
/// command named here would be one the operator cannot run. Pointing
/// at the surface that does carry the capability is the most a hint
/// can honestly do until the command-line side has a read path.
fn reference_unreadable() -> Problem {
    Problem::new("reference ids are not readable from the command line yet")
        .detail(
            "The reference read surface is published as MCP tools. The control plane, \
             which is what the command line talks to, carries no matching method.",
        )
        .hint(
            "An agent client connected over MCP can read the entry with the \
             `reference.lookup` tool. The command-line side waits on a refs query model.",
        )
}

/// Renders one `library.show_book` response as a key-value card: the
/// intake identity, the basename of the ingested file, the effective
/// biblio section, contributor and override counts, and the shape of
/// the ingested TOC. The rest of the source-side record — the recorded
/// path, the hash, the intake timestamp, the page count and the byte
/// size — stays in the `--json` payload: the card names the file, it
/// does not reproduce the intake row.
///
/// `title` is rowed on its own above, so the biblio section skips it
/// rather than printing it verbatim twice.
fn format_book_detail(response: &Value) -> String {
    let mut t = KvTable::new();
    if let Some(id) = response.get("intake_id").and_then(Value::as_i64) {
        t.push("intake_id", id.to_string());
    }
    for key in ["title", "status", "format", "source_filename"] {
        if let Some(val) = response.get(key).and_then(Value::as_str) {
            t.push(key, val);
        }
    }
    if let Some(biblio) = response.get("effective_biblio").and_then(Value::as_object) {
        for (k, v) in biblio {
            if k == "title" {
                continue;
            }
            let s = v
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| v.to_string());
            t.push(format!("biblio.{k}"), s);
        }
    }
    if let Some(arr) = response.get("contributors").and_then(Value::as_array) {
        t.push("contributors", arr.len().to_string());
    }
    if let Some(arr) = response.get("overrides").and_then(Value::as_array) {
        t.push("overrides", arr.len().to_string());
    }
    if let Some(stats) = response.get("toc_stats").and_then(Value::as_object) {
        let entries = stats.get("entry_count").and_then(Value::as_u64);
        let depth = stats.get("max_depth").and_then(Value::as_i64);
        if let (Some(entries), Some(depth)) = (entries, depth) {
            t.push("toc", format!("{entries} entries, depth {depth}"));
        }
    }
    t.render()
}

#[cfg(test)]
mod tests {
    use bookrack_runtime::control::methods::REGISTRY;

    use super::*;

    /// One id of each kind, so a test that walks the set cannot go
    /// stale by omission: adding a pipeline without extending this
    /// makes the `match` below fail to compile.
    fn one_of_each() -> Vec<TypedItemId> {
        let sample = TypedItemId::Book(0);
        match sample {
            TypedItemId::Book(_) | TypedItemId::Paper(_) | TypedItemId::Reference { .. } => {}
        }
        vec![
            TypedItemId::Book(12),
            TypedItemId::Paper(101),
            TypedItemId::Reference {
                book_slug: "name_alpha".into(),
                entry_key: Some("smith".into()),
            },
        ]
    }

    /// The two readable kinds address two different methods, each
    /// carrying its own intake id.
    ///
    /// An implementation that sent `library.show_book` for both would
    /// still return a record on a library where the number exists in
    /// both catalogs — the wrong record, silently. The method name is
    /// the only thing that tells the two apart before a daemon is in
    /// the picture.
    #[test]
    fn each_kind_routes_to_the_method_that_reads_it() {
        let book = call_for(&TypedItemId::Book(12)).expect("a book id routes");
        assert_eq!(book.method, "library.show_book");
        assert_eq!(book.params, json!({ "intake_id": 12 }));

        let paper = call_for(&TypedItemId::Paper(101)).expect("a paper id routes");
        assert_eq!(paper.method, "library.show_paper");
        assert_eq!(paper.params, json!({ "intake_id": 101 }));
    }

    /// Every kind is accounted for, and no two readable kinds share a
    /// method name.
    #[test]
    fn the_projection_covers_every_kind_and_repeats_no_method() {
        let mut methods: Vec<&'static str> = Vec::new();
        for id in one_of_each() {
            match call_for(&id) {
                Ok(call) => methods.push(call.method),
                // The one kind with no read path; its refusal is
                // asserted on its own below.
                Err(_) => assert!(matches!(id, TypedItemId::Reference { .. })),
            }
        }
        methods.sort_unstable();
        let before = methods.len();
        methods.dedup();
        assert_eq!(
            methods.len(),
            before,
            "two kinds project onto the same method: {methods:?}"
        );
    }

    /// The method names this verb sends are ones the daemon answers.
    ///
    /// The client-wide scan in `helpers` only sees literals sitting in
    /// the method argument of a call; here they sit in a projection and
    /// reach `dispatch` through a variable, so this verb would
    /// otherwise cross the wire unchecked.
    #[test]
    fn every_method_the_projection_sends_is_one_the_daemon_answers() {
        for id in one_of_each() {
            let Ok(call) = call_for(&id) else { continue };
            assert!(
                REGISTRY.iter().any(|sig| sig.name == call.method),
                "{id} projects onto `{}`, which the daemon's method table does not carry",
                call.method
            );
        }
    }

    /// A reference id is refused with all three parts: an operator who
    /// only saw the summary would have no way to learn that the
    /// capability exists elsewhere.
    #[test]
    fn a_reference_id_is_refused_with_a_detail_and_a_hint() {
        let refused = call_for(&TypedItemId::Reference {
            book_slug: "name_alpha".into(),
            entry_key: Some("smith".into()),
        })
        .err()
        .expect("a reference id has no read path here");
        let BookrackCliError::ItemIdUnusable { problem } = refused else {
            panic!("a reference id should be refused as an unusable item id");
        };
        assert!(
            problem.data.detail.is_some(),
            "the refusal states no detail"
        );
        let hint = problem.data.hint.expect("the refusal offers no hint");
        assert!(
            hint.contains("MCP"),
            "the hint should name the surface that does carry the capability: {hint}"
        );
        assert!(
            !hint.contains("bookrack "),
            "the hint names a command, and no command reaches a reference read: {hint}"
        );
    }

    /// The book card rows the fields a `BookDetail` carries and no
    /// others: the paper card's `audit` and `abstract` rows have no
    /// counterpart in the book DTO, and `title` is not repeated by the
    /// biblio section that also holds it.
    #[test]
    fn the_book_card_rows_the_book_dto_without_repeating_the_title() {
        let card = format_book_detail(&json!({
            "intake_id": 12,
            "title": "Synthetic Title",
            "status": "ready",
            "format": "epub",
            "source_filename": "synthetic.epub",
            "effective_biblio": { "title": "Synthetic Title", "publisher": "Synthetic Press" },
            "contributors": [{ "name": "a" }, { "name": "b" }],
            "overrides": [{ "field": "title" }],
            "toc_stats": { "entry_count": 42, "max_depth": 3 },
        }));
        assert_eq!(
            card.matches("Synthetic Title").count(),
            1,
            "the title is rowed twice:\n{card}"
        );
        assert!(card.contains("biblio.publisher"), "{card}");
        assert!(card.contains("contributors"), "{card}");
        assert!(card.contains("42 entries, depth 3"), "{card}");
    }

    /// A book with no ingested corpus has no TOC row rather than an
    /// empty one.
    #[test]
    fn the_book_card_drops_the_toc_row_when_the_book_has_no_corpus() {
        let card = format_book_detail(&json!({ "intake_id": 12, "toc_stats": Value::Null }));
        assert!(!card.contains("toc"), "{card}");
    }
}
