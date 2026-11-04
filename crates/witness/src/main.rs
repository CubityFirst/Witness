//! `witness.exe`: the native UI + Windows shell around `witness_core`.
//!
//! Start-up order (design §4.1): panic hook → UI-thread mark → single
//! instance → settings → `AppCore::start` (recovery, watcher and pipeline
//! run even if the window has trouble) → `run_native` with an explicit Glow
//! renderer, retried once on wgpu/DX12 if Glow failed before the app creator
//! ran (design §1.2).

// Release builds have no console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod bridge;
mod model;
mod panic;
mod platform;
mod settings_store;
mod snapshot;
mod ui;
mod ui_state;

use app::{StartParts, WitnessApp};
use bridge::UiSink;
use eframe::egui;
use platform::single_instance::{self, Acquire};
use platform::system::error_box;
use std::cell::RefCell;
use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use witness_core::notify::WinToastNotifier;
use witness_core::settings::Settings;
use witness_core::AppCore;

/// Which renderer a `run_native` attempt uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RendererChoice {
    Glow,
    #[cfg(feature = "wgpu-fallback")]
    Wgpu,
}

fn main() {
    panic::install();
    witness_core::service::mark_ui_thread();

    let args: Vec<OsString> = std::env::args_os().collect();
    let instance_guard = match single_instance::acquire(&args) {
        Acquire::Primary(guard) => guard,
        // The running instance has been asked to show itself.
        Acquire::Secondary => std::process::exit(0),
    };
    let instance_hwnd = instance_guard.raw_hwnd();
    panic::register_instance(instance_guard.raw_mutex(), instance_hwnd);

    let settings = match Settings::load_result() {
        Ok(settings) => settings,
        Err(error) => {
            // The box pumps messages: the instance window must refuse a
            // relaunch (which then waits and takes over) rather than answer
            // "shown" for a process that is about to exit.
            single_instance::begin_exit_raw(instance_hwnd);
            error_box(&format!("Witness could not start: {error}"));
            std::process::exit(1);
        }
    };
    let minimized = args.iter().skip(1).any(|a| a == "--minimized");

    let (sink, rx) = UiSink::new();
    let core = match AppCore::start(settings, Arc::new(sink.clone()), Arc::new(WinToastNotifier)) {
        Ok(core) => core,
        Err(error) => {
            single_instance::begin_exit_raw(instance_hwnd);
            error_box(&format!("Witness could not start: {error:#}"));
            std::process::exit(1);
        }
    };
    panic::register_core(core.clone());

    // The creator may run at most once per process (UiSink's context and the
    // tray's / hotkeys' event handlers are once-cells, and the tray must stay
    // the process's first icon), so its inputs live outside the closure and
    // survive a renderer retry.
    let parts = RefCell::new(Some(StartParts {
        core: core.clone(),
        sink,
        rx,
        instance_guard,
        minimized,
        snapshot: snapshot::config(),
    }));
    let creator_ran = AtomicBool::new(false);

    let (first, fallback) = renderer_plan();
    #[allow(unused_mut)] // reassigned only with the wgpu fallback
    let mut result = run(first, &parts, &creator_ran);
    if let Err(error) = &result {
        if fallback && !creator_ran.load(Ordering::SeqCst) {
            log::warn!("the OpenGL renderer failed ({error}); retrying with Direct3D 12");
            #[cfg(feature = "wgpu-fallback")]
            {
                result = run(RendererChoice::Wgpu, &parts, &creator_ran);
            }
        }
    }

    match result {
        // Normal exits never get here (they `process::exit` from the app);
        // finish cleanly if the event loop ends anyway.
        Ok(()) => {
            log::warn!("the event loop ended unexpectedly; shutting down");
            let shutdown_core = core.clone();
            let _ = std::thread::Builder::new()
                .name("shutdown".into())
                .spawn(move || shutdown_core.shutdown())
                .map(|handle| handle.join());
            log::logger().flush();
            std::process::exit(0);
        }
        Err(error) => {
            log::error!("could not start the window: {error}");
            // Emergency teardown (design §4.6): save any recording, tell the
            // user, exit. The instance mutex stays held until the end: the
            // core's threads run on behind the message box, and a relaunch
            // must not start a second core on the same database (it waits,
            // then takes over once this process is gone). If the app was
            // created, it and the tray are already gone; its leaked guard
            // is released by the exit itself.
            bridge::mark_ui_dead();
            single_instance::begin_exit_raw(instance_hwnd);
            let stopped = core.emergency_stop(Duration::from_secs(10));
            log::logger().flush();
            let saved = if stopped {
                " The recording in progress was saved."
            } else {
                ""
            };
            error_box(&format!(
                "Witness could not start its window: {error}{saved}"
            ));
            if let Some(parts) = parts.borrow_mut().take() {
                parts.instance_guard.release();
            }
            std::process::exit(1);
        }
    }
}

