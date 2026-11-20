// SPDX-License-Identifier: Apache-2.0

//! `bookrack setup`: install the native components this build manages
//! under the per-user data directory, or list which are installed.
//!
//! Host-level and offline of any library: nothing here resolves a data
//! root or talks to a daemon. A running daemon picks an installed
//! component up on its next start.

use std::path::Path;

use bookrack_config::reranker_model_pin::{RERANKER_MODEL_PINS, locate_reranker_model};
use eyre::Result;
use serde_json::{Value, json};

/// One line of JSON for an install step: which component, where the
/// artifact is, and whether this invocation downloaded it.
pub fn install_line(install: &str, path: &Path, downloaded: bool) -> Value {
    json!({
        "install": install,
        "path": path.display().to_string(),
        "downloaded": downloaded,
    })
}

/// Print what one install step did.
fn report(install: &str, path: &Path, downloaded: bool, from_archive: bool, json: bool) {
    if json {
        println!("{}", install_line(install, path, downloaded));
    } else if downloaded || from_archive {
        println!("installed {}", path.display());
    } else {
        println!("{} already installed", path.display());
    }
}

/// Install the pinned PDFium library: from `archive` when given, and
/// otherwise by download unless the library is already in place.
pub async fn install_pdfium(archive: Option<&Path>, json: bool) -> Result<()> {
    let outcome = match archive {
        Some(archive) => bookrack_runtime::pdfium_install::install_pdfium_from_archive(archive)?,
        None => bookrack_runtime::pdfium_install::install_pinned_pdfium().await?,
    };
    report(
        "pdfium",
        &outcome.path,
        outcome.downloaded,
        archive.is_some(),
        json,
    );
    Ok(())
}

/// Install whichever pinned reranker artifacts are missing: the
/// llama-server build, then every pinned model.
pub async fn install_reranker(json: bool) -> Result<()> {
    for outcome in bookrack_runtime::reranker_install::install_reranker().await? {
        report("reranker", &outcome.path, outcome.downloaded, false, json);
    }
    Ok(())
}

/// List each managed component and whether it is installed. Touches no
/// network and writes nothing.
pub fn status(json: bool) -> Result<()> {
    let mut components: Vec<(String, Option<String>)> = vec![
        (
            "pdfium".to_string(),
            bookrack_runtime::pdfium_install::installed_pdfium()
                .map(|path| path.display().to_string()),
        ),
        (
            "reranker llama-server".to_string(),
            bookrack_config::llama_server_pin::locate_llama_server()
                .path
                .map(|path| path.display().to_string()),
        ),
    ];
    for pin in RERANKER_MODEL_PINS {
        components.push((
            format!("reranker model {}", pin.tag),
            locate_reranker_model(pin.tag)
                .path
                .map(|path| path.display().to_string()),
        ));
    }

    if json {
        let rows: Vec<Value> = components
            .iter()
            .map(|(name, path)| json!({ "component": name, "installed": path.is_some(), "path": path }))
            .collect();
        println!("{}", json!({ "components": rows }));
        return Ok(());
    }
    if crate::render::ctx().is_quiet() {
        return Ok(());
    }
    let mut table = crate::render::table::RowTable::new(["component", "state", "path"]);
    for (name, path) in &components {
        table.push_row([
            name.as_str(),
            if path.is_some() {
                "installed"
            } else {
                "not installed"
            },
            path.as_deref().unwrap_or("-"),
        ]);
    }
    println!("{}", table.render());
    println!("Install with `bookrack setup pdfium` or `bookrack setup reranker`.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_line_names_the_install_its_path_and_whether_it_downloaded() {
        let line = install_line("reranker", Path::new("/managed/model.gguf"), false);
        assert_eq!(line["install"], "reranker");
        assert_eq!(line["path"], "/managed/model.gguf");
        assert_eq!(line["downloaded"], false);
        assert_eq!(line.as_object().map(|o| o.len()), Some(3));
    }
}
