// SPDX-License-Identifier: Apache-2.0

//! `bookrack show <kind>:<id>` — one item, addressed by an id that
//! carries its own pipeline.
//!
//! The verb fixes no namespace, so the id is what decides which
//! catalog is read: `book:12`, `paper:101` and `reference:names/key`
//! reach three different control-plane methods and render three
//! different cards. That decision is made once, in [`call_for`], which
//! pairs the method with the renderer for its response and with the
//! test for "the library has no such item" — a second table keyed by
//! kind is how the three would drift into disagreeing about which
//! pipeline an id named.
//!
//! A `reference:` id without a key names a whole reference book, which
//! this verb does not read. That refusal happens before a connection
//! is opened, so it reads as a capability boundary rather than as a
//! daemon that is not running.
//!
//! An id the library does not hold is refused after the read, with the
//! same exit code whatever the output mode: a `null` body or an empty
//! hit list is not a card to draw, and printing it as one would make
//! "nothing there" look like a record.

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
    if (call.missing)(&response) {
        let library = helpers::answering_library(&client).await;
        return Err(BookrackCliError::ItemNotFound {
            problem: not_found_problem(&id, library.as_deref()),
        }
        .into());
    }
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
/// renderer for its response and the test for an empty answer.
struct Call {
    /// Method the id addresses.
    method: &'static str,
    /// Parameters for that method. The library selection is injected
    /// downstream by `library_param`, not here.
    params: Value,
    /// Card the human-readable branch draws from the response.
    card: fn(&Value) -> String,
    /// Whether the response says the library has no such item.
    missing: fn(&Value) -> bool,
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
            missing: is_null,
        }),
        TypedItemId::Paper(intake_id) => Ok(Call {
            method: "library.show_paper",
            params: json!({ "intake_id": intake_id }),
            card: format_paper_detail,
            missing: is_null,
        }),
        TypedItemId::Reference {
            book_slug,
            entry_key: Some(key),
        } => Ok(Call {
            method: "reference.lookup",
            params: json!({ "book": book_slug, "entry_key": key }),
            card: format_reference_entry,
            missing: has_no_hits,
        }),
        TypedItemId::Reference {
            entry_key: None, ..
        } => Err(BookrackCliError::ItemIdUnusable {
            problem: whole_reference_book(),
        }),
    }
}

/// A `library.show_book` / `library.show_paper` body for an intake id
/// the catalog does not hold.
pub(super) fn is_null(response: &Value) -> bool {
    response.is_null()
}

/// A `reference.lookup` body that found no entry under the key, as
/// given or in its latin form.
fn has_no_hits(response: &Value) -> bool {
    response
        .get("hits")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
}

/// The refusal a `reference:` id without a key earns at this surface.
fn whole_reference_book() -> Problem {
    Problem::new("cannot show a whole reference book")
        .detail(
            "The `show` verb reads one entry; a reference id without a key names the book \
             itself.",
        )
        .hint(
            "Name an entry as `reference:<slug>/<key>`. `bookrack distill list` lists the \
             reference books.",
        )
}

