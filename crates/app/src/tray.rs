// SPDX-License-Identifier: Apache-2.0

//! Tray menu and tray-icon click routing.

use bookrack_runtime::control::jsonrpc::Request;
use bookrack_runtime::control::methods::{MethodContext, dispatch};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use tauri::tray::{MouseButton, TrayIconEvent};
use tauri::{
    AppHandle, Manager,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
};

const TRAY_OPEN: &str = "tray:open";
const TRAY_QUIT: &str = "tray:quit";

pub fn install(app: &AppHandle, ctx: MethodContext) {
    let menu = build_menu(app);
    let mut builder = TrayIconBuilder::new().menu(&menu).on_menu_event({
        let app = app.clone();
        move |_tray, ev| handle_menu_event(&app, &ctx, ev.id.as_ref())
    });
    // The compile-time-embedded `icons/icon.png` doubles as the tray
    // glyph; `icon_as_template` lets macOS recolour it per menu-bar
    // appearance.
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone()).icon_as_template(true);
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        builder = builder.on_tray_icon_event({
            let app = app.clone();
            move |_tray, ev| handle_tray_event(&app, ev)
        });
    }
    let _ = builder.build(app);
}

fn build_menu(app: &AppHandle) -> Menu<tauri::Wry> {
    let open = MenuItem::with_id(app, TRAY_OPEN, "Open main window", true, None::<&str>).unwrap();
    let quit = MenuItem::with_id(app, TRAY_QUIT, "Quit", true, None::<&str>).unwrap();
    let sep = PredefinedMenuItem::separator(app).unwrap();
    Menu::with_items(app, &[&open, &sep, &quit]).unwrap()
}

fn handle_menu_event(app: &AppHandle, ctx: &MethodContext, id: &str) {
    match id {
        TRAY_OPEN => {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }
        TRAY_QUIT => {
            // Routed through dispatch (not a bare shutdown_tx.send) so
            // daemon.shutdown keeps its event / state-transition
            // semantics identical to the REPL and socket clients.
            let ctx = ctx.clone();
            tauri::async_runtime::spawn(async move {
                let req = Request {
                    jsonrpc: "2.0".to_string(),
                    id: None,
                    method: "daemon.shutdown".to_string(),
                    params: None,
                };
                let _ = dispatch(&req, &ctx).await;
            });
        }
        _ => {}
    }
}

/// macOS / Windows: left click opens the main window. Linux tray
/// libraries vary on whether left-click events even land, so the
/// handler is not installed there and interaction stays menu-only.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn handle_tray_event(app: &AppHandle, event: TrayIconEvent) {
    if let TrayIconEvent::Click {
        button: MouseButton::Left,
        ..
    } = event
        && let Some(w) = app.get_webview_window("main")
    {
        let _ = w.show();
        let _ = w.set_focus();
    }
}
