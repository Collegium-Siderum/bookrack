// SPDX-License-Identifier: Apache-2.0

//! [`ActorKind`] — who or what made a recorded change.
//!
//! Every audit row, in both `metadata_audit` and `book_pipeline_audit`,
//! names the kind of actor responsible. The kind is a small closed set
//! pinned by a `CHECK` constraint on each table's `actor_kind` column;
//! the variable part — a model name, an import source — rides a separate
//! free-text `actor_detail` column. Filtering stays reliable on the kind
//! while the detail stays free-form.

/// Who or what is responsible for a recorded change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorKind {
    /// A human operator.
    Human,
    /// A large language model.
    Llm,
    /// A bulk import from an external source.
    Import,
    /// An automated pipeline stage.
    Pipeline,
    /// The system itself — housekeeping with no external trigger.
    System,
}

impl ActorKind {
    /// Every actor kind.
    pub const ALL: [ActorKind; 5] = [
        ActorKind::Human,
        ActorKind::Llm,
        ActorKind::Import,
        ActorKind::Pipeline,
        ActorKind::System,
    ];

    /// The database string form. These five strings are pinned by a
    /// `CHECK` constraint on every audit table's `actor_kind` column.
    pub const fn as_str(self) -> &'static str {
        match self {
            ActorKind::Human => "human",
            ActorKind::Llm => "llm",
            ActorKind::Import => "import",
            ActorKind::Pipeline => "pipeline",
            ActorKind::System => "system",
        }
    }

    /// Parse the database string form, or `None` if unrecognized.
    pub fn from_db_str(s: &str) -> Option<ActorKind> {
        ActorKind::ALL.into_iter().find(|kind| kind.as_str() == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actor_kind_db_strings_round_trip() {
        for kind in ActorKind::ALL {
            assert_eq!(ActorKind::from_db_str(kind.as_str()), Some(kind));
        }
        assert_eq!(ActorKind::from_db_str("not_an_actor"), None);
    }
}
