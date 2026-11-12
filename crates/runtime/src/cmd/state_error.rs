// SPDX-License-Identifier: Apache-2.0

//! A store state a write command will not act on.
//!
//! [`CmdInputError`](crate::cmd::input_error::CmdInputError) covers a
//! refusal the caller resolves by changing what it passed. The refusals
//! here are resolved by the operator bringing the library into a state
//! the command can work on: resending the call with other parameters
//! cannot help, so the control plane maps them onto the state code
//! rather than onto invalid params.

use bookrack_core::{Explain, Problem};

/// A library state a write command refuses. Every variant names the
/// command that brings the library out of it.
#[derive(Debug, thiserror::Error)]
pub enum CmdStateError {
    /// The corpus is stamped with a vector dimension and the chunks
    /// table those vectors live in is not on disk.
    #[error("vector table is missing from the library")]
    VectorTableMissing {
        /// The dimension the corpus stamp records.
        dim: usize,
        /// The command that re-embeds the library and writes the table.
        reset_command: &'static str,
    },
}

impl Explain for CmdStateError {
    fn explain(&self) -> Problem {
        match self {
            CmdStateError::VectorTableMissing { dim, reset_command } => {
                Problem::new("vector table is missing from the library")
                    .detail(format!(
                        "The corpus records vectors of dimension {dim}, and the vector store \
                         holds no chunks table."
                    ))
                    .hint(format!(
                        "Run `{reset_command}` to re-embed the library and write the table again."
                    ))
            }
        }
    }
}
