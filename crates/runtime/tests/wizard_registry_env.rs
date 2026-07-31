// SPDX-License-Identifier: Apache-2.0

//! Wizard finalize honours `BOOKRACK_REGISTRY`.
//!
//! Lives in its own test binary because the test pins process-global
//! environment variables; keeping it isolated means the pin can never
//! race another test's environment reads under a threaded test runner.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use bookrack_runtime::wizard::{
    DataRootHint, FinalizeSummary, OllamaStep, PdfiumChoice, PdfiumInstallOutcome, PdfiumReport,
    SmokeOutcome, Wizard, WizardDriver, WizardOpts,
};
use bookrack_test_support::{ProcessEnv, Sandbox, process_env};
use eyre::Result;

/// The pinned registry file name. A file that does not exist is the
/// point of this test — the wizard has to create it — so the registry
/// is a *named* deviation from the sandbox default rather than the
/// default itself.
const PINNED: &str = "pinned-registry.toml";

/// Isolate the process with the registry variable pointing at a file
/// nothing has created.
fn world() -> &'static Sandbox {
    process_env(ProcessEnv::isolated().registry_absent(PINNED))
}

/// Count files named `registry.toml` under `root`, recursively. The
/// pinned registry uses a different filename, so any hit means the
/// wizard fell back to a platform-default path despite the env pin.
fn count_default_registries(root: &Path) -> usize {
    let mut hits = 0;
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            hits += count_default_registries(&path);
        } else if path.file_name().is_some_and(|n| n == "registry.toml") {
            hits += 1;
        }
    }
    hits
}

#[derive(Default)]
struct MockDriver {
    data_root: PathBuf,
    registry: Mutex<Option<PathBuf>>,
}

#[async_trait]
impl WizardDriver for MockDriver {
    async fn step_data_root(&self, _hint: DataRootHint) -> Result<PathBuf> {
        Ok(self.data_root.clone())
    }
    async fn step_pdfium(&self, _r: &PdfiumReport) -> Result<PdfiumChoice> {
        Ok(PdfiumChoice::Continue)
    }
    async fn step_pdfium_install(&self, _o: &PdfiumInstallOutcome) -> Result<()> {
        Ok(())
    }
    async fn step_ollama(&self, _s: &OllamaStep<'_>) -> Result<()> {
        Ok(())
    }
    async fn step_smoke(&self, _o: &SmokeOutcome) -> Result<()> {
        Ok(())
    }
    async fn step_finalize(&self, s: &FinalizeSummary) -> Result<()> {
        *self.registry.lock().unwrap() = s.registry.clone();
        Ok(())
    }
}

#[tokio::test]
async fn finalize_registers_into_the_env_named_registry() {
    let sandbox = world();
    let pinned = sandbox.path().join(PINNED);
    let dir = tempfile::tempdir().unwrap();
    let driver = MockDriver {
        data_root: dir.path().to_path_buf(),
        ..Default::default()
    };
    let opts = WizardOpts {
        no_smoke: true,
        ..Default::default()
    };
    Wizard::run(&driver, opts).await.unwrap();

    assert_eq!(
        driver.registry.lock().unwrap().as_deref(),
        Some(pinned.as_path()),
        "finalize must report the env-named registry"
    );
    let text = std::fs::read_to_string(&pinned).expect("pinned registry should be written");
    assert!(text.contains("[libraries.default]"), "{text}");
    assert!(text.contains(dir.path().to_str().unwrap()), "{text}");
    assert_eq!(
        count_default_registries(&sandbox.home()),
        0,
        "no platform-default registry may appear under the redirected HOME"
    );
}
