// SPDX-License-Identifier: Apache-2.0

//! CSL-JSON item model and the two-way conversion between it and the
//! catalog's paper-side columns.
//!
//! The conversion goes through three plain data shapes — [`CslItem`],
//! [`CslName`], [`CslDate`] — modelled after CSL 1.0.2 with a head set
//! of typed fields plus a flattened `other` map. The head set is the
//! columns the catalog stores discretely; the map carries every CSL
//! field the catalog does not have a column for, so a round-trip
//! preserves arbitrary CSL content through the opaque `extras_json`
//! blob.
//!
//! Two adapters bracket the catalog: [`from_catalog`] reads a stored
//! row and rebuilds a `CslItem`; [`split_into_catalog`] decomposes a
//! `CslItem` into a `NewPublicationAttrs` plus a vector of
//! `NewContributor`s ready to write. Both adapters are pure — no
//! `Catalog` handle, no IO — so callers compose them however they like
//! and tests run without a database.

use bookrack_core::ItemKind;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::node_contributors::{NewContributor, NodeContributor};
use crate::node_publication_attrs::{NewPublicationAttrs, PublicationAttrs};

/// A CSL 1.0.2 item. Head fields cover what the catalog stores
/// discretely; everything else rides on [`Self::other`] verbatim.
///
/// `subtitle` is not a CSL 1.0.2 head field, but the catalog stores it
/// as a discrete column, so this struct exposes it at the head so the
/// round-trip preserves it. CSL processors that do not recognise the
/// key simply ignore it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CslItem {
    /// A stable identifier for the item. Filled in by [`from_catalog`]
    /// as `"intake-<intake_id>"` so a citation processor can address
    /// the row.
    pub id: String,
    /// CSL item type, e.g. `"article-journal"` or `"book"`.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none", default)]
    pub item_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub subtitle: Option<String>,
    #[serde(
        rename = "container-title",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub container_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub publisher: Option<String>,
    #[serde(
        rename = "publisher-place",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub publisher_place: Option<String>,
    /// CSL `issued` date — the publication date.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub issued: Option<CslDate>,
    #[serde(rename = "DOI", skip_serializing_if = "Option::is_none", default)]
    pub doi: Option<String>,
    #[serde(rename = "ISBN", skip_serializing_if = "Option::is_none", default)]
    pub isbn: Option<String>,
    #[serde(rename = "ISSN", skip_serializing_if = "Option::is_none", default)]
    pub issn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub edition: Option<String>,
    /// CSL `abstract`. Renamed because `abstract` is a Rust keyword.
    #[serde(rename = "abstract", skip_serializing_if = "Option::is_none", default)]
    pub abstract_text: Option<String>,
    /// CSL free-form `note`. Doubles as the arXiv carrier:
    /// [`from_catalog`] emits `"arXiv: <id>"` when an `arxiv_id` is
    /// present, and [`split_into_catalog`] parses the same prefix back
    /// out into the discrete column.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub note: Option<String>,
    /// CSL `author` list. Empty when no `author` contributors exist.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub author: Vec<CslName>,
    /// CSL `editor` list.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub editor: Vec<CslName>,
    /// CSL `translator` list.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub translator: Vec<CslName>,
    /// Every CSL field outside the head set. Round-trips through the
    /// catalog as opaque text in the `extras_json` column.
    #[serde(flatten, default)]
    pub other: Map<String, Value>,
}

/// A CSL-JSON name. Either `family` + `given` are set (a structured
/// name) or `literal` is set (an institutional / unsplittable name).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CslName {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub given: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub literal: Option<String>,
    /// ORCID iD. CSL 1.0.2 has no canonical place for this, so the
    /// extension key `"ORCID"` is used by convention.
    #[serde(rename = "ORCID", skip_serializing_if = "Option::is_none", default)]
    pub orcid: Option<String>,
}

/// A CSL-JSON date. Carries `date-parts` for structured dates and
/// `raw` / `literal` for opaque ones.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CslDate {
    /// One or two date triples (`[[Y]]`, `[[Y, M]]`, `[[Y, M, D]]`, or
    /// a pair of those for a range).
    #[serde(
        rename = "date-parts",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub date_parts: Option<Vec<Vec<i32>>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub literal: Option<String>,
}

