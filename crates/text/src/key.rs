// SPDX-License-Identifier: Apache-2.0

//! Lookup-key folding.

/// Fold a headword or a query into its Latin lookup key: lowercase,
/// then every character that is not alphanumeric removed.
///
/// CJK characters count as alphanumeric, so the fold keeps them and
/// only drops the spaces and punctuation between them.
pub fn latin_key(input: &str) -> String {
    input
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin_key_strips_punctuation_spaces_and_case() {
        assert_eq!(latin_key("Smith, J."), "smithj");
        assert_eq!(latin_key("Foo-Bar"), "foobar");
        assert_eq!(latin_key("Objet a"), "objeta");
        assert_eq!(latin_key("Dasein!"), "dasein");
        assert_eq!(latin_key("  jouissance  "), "jouissance");
    }

    #[test]
    fn latin_key_keeps_cjk_characters() {
        // `char::is_alphanumeric` is true for CJK, so a spaced CJK input
        // folds onto its compact form and a compact one onto itself.
        assert_eq!(latin_key("\u{6C49} \u{5B57}"), "\u{6C49}\u{5B57}");
        assert_eq!(latin_key("\u{6C49}\u{5B57}"), "\u{6C49}\u{5B57}");
    }

    #[test]
    fn latin_key_of_punctuation_alone_is_empty() {
        assert_eq!(latin_key(" -- ! "), "");
    }
}
