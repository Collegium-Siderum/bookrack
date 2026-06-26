// SPDX-License-Identifier: Apache-2.0

//! Effective metadata — the base layer merged with user overrides.
//!
//! `node_publication_attrs` holds attributes as extracted;
//! `node_overrides` holds the user's corrections. Neither alone is the
//! value a consumer should read. The *effective* value is the two
//! merged, computed here in Rust — there is no SQL view (decision D4).
//!
//! A volume additionally inherits a few fields from its parent set; that
//! rule (decision Q2-4.3) lives here as [`EffectiveAttrs::inherit_from`].
//! Walking the parent chain belongs to the caller, since the tree is in
//! `corpus.db`; this module supplies the per-node merge and the
//! inheritance rule it composes with.

use std::collections::BTreeMap;

use bookrack_core::ItemKind;

use crate::node_publication_attrs::PublicationAttrs;
use crate::{Catalog, Result};

/// Fields a volume inherits from its parent set/work when it has none of
/// its own, per decision Q2-4.3. A multi-volume set shares one publisher
/// and is one series, so those carry down; per-volume fields — title,
/// ISBN, and the rest — do not, and are deliberately absent here.
const INHERITABLE_FIELDS: &[&str] = &["publisher", "series"];

/// The bibliographic fields a curator may override. This is the
/// validation set for the metadata write surface: `metadata.set`
/// rejects any field name outside it, so a typo cannot create an
/// override row no consumer will ever read.
///
/// The names match the `node_publication_attrs` columns surfaced by
/// [`base_pairs`], minus the pipeline-owned bookkeeping columns
/// (`source_format`, `source`, `confidence`, `audit_verdict`,
/// `enriched_by`) — overriding those would forge provenance the audit
/// machinery relies on. The six paper-side discrete columns (doi,
/// arxiv_id, issn, container_title, abstract_text, csl_type) are
/// included so a curator can correct DOI typos and the like; the
/// opaque `extras_json` blob is deliberately excluded — the editing
/// surface only accepts discrete fields. A test pins the lists
/// together.
pub const EDITABLE_FIELDS: &[&str] = &[
    "title",
    "subtitle",
    "publisher",
    "year",
    "publication_date",
    "isbn",
    "series",
    "series_number",
    "edition",
    "language",
    "pub_place",
    "original_title",
    "original_language",
    "original_year",
    "doi",
    "arxiv_id",
    "issn",
    "container_title",
    "abstract_text",
    "csl_type",
];

/// The effective metadata of one node: its base-layer attributes with
/// the user's overrides applied.
///
/// A field present in the view has that effective value. An absent field
/// has none — either nothing set it, or an override deliberately
/// nullified it; a consumer treats both as "no value", so the view does
/// not distinguish them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveAttrs {
    /// The book whose node these attributes describe.
    pub intake_id: i64,
    /// The logical address of the node within the book's partition.
    pub scope: String,
    /// Effective field values, keyed by field name.
    fields: BTreeMap<String, String>,
}

impl EffectiveAttrs {
    /// The effective value of `field`, or `None` if it has none.
    pub fn get(&self, field: &str) -> Option<&str> {
        self.fields.get(field).map(String::as_str)
    }

    /// The effective (field, value) pairs, in field-name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.fields.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Return a copy with this node's *inheritable* unset fields filled
    /// in from `parent`.
    ///
    /// Applies the volume→set rule (Q2-4.3): for each field in
    /// [`INHERITABLE_FIELDS`], if this node has no effective value but
    /// the parent does, the parent's value is taken. Fields this node
    /// already sets, and non-inheritable fields, are left untouched.
    /// Inheritance up a multi-level tree is the caller's loop: walk from
    /// the root down, each node inheriting from its already-merged parent.
    pub fn inherit_from(&self, parent: &EffectiveAttrs) -> EffectiveAttrs {
        let mut merged = self.clone();
        for &field in INHERITABLE_FIELDS {
            if !merged.fields.contains_key(field)
                && let Some(value) = parent.fields.get(field)
            {
                merged.fields.insert(field.to_string(), value.clone());
            }
        }
        merged
    }
}