/// Build a [`CslItem`] from a catalog row plus its contributor rows.
///
/// Contributors are grouped by `role`: `"author"` → [`CslItem::author`],
/// `"editor"` → [`CslItem::editor`], `"translator"` →
/// [`CslItem::translator`]. Any other role rides in the head fields if
/// the catalog knows about it, or is dropped otherwise — the catalog's
/// other roles map onto CSL fields the head set does not expose.
pub fn from_catalog(attrs: &PublicationAttrs, contributors: &[NodeContributor]) -> CslItem {
    let other: Map<String, Value> = attrs
        .extras_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| match v {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default();

    let note = attrs.arxiv_id.as_deref().map(|id| format!("arXiv: {id}"));

    let issued = build_issued(attrs);

    let author = contributors_with_role(contributors, "author");
    let editor = contributors_with_role(contributors, "editor");
    let translator = contributors_with_role(contributors, "translator");

    CslItem {
        id: format!("intake-{}", attrs.intake_id),
        item_type: attrs.csl_type.clone(),
        title: attrs.title.clone(),
        subtitle: attrs.subtitle.clone(),
        container_title: attrs.container_title.clone(),
        publisher: attrs.publisher.clone(),
        publisher_place: attrs.pub_place.clone(),
        issued,
        doi: attrs.doi.clone(),
        isbn: attrs.isbn.clone(),
        issn: attrs.issn.clone(),
        language: attrs.language.clone(),
        edition: attrs.edition.clone(),
        abstract_text: attrs.abstract_text.clone(),
        note,
        author,
        editor,
        translator,
        other,
    }
}

/// Decompose a [`CslItem`] into the catalog rows needed to persist it.
///
/// Returns a `NewPublicationAttrs` keyed by `(intake_id, kind)` and one
/// `NewContributor` per CSL name across the author / editor /
/// translator lists, ordered as they appear in the item.
pub fn split_into_catalog(
    item: &CslItem,
    intake_id: i64,
    kind: ItemKind,
) -> (NewPublicationAttrs, Vec<NewContributor>) {
    let mut attrs = NewPublicationAttrs::new(intake_id, kind);
    attrs.title = item.title.clone();
    attrs.subtitle = item.subtitle.clone();
    attrs.publisher = item.publisher.clone();
    attrs.pub_place = item.publisher_place.clone();
    attrs.edition = item.edition.clone();
    attrs.language = item.language.clone();
    attrs.isbn = item.isbn.clone();
    attrs.doi = item.doi.clone();
    attrs.issn = item.issn.clone();
    attrs.container_title = item.container_title.clone();
    attrs.abstract_text = item.abstract_text.clone();
    attrs.csl_type = item.item_type.clone();
    attrs.arxiv_id = item
        .note
        .as_deref()
        .and_then(arxiv_from_note)
        .map(str::to_string);
    let (year, publication_date) = split_issued(item.issued.as_ref());
    attrs.year = year;
    attrs.publication_date = publication_date;
    attrs.extras_json = if item.other.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&item.other).expect("flattened other map serialises"))
    };

    let mut contributors = Vec::new();
    for (role, names) in [
        ("author", &item.author),
        ("editor", &item.editor),
        ("translator", &item.translator),
    ] {
        for (ordinal, name) in names.iter().enumerate() {
            let display = name_display(name);
            let mut new =
                NewContributor::new(intake_id, kind, role, ordinal as i64, "extracted", display);
            if let Some(family) = &name.family {
                new = new.family(family.clone());
            }
            if let Some(given) = &name.given {
                new = new.given(given.clone());
            }
            if let Some(orcid) = &name.orcid {
                new = new.orcid(orcid.clone());
            }
            contributors.push(new);
        }
    }
    (attrs, contributors)
}

fn contributors_with_role(rows: &[NodeContributor], role: &str) -> Vec<CslName> {
    let mut filtered: Vec<&NodeContributor> = rows.iter().filter(|r| r.role == role).collect();
    filtered.sort_by_key(|r| r.ordinal);
    filtered
        .into_iter()
        .map(|r| CslName {
            family: r.family.clone(),
            given: r.given.clone(),
            literal: match (&r.family, &r.given) {
                // A structured name suppresses `literal` so the CSL
                // processor's name renderer drives the output.
                (Some(_), _) | (_, Some(_)) => None,
                _ => Some(r.name.clone()),
            },
            orcid: r.orcid.clone(),
        })
        .collect()
}

fn name_display(name: &CslName) -> String {
    match (name.given.as_deref(), name.family.as_deref()) {
        (Some(g), Some(f)) => format!("{g} {f}"),
        (None, Some(f)) => f.to_string(),
        (Some(g), None) => g.to_string(),
        (None, None) => name.literal.clone().unwrap_or_default(),
    }
}

fn build_issued(attrs: &PublicationAttrs) -> Option<CslDate> {
    if let Some(date) = attrs.publication_date.as_deref()
        && let Some(parts) = parse_iso_date(date)
    {
        return Some(CslDate {
            date_parts: Some(vec![parts]),
            raw: None,
            literal: None,
        });
    }
    if let Some(year) = attrs.year.as_deref()
        && let Ok(y) = year.parse::<i32>()
    {
        return Some(CslDate {
            date_parts: Some(vec![vec![y]]),
            raw: None,
            literal: None,
        });
    }
    None
}

fn split_issued(issued: Option<&CslDate>) -> (Option<String>, Option<String>) {
    let Some(date) = issued else {
        return (None, None);
    };
    if let Some(parts) = date.date_parts.as_deref().and_then(|p| p.first()) {
        let year = parts.first().map(|y| y.to_string());
        let publication_date = match parts.len() {
            3 => Some(format!("{:04}-{:02}-{:02}", parts[0], parts[1], parts[2])),
            _ => None,
        };
        return (year, publication_date);
    }
    if let Some(raw) = date.raw.as_deref()
        && raw.len() >= 4
        && raw[..4].chars().all(|c| c.is_ascii_digit())
    {
        return (Some(raw[..4].to_string()), None);
    }
    (None, None)
}

