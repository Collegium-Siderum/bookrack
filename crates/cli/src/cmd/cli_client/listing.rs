// SPDX-License-Identifier: Apache-2.0

//! `bookrack list` — both catalogs from one verb, and the pieces every
//! listing shares.
//!
//! A listing addresses no namespace of its own, so the id it prints has
//! to carry the pipeline that answers it — otherwise `101` in a paper
//! table and `101` in a book table read as the same item. The id is
//! built through [`TypedItemId`] rather than by formatting the two
//! fields, so what a table prints is what `FromStr` reads back.
//!
//! The two catalogs are read by two methods and paged independently:
//! `--limit` bounds each side, and each side reports its own total. A
//! merged page would have to invent an order across two corpora that
//! share no sort key, so what this verb merges is the presentation,
//! not the result set. The `--json` payload says so in its shape — one
//! `items` array with each row naming its own kind, and a `pages` block
//! carrying only the sides that were actually asked.

use std::path::PathBuf;

use bookrack_cli::library_param;
use bookrack_cli::render::ctx;
use bookrack_cli::render::human::truncate_to;
use bookrack_cli::render::table::RowTable;
use bookrack_cli_grammar::{ListArgs, Scope};
use bookrack_control_client::ControlClient;
use bookrack_core::{ItemKind, TypedItemId};
use eyre::Result;
use serde_json::{Map, Value, json};

use super::helpers;
use super::papers::format_paper_list;
use super::status::default_served;

/// One side of a listing: the pipeline it reads, the method that pages
/// it, the key its response carries rows under, and the renderer for
/// its table.
///
/// Held as data rather than as a `match` per question, so a side
/// cannot end up read through one pipeline's method and rendered as
/// another's.
struct Side {
    /// The pipeline whose rows this side carries.
    kind: ItemKind,
    /// Control-plane method that pages it.
    list_method: &'static str,
    /// Key the response carries its rows under.
    rows_key: &'static str,
    /// Human table for one page of it.
    table: fn(&Value) -> String,
}

const BOOK: Side = Side {
    kind: ItemKind::Book,
    list_method: "library.list_books",
    rows_key: "books",
    table: format_book_list,
};

const PAPER: Side = Side {
    kind: ItemKind::Paper,
    list_method: "library.list_papers",
    rows_key: "papers",
    table: format_paper_list,
};

/// The sides one scope reaches, in the order they are printed.
///
/// Exhaustive on purpose: a fourth scope value fails to compile here
/// rather than silently reaching whichever sides a `_` arm picked.
fn sides(scope: Scope) -> &'static [Side] {
    match scope {
        Scope::Book => &[BOOK],
        Scope::Paper => &[PAPER],
        Scope::All => &[BOOK, PAPER],
    }
}

pub async fn list(args: ListArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    let params = page_params(&args);
    let mut pages: Vec<(&Side, Value)> = Vec::new();
    for side in sides(args.scope) {
        let response = helpers::dispatch(&client, side.list_method, params.clone()).await?;
        pages.push((side, response));
    }

    if ctx().is_quiet() {
        return Ok(());
    }
    let library = answering_library(&client).await;
    let library = library.as_deref();
    if ctx().is_json() {
        helpers::print_value(&compose(&pages, library));
    } else {
        println!("{}", render(&pages, library));
    }
    Ok(())
}

/// Paging parameters, sent to each side unchanged. The server-side cap
/// is not restated here: a second copy of that number would drift from
/// the one `config fixed` reports.
fn page_params(args: &ListArgs) -> Value {
    let mut params = json!({});
    if let Some(n) = args.limit {
        params["limit"] = Value::from(n);
    }
    if let Some(n) = args.offset {
        params["offset"] = Value::from(n);
    }
    params
}

/// The library the rows came from, or `None` when nobody can say.
///
/// A named invocation already knows: the selection is what routed the
/// calls above. An unnamed one has to ask the daemon, because the
/// library it reaches is the registry default among the served set,
/// which is not necessarily the one the daemon came up under.
async fn answering_library(client: &ControlClient) -> Option<String> {
    let selected = library_param::selected();
    if selected.is_some() {
        return card_library(selected, None);
    }
    let status = helpers::dispatch(client, "status", Value::Null).await.ok();
    card_library(None, status.as_ref())
}

