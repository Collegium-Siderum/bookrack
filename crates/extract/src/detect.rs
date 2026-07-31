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

#[cfg(test)]
impl Format {
    /// Whether [`crate::extract`] has an adapter for this format. The
    /// match mirrors the dispatch in `extract()`; the two change
    /// together.
    fn has_adapter(self) -> bool {
        matches!(
            self,
            Format::Epub | Format::Pdf | Format::Html | Format::Txt
        )
    }
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

/// File extensions [`detect`] maps to a format with an extraction
/// adapter — the authoritative allowlist front ends consult before
/// enqueueing. A path with any other extension fails extraction with
/// `ExtractError::UnsupportedFormat`.
pub const SUPPORTED_EXTENSIONS: &[&str] = &["epub", "pdf", "txt", "html", "htm", "xhtml"];

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn allowlist_matches_adapter_surface() {
        // Every extension `detect` knows, plus representatives of the
        // unknown-extension and no-extension cases. An extension is in
        // `SUPPORTED_EXTENSIONS` exactly when the format it detects to
        // has an adapter.
        let cases = [
            "epub", "pdf", "mobi", "azw3", "djvu", "djv", "html", "htm", "xhtml", "txt", "zip",
            "docx",
        ];
        for ext in cases {
            let path = PathBuf::from(format!("book.{ext}"));
            assert_eq!(
                SUPPORTED_EXTENSIONS.contains(&ext),
                detect(&path).has_adapter(),
                "allowlist and adapter surface disagree on .{ext}",
            );
        }
        assert!(!detect(&PathBuf::from("no-extension")).has_adapter());
    }

    #[test]
    fn detect_is_case_insensitive() {
        assert_eq!(detect(&PathBuf::from("A.EPUB")), Format::Epub);
        assert_eq!(detect(&PathBuf::from("B.Html")), Format::Html);
    }
}