/// The three-part refusal for an id the library does not hold.
/// `library` is the name the read was answered under, when the
/// invocation or the daemon could say.
pub(super) fn not_found_problem(id: &TypedItemId, library: Option<&str>) -> Problem {
    let summary = match library {
        Some(name) => format!("no item \"{id}\" in library \"{name}\""),
        None => format!("no item \"{id}\" in this library"),
    };
    let (detail, hint) = match id {
        TypedItemId::Book(intake_id) => (
            format!("The library has no book with intake id {intake_id}."),
            "Run `bookrack list` to see the books and their ids.",
        ),
        TypedItemId::Paper(intake_id) => (
            format!("The library has no paper with intake id {intake_id}."),
            "Run `bookrack papers list` to see the papers and their ids.",
        ),
        TypedItemId::Reference { .. } => (
            "The lookup tried the key as given and in its latin form.".to_string(),
            "Run `bookrack distill list` to see the reference books.",
        ),
    };
    Problem::new(summary).detail(detail).hint(hint)
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

/// Renders one `reference.lookup` response as one key-value card per
/// hit: the book, the canonical key, the headword and aliases, every
/// payload key as its own row, the quality flags and the overlay stamp
/// when there are any, and how the key was resolved when it was not
/// matched as written.
///
/// A string payload value prints as itself, an array as its elements
/// joined by commas, anything else as compact JSON.
///
/// The resolution rows read the whole response, not one hit: a
/// `redirect_followed` names the key the redirect left from, and when
/// that key is not the one the operator typed the lookup went through
/// the latin form first, so both rows are drawn. Without a redirect, a
/// hit whose key differs from the query was reached through the latin
/// form alone.
fn format_reference_entry(response: &Value) -> String {
    let queried = response.get("entry_key").and_then(Value::as_str);
    let redirected_from = response.get("redirect_followed").and_then(Value::as_str);
    let hits = response
        .get("hits")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut cards = Vec::with_capacity(hits.len());
    for hit in hits {
        let mut t = KvTable::new();
        if let Some(book) = hit.get("book_slug").and_then(Value::as_str) {
            t.push("book", book);
        }
        let key = hit.get("entry_key").and_then(Value::as_str);
        if let Some(key) = key {
            t.push("key", key);
        }
        if let Some(headword) = hit.get("headword").and_then(Value::as_str) {
            t.push("headword", headword);
        }
        if let Some(aliases) = hit.get("aliases").and_then(Value::as_array)
            && !aliases.is_empty()
        {
            t.push("aliases", join_values(aliases));
        }
        if let Some(payload) = hit.get("payload").and_then(Value::as_object) {
            for (k, v) in payload {
                t.push(k, payload_cell(v));
            }
        }
        if let Some(flags) = hit.get("quality_flags").and_then(Value::as_array)
            && !flags.is_empty()
        {
            t.push("quality_flags", join_values(flags));
        }
        if hit.get("has_overlay").and_then(Value::as_bool) == Some(true) {
            for row in ["edited_at", "edit_reason"] {
                if let Some(val) = hit.get(row).and_then(Value::as_str) {
                    t.push(row, val);
                }
            }
        }
        match redirected_from {
            Some(from) => {
                t.push("redirected from", from);
                if queried.is_some_and(|q| q != from) {
                    t.push("matched as", from);
                }
            }
            None => {
                if let Some(key) = key
                    && queried.is_some_and(|q| q != key)
                {
                    t.push("matched as", key);
                }
            }
        }
        cards.push(t.render());
    }
    cards.join("\n")
}

fn payload_cell(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => join_values(items),
        other => other.to_string(),
    }
}