/// The library name a card or a listing should print, or `None` when
/// nobody can say: the invocation named one, else the library an
/// unnamed call resolves to — the registry default among the served
/// set.
///
/// The answer to an unnamed call is not `status`'s `library` field.
/// That one reports the primary, the library the daemon came up under,
/// and a call naming none reaches the registry default instead. Taking
/// the primary would put a name under records that did not come from
/// it.
///
/// Nothing is guessed when the daemon cannot be asked: a row that is
/// sometimes a placeholder is worse than a row that is sometimes
/// absent, because only the second one is honest about not knowing.
pub(super) fn card_library(selected: Option<&str>, status: Option<&Value>) -> Option<String> {
    if let Some(name) = selected {
        return Some(name.to_string());
    }
    let served = status?.get("served")?;
    default_served(served).map(String::from)
}

/// The id a listing row prints for one catalog row, or `None` for a
/// kind whose rows are not addressed by an intake id.
///
/// The kind comes from the caller because the catalog does not carry
/// it: `intake` rows are separated by which database file they live
/// in, not by a column, so a table that reads one side names that side
/// and a merged listing takes the kind from the row's own side.
///
/// Built through [`TypedItemId`] rather than by formatting the kind and
/// the id into a string. Formatting would also produce something with a
/// colon in it, for a kind that has no such syntax and for a payload
/// shape the kind does not take — and the round-trip is the whole point
/// of printing the id this way.
pub(super) fn row_id(kind: ItemKind, intake_id: i64) -> Option<TypedItemId> {
    match kind {
        ItemKind::Book => Some(TypedItemId::Book(intake_id)),
        ItemKind::Paper => Some(TypedItemId::Paper(intake_id)),
        // A reference book is addressed by its slug; there is no
        // integer to render.
        ItemKind::Reference => None,
    }
}

/// The `--json` payload for one invocation.
///
/// Assembled here rather than forwarded, because no single method
/// answers this verb. Three properties the shape commits to:
///
///   * `items` is flat and every row names its own `kind`, so a
///     consumer never has to know which side it is reading;
///   * `pages` carries only the sides that were asked, so a total of
///     zero and a side that was not read stay distinguishable;
///   * `library` is absent rather than null when the daemon could not
///     be asked.
///
/// Row fields are carried over untouched: this adds `id` and `kind`
/// and renames nothing, so a reader can still resolve the rest against
/// the method's own documented summary.
fn compose(pages: &[(&Side, Value)], library: Option<&str>) -> Value {
    let mut items = Vec::new();
    let mut page_totals = Map::new();
    for (side, response) in pages {
        for row in rows_of(side, response) {
            items.push(with_id(side.kind, row));
        }
        let mut page = Map::new();
        for key in ["total", "truncated"] {
            if let Some(value) = response.get(key) {
                page.insert(key.to_string(), value.clone());
            }
        }
        page_totals.insert(side.kind.as_scope_str().to_string(), Value::Object(page));
    }
    let mut out = Map::new();
    out.insert("items".to_string(), Value::Array(items));
    out.insert("pages".to_string(), Value::Object(page_totals));
    if let Some(name) = library {
        out.insert("library".to_string(), Value::from(name));
    }
    Value::Object(out)
}

