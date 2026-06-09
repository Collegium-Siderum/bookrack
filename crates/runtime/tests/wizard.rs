// SPDX-License-Identifier: Apache-2.0

//! Wizard runner integration tests, driven through a mock driver.
//!
//! Both tests pass `no_smoke = true`, so the only network touchpoint
//! is step 3's `probe_ollama`, which resolves to a `reachable = false`
//! report (never an `Err`) when no daemon listens — the mock driver
//! accepts either outcome, keeping the tests deterministic offline.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use async_trait::async_trait;
use bookrack_config::ROOT_CONFIG_NAME;
use bookrack_runtime::wizard::{
    DataRootHint, FinalizeSummary, OllamaStep, PdfiumReport, SmokeOutcome, Wizard, WizardDriver,
    WizardOpts,
};

static HOME_DIR: OnceLock<tempfile::TempDir> = OnceLock::new();

/// Point the platform config-directory lookup at a tempdir, so the
/// finalize step's registry merge never touches the developer's real
/// registry. `dirs::config_dir()` resolves through `HOME` on macOS and
/// `XDG_CONFIG_HOME` on Linux; both are redirected before any test
/// body reads the environment.
fn isolate_home() {
    HOME_DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("home tempdir");
        // SAFETY: env is mutated exactly once, inside
        // `OnceLock::get_or_init`'s single-initialization guarantee,
        // as the first statement of every test in this binary, before
        // any concurrent env reads.
        unsafe {
            std::env::set_var("HOME", dir.path());
            std::env::set_var("XDG_CONFIG_HOME", dir.path().join("xdg-config"));
        }
        dir
    });
}

#[derive(Default)]
struct MockDriver {
    visited: Mutex<Vec<&'static str>>,
    data_root: PathBuf,
}

#[async_trait]
impl WizardDriver for MockDriver {
    async fn step_data_root(&self, _hint: DataRootHint) -> Result<PathBuf> {
        self.visited.lock().unwrap().push("data_root");
        Ok(self.data_root.clone())
    }
    async fn step_pdfium(&self, _r: &PdfiumReport) -> Result<()> {
        self.visited.lock().unwrap().push("pdfium");
        Ok(())
    }
    async fn step_ollama(&self, _s: &OllamaStep<'_>) -> Result<()> {
        self.visited.lock().unwrap().push("ollama");
        Ok(())
    }
    async fn step_smoke(&self, _o: &SmokeOutcome) -> Result<()> {
        self.visited.lock().unwrap().push("smoke");
        Ok(())
    }
    async fn step_finalize(&self, _s: &FinalizeSummary) -> Result<()> {
        self.visited.lock().unwrap().push("finalize");
        Ok(())
    }
}

#[tokio::test]
async fn five_steps_in_fixed_order_no_smoke() {
    isolate_home();
    let dir = tempfile::tempdir().unwrap();
    let driver = MockDriver {
        data_root: dir.path().to_path_buf(),
        ..Default::default()
    };
    // no_smoke=true so the test never ingests; step_smoke still fires
    // with SmokeOutcome::Skipped.
    let opts = WizardOpts {
        no_smoke: true,
        ..Default::default()
    };
    Wizard::run(&driver, opts).await.unwrap();
    let visited = driver.visited.lock().unwrap().clone();
    assert_eq!(
        visited,
        vec!["data_root", "pdfium", "ollama", "smoke", "finalize"]
    );
}

#[tokio::test]
async fn finalize_writes_config_and_skeleton() {
    isolate_home();
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

    let cfg = dir.path().join(ROOT_CONFIG_NAME);
    assert!(cfg.exists(), "config.toml should be created");
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.contains("ollama_url = "));
    assert!(text.contains("embed_model = "));

    for sub in ["sources", "books", "logs", "audit-rules"] {
        assert!(dir.path().join(sub).is_dir(), "{sub} missing");
    }
}