fn parse_iso_date(date: &str) -> Option<Vec<i32>> {
    let mut parts = date.split('-');
    let y = parts.next()?.parse::<i32>().ok()?;
    let m = parts.next()?.parse::<i32>().ok()?;
    let d = parts.next()?.parse::<i32>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(vec![y, m, d])
}

fn arxiv_from_note(note: &str) -> Option<&str> {
    let rest = note.strip_prefix("arXiv:")?.trim_start();
    if rest.is_empty() { None } else { Some(rest) }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{Catalog, NewIntake};

    /// Round-trip a [`CslItem`] through an in-memory catalog and the
    /// two adapters. Returns the rebuilt item so the test can compare
    /// it to the input.
    fn round_trip(item: &CslItem, expected_intake_id: i64) -> CslItem {
        let mut catalog = Catalog::open_in_memory().expect("open");
        let intake_id = catalog
            .register_intake(&NewIntake::new(format!("sha-{expected_intake_id}")))
            .expect("register")
            .into_intake()
            .intake_id;
        assert_eq!(intake_id, expected_intake_id);
        let (attrs, contributors) = split_into_catalog(item, intake_id, ItemKind::Book);
        catalog.upsert_publication_attrs(&attrs).expect("upsert");
        for new in &contributors {
            catalog.add_contributor(new).expect("contributor");
        }
        let read_attrs = catalog
            .publication_attrs(intake_id, ItemKind::Book)
            .expect("read attrs")
            .expect("present");
        let read_contributors = catalog
            .contributors_for_address(intake_id, ItemKind::Book)
            .expect("read contributors");
        from_catalog(&read_attrs, &read_contributors)
    }

    /// A structured `author` name. ORCID rides via the CSL extension
    /// key, family / given are split, and the literal slot stays empty
    /// so a CSL processor renders the structured form.
    fn author(family: &str, given: &str) -> CslName {
        CslName {
            family: Some(family.into()),
            given: Some(given.into()),
            literal: None,
            orcid: None,
        }
    }

    #[test]
    fn an_article_journal_with_a_doi_round_trips_through_the_catalog() {
        let mut other = Map::new();
        other.insert("volume".to_string(), Value::String("42".to_string()));
        other.insert("issue".to_string(), Value::String("3".to_string()));
        other.insert("page".to_string(), Value::String("101-128".to_string()));
        let item = CslItem {
            id: "intake-1".to_string(),
            item_type: Some("article-journal".into()),
            title: Some("Synthetic Findings in Test Spaces".into()),
            container_title: Some("Journal of Synthetic Studies".into()),
            publisher: Some("Synthetic Press".into()),
            publisher_place: Some("Nowhere".into()),
            issued: Some(CslDate {
                date_parts: Some(vec![vec![2020, 1, 15]]),
                ..CslDate::default()
            }),
            doi: Some("10.5555/synthetic.0001".into()),
            issn: Some("0000-0000".into()),
            language: Some("en".into()),
            abstract_text: Some("Synthetic abstract.".into()),
            author: vec![author("Author", "First"), author("Author", "Second")],
            other,
            ..CslItem::default()
        };
        let back = round_trip(&item, 1);
        assert_eq!(back, item);
    }

    #[test]
    fn a_conference_paper_carries_its_arxiv_id_through_the_note_round() {
        let item = CslItem {
            id: "intake-1".to_string(),
            item_type: Some("paper-conference".into()),
            title: Some("Yet Another Synthetic Paper".into()),
            container_title: Some("Proceedings of the Synthetic Conference".into()),
            issued: Some(CslDate {
                date_parts: Some(vec![vec![2021]]),
                ..CslDate::default()
            }),
            doi: Some("10.5555/synthetic.0002".into()),
            note: Some("arXiv: 2101.00001".into()),
            author: vec![CslName {
                family: Some("Third".into()),
                given: Some("Author".into()),
                literal: None,
                orcid: Some("0000-0001-2345-6789".into()),
            }],
            ..CslItem::default()
        };
        let back = round_trip(&item, 1);
        // The arxiv_id is reconstructed from the same note prefix and
        // the ORCID rides through the extension key on `CslName`.
        assert_eq!(back, item);
    }

    #[test]
    fn a_book_with_a_literal_name_round_trips() {
        let item = CslItem {
            id: "intake-1".to_string(),
            item_type: Some("book".into()),
            title: Some("A Synthetic Treatise".into()),
            subtitle: Some("On Round-Tripping".into()),
            publisher: Some("Synthetic Press".into()),
            publisher_place: Some("Nowhere".into()),
            issued: Some(CslDate {
                date_parts: Some(vec![vec![2019]]),
                ..CslDate::default()
            }),
            isbn: Some("978-0-00-000000-0".into()),
            language: Some("en".into()),
            author: vec![CslName {
                family: None,
                given: None,
                literal: Some("Anonymous Society".into()),
                orcid: None,
            }],
            editor: vec![author("Editor", "Lead")],
            ..CslItem::default()
        };
        let back = round_trip(&item, 1);
        assert_eq!(back, item);
    }
}