/// One response's rows, or nothing when the side reported none.
fn rows_of<'a>(side: &Side, response: &'a Value) -> &'a [Value] {
    response
        .get(side.rows_key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// One catalog row with its typed id and its kind alongside the fields
/// the method sent.
fn with_id(kind: ItemKind, row: &Value) -> Value {
    let mut obj = row.as_object().cloned().unwrap_or_default();
    obj.insert("kind".to_string(), Value::from(kind.as_scope_str()));
    if let Some(id) = row
        .get("intake_id")
        .and_then(Value::as_i64)
        .and_then(|intake_id| row_id(kind, intake_id))
    {
        obj.insert("id".to_string(), Value::from(id.to_string()));
    }
    Value::Object(obj)
}

/// The human rendering: one section per side, then the library the
/// rows came from.
///
/// A section is named only when there is more than one: a heading over
/// the single table a `--scope book` page can hold states something the
/// command line already said.
fn render(pages: &[(&Side, Value)], library: Option<&str>) -> String {
    let name_sections = pages.len() > 1;
    let mut out = String::new();
    for (side, response) in pages {
        if name_sections {
            out.push_str(side.rows_key);
            out.push('\n');
        }
        out.push_str(&(side.table)(response));
        out.push('\n');
    }
    match library {
        Some(name) => out.push_str(&format!("library: {name}")),
        // Trailing blank lines would be the only trace of a row that is
        // absent on purpose.
        None => out.truncate(out.trim_end().len()),
    }
    out
}

/// The count line a page ends on, or `None` when there is nothing to
/// say: a response that carries no total says nothing about the size of
/// the result set, and neither does the footer.
///
/// Shared by both sides so the two tables cannot come to word the same
/// fact differently.
pub(super) fn page_footer(response: &Value, shown: usize) -> Option<String> {
    let total = response.get("total").and_then(Value::as_u64)?;
    let truncated = response
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if truncated {
        Some(format!(
            "(showing {shown} of {total}; pass --limit to see more)"
        ))
    } else if total as usize != shown {
        Some(format!("({shown} of {total})"))
    } else {
        None
    }
}

/// Renders one `library.list_books` page as a table of `id`, `title`,
/// `author`, `format`, and `status`, each cell cut to a fixed width. A
/// row whose title was never extracted shows its `source_filename` in
/// the title cell instead, so the row is still identifiable without
/// widening the table; the cell reads `-` only when neither is
/// recorded.
///
/// The columns are the book summary's own, which is why they do not
/// match the paper table's: `format` and `status` have no paper-side
/// counterpart worth a column, and `year` and `container` have no
/// book-side one.
fn format_book_list(response: &Value) -> String {
    let books = response.get("books").and_then(Value::as_array);
    let rows = match books {
        Some(arr) if !arr.is_empty() => arr,
        _ => return "no books match".to_string(),
    };
    let mut table = RowTable::new(["id", "title", "author", "format", "status"]);
    for row in rows {
        let id = row
            .get("intake_id")
            .and_then(Value::as_i64)
            .and_then(|i| row_id(ItemKind::Book, i))
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".to_string());
        let title = row
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| row.get("source_filename").and_then(Value::as_str))
            .map(|s| truncate_to(s, 48))
            .unwrap_or_else(|| "-".to_string());
        let author = row
            .get("top_contributor")
            .and_then(Value::as_str)
            .map(|s| truncate_to(s, 24))
            .unwrap_or_else(|| "-".to_string());
        let format = row
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_string();
        let status = row
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_string();
        table.push_row([id, title, author, format, status]);
    }
    let mut out = table.render();
    if let Some(footer) = page_footer(response, rows.len()) {
        out.push('\n');
        out.push_str(&footer);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use bookrack_cli::render::OutputMode;
    use bookrack_runtime::control::methods::REGISTRY;

    use super::super::intake::format_ocr_pending;
    use super::super::papers::paper_list_output;
    use super::*;

    /// Every kind is answered, and the two that carry an intake id
    /// render the id the parser reads back for that same kind.
    ///
    /// The round trip goes through the kind as well as the number: an
    /// implementation that projected both catalogs onto one prefix
    /// would still produce a parseable id, and would still name the
    /// wrong item.
    #[test]
    fn each_kind_projects_onto_the_id_that_addresses_it() {
        assert_eq!(row_id(ItemKind::Book, 12), Some(TypedItemId::Book(12)));
        assert_eq!(row_id(ItemKind::Paper, 12), Some(TypedItemId::Paper(12)));
        assert_eq!(row_id(ItemKind::Reference, 12), None);

        for kind in [ItemKind::Book, ItemKind::Paper] {
            let rendered = row_id(kind, 12)
                .expect("an intake-addressed kind")
                .to_string();
            let parsed = TypedItemId::from_str(&rendered).expect("a rendered id parses");
            assert_eq!(
                parsed.kind(),
                kind,
                "{rendered:?} reads back as {:?}",
                parsed.kind()
            );
        }
    }

    /// The first cell of every body row of a rendered table, in row
    /// order. The tables draw their borders with box-drawing characters
    /// and an id carries no whitespace, so the leading token of a body
    /// row is the whole id cell.
    fn id_cells(table: &str) -> Vec<String> {
        table
            .lines()
            .filter(|line| line.starts_with('\u{2502}'))
            .skip(1)
            .filter_map(|line| {
                line.trim_matches('\u{2502}')
                    .split_whitespace()
                    .next()
                    .map(str::to_string)
            })
            .collect()
    }

    /// Every id a paper listing prints parses back to the item it
    /// names.
    ///
    /// Asserted by reading each cell back rather than by matching a
    /// prefix: an implementation that pastes a kind string in front of
    /// the number would satisfy "contains a colon" while printing
    /// something no command accepts.
    #[test]
    fn every_id_a_paper_listing_prints_reads_back_as_the_paper_it_names() {
        let table = format_paper_list(&paper_page());
        let cells = id_cells(&table);
        assert_eq!(cells.len(), 2, "one id cell per row in:\n{table}");
        for (cell, intake_id) in cells.iter().zip([101, 102]) {
            assert_eq!(
                TypedItemId::from_str(cell).map_err(|e| e.to_string()),
                Ok(TypedItemId::Paper(intake_id)),
                "the id cell {cell:?} is not an id any command accepts, in:\n{table}"
            );
        }
    }

    /// Every id a book listing prints parses back the same way. The
    /// book table is new, so it gets the assertion in its own right
    /// rather than inheriting the paper table's.
    #[test]
    fn every_id_a_book_listing_prints_reads_back_as_the_book_it_names() {
        let table = format_book_list(&book_page());
        let cells = id_cells(&table);
        assert_eq!(cells.len(), 2, "one id cell per row in:\n{table}");
        for (cell, intake_id) in cells.iter().zip([12, 13]) {
            assert_eq!(
                TypedItemId::from_str(cell).map_err(|e| e.to_string()),
                Ok(TypedItemId::Book(intake_id)),
                "the id cell {cell:?} is not an id any command accepts, in:\n{table}"
            );
        }
    }

    /// The OCR worklist prints book-side intake ids, so it prints them
    /// the way the book listings do. Its rows are book rows throughout:
    /// `distill` registers no intake, so no reference row can reach
    /// this table.
    #[test]
    fn every_id_the_ocr_worklist_prints_reads_back_as_the_book_it_names() {
        let response = json!({
            "items": [
                { "intake_id": 7, "pages": 12, "source_path": "/inbox/one.pdf" },
                { "intake_id": 8, "pages": 30, "source_path": "/inbox/two.pdf" },
            ],
            "total": 2,
            "truncated": false,
        });
        let table = format_ocr_pending(&response);
        let cells = id_cells(&table);
        assert_eq!(cells.len(), 2, "one id cell per row in:\n{table}");
        for (cell, intake_id) in cells.iter().zip([7, 8]) {
            assert_eq!(
                TypedItemId::from_str(cell).map_err(|e| e.to_string()),
                Ok(TypedItemId::Book(intake_id)),
                "the id cell {cell:?} is not an id any command accepts, in:\n{table}"
            );
        }
    }

    /// A typed id is composed by the human renderer and reaches no
    /// forwarded payload.
    ///
    /// Both branches are asserted from the same response, because what
    /// discriminates is that they disagree: an implementation that
    /// projected the id onto the response before choosing a branch
    /// would satisfy the human half and put `paper:101` on the wire
    /// shape a script reads. `--json` on a namespaced verb is
    /// documented against `library.list_papers`, and a key the CLI
    /// invented would make that document wrong.
    #[test]
    fn a_typed_id_reaches_the_table_and_not_the_forwarded_payload() {
        let response = json!({
            "papers": [{ "intake_id": 101, "title": "Alpha" }],
            "total": 1,
            "truncated": false,
        });

        let table = paper_list_output(OutputMode::Human, &response).expect("a human table");
        assert!(
            table.contains("paper:101"),
            "the table should print the typed id:\n{table}"
        );

        let payload = paper_list_output(OutputMode::Json, &response).expect("a json payload");
        let parsed: Value = serde_json::from_str(&payload).expect("one JSON document");
        assert_eq!(
            parsed, response,
            "the forwarded payload is not the response it was given"
        );
        assert!(
            !payload.contains("paper:") && !payload.contains("book:"),
            "a typed id reached the forwarded payload:\n{payload}"
        );
    }

    fn book_page() -> Value {
        json!({
            "books": [
                { "intake_id": 12, "title": "Alpha", "status": "ready", "format": "epub" },
                { "intake_id": 13, "title": "Beta", "status": "pending", "format": "pdf" },
            ],
            "total": 7,
            "truncated": false,
        })
    }

    fn paper_page() -> Value {
        json!({
            "papers": [
                { "intake_id": 101, "title": "Gamma" },
                { "intake_id": 102, "title": "Delta" },
            ],
            "total": 3,
            "truncated": true,
        })
    }

    fn both_sides() -> Vec<(&'static Side, Value)> {
        vec![(&BOOK, book_page()), (&PAPER, paper_page())]
    }

    /// Each side reports its own total, under its own key.
    ///
    /// The tempting simplification is one `total` summing both sides.
    /// It cannot be right: the two sides page independently, so a sum
    /// answers no question a reader can act on — neither "how many more
    /// books are there" nor "did this page cover the papers".
    #[test]
    fn each_side_keeps_its_own_page_totals() {
        let composed = compose(&both_sides(), None);
        assert_eq!(composed["pages"]["book"]["total"], json!(7));
        assert_eq!(composed["pages"]["paper"]["total"], json!(3));
        assert_eq!(composed["pages"]["book"]["truncated"], json!(false));
        assert_eq!(composed["pages"]["paper"]["truncated"], json!(true));
    }

    /// A side that was not read has no entry at all.
    ///
    /// An implementation that always emitted both skeletons would make
    /// "this side holds nothing" and "this side was not asked"
    /// indistinguishable — and the first is a fact about the library
    /// while the second is a fact about the command.
    #[test]
    fn a_side_that_was_not_read_carries_no_page_block() {
        let composed = compose(&[(&BOOK, book_page())], None);
        assert!(composed["pages"].get("book").is_some(), "{composed}");
        assert_eq!(
            composed["pages"].get("paper"),
            None,
            "a side that was never asked reported a page: {composed}"
        );
    }

    /// Every row's prefix comes from the side that answered it.
    ///
    /// The failure this pins is specific to the merged payload: an
    /// implementation taking the kind from `--scope` rather than from
    /// the row would be correct on both single-sided scopes and wrong
    /// on half the rows under `--scope all`.
    #[test]
    fn every_row_names_the_side_it_came_from() {
        let composed = compose(&both_sides(), None);
        let items = composed["items"].as_array().expect("an items array");
        assert_eq!(items.len(), 4, "{composed}");
        for item in items {
            let id = item["id"].as_str().expect("every row carries an id");
            let parsed = TypedItemId::from_str(id).expect("a listed id parses");
            assert_eq!(
                parsed.kind().as_scope_str(),
                item["kind"].as_str().expect("every row names its kind"),
                "row {item} prints an id from another pipeline"
            );
        }
        assert_eq!(items[0]["id"], json!("book:12"));
        assert_eq!(items[2]["id"], json!("paper:101"));
        assert_eq!(
            items[0]["title"],
            json!("Alpha"),
            "the row's own fields are carried over untouched"
        );
    }

    /// The scope vocabulary is the catalog's own, and the kind with no
    /// listing is absent from it.
    ///
    /// `reference` is not a `--scope` value because no method lists
    /// reference rows. Asserting only the two present values would let
    /// a third be added silently, which is the mistake that reads as
    /// generous and produces a flag value nothing answers.
    #[test]
    fn the_scope_vocabulary_matches_the_catalog_scope_strings() {
        assert_eq!(Scope::Book.to_string(), ItemKind::Book.as_scope_str());
        assert_eq!(Scope::Paper.to_string(), ItemKind::Paper.as_scope_str());
        let values: Vec<String> = <Scope as clap::ValueEnum>::value_variants()
            .iter()
            .map(Scope::to_string)
            .collect();
        assert_eq!(values, ["book", "paper", "all"]);
        assert!(
            !values.contains(&ItemKind::Reference.as_scope_str().to_string()),
            "reference became a scope value without a method that lists it: {values:?}"
        );
    }

    /// Every method this verb sends is one the daemon answers, and one
    /// the library selection is routed into.
    ///
    /// The client-wide scan in `helpers` only sees literals sitting in
    /// the method argument of a call; here they sit in a table and
    /// reach `dispatch` through a variable, so these would otherwise
    /// cross the wire unchecked. The `library_key` half is what makes
    /// `--library` reach both sides without a line of wiring in this
    /// module.
    #[test]
    fn every_method_this_verb_sends_is_routed_by_the_daemon() {
        for side in sides(Scope::All) {
            let sig = REGISTRY
                .iter()
                .find(|sig| sig.name == side.list_method)
                .unwrap_or_else(|| {
                    panic!("`{}` is not in the daemon's method table", side.list_method)
                });
            assert_eq!(
                sig.library_key,
                Some("library"),
                "`{}` does not take a library selection",
                side.list_method
            );
        }
    }

    /// Sections are named when a page holds more than one, and not when
    /// it holds one.
    #[test]
    fn sections_are_named_only_when_a_page_holds_more_than_one() {
        let both = render(&both_sides(), None);
        assert!(both.starts_with("books\n"), "{both}");
        assert!(both.contains("\npapers\n"), "{both}");

        let one = render(&[(&PAPER, paper_page())], None);
        assert!(
            !one.contains("papers\n"),
            "a single-sided page named its only section:\n{one}"
        );
    }

    /// The library row is the last line when there is an answer, and no
    /// line at all when there is not.
    #[test]
    fn the_library_row_is_printed_only_when_it_can_be_answered() {
        let named = render(&[(&BOOK, book_page())], Some("alpha"));
        assert!(named.ends_with("\nlibrary: alpha"), "{named}");

        let unanswered = render(&[(&BOOK, book_page())], None);
        assert!(!unanswered.contains("library:"), "{unanswered}");
        assert!(
            !unanswered.ends_with('\n'),
            "an absent library row left a trailing blank line:\n{unanswered:?}"
        );
    }

    /// A daemon serving two libraries, come up under the one the
    /// registry does not point at. The shape `status` reports for it,
    /// and the shape the library row has to read correctly.
    fn two_served_primary_is_not_default() -> Value {
        json!({
            "library": "beta",
            "data_dir": "/data/beta",
            "served": [
                { "name": "alpha", "data_dir": "/data/alpha", "default": true, "primary": false },
                { "name": "beta", "data_dir": "/data/beta", "default": false, "primary": true },
            ],
        })
    }

    /// An unnamed invocation reaches the registry default, so that is
    /// the library the row names.
    ///
    /// The tempting implementation reads `status`'s `library` field,
    /// which reports the primary. On the daemon above that would put
    /// `beta` under records fetched from `alpha` — an identity line and
    /// contents from two libraries.
    #[test]
    fn an_unnamed_invocation_names_the_library_it_actually_reached() {
        let status = two_served_primary_is_not_default();
        assert_eq!(
            card_library(None, Some(&status)).as_deref(),
            Some("alpha"),
            "an unnamed call reaches the registry default, not the primary",
        );
    }

    /// One library is the ordinary case, and it is the one an
    /// implementation reusing the status card's `served` row would get
    /// wrong: that row is suppressed below two libraries, and a name
    /// built on it would go missing exactly where operators see it
    /// most.
    #[test]
    fn a_single_library_daemon_still_names_its_library() {
        let status = json!({
            "library": "solo",
            "data_dir": "/data/solo",
            "served": [
                { "name": "solo", "data_dir": "/data/solo", "default": true, "primary": true },
            ],
        });
        assert_eq!(card_library(None, Some(&status)).as_deref(), Some("solo"));
    }

    /// A named invocation already knows the answer and does not
    /// second-guess it against the daemon: the selection is what routed
    /// the calls that produced the records.
    #[test]
    fn a_named_invocation_reports_the_library_it_named() {
        let status = two_served_primary_is_not_default();
        assert_eq!(
            card_library(Some("beta"), Some(&status)).as_deref(),
            Some("beta"),
        );
    }

    /// Nothing to say and nothing said: no placeholder, no empty cell,
    /// no row. Covers both ways the answer can be missing — the daemon
    /// was not asked, and the daemon answered without a served set.
    #[test]
    fn an_unanswerable_library_prints_no_row_at_all() {
        assert_eq!(card_library(None, None), None);
        let no_served = json!({ "library": "solo", "data_dir": "/data/solo" });
        assert_eq!(card_library(None, Some(&no_served)), None);
        let unreadable = json!({ "library": "solo", "served": Value::Null });
        assert_eq!(card_library(None, Some(&unreadable)), None);
    }

    /// The library is absent from the payload rather than null, so a
    /// consumer testing for the key gets the same answer the human row
    /// gives by not printing.
    #[test]
    fn an_unanswerable_library_is_absent_from_the_payload() {
        let composed = compose(&both_sides(), None);
        assert_eq!(composed.get("library"), None, "{composed}");
        let named = compose(&both_sides(), Some("alpha"));
        assert_eq!(named["library"], json!("alpha"));
    }
}
