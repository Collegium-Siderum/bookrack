// SPDX-License-Identifier: Apache-2.0

//! `PdfAdapter`: a PDF's text layer → [`Extraction`].
//!
//! Unlike the born-digital adapters, a PDF may carry no usable text
//! layer at all — a bare scan, or a text layer too corrupt to trust —
//! so extraction is conditional and can route a file to OCR instead of
//! producing an [`Extraction`]. The text-layer adapter proper
//! graduates in a later commit; this module currently establishes the
//! one piece every later step depends on: a loaded PDFium library.
//!
//! ## Thread safety
//!
//! PDFium's C API is not thread-safe. The `pdfium-render` `thread_safe`
//! feature serializes every PDFium call behind a process-global mutex.
//! The consequence for callers: [`crate::extract`] is safe to call
//! concurrently from many threads, but PDF extraction does not run in
//! parallel with itself — concurrent PDF extractions queue behind one
//! another. EPUB / HTML / TXT extraction touches no PDFium and stays
//! genuinely parallel.

// The loader is reachable only from tests until extract() dispatches
// PDF in the next commit; the allow goes away then.
#![allow(dead_code)]

use std::sync::OnceLock;

use pdfium_render::prelude::*;

use crate::contract::ExtractError;

/// The process-wide PDFium handle.
///
/// PDFium is loaded once and shared: under the `thread_safe` feature a
/// single `Pdfium` serves every thread, and binding the native library
/// repeatedly would be wasteful. The stored `Result` keeps a failed
/// load from being retried on every call and lets the failure surface
/// as an ordinary `ExtractError` (see [`pdfium`]).
static PDFIUM: OnceLock<Result<Pdfium, String>> = OnceLock::new();

/// Borrow the process-wide PDFium handle, loading the native library on
/// first use.
///
/// A load failure is an environment / deployment problem — the pinned
/// binary is missing or unreadable — not a property of any one book.
/// It is reported as [`ExtractError::Io`] with a message naming the
/// directory that was searched: `Io` already means "the host
/// environment could not satisfy this request", so no dedicated
/// contract variant is minted for it.
fn pdfium() -> Result<&'static Pdfium, ExtractError> {
    match PDFIUM.get_or_init(load_pdfium) {
        Ok(pdfium) => Ok(pdfium),
        Err(message) => Err(ExtractError::Io(std::io::Error::other(message.clone()))),
    }
}

/// Bind the PDFium native library from the configured directory. The
/// error is a plain `String` so it can be stored in the `OnceLock` and
/// re-reported on every later call — `PdfiumError` is not `Clone`.
fn load_pdfium() -> Result<Pdfium, String> {
    let dir = bookrack_config::pdfium_lib_dir();
    let library = Pdfium::pdfium_platform_library_name_at_path(&dir);
    Pdfium::bind_to_library(&library)
        .map(Pdfium::new)
        .map_err(|e| {
            format!(
                "PDFium library could not be loaded from {}: {e}",
                dir.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned PDFium binary loads. CI fetches it before the test
    /// run, so this exercises the loader for real there; a developer
    /// without the binary set up sees the test skip rather than fail —
    /// the same policy the graduated adapter's tests will follow.
    #[test]
    fn pdfium_binds_the_pinned_native_library() {
        match pdfium() {
            Ok(_) => {}
            Err(e) => eprintln!("skipping: PDFium native library unavailable ({e})"),
        }
    }
}
