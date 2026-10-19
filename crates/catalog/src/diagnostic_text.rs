// SPDX-License-Identifier: Apache-2.0

//! Diagnostic text read back from the catalog.
//!
//! Three columns hold failure text — `item_state.last_error`,
//! `item_pipeline_audit.error_message`, `mcp_tool_calls.error_msg` —
//! and a row written before errors were redacted at their source may
//! still carry the build-machine prefix of a dependency source
//! location. Every read of those columns passes through [`redacted`];
//! the stored text is left as it was.

/// `text` with build-machine source prefixes removed
/// ([`bookrack_core::redact_source_paths`]).
pub(crate) fn redacted(text: Option<String>) -> Option<String> {
    text.map(|t| match bookrack_core::redact_source_paths(&t) {
        std::borrow::Cow::Borrowed(_) => t,
        std::borrow::Cow::Owned(redacted) => redacted,
    })
}

/// A failure text as it was stored before redaction, and as it reads
/// back.
#[cfg(test)]
pub(crate) mod fixture {
    /// A failure carrying a dependency location under a build-machine
    /// prefix.
    pub(crate) const STORED: &str = "train failed, /disk/cache/cargo/registry/src/\
                                     index.crates.io-0123456789abcdef/dep-1.0.0/src/train.rs:10:4: \
                                     train failed";
    /// [`STORED`] as every read returns it.
    pub(crate) const READ: &str = "train failed, dep-1.0.0/src/train.rs:10:4: train failed";
}
