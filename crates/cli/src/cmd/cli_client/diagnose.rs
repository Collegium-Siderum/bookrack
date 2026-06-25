//! `bookrack diagnose` — control-plane wrapper.

use std::path::PathBuf;

use eyre::Result;
use serde_json::json;

use super::helpers;

pub async fn run(
    out: Option<PathBuf>,
    days: u32,
    no_scrub: bool,
    runtime_dir: Option<PathBuf>,
) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    let params = json!({
        "out": out,
        "days": days,
        "no_scrub": no_scrub,
    });
    helpers::call_and_print(&client, "diagnose.run", params).await
}
