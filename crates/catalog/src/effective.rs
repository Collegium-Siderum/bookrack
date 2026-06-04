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

use crate::node_publication_attrs::PublicationAttrs;
use crate::{Catalog, Result};

/// Fields a volume inherits from its parent set/work when it has none of
/// its own, per decision Q2-4.3. A multi-volume set shares one publisher
/// and is one series, so those carry down; per-volume fields — title,
/// ISBN, and the rest — do not, and are deliberately absent here.
const INHERITABLE_FIELDS: &[&str] = &["publisher", "series"];

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
    /// override keeps its base value. An override may also carry a field
    /// the base layer never had — the EAV table is the catch-all for
    /// attributes `node_publication_attrs` has no column for.
    ///
    /// This does *not* apply volume→set inheritance; compose
    /// [`EffectiveAttrs::inherit_from`] for that, since the parent is
    /// reached through the `corpus.db` tree the caller holds.
    pub fn effective_publication_attrs(
        &self,
        intake_id: i64,
        scope: &str,
    ) -> Result<EffectiveAttrs> {
        let mut fields: BTreeMap<String, String> = BTreeMap::new();
        if let Some(base) = self.publication_attrs(intake_id, scope)? {
            for (name, value) in base_pairs(&base) {
                fields.insert(name.to_string(), value);
            }
        }
        for over in self.overrides_for_address(intake_id, scope)? {
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
            scope: scope.to_string(),
            fields,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_overrides::NewOverride;
    use crate::node_publication_attrs::NewPublicationAttrs;

    /// Two logical addresses in one book: a set and one of its volumes.
    const INTAKE: i64 = 1;
    const SET: &str = "work:set";
    const VOL: &str = "work:vol";

    /// Write a base layer with a title and publisher for `(intake, scope)`.
    fn seed_base(catalog: &Catalog, intake_id: i64, scope: &str) {
        let mut attrs = NewPublicationAttrs::new(intake_id, scope);
        attrs.title = Some("Base Title".into());
        attrs.publisher = Some("Base Publisher".into());
        catalog.upsert_publication_attrs(&attrs).expect("base");
    }

    #[test]
    fn effective_is_the_base_layer_when_there_are_no_overrides() {
        let catalog = Catalog::open_in_memory().expect("open");
        seed_base(&catalog, INTAKE, SET);

        let eff = catalog
            .effective_publication_attrs(INTAKE, SET)
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
        let mut attrs = NewPublicationAttrs::new(INTAKE, SET);
        attrs.title = Some("Base Title".into());
        attrs.pub_place = Some("New York".into());
        attrs.original_year = Some("1949".into());
        catalog.upsert_publication_attrs(&attrs).expect("base");

        let eff = catalog
            .effective_publication_attrs(INTAKE, SET)
            .expect("effective");
        assert_eq!(eff.get("pub_place"), Some("New York"));
        assert_eq!(eff.get("original_year"), Some("1949"));
    }

    #[test]
    fn an_override_value_replaces_the_base_value() {
        let catalog = Catalog::open_in_memory().expect("open");
        seed_base(&catalog, INTAKE, SET);
        catalog
            .set_override(&NewOverride::new(
                INTAKE,
                SET,
                "title",
                Some("Override Title".into()),
                "human",
            ))
            .expect("override");

        let eff = catalog
            .effective_publication_attrs(INTAKE, SET)
            .expect("effective");
        assert_eq!(eff.get("title"), Some("Override Title"));
    }

    #[test]
    fn an_explicit_null_override_removes_the_field() {
        let catalog = Catalog::open_in_memory().expect("open");
        seed_base(&catalog, INTAKE, SET);
        catalog
            .set_override(&NewOverride::new(INTAKE, SET, "publisher", None, "human"))
            .expect("nullify");

        let eff = catalog
            .effective_publication_attrs(INTAKE, SET)
            .expect("effective");
        assert_eq!(eff.get("publisher"), None);
        assert_eq!(eff.get("title"), Some("Base Title"));
    }

    #[test]
    fn an_override_can_introduce_a_field_absent_from_the_base() {
        let catalog = Catalog::open_in_memory().expect("open");
        seed_base(&catalog, INTAKE, SET);
        // `imprint` is not a node_publication_attrs column; it rides the
        // EAV override table and still surfaces in the effective view.
        catalog
            .set_override(&NewOverride::new(
                INTAKE,
                SET,
                "imprint",
                Some("An Imprint".into()),
                "human",
            ))
            .expect("override");

        let eff = catalog
            .effective_publication_attrs(INTAKE, SET)
            .expect("effective");
        assert_eq!(eff.get("imprint"), Some("An Imprint"));
    }

    #[test]
    fn a_volume_inherits_only_the_inheritable_fields_from_its_set() {
        let catalog = Catalog::open_in_memory().expect("open");
        // The set carries publisher, series, and its own title and ISBN.
        let mut set_attrs = NewPublicationAttrs::new(INTAKE, SET);
        set_attrs.title = Some("The Whole Set".into());
        set_attrs.publisher = Some("Set Publisher".into());
        set_attrs.series = Some("Set Series".into());
        set_attrs.isbn = Some("set-isbn".into());
        catalog.upsert_publication_attrs(&set_attrs).expect("set");
        // The volume carries only its own title.
        let mut vol_attrs = NewPublicationAttrs::new(INTAKE, VOL);
        vol_attrs.title = Some("Volume One".into());
        catalog
            .upsert_publication_attrs(&vol_attrs)
            .expect("volume");

        let set = catalog
            .effective_publication_attrs(INTAKE, SET)
            .expect("set effective");
        let volume = catalog
            .effective_publication_attrs(INTAKE, VOL)
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
        let mut set_attrs = NewPublicationAttrs::new(INTAKE, SET);
        set_attrs.publisher = Some("Set Publisher".into());
        catalog.upsert_publication_attrs(&set_attrs).expect("set");
        let mut vol_attrs = NewPublicationAttrs::new(INTAKE, VOL);
        vol_attrs.publisher = Some("Volume Publisher".into());
        catalog
            .upsert_publication_attrs(&vol_attrs)
            .expect("volume");

        let set = catalog
            .effective_publication_attrs(INTAKE, SET)
            .expect("set");
        let volume = catalog
            .effective_publication_attrs(INTAKE, VOL)
            .expect("volume");
        assert_eq!(
            volume.inherit_from(&set).get("publisher"),
            Some("Volume Publisher")
        );
    }
}
