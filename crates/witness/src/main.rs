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

fn native_options(renderer: RendererChoice) -> eframe::NativeOptions {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Witness")
        .with_inner_size([460.0, 620.0])
        .with_min_inner_size([460.0, 360.0])
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
