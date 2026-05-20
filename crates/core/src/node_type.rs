// SPDX-License-Identifier: Apache-2.0

//! The node-type enumeration.
//!
//! Every corpus node is one of eighteen types, in three groups:
//! organizing nodes form the TOC tree, prose leaves carry searchable
//! body text, and structural leaves carry non-prose page artifacts.
//! The group decides which per-node fields are populated — only prose
//! leaves carry the content hashes, only organizing nodes carry the
//! subtree content signature.

/// The type of a corpus node. Stored in the database as the snake_case
/// string returned by [`NodeType::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeType {
    // Organizing nodes: the TOC tree above the leaves.
    Collection,
    Volume,
    Work,
    Chapter,
    Section,
    Subsection,
    // Prose leaves: searchable body text.
    Paragraph,
    Heading,
    Footnote,
    Quote,
    Poem,
    // Structural leaves: non-prose page artifacts.
    Figure,
    FigureCaption,
    Table,
    Formula,
    Code,
    RunningHeader,
    ImageGarbage,
}

impl NodeType {
    /// Every node type, in declaration order.
    pub const ALL: [NodeType; 18] = [
        NodeType::Collection,
        NodeType::Volume,
        NodeType::Work,
        NodeType::Chapter,
        NodeType::Section,
        NodeType::Subsection,
        NodeType::Paragraph,
        NodeType::Heading,
        NodeType::Footnote,
        NodeType::Quote,
        NodeType::Poem,
        NodeType::Figure,
        NodeType::FigureCaption,
        NodeType::Table,
        NodeType::Formula,
        NodeType::Code,
        NodeType::RunningHeader,
        NodeType::ImageGarbage,
    ];

    /// The database string form.
    pub const fn as_str(self) -> &'static str {
        match self {
            NodeType::Collection => "collection",
            NodeType::Volume => "volume",
            NodeType::Work => "work",
            NodeType::Chapter => "chapter",
            NodeType::Section => "section",
            NodeType::Subsection => "subsection",
            NodeType::Paragraph => "paragraph",
            NodeType::Heading => "heading",
            NodeType::Footnote => "footnote",
            NodeType::Quote => "quote",
            NodeType::Poem => "poem",
            NodeType::Figure => "figure",
            NodeType::FigureCaption => "figure_caption",
            NodeType::Table => "table",
            NodeType::Formula => "formula",
            NodeType::Code => "code",
            NodeType::RunningHeader => "running_header",
            NodeType::ImageGarbage => "image_garbage",
        }
    }

    /// Parse the database string form, or `None` if unrecognized.
    pub fn from_db_str(s: &str) -> Option<NodeType> {
        NodeType::ALL.into_iter().find(|t| t.as_str() == s)
    }

    /// An organizing node — part of the TOC tree above the leaves.
    /// Organizing nodes carry the subtree content signature.
    pub const fn is_organizing(self) -> bool {
        matches!(
            self,
            NodeType::Collection
                | NodeType::Volume
                | NodeType::Work
                | NodeType::Chapter
                | NodeType::Section
                | NodeType::Subsection
        )
    }

    /// A prose leaf — carries searchable body text and the per-leaf
    /// content hashes.
    pub const fn is_prose_leaf(self) -> bool {
        matches!(
            self,
            NodeType::Paragraph
                | NodeType::Heading
                | NodeType::Footnote
                | NodeType::Quote
                | NodeType::Poem
        )
    }

    /// A structural leaf — a non-prose page artifact.
    pub const fn is_structural_leaf(self) -> bool {
        matches!(
            self,
            NodeType::Figure
                | NodeType::FigureCaption
                | NodeType::Table
                | NodeType::Formula
                | NodeType::Code
                | NodeType::RunningHeader
                | NodeType::ImageGarbage
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn all_is_complete_and_distinct() {
        let distinct: HashSet<NodeType> = NodeType::ALL.into_iter().collect();
        assert_eq!(distinct.len(), 18, "ALL has a duplicate or missing variant");
    }

    #[test]
    fn db_string_round_trips() {
        for t in NodeType::ALL {
            assert_eq!(NodeType::from_db_str(t.as_str()), Some(t));
        }
        assert_eq!(NodeType::from_db_str("not_a_type"), None);
        assert_eq!(NodeType::from_db_str(""), None);
    }

    #[test]
    fn every_type_is_in_exactly_one_group() {
        for t in NodeType::ALL {
            let in_group = [t.is_organizing(), t.is_prose_leaf(), t.is_structural_leaf()];
            let count = in_group.iter().filter(|&&g| g).count();
            assert_eq!(count, 1, "{t:?} must belong to exactly one group");
        }
    }

    #[test]
    fn group_spot_checks() {
        assert!(NodeType::Chapter.is_organizing());
        assert!(NodeType::Paragraph.is_prose_leaf());
        // Heading is a prose leaf, not an organizing node — it carries
        // body text and the per-leaf hashes like any other prose leaf.
        assert!(NodeType::Heading.is_prose_leaf());
        assert!(NodeType::Table.is_structural_leaf());
    }
}
