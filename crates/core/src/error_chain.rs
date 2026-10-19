// SPDX-License-Identifier: Apache-2.0

//! Source-chain formatting for error values crossing a string boundary.

use std::borrow::Cow;
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
///
/// The line leaves the process, so the build-machine prefix of any
/// dependency source location in it is removed
/// ([`crate::redact_source_paths`]).
pub fn error_chain(err: &(dyn Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut cursor = err.source();
    while let Some(cause) = cursor {
        let _ = write!(out, ": {cause}");
        cursor = cause.source();
    }
    match crate::redact_source_paths(&out) {
        Cow::Borrowed(_) => out,
        Cow::Owned(redacted) => redacted,
    }
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

    /// A source deep in the chain whose text carries a dependency source
    /// location, as a third-party error would.
    #[derive(Debug)]
    struct Located;

    impl std::fmt::Display for Located {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(
                "train failed, /disk/cache/cargo/registry/src/index.crates.io-0123456789abcdef/\
                 dep-1.0.0/src/train.rs:10:4: train failed",
            )
        }
    }

    impl Error for Located {}

    #[derive(Debug)]
    struct Outer(Located);

    impl std::fmt::Display for Outer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("store error")
        }
    }

    impl Error for Outer {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn a_flattened_chain_drops_the_build_prefix_of_a_source_location() {
        assert_eq!(
            error_chain(&Outer(Located)),
            "store error: train failed, dep-1.0.0/src/train.rs:10:4: train failed"
        );
    }

    #[test]
    fn io_error_chain_keeps_the_os_error_text() {
        let io = std::io::Error::from_raw_os_error(24);
        let chained = error_chain(&io);
        assert!(chained.contains("os error 24"), "got: {chained}");
    }
}
