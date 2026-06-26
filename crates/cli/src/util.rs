// SPDX-License-Identifier: Apache-2.0

//! Cross-cutting helpers shared by the `cmd/*` modules.
//!
//! Destructive-action confirmation lives in
//! [`bookrack_cli::render::confirm`]; reach for `confirm_destructive`
//! with `ConfirmMode::Soft` or `ConfirmMode::Hard { token }` for any
//! new prompt instead of re-rolling stdin handling.

/// Recursively collect every `.pdf` file under `dir`. The directory is
/// walked breadth-first; unreadable subdirectories are skipped. Used by
/// the paper-side ingest dispatch to expand `--recursive` into the
/// `paths` array `glean.submit` expects.
pub fn collect_pdf_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn is_pdf(p: &std::path::Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false)
    }
    fn visit(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                visit(&p, out);
            } else if is_pdf(&p) {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    visit(dir, &mut out);
    out
}
