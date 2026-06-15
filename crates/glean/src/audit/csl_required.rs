// SPDX-License-Identifier: Apache-2.0

//! Required-field matrix driven by CSL type.
//!
//! Each CSL item type the papers pipeline emits carries its own
//! minimum-viable field set. An `article-journal` needs a container
//! title and a DOI; a `paper-conference` needs a venue; a preprint
//! (`article`) needs an arXiv id or a DOI; a `thesis` needs an
//! institution. Lumping every paper under one flat list mis-grades
//! both directions: it floors clean preprints (no container) and
//! ignores missing essentials on conference papers.
//!
//! The matrix is consulted at audit time after the profile decides
//! whether a given field is in scope at all.

use bookrack_extract::CslType;

/// How essential a field is to the paper, per CSL type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequirementLevel {
    /// Missing the field floors the verdict to `NeedsWork`.
    Required,
    /// Missing the field downgrades it to `Weak` but does not floor
    /// the verdict.
    Recommended,
    /// The field is not graded for this CSL type.
    Optional,
}

/// Look up the requirement level of one field for a given CSL type.
///
/// `csl_type = None` falls back to the generic minimum (title +
/// author + year required), which is what we apply when extraction
/// could not infer a type.
pub fn requirement(csl_type: Option<CslType>, field: &str) -> RequirementLevel {
    use RequirementLevel::*;
    match csl_type {
        Some(CslType::ArticleJournal) => match field {
            "title" | "author" | "year" | "container_title" | "doi" => Required,
            "issn" | "abstract" | "volume" => Recommended,
            _ => Optional,
        },
        Some(CslType::PaperConference) => match field {
            "title" | "author" | "year" | "container_title" => Required,
            "doi" | "abstract" | "page" => Recommended,
            _ => Optional,
        },
        Some(CslType::Chapter) => match field {
            "title" | "author" | "year" | "container_title" => Required,
            "doi" | "page" => Recommended,
            _ => Optional,
        },
        Some(CslType::Thesis) => match field {
            "title" | "author" | "year" | "publisher" => Required,
            "abstract" => Recommended,
            _ => Optional,
        },
        Some(CslType::Report) => match field {
            "title" | "author" | "year" => Required,
            "doi" | "publisher" => Recommended,
            _ => Optional,
        },
        Some(CslType::Book) | Some(CslType::Webpage) => match field {
            "title" | "author" | "year" => Required,
            "publisher" => Recommended,
            _ => Optional,
        },
        None => match field {
            "title" | "author" | "year" => Required,
            "doi" | "abstract" => Recommended,
            _ => Optional,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use RequirementLevel::*;

    #[test]
    fn article_journal_requires_container_and_doi() {
        let t = Some(CslType::ArticleJournal);
        assert_eq!(requirement(t, "title"), Required);
        assert_eq!(requirement(t, "author"), Required);
        assert_eq!(requirement(t, "year"), Required);
        assert_eq!(requirement(t, "container_title"), Required);
        assert_eq!(requirement(t, "doi"), Required);
        assert_eq!(requirement(t, "issn"), Recommended);
        assert_eq!(requirement(t, "abstract"), Recommended);
        assert_eq!(requirement(t, "arxiv_id"), Optional);
    }

    #[test]
    fn paper_conference_requires_venue_not_doi() {
        let t = Some(CslType::PaperConference);
        assert_eq!(requirement(t, "container_title"), Required);
        assert_eq!(requirement(t, "doi"), Recommended);
        assert_eq!(requirement(t, "page"), Recommended);
    }

    #[test]
    fn thesis_requires_publisher_as_institution() {
        let t = Some(CslType::Thesis);
        assert_eq!(requirement(t, "publisher"), Required);
        assert_eq!(requirement(t, "doi"), Optional);
        assert_eq!(requirement(t, "container_title"), Optional);
    }

    #[test]
    fn unknown_csl_type_falls_back_to_generic_minimum() {
        assert_eq!(requirement(None, "title"), Required);
        assert_eq!(requirement(None, "year"), Required);
        assert_eq!(requirement(None, "container_title"), Optional);
        assert_eq!(requirement(None, "doi"), Recommended);
    }
}
