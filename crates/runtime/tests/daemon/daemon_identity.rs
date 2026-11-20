// SPDX-License-Identifier: Apache-2.0

//! The daemon's identity on the control plane.
//!
//! `daemon.version`, the `daemon.version` snapshot a subscriber is
//! sent, and `status` report the same six fields with the same values,
//! and each start draws an instance id of its own.

#![cfg(unix)]

use bookrack_core::DaemonIdentity;
use eyre::Result;
use serde_json::{Map, Value};

use crate::common::{build_opts, connect, join_with_deadline, recv, send};
use bookrack_test_support::{ProcessEnv, process_env};

const IDENTITY_KEYS: [&str; 6] = [
    "instance_id",
    "version",
    "pid",
    "runtime_dir",
    "control_sock",
    "started_at",
];

/// The six identity fields of `value`, each of which must be present.
fn identity_fields(value: &Value, surface: &str) -> Map<String, Value> {
    IDENTITY_KEYS
        .iter()
        .map(|key| {
            let field = value
                .get(key)
                .unwrap_or_else(|| panic!("{surface} carries no `{key}`: {value}"));
            (key.to_string(), field.clone())
        })
        .collect()
}

/// Start a daemon, read its identity off the three surfaces, check they
/// agree with each other and with the runtime's own, and stop it.
async fn identity_of_one_start() -> Result<DaemonIdentity> {
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        false,
    ))
    .await?;
    let own = runtime.identity.clone();
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let expected = own.clone();
    let driver = tokio::spawn(async move {
        let (mut reader, mut writer) = connect(&sock).await?;
        let own_fields = identity_fields(&serde_json::to_value(&expected)?, "the runtime");

        send(
            &mut writer,
            r#"{"jsonrpc":"2.0","id":1,"method":"daemon.version"}"#,
        )
        .await?;
        let version = recv(&mut reader).await?;
        assert_eq!(
            identity_fields(&version["result"], "daemon.version"),
            own_fields
        );

        send(&mut writer, r#"{"jsonrpc":"2.0","id":2,"method":"status"}"#).await?;
        let status = recv(&mut reader).await?;
        assert_eq!(identity_fields(&status["result"], "status"), own_fields);
        for kept in ["state", "queue_pending", "library", "data_dir", "served"] {
            assert!(
                status["result"].get(kept).is_some(),
                "status lost `{kept}`: {status}"
            );
        }

        send(
            &mut writer,
            r#"{"jsonrpc":"2.0","id":3,"method":"events.snapshot","params":{"channels":["daemon.version"]}}"#,
        )
        .await?;
        let snapshot = recv(&mut reader).await?;
        let value = &snapshot["result"]["daemon.version"];
        assert_eq!(identity_fields(value, "the snapshot"), own_fields);

        send(
            &mut writer,
            r#"{"jsonrpc":"2.0","id":4,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await?;

    assert_eq!(own.pid, std::process::id());
    assert_eq!(own.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(own.runtime_dir, runtime_root.path().display().to_string());
    assert_eq!(
        own.control_sock.as_deref(),
        Some(
            runtime_root
                .path()
                .join("control.sock")
                .display()
                .to_string()
                .as_str()
        )
    );
    let instance = uuid::Uuid::parse_str(&own.instance_id)?;
    assert_eq!(instance.get_version_num(), 7, "{}", own.instance_id);
    chrono::DateTime::parse_from_rfc3339(&own.started_at)?;
    Ok(own)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_control_surface_reports_the_same_six_identity_fields() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let first = identity_of_one_start().await?;
    let second = identity_of_one_start().await?;
    assert_ne!(
        first.instance_id, second.instance_id,
        "two starts share an instance id"
    );
    Ok(())
}