fn join_values(items: &[Value]) -> String {
    items
        .iter()
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
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

    /// A `reference.lookup` body with one hit, as the control plane
    /// returns it. `queried` is the key the caller sent, `key` the
    /// canonical key of the hit, `redirected_from` the key a redirect
    /// left from.
    fn lookup_body(queried: &str, key: &str, redirected_from: Option<&str>) -> Value {
        json!({
            "entry_key": queried,
            "hits": [{
                "book_slug": "names",
                "entry_key": key,
                "headword": "Ashgrove, Wren",
                "aliases": ["Wren Ashgrove"],
                "payload": {
                    "country": "UK",
                    "variants": ["Ash", "Grove"],
                    "year_span": { "birth": 1900 },
                },
                "source": { "book_slug": "names", "page": 1, "sheet": 1 },
                "quality_flags": ["pair_mismatch"],
                "edit_reason": "corrected the country",
                "edited_at": "2026-06-25T01:00:00Z",
                "has_overlay": true,
            }],
            "primary_by_authority": 0,
            "redirect_followed": redirected_from,
        })
    }

    /// The three readable kinds address three different methods, each
    /// carrying its own identifier.
    ///
    /// An implementation that sent `library.show_book` for both intake
    /// kinds would still return a record on a library where the number
    /// exists in both catalogs — the wrong record, silently. The method
    /// name is the only thing that tells the two apart before a daemon
    /// is in the picture.
    #[test]
    fn each_kind_routes_to_the_method_that_reads_it() {
        let book = call_for(&TypedItemId::Book(12)).expect("a book id routes");
        assert_eq!(book.method, "library.show_book");
        assert_eq!(book.params, json!({ "intake_id": 12 }));

        let paper = call_for(&TypedItemId::Paper(101)).expect("a paper id routes");
        assert_eq!(paper.method, "library.show_paper");
        assert_eq!(paper.params, json!({ "intake_id": 101 }));

        let entry = call_for(&TypedItemId::Reference {
            book_slug: "names".into(),
            entry_key: Some("Ashgrove, Wren".into()),
        })
        .expect("a keyed reference id routes");
        assert_eq!(entry.method, "reference.lookup");
        assert_eq!(
            entry.params,
            json!({ "book": "names", "entry_key": "Ashgrove, Wren" })
        );
    }

    /// Every kind reads, and no two kinds share a method name.
    #[test]
    fn the_projection_covers_every_kind_and_repeats_no_method() {
        let mut methods: Vec<&'static str> = Vec::new();
        for id in one_of_each() {
            let call = call_for(&id).unwrap_or_else(|e| panic!("{id} has no read path: {e}"));
            methods.push(call.method);
        }
        assert_eq!(methods.len(), 3);
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

    /// A reference id without a key is refused with all three parts,
    /// and the hint names the form that does read.
    #[test]
    fn a_reference_id_without_a_key_is_refused_with_a_detail_and_a_hint() {
        let refused = call_for(&TypedItemId::Reference {
            book_slug: "name_alpha".into(),
            entry_key: None,
        })
        .err()
        .expect("a whole reference book has no read path here");
        let BookrackCliError::ItemIdUnusable { problem } = refused else {
            panic!("a whole reference book should be refused as an unusable item id");
        };
        assert_eq!(problem.summary, "cannot show a whole reference book");
        assert!(
            problem.data.detail.is_some(),
            "the refusal states no detail"
        );
        let hint = problem.data.hint.expect("the refusal offers no hint");
        assert!(hint.contains("reference:<slug>/<key>"), "{hint}");
        assert!(hint.contains("distill list"), "{hint}");
    }

    /// The card rows what a hit carries: the identity, every payload
    /// key in its own shape, the flags and the overlay stamp.
    #[test]
    fn the_reference_card_rows_the_hit_and_its_payload() {
        let card = format_reference_entry(&lookup_body("ashgrovewren", "ashgrovewren", None));
        for (key, value) in [
            ("book", "names"),
            ("key", "ashgrovewren"),
            ("headword", "Ashgrove, Wren"),
            ("aliases", "Wren Ashgrove"),
            ("country", "UK"),
            ("variants", "Ash, Grove"),
            ("year_span", "{\"birth\":1900}"),
            ("quality_flags", "pair_mismatch"),
            ("edited_at", "2026-06-25T01:00:00Z"),
            ("edit_reason", "corrected the country"),
        ] {
            assert!(
                card.contains(key) && card.contains(value),
                "the card lacks the {key} row with {value:?}:\n{card}"
            );
        }
        assert!(!card.contains("matched as"), "{card}");
        assert!(!card.contains("redirected from"), "{card}");
    }

    /// A hit reached through the latin form says so; a hit reached
    /// through a redirect says where it left from; a hit reached by
    /// both says both, naming the latin form each time.
    #[test]
    fn the_reference_card_says_how_the_key_was_resolved() {
        let fallback = format_reference_entry(&lookup_body("Ashgrove, Wren", "ashgrovewren", None));
        assert!(
            fallback.contains("matched as") && fallback.contains("ashgrovewren"),
            "{fallback}"
        );
        assert!(!fallback.contains("redirected from"), "{fallback}");

        let redirect = format_reference_entry(&lookup_body("jones", "smith", Some("jones")));
        assert!(redirect.contains("redirected from"), "{redirect}");
        assert!(!redirect.contains("matched as"), "{redirect}");

        let both = format_reference_entry(&lookup_body("Jones, A", "smith", Some("jonesa")));
        assert!(both.contains("redirected from"), "{both}");
        assert!(both.contains("matched as"), "{both}");
        assert_eq!(both.matches("jonesa").count(), 2, "{both}");
    }

    /// A `null` body and an empty hit list are "no such item"; a body
    /// with content is not.
    #[test]
    fn a_null_body_or_an_empty_hit_list_is_a_missing_item() {
        assert!(is_null(&Value::Null));
        assert!(!is_null(&json!({ "intake_id": 12 })));
        assert!(has_no_hits(&json!({ "entry_key": "x", "hits": [] })));
        assert!(has_no_hits(&json!({ "entry_key": "x" })));
        assert!(!has_no_hits(&lookup_body("x", "x", None)));
    }

    /// The refusal names the id as the operator wrote it, the library
    /// when one is known, and a listing command for the kind.
    #[test]
    fn the_not_found_problem_names_the_id_the_library_and_a_listing() {
        let book = not_found_problem(&TypedItemId::Book(999), Some("alpha"));
        assert_eq!(book.summary, "no item \"book:999\" in library \"alpha\"");
        assert!(book.data.detail.as_deref().unwrap().contains("999"));
        assert!(
            book.data
                .hint
                .as_deref()
                .unwrap()
                .contains("`bookrack list`")
        );

        let paper = not_found_problem(&TypedItemId::Paper(999), None);
        assert_eq!(paper.summary, "no item \"paper:999\" in this library");
        assert!(paper.data.hint.as_deref().unwrap().contains("papers list"));

        let entry = not_found_problem(
            &TypedItemId::Reference {
                book_slug: "names".into(),
                entry_key: Some("nobody".into()),
            },
            Some("alpha"),
        );
        assert_eq!(
            entry.summary,
            "no item \"reference:names/nobody\" in library \"alpha\""
        );
        assert!(entry.data.detail.as_deref().unwrap().contains("latin form"));
        assert!(entry.data.hint.as_deref().unwrap().contains("distill list"));
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