/// (first renderer, whether a wgpu retry is allowed). `WITNESS_RENDERER`
/// = `glow` disables the fallback; `wgpu` forces wgpu.
fn renderer_plan() -> (RendererChoice, bool) {
    let fallback_built = cfg!(feature = "wgpu-fallback");
    match std::env::var("WITNESS_RENDERER").ok().as_deref() {
        Some("glow") => (RendererChoice::Glow, false),
        Some("wgpu") => {
            #[cfg(feature = "wgpu-fallback")]
            {
                (RendererChoice::Wgpu, false)
            }
            #[cfg(not(feature = "wgpu-fallback"))]
            {
                log::info!(
                    "WITNESS_RENDERER=wgpu ignored: built without the wgpu-fallback feature"
                );
                (RendererChoice::Glow, false)
            }
        }
        Some(other) => {
            log::warn!("unknown WITNESS_RENDERER={other:?}; using glow");
            (RendererChoice::Glow, fallback_built)
        }
        None => (RendererChoice::Glow, fallback_built),
    }
}

fn run(
    renderer: RendererChoice,
    parts: &RefCell<Option<StartParts>>,
    creator_ran: &AtomicBool,
) -> eframe::Result {
    log::info!("starting the window ({renderer:?})");
    eframe::run_native(
        "Witness",
        native_options(renderer),
        Box::new(|cc| {
            creator_ran.store(true, Ordering::SeqCst);
            let parts = parts
                .borrow_mut()
                .take()
                .ok_or("the app creator ran twice")?;
            let app = WitnessApp::new(cc, parts)?;
            Ok(Box::new(app))
        }),
    )
}

/// The window's first size in content points: wide enough for the split
/// layout (meetings beside the transcript, `ui::SPLIT_MIN_WIDTH`) with room
/// to spare.
const DEFAULT_CONTENT_SIZE: [f32; 2] = [1040.0, 700.0];
/// Kept clear around a default window clamped to a small work area.
const WORK_AREA_MARGIN: f32 = 32.0;

/// The default inner size and (centred) position, in logical points, for a
/// work area of `work` (logical x, y, w, h). The app zooms by the Windows
/// text-size setting (`text_scale`), so the window grows with it to keep
/// the same content width; a work area too small for that wins (the window
/// then fits it and may show a single column).
fn default_window_rect(text_scale: f32, work: [f32; 4]) -> ([f32; 2], [f32; 2]) {
    let [x, y, w, h] = work;
    let fit = |want: f32, avail: f32| want.min(avail - 2.0 * WORK_AREA_MARGIN).max(380.0);
    let size = [
        fit(DEFAULT_CONTENT_SIZE[0] * text_scale, w),
        fit(DEFAULT_CONTENT_SIZE[1] * text_scale, h),
    ];
    let pos = [
        x + ((w - size[0]) / 2.0).max(0.0),
        y + ((h - size[1]) / 2.0).max(0.0),
    ];
    (size, pos)
}

