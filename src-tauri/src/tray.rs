//! System tray: Open / Record now ⇄ Stop recording / status line / Quit.
//! Quit stops any active recording cleanly first; a pending transcription
//! simply resumes on next launch (statuses in the db drive recovery).

use crate::commands;
use crate::state::AppState;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{App, AppHandle, Emitter, Manager, Wry};

pub const TRAY_ID: &str = "witness-tray";

/// Set while the window is shown as a tray flyout: the next focus loss
/// hides it again (see the Focused handler in main.rs).
static POPUP_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// When the flyout appeared — programmatic move/resize during opening must
/// not count as the user "arranging" it.
static POPUP_SHOWN_AT: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);

pub fn take_popup_mode() -> bool {
    POPUP_MODE.swap(false, std::sync::atomic::Ordering::SeqCst)
}

pub fn is_popup_mode() -> bool {
    POPUP_MODE.load(std::sync::atomic::Ordering::SeqCst)
}

/// Convert the flyout into a normal pinned window (controls come back).
pub fn pin_popup(window: &tauri::WebviewWindow) {
    if POPUP_MODE.swap(false, std::sync::atomic::Ordering::SeqCst) {
        let _ = window.emit("tray-popup", false);
    }
}

/// A user-initiated resize/move of the flyout "pins" it into a normal
/// window: blur no longer dismisses it (the resize itself steals focus,
/// which used to close it mid-drag) and the window controls come back.
pub fn maybe_pin_popup(app: &AppHandle) {
    use std::sync::atomic::Ordering;
    if !POPUP_MODE.load(Ordering::SeqCst) {
        return;
    }
    let opening = POPUP_SHOWN_AT
        .lock()
        .unwrap()
        .map(|t| t.elapsed() < std::time::Duration::from_millis(600))
        .unwrap_or(false);
    if opening {
        return;
    }
    POPUP_MODE.store(false, Ordering::SeqCst);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.emit("tray-popup", false);
    }
}

pub struct TrayHandles {
    record: MenuItem<Wry>,
    status: MenuItem<Wry>,
}

fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.emit("tray-popup", false);
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Show the main window as a flyout above the tray click, clamped to the
/// monitor's work area so it never draws under the taskbar.
fn show_popup(app: &AppHandle, click: tauri::PhysicalPosition<f64>) {
    if let Some(window) = app.get_webview_window("main") {
        let size = window
            .outer_size()
            .unwrap_or(tauri::PhysicalSize { width: 460, height: 360 });
        let (w, h) = (size.width as i32, size.height as i32);

        let (mut x, mut y) = (click.x as i32 - w + 20, click.y as i32 - h - 12);
        if let Ok(Some(monitor)) = app.monitor_from_point(click.x, click.y) {
            // Work area excludes the taskbar.
            let area = monitor.work_area();
            let (ax, ay) = (area.position.x, area.position.y);
            let (aw, ah) = (area.size.width as i32, area.size.height as i32);
            x = x.clamp(ax, (ax + aw - w - 8).max(ax));
            y = y.min(ay + ah - h - 8).max(ay);
        }
        *POPUP_SHOWN_AT.lock().unwrap() = Some(std::time::Instant::now());
        let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition { x, y }));
        let _ = window.emit("tray-popup", true);
        POPUP_MODE.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

pub fn build(app: &App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Witness", true, None::<&str>)?;
    let record = MenuItem::with_id(app, "record", "Record now", true, None::<&str>)?;
    let status = MenuItem::with_id(app, "status", "Idle", false, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &sep1, &record, &status, &sep2, &quit])?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(app.default_window_icon().expect("bundled window icon").clone())
        .tooltip("Witness — idle")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main(app),
            "record" => {
                let recording = app
                    .state::<AppState>()
                    .recorder
                    .lock()
                    .unwrap()
                    .is_some();
                let result = if recording {
                    commands::do_stop_recording(app, true)
                } else {
                    commands::do_start_recording(app, "manual").map(|_| ())
                };
                if let Err(e) = result {
                    log::warn!("tray record toggle: {e}");
                }
            }
            "quit" => {
                // Finalize any active recording before exiting.
                if app.state::<AppState>().recorder.lock().unwrap().is_some() {
                    if let Err(e) = commands::do_stop_recording(app, true) {
                        log::error!("stopping recording on quit: {e}");
                    }
                }
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| match event {
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                position,
                ..
            } => {
                show_popup(tray.app_handle(), position);
            }
            TrayIconEvent::DoubleClick { button: MouseButton::Left, position, .. } => {
                show_popup(tray.app_handle(), position);
            }
            _ => {}
        })
        .build(app)?;

    app.manage(TrayHandles { record, status });
    Ok(())
}

/// Reflect recording state in the tray menu, tooltip and icon (red = live).
pub fn update(app: &AppHandle, recording: bool) {
    if let Some(handles) = app.try_state::<TrayHandles>() {
        let _ = handles
            .record
            .set_text(if recording { "Stop recording" } else { "Record now" });
        let _ = handles
            .status
            .set_text(if recording { "● Recording" } else { "Idle" });
    }
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(if recording {
            "Witness — recording"
        } else {
            "Witness — idle"
        }));
        let icon = if recording {
            tauri::image::Image::from_bytes(include_bytes!("../icons/tray-rec.png")).ok()
        } else {
            app.default_window_icon().cloned()
        };
        if let Some(icon) = icon {
            let _ = tray.set_icon(Some(icon));
        }
    }
}