/// The base-layer (field, value) pairs of a row, skipping unset fields.
///
/// The struct is destructured exhaustively, so a new column on
/// [`PublicationAttrs`] fails to compile here until it is given a field
/// name — the same name `node_overrides.field` would use to override it.
fn base_pairs(attrs: &PublicationAttrs) -> Vec<(&'static str, String)> {
    let PublicationAttrs {
        intake_id: _,
        scope: _,
        title,
        subtitle,
        publisher,
        year,
        publication_date,
        isbn,
        series,
        series_number,
        edition,
        language,
        pub_place,
        original_title,
        original_language,
        original_year,
        source_format,
        source,
        confidence,
        audit_verdict,
        enriched_by,
        doi,
        arxiv_id,
        issn,
        container_title,
        abstract_text,
        csl_type,
        // The paper extras blob is intentionally not surfaced through the
        // effective view: it is an opaque JSON passthrough, not a
        // field-level override target. See `EDITABLE_FIELDS`.
        extras_json: _,
    } = attrs;
    [
        ("title", title),
        ("subtitle", subtitle),
        ("publisher", publisher),
        ("year", year),
        ("publication_date", publication_date),
        ("isbn", isbn),
        ("series", series),
        ("series_number", series_number),
        ("edition", edition),
        ("language", language),
        ("pub_place", pub_place),
        ("original_title", original_title),
        ("original_language", original_language),
        ("original_year", original_year),
        ("source_format", source_format),
        ("source", source),
        ("confidence", confidence),
        ("audit_verdict", audit_verdict),
        ("enriched_by", enriched_by),
        ("doi", doi),
        ("arxiv_id", arxiv_id),
        ("issn", issn),
        ("container_title", container_title),
        ("abstract_text", abstract_text),
        ("csl_type", csl_type),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.as_ref().map(|v| (name, v.clone())))
    .collect()
}

impl Catalog {
    /// Compute the effective metadata of one node: its base-layer
    /// attributes with the user's overrides applied.
    ///
    /// An override with a value replaces the base value; an override
    /// that is an explicit NULL removes the field; a field with no
    /// override keeps its base value. A stored override row whose field
    /// has no base column is still applied — rows that predate the
    /// [`EDITABLE_FIELDS`] validation on the write surface remain
    /// readable until cleared.
    ///
    /// This does *not* apply volume→set inheritance; compose
    /// [`EffectiveAttrs::inherit_from`] for that, since the parent is
    /// reached through the `corpus.db` tree the caller holds.
    pub fn effective_publication_attrs(
        &self,
        intake_id: i64,
        kind: ItemKind,
    ) -> Result<EffectiveAttrs> {
        let mut fields: BTreeMap<String, String> = BTreeMap::new();
        if let Some(base) = self.publication_attrs(intake_id, kind)? {
            for (name, value) in base_pairs(&base) {
                fields.insert(name.to_string(), value);
            }
        }
        for over in self.overrides_for_address(intake_id, kind)? {
            match over.value {
                Some(value) => {
                    fields.insert(over.field, value);
                }
                None => {
                    fields.remove(&over.field);
                }
            }
        }
        Ok(EffectiveAttrs {
            intake_id,
            scope: kind.as_scope_str().to_string(),
            fields,
        })
    }

