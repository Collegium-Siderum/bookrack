// SPDX-License-Identifier: Apache-2.0

//! Shared pieces of the listing surface: what a row prints for its id,
//! and the assertions that keep every table printing the same shape.
//!
//! A listing addresses no namespace of its own, so the id it prints has
//! to carry the pipeline that answers it — otherwise `101` in a paper
//! table and `101` in a book table read as the same item. The id is
//! built through [`TypedItemId`] rather than by formatting the two
//! fields, so what a table prints is what `FromStr` reads back.

use bookrack_core::{ItemKind, TypedItemId};

/// The id a listing row prints for one catalog row, or `None` for a
/// kind whose rows are not addressed by an intake id.
///
/// The kind comes from the caller because the catalog does not carry
/// it: `intake` rows are separated by which database file they live
/// in, not by a column, so a table that reads one side names that side
/// and a table that merges both takes the kind from the row's own
/// side.
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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use bookrack_cli::render::OutputMode;
    use serde_json::{Value, json};

    use super::super::intake::format_ocr_pending;
    use super::super::papers::{format_paper_list, paper_list_output};
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
        let response = json!({
            "papers": [
                { "intake_id": 101, "title": "Alpha" },
                { "intake_id": 102, "title": "Beta" },
            ],
            "total": 2,
            "truncated": false,
        });
        let table = format_paper_list(&response);
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
    /// Both branches are asserted from the same response, because
    /// what discriminates is that they disagree: an implementation
    /// that projected the id onto the response before choosing a
    /// branch would satisfy the human half and put `paper:101` on the
    /// wire shape a script reads. `--json` is documented against
    /// `library.list_papers`, and a key the CLI invented would make
    /// that document wrong.
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
}