/// The primary monitor's work area in logical points (x, y, w, h).
fn primary_work_area() -> [f32; 4] {
    let origin = winit::dpi::PhysicalPosition::new(0, 0);
    let area = platform::system::work_area_at(origin);
    let scale = platform::system::dpi_at(origin).max(1) as f32 / 96.0;
    [
        area.x as f32 / scale,
        area.y as f32 / scale,
        area.w as f32 / scale,
        area.h as f32 / scale,
    ]
}

fn native_options(renderer: RendererChoice) -> eframe::NativeOptions {
    let snapshot = snapshot::config();
    let (size, position) = match &snapshot {
        Some(s) => (s.size, None),
        None => {
            let (size, pos) =
                default_window_rect(platform::system::text_scale_factor(), primary_work_area());
            (size, Some(pos))
        }
    };
    let min_size = match &snapshot {
        Some(s) => [s.size[0].min(380.0), s.size[1].min(360.0)],
        None => [380.0, 360.0],
    };
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Witness")
        .with_inner_size(size)
        .with_min_inner_size(min_size);
    if let Some(pos) = position {
        viewport = viewport.with_position(pos);
    }
    let mut viewport = viewport
        .with_decorations(false)
        .with_resizable(true)
        // Nothing drops files on Witness, and winit's OleInitialize for
        // drag-and-drop must never fight the core over the COM apartment.
        .with_drag_and_drop(false);
    match eframe::icon_data::from_png_bytes(include_bytes!("../icons/128x128.png")) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(error) => log::warn!("window icon: {error}"),
    }

    #[allow(unused_mut)] // only the wgpu fallback adjusts it
    let mut options = eframe::NativeOptions {
        viewport,
        // Glow explicitly: with both features on, eframe defaults to wgpu.
        renderer: eframe::Renderer::Glow,
        persist_window: false,
        ..Default::default()
    };
    match renderer {
        RendererChoice::Glow => {}
        #[cfg(feature = "wgpu-fallback")]
        RendererChoice::Wgpu => {
            use eframe::{egui_wgpu, wgpu};
            options.renderer = eframe::Renderer::Wgpu;
            options.wgpu_options.wgpu_setup =
                egui_wgpu::WgpuSetup::CreateNew(egui_wgpu::WgpuSetupCreateNew {
                    power_preference: wgpu::PowerPreference::LowPower,
                    instance_descriptor: wgpu::InstanceDescriptor {
                        backends: wgpu::Backends::DX12,
                        // wgpu-types 30 has no Default impl; WGPU_* env vars still apply.
                        ..wgpu::InstanceDescriptor::new_without_display_handle_from_env()
                    },
                    ..egui_wgpu::WgpuSetupCreateNew::without_display_handle()
                });
        }
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_window_opens_as_a_split() {
        // 1080p at 100 %: the full default, centred in the work area.
        let (size, pos) = default_window_rect(1.0, [0.0, 0.0, 1920.0, 1032.0]);
        assert_eq!(size, DEFAULT_CONTENT_SIZE);
        assert_eq!(pos, [440.0, 166.0]);
        assert!(size[0] >= ui::SPLIT_MIN_WIDTH);
        // Larger text: the window grows so the content width stays a split.
        let (size, _) = default_window_rect(1.25, [0.0, 0.0, 2560.0, 1392.0]);
        assert!(size[0] / 1.25 >= ui::SPLIT_MIN_WIDTH);
    }

    #[test]
    fn a_small_work_area_clamps_the_default_window() {
        let (size, pos) = default_window_rect(1.0, [100.0, 0.0, 1024.0, 728.0]);
        assert_eq!(size, [1024.0 - 64.0, 728.0 - 64.0]);
        assert_eq!(pos, [132.0, 32.0]);
        // Never below the minimum window size.
        assert_eq!(
            default_window_rect(1.0, [0.0, 0.0, 300.0, 300.0]).0,
            [380.0, 380.0]
        );
    }
}