    /// Compute the effective metadata of many nodes at `scope = kind` in
    /// two queries — one over the base layer, one over the overrides —
    /// then merge each pair in memory with the same rule as
    /// [`Catalog::effective_publication_attrs`]. Each id in `intake_ids`
    /// appears in the returned map even if its base layer and overrides
    /// are both absent, mapped to an `EffectiveAttrs` with no fields.
    ///
    /// An empty `intake_ids` slice returns an empty map without
    /// touching the database.
    pub fn effective_publication_attrs_for_intakes(
        &self,
        intake_ids: &[i64],
        kind: ItemKind,
    ) -> Result<BTreeMap<i64, EffectiveAttrs>> {
        if intake_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let bases = self.publication_attrs_for_intakes(intake_ids, kind)?;
        let mut overrides = self.overrides_for_addresses(intake_ids, kind)?;
        let scope = kind.as_scope_str().to_string();
        let mut out: BTreeMap<i64, EffectiveAttrs> = BTreeMap::new();
        for &intake_id in intake_ids {
            let mut fields: BTreeMap<String, String> = BTreeMap::new();
            if let Some(base) = bases.get(&intake_id) {
                for (name, value) in base_pairs(base) {
                    fields.insert(name.to_string(), value);
                }
            }
            if let Some(overs) = overrides.remove(&intake_id) {
                for over in overs {
                    match over.value {
                        Some(value) => {
                            fields.insert(over.field, value);
                        }
                        None => {
                            fields.remove(&over.field);
                        }
                    }
                }
            }
            out.insert(
                intake_id,
                EffectiveAttrs {
                    intake_id,
                    scope: scope.clone(),
                    fields,
                },
            );
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_overrides::NewOverride;
    use crate::node_publication_attrs::NewPublicationAttrs;

    /// Two distinct logical addresses on one intake. The pre-A1.3 form
    /// used FRBR-style `work:set` / `work:vol` strings; the tests below
    /// exercise the scope-separation and inherit-from logic, which is
    /// indifferent to the scope's identity — picking the two
    /// [`ItemKind`] variants preserves that intent inside the new type.
    const INTAKE: i64 = 1;
    const KIND_A: ItemKind = ItemKind::Book;
    const KIND_B: ItemKind = ItemKind::Paper;

    /// Write a base layer with a title and publisher for `(intake, kind)`.
    fn seed_base(catalog: &Catalog, intake_id: i64, kind: ItemKind) {
        let mut attrs = NewPublicationAttrs::new(intake_id, kind);
        attrs.title = Some("Base Title".into());
        attrs.publisher = Some("Base Publisher".into());
        catalog.upsert_publication_attrs(&attrs).expect("base");
    }

    /// Bookkeeping columns the pipeline writes and the curator may not:
    /// provenance (`source_format`, `source`, `enriched_by`) and the
    /// audit verdict pair (`confidence`, `audit_verdict`).
    const PIPELINE_FIELDS: &[&str] = &[
        "source_format",
        "source",
        "confidence",
        "audit_verdict",
        "enriched_by",
    ];

    #[test]
    fn editable_and_pipeline_fields_cover_exactly_the_base_columns() {
        // `base_pairs` destructures `PublicationAttrs` exhaustively, so a
        // new column fails to compile there first; this test then forces
        // it to be classified as either curator-editable or
        // pipeline-owned before the write surface accepts or rejects it.
        // `extras_json` is a documented exception: an opaque JSON blob
        // that flows through the row but does not enter `base_pairs` or
        // either of the two lists.
        let attrs = PublicationAttrs {
            intake_id: 1,
            scope: KIND_A.as_scope_str().to_string(),
            title: Some("x".into()),
            subtitle: Some("x".into()),
            publisher: Some("x".into()),
            year: Some("x".into()),
            publication_date: Some("x".into()),
            isbn: Some("x".into()),
            series: Some("x".into()),
            series_number: Some("x".into()),
            edition: Some("x".into()),
            language: Some("x".into()),
            pub_place: Some("x".into()),
            original_title: Some("x".into()),
            original_language: Some("x".into()),
            original_year: Some("x".into()),
            source_format: Some("x".into()),
            source: Some("x".into()),
            confidence: Some("x".into()),
            audit_verdict: Some("x".into()),
            enriched_by: Some("x".into()),
            doi: Some("x".into()),
            arxiv_id: Some("x".into()),
            issn: Some("x".into()),
            container_title: Some("x".into()),
            abstract_text: Some("x".into()),
            csl_type: Some("x".into()),
            extras_json: Some("x".into()),
        };
        let names: Vec<&str> = base_pairs(&attrs)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        let got: std::collections::BTreeSet<&str> = names.iter().copied().collect();
        let expected: std::collections::BTreeSet<&str> = EDITABLE_FIELDS
            .iter()
            .chain(PIPELINE_FIELDS)
            .copied()
            .collect();
        assert_eq!(got, expected);
        assert_eq!(names.len(), EDITABLE_FIELDS.len() + PIPELINE_FIELDS.len());
        // The opaque blob deliberately stays out of both lists.
        assert!(!EDITABLE_FIELDS.contains(&"extras_json"));
        assert!(!PIPELINE_FIELDS.contains(&"extras_json"));
        assert!(!got.contains("extras_json"));
    }

    #[test]
    fn effective_is_the_base_layer_when_there_are_no_overrides() {
        let catalog = Catalog::open_in_memory().expect("open");
        seed_base(&catalog, INTAKE, KIND_A);

        let eff = catalog
            .effective_publication_attrs(INTAKE, KIND_A)
            .expect("effective");
        assert_eq!(eff.get("title"), Some("Base Title"));
        assert_eq!(eff.get("publisher"), Some("Base Publisher"));
        assert_eq!(eff.get("isbn"), None);

        let pairs: Vec<(&str, &str)> = eff.iter().collect();
        assert!(pairs.contains(&("title", "Base Title")));
    }

    #[test]
    fn the_pre_frbr_pub_place_and_original_year_columns_flow_through_the_view() {
        let catalog = Catalog::open_in_memory().expect("open");
        let mut attrs = NewPublicationAttrs::new(INTAKE, KIND_A);
        attrs.title = Some("Base Title".into());
        attrs.pub_place = Some("New York".into());
        attrs.original_year = Some("1949".into());
        catalog.upsert_publication_attrs(&attrs).expect("base");

        let eff = catalog
            .effective_publication_attrs(INTAKE, KIND_A)
            .expect("effective");
        assert_eq!(eff.get("pub_place"), Some("New York"));
        assert_eq!(eff.get("original_year"), Some("1949"));
    }

    #[test]
    fn an_override_value_replaces_the_base_value() {
        let catalog = Catalog::open_in_memory().expect("open");
        seed_base(&catalog, INTAKE, KIND_A);
        catalog
            .set_override(&NewOverride::new(
                INTAKE,
                KIND_A,
                "title",
                Some("Override Title".into()),
                "human",
            ))
            .expect("override");

        let eff = catalog
            .effective_publication_attrs(INTAKE, KIND_A)
            .expect("effective");
        assert_eq!(eff.get("title"), Some("Override Title"));
    }

    #[test]
    fn an_explicit_null_override_removes_the_field() {
        let catalog = Catalog::open_in_memory().expect("open");
        seed_base(&catalog, INTAKE, KIND_A);
        catalog
            .set_override(&NewOverride::new(
                INTAKE,
                KIND_A,
                "publisher",
                None,
                "human",
            ))
            .expect("nullify");

        let eff = catalog
            .effective_publication_attrs(INTAKE, KIND_A)
            .expect("effective");
        assert_eq!(eff.get("publisher"), None);
        assert_eq!(eff.get("title"), Some("Base Title"));
    }

    #[test]
    fn an_override_can_introduce_a_field_absent_from_the_base() {
        let catalog = Catalog::open_in_memory().expect("open");
        seed_base(&catalog, INTAKE, KIND_A);
        // `imprint` is not a node_publication_attrs column; it rides the
        // EAV override table and still surfaces in the effective view.
        catalog
            .set_override(&NewOverride::new(
                INTAKE,
                KIND_A,
                "imprint",
                Some("An Imprint".into()),
                "human",
            ))
            .expect("override");

        let eff = catalog
            .effective_publication_attrs(INTAKE, KIND_A)
            .expect("effective");
        assert_eq!(eff.get("imprint"), Some("An Imprint"));
    }

    #[test]
    fn a_volume_inherits_only_the_inheritable_fields_from_its_set() {
        let catalog = Catalog::open_in_memory().expect("open");
        // The set carries publisher, series, and its own title and ISBN.
        let mut set_attrs = NewPublicationAttrs::new(INTAKE, KIND_A);
        set_attrs.title = Some("The Whole Set".into());
        set_attrs.publisher = Some("Set Publisher".into());
        set_attrs.series = Some("Set Series".into());
        set_attrs.isbn = Some("set-isbn".into());
        catalog.upsert_publication_attrs(&set_attrs).expect("set");
        // The volume carries only its own title.
        let mut vol_attrs = NewPublicationAttrs::new(INTAKE, KIND_B);
        vol_attrs.title = Some("Volume One".into());
        catalog
            .upsert_publication_attrs(&vol_attrs)
            .expect("volume");

        let set = catalog
            .effective_publication_attrs(INTAKE, KIND_A)
            .expect("set effective");
        let volume = catalog
            .effective_publication_attrs(INTAKE, KIND_B)
            .expect("volume effective");
        let merged = volume.inherit_from(&set);

        // Inheritable and unset on the volume → taken from the set.
        assert_eq!(merged.get("publisher"), Some("Set Publisher"));
        assert_eq!(merged.get("series"), Some("Set Series"));
        // The volume keeps its own title; the set's title does not bleed down.
        assert_eq!(merged.get("title"), Some("Volume One"));
        // ISBN is per-volume and not inheritable — it stays absent.
        assert_eq!(merged.get("isbn"), None);
    }

    #[test]
    fn inheritance_does_not_overwrite_a_volumes_own_value() {
        let catalog = Catalog::open_in_memory().expect("open");
        let mut set_attrs = NewPublicationAttrs::new(INTAKE, KIND_A);
        set_attrs.publisher = Some("Set Publisher".into());
        catalog.upsert_publication_attrs(&set_attrs).expect("set");
        let mut vol_attrs = NewPublicationAttrs::new(INTAKE, KIND_B);
        vol_attrs.publisher = Some("Volume Publisher".into());
        catalog
            .upsert_publication_attrs(&vol_attrs)
            .expect("volume");

        let set = catalog
            .effective_publication_attrs(INTAKE, KIND_A)
            .expect("set");
        let volume = catalog
            .effective_publication_attrs(INTAKE, KIND_B)
            .expect("volume");
        assert_eq!(
            volume.inherit_from(&set).get("publisher"),
            Some("Volume Publisher")
        );
    }

    #[test]
    fn the_six_paper_columns_flow_through_the_view_and_accept_overrides() {
        let catalog = Catalog::open_in_memory().expect("open");
        let mut attrs = NewPublicationAttrs::new(INTAKE, KIND_A);
        attrs.doi = Some("10.5555/synthetic.0001".into());
        attrs.arxiv_id = Some("0000.00000".into());
        attrs.issn = Some("0000-0000".into());
        attrs.container_title = Some("Container Title".into());
        attrs.abstract_text = Some("Synthetic abstract.".into());
        attrs.csl_type = Some("article-journal".into());
        // The blob rides along on the row but never enters the
        // effective view (see `base_pairs` and `EDITABLE_FIELDS`).
        attrs.extras_json = Some("{\"note\":\"ignored\"}".into());
        catalog.upsert_publication_attrs(&attrs).expect("base");

        // Each paper-side discrete column is surfaced by the effective
        // view exactly like the original 14 bibliographic columns.
        let view = catalog
            .effective_publication_attrs(INTAKE, KIND_A)
            .expect("effective");
        assert_eq!(view.get("doi"), Some("10.5555/synthetic.0001"));
        assert_eq!(view.get("arxiv_id"), Some("0000.00000"));
        assert_eq!(view.get("issn"), Some("0000-0000"));
        assert_eq!(view.get("container_title"), Some("Container Title"));
        assert_eq!(view.get("abstract_text"), Some("Synthetic abstract."));
        assert_eq!(view.get("csl_type"), Some("article-journal"));
        // The opaque blob never appears under either key in the view.
        assert_eq!(view.get("extras_json"), None);

        // And each one is overridable — `metadata.set` would route to
        // the same path. Replace each, then read back through the view.
        for (field, value) in [
            ("doi", "10.5555/override"),
            ("arxiv_id", "1111.11111"),
            ("issn", "1111-1111"),
            ("container_title", "Overridden Container"),
            ("abstract_text", "Overridden abstract."),
            ("csl_type", "book"),
        ] {
            catalog
                .set_override(&NewOverride::new(
                    INTAKE,
                    KIND_A,
                    field,
                    Some(value.into()),
                    "human",
                ))
                .expect("override");
        }
        let after = catalog
            .effective_publication_attrs(INTAKE, KIND_A)
            .expect("effective after overrides");
        assert_eq!(after.get("doi"), Some("10.5555/override"));
        assert_eq!(after.get("arxiv_id"), Some("1111.11111"));
        assert_eq!(after.get("issn"), Some("1111-1111"));
        assert_eq!(after.get("container_title"), Some("Overridden Container"));
        assert_eq!(after.get("abstract_text"), Some("Overridden abstract."));
        assert_eq!(after.get("csl_type"), Some("book"));
    }

    #[test]
    fn effective_publication_attrs_for_intakes_empty_input_skips_the_query() {
        let catalog = Catalog::open_in_memory().expect("open");
        let map = catalog
            .effective_publication_attrs_for_intakes(&[], KIND_A)
            .expect("read");
        assert!(map.is_empty());
    }

    #[test]
    fn effective_publication_attrs_for_intakes_matches_single_row_per_intake() {
        let catalog = Catalog::open_in_memory().expect("open");
        seed_base(&catalog, 1, KIND_A);
        seed_base(&catalog, 2, KIND_A);
        catalog
            .set_override(&NewOverride::new(
                1,
                KIND_A,
                "title",
                Some("Override One".into()),
                "human",
            ))
            .expect("override 1.title");
        catalog
            .set_override(&NewOverride::new(2, KIND_A, "publisher", None, "human"))
            .expect("override 2.publisher nullify");
        // Intake 3 has neither base nor overrides on this scope.
        // Intake 4 lives only on the other scope.
        seed_base(&catalog, 4, KIND_B);

        let map = catalog
            .effective_publication_attrs_for_intakes(&[1, 2, 3, 4], KIND_A)
            .expect("read");
        assert_eq!(map.len(), 4);
        let one = map.get(&1).expect("present");
        let single = catalog
            .effective_publication_attrs(1, KIND_A)
            .expect("single");
        assert_eq!(one, &single);
        assert_eq!(one.get("title"), Some("Override One"));
        let two = map.get(&2).expect("present");
        assert_eq!(two.get("publisher"), None, "explicit nullify removes field");
        let three = map.get(&3).expect("present");
        assert_eq!(three.iter().count(), 0);
        let four = map.get(&4).expect("present");
        assert_eq!(
            four.iter().count(),
            0,
            "base lives on the other scope; book-scope batch reads it as empty"
        );
    }
}
