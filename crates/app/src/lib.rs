// SPDX-License-Identifier: Apache-2.0

//! Tauri shell that hosts the bookrack daemon in-process. The window
//! is a logo panel for now; the daemon's control socket is the real
//! surface — terminal `bookrack` subcommands attach to it as usual.

use std::sync::Arc;

use anyhow::Result;
use tauri::{Manager, RunEvent, WindowEvent};

mod launch;
mod runtime_handle;
mod tray;

use bookrack_config::{LibrarySelection, LogConfig};
use bookrack_runtime::{DaemonRuntime, LaunchMode, RuntimeOpts};
use launch::handle_gui_second_launch;
use runtime_handle::RuntimeHandle;

pub fn run() -> Result<()> {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(rt) = app.try_state::<RuntimeHandle>() {
                rt.0.tray_focus_signal.notify_one();
            }
        }))
        .setup(|app| {
            // Keep the process out of the macOS dock: tray-resident
            // app, no bundle yet, so the Info.plist `LSUIElement` key
            // has nowhere to live and the policy is set at runtime.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let opts = RuntimeOpts {
                    selection: LibrarySelection::default(),
                    runtime_dir: None,
                    mcp_addr: None,
                    no_mcp: false,
                    spawn_queue_worker: true,
                    log_config: LogConfig::from_env(),
                    caller: bookrack_ops::Caller::gui(),
                    mcp_tools: bookrack_mcp::list_tools(),
                    launch_mode: LaunchMode::Gui,
                };
                let runtime = match DaemonRuntime::start(opts).await {
                    Ok(rt) => rt,
                    Err(err) if bookrack_session::is_lock_conflict(&err) => {
                        handle_gui_second_launch().await;
                        handle.exit(0);
                        return;
                    }
                    Err(other) => {
                        eprintln!("bookrack-app: daemon start failed: {other:#}");
                        handle.exit(1);
                        return;
                    }
                };
                let ctx = runtime.method_context.clone();
                handle.manage(RuntimeHandle(ctx.clone()));
                tray::install(&handle, ctx.clone());
                spawn_tray_focus_consumer(handle.clone(), Arc::clone(&ctx.tray_focus_signal));

                // run_until_shutdown consumes the runtime by value and
                // owns the drain logic (worker / MCP / accept-loop join
                // with timeout). The foreground handle mirrors the
                // CLI's headless path: an async task resolving on the
                // shutdown broadcast, so no blocking thread outlives
                // the drain and stalls runtime teardown.
                let mcp_handle = bookrack_mcp::spawn_listener(&runtime);
                let mut shutdown_rx = runtime.shutdown_tx.subscribe();
                let fg_handle = tokio::spawn(async move {
                    let _ = shutdown_rx.recv().await;
                    anyhow::Ok(())
                });
                if let Err(err) = runtime.run_until_shutdown(mcp_handle, fg_handle).await {
                    eprintln!("bookrack-app: daemon exited with error: {err:#}");
                }
                handle.exit(0);
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())?
        .run(|app_handle, event| {
            if let RunEvent::ExitRequested { .. } = event
                && let Some(rt) = app_handle.try_state::<RuntimeHandle>()
            {
                let _ = rt.0.shutdown_tx.send(());
            }
        });
    Ok(())
}

fn spawn_tray_focus_consumer(handle: tauri::AppHandle, notify: Arc<tokio::sync::Notify>) {
    tauri::async_runtime::spawn(async move {
        loop {
            notify.notified().await;
            if let Some(w) = handle.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }
    });
}
