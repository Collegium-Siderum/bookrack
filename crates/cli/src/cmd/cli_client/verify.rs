//! `bookrack verify` — control-plane wrapper.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;

use super::helpers;

pub async fn run(runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    helpers::call_and_print(&client, "verify.run", Value::Null).await
}
