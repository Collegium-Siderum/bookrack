// SPDX-License-Identifier: Apache-2.0

//! Source-format detection.

use std::path::Path;

/// A recognized source format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Epub,
    Pdf,
    Mobi,
    Azw3,
    Djvu,
    Html,
    Txt,
    Unknown,
}

impl Format {
    /// A short lowercase name, used to report an unsupported format.
    pub fn label(self) -> &'static str {
        match self {
            Format::Epub => "epub",
            Format::Pdf => "pdf",
            Format::Mobi => "mobi",
            Format::Azw3 => "azw3",
            Format::Djvu => "djvu",
            Format::Html => "html",
            Format::Txt => "txt",
            Format::Unknown => "unknown",
        }
    }
}

/// Detect a file's format from its extension. A magic-byte check (zip
/// container + `mimetype` member for EPUB, `%PDF` for PDF) is left to a
/// later round.
pub fn detect(path: &Path) -> Format {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("epub") => Format::Epub,
        Some("pdf") => Format::Pdf,
        Some("mobi") => Format::Mobi,
        Some("azw3") => Format::Azw3,
        Some("djvu" | "djv") => Format::Djvu,
        Some("html" | "htm" | "xhtml") => Format::Html,
        Some("txt") => Format::Txt,
        _ => Format::Unknown,
    }
}
