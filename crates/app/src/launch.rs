// SPDX-License-Identifier: Apache-2.0

//! GUI second-launch path: peek lock -> probe -> tray.focus -> exit 0.
//! Mirrors the `LaunchMode::Gui` arm of the CLI lock-conflict handler.

use std::time::Duration;

use bookrack_control_client::ControlSocket;
use bookrack_runtime::control::{HealthProbe, probe};
use bookrack_session::{peek_lock, resolve_runtime_dir, tty_lock_name};
use serde_json::Value;

pub async fn handle_gui_second_launch() {
    let Ok(runtime_dir) = resolve_runtime_dir(None) else {
        eprintln!("bookrack-app: cannot resolve runtime dir during handoff");
        return;
    };
    let lock_path = runtime_dir.join(tty_lock_name());
    let Ok(Some(lock)) = peek_lock(&lock_path) else {
        eprintln!("bookrack-app: lock file missing during second-launch handoff");
        return;
    };
    match probe(&lock, Duration::from_secs(2)).await {
        HealthProbe::Healthy(_pid, sock) => {
            let socket = ControlSocket::from_path(sock);
            let Ok(client) = bookrack_control_client::connect(&socket).await else {
                eprintln!("bookrack-app: cannot connect to existing daemon");
                return;
            };
            let _ = client.call::<Value>("tray.focus", Value::Null).await;
        }
        HealthProbe::Stale | HealthProbe::Unprobeable => {
            eprintln!(
                "bookrack-app: stale lock at {}. Remove the file and reopen.",
                lock_path.display(),
            );
        }
    }
}
