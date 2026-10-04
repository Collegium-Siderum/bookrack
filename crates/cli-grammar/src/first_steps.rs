// SPDX-License-Identifier: Apache-2.0

//! The first commands after `bookrack init`, held once so every surface
//! that offers them prints the same list. The setup wizard's closing
//! screen and the root `--help` trailer both render [`FIRST_STEPS`]
//! through [`first_step_lines`]; neither carries a copy of its own.

/// One step of the first run: the invocation after `bookrack `, and a
/// note on where it runs. An empty note renders as none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstStep {
    pub invocation: &'static str,
    pub note: &'static str,
}

/// The first three commands after `bookrack init`, in the order they
/// have to run: a daemon, a book, a search. Every invocation parses
/// against the binary (a test in `crates/cli` holds them to that), and
/// a surface that offers them renders this list rather than a copy.
pub const FIRST_STEPS: &[FirstStep] = &[
    FirstStep {
        invocation: "run",
        note: "terminal 1: the daemon",
    },
    FirstStep {
        invocation: "ingest /path/to/book.epub",
        note: "terminal 2",
    },
    FirstStep {
        invocation: "search \"your question\"",
        note: "",
    },
];

/// The steps as indented lines, `bookrack <invocation>` padded so the
/// notes line up as `# <note>`. A step without a note ends at its
/// invocation, with no padding after it.
pub fn first_step_lines() -> Vec<String> {
    let width = FIRST_STEPS
        .iter()
        .filter(|step| !step.note.is_empty())
        .map(|step| step.invocation.chars().count())
        .max()
        .unwrap_or(0);
    FIRST_STEPS
        .iter()
        .map(|step| {
            let command = format!("bookrack {}", step.invocation);
            if step.note.is_empty() {
                format!("  {command}")
            } else {
                let pad = width - step.invocation.chars().count();
                format!("  {command}{:pad$}    # {}", "", step.note)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_render_every_step_once_in_order() {
        let lines = first_step_lines();
        assert_eq!(lines.len(), FIRST_STEPS.len());
        for (line, step) in lines.iter().zip(FIRST_STEPS) {
            assert!(
                line.starts_with(&format!("  bookrack {}", step.invocation)),
                "{line:?} does not open with its step {:?}",
                step.invocation
            );
        }
    }

    #[test]
    fn notes_line_up_and_a_missing_note_leaves_no_trace() {
        let lines = first_step_lines();
        let hash_columns: Vec<usize> = lines
            .iter()
            .filter_map(|line| line.find("    # "))
            .collect();
        assert!(
            hash_columns.len() >= 2,
            "need two noted steps to check alignment"
        );
        assert!(
            hash_columns.windows(2).all(|pair| pair[0] == pair[1]),
            "notes start at different columns: {lines:?}"
        );
        for (line, step) in lines.iter().zip(FIRST_STEPS) {
            if step.note.is_empty() {
                assert!(!line.contains('#'), "{line:?} carries a marker for no note");
                assert_eq!(line, line.trim_end(), "{line:?} has trailing padding");
            }
        }
    }
}
