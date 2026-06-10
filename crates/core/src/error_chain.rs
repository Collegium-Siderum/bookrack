// SPDX-License-Identifier: Apache-2.0

//! Source-chain formatting for error values crossing a string boundary.

use std::error::Error;
use std::fmt::Write;

/// Format `err` and every transitive [`Error::source`] as a single
/// `": "`-joined line, e.g. `"vector store error: vectors_meta IO
/// error: Too many open files (os error 24)"`.
///
/// `Display` on a wrapper error prints only the outermost message;
/// persistence boundaries (audit rows, `last_error` columns, queue job
/// errors) that store a plain string must flatten the chain themselves
/// or the root cause is lost. This is the one place that flattening is
/// implemented.
pub fn error_chain(err: &(dyn Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut cursor = err.source();
    while let Some(cause) = cursor {
        let _ = write!(out, ": {cause}");
        cursor = cause.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Leaf;

    impl std::fmt::Display for Leaf {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("root cause")
        }
    }

    impl Error for Leaf {}

    #[derive(Debug)]
    struct Wrapper(Leaf);

    impl std::fmt::Display for Wrapper {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("outer context")
        }
    }

    impl Error for Wrapper {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn single_error_prints_its_display_only() {
        assert_eq!(error_chain(&Leaf), "root cause");
    }

    #[test]
    fn wrapped_error_appends_each_source() {
        assert_eq!(error_chain(&Wrapper(Leaf)), "outer context: root cause");
    }

    #[test]
    fn io_error_chain_keeps_the_os_error_text() {
        let io = std::io::Error::from_raw_os_error(24);
        let chained = error_chain(&io);
        assert!(chained.contains("os error 24"), "got: {chained}");
    }
}
