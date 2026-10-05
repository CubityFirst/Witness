//! The main (root) window: show/hide/minimize/drag/resize through winit and
//! Win32 directly — synchronous, in physical units, independent of the egui
//! pass state — plus the tray-flyout state machine and the `--minimized`
//! start (design §4.3).
//!
//! STUB (F0.2): the signatures are final. The basic window operations use
//! plain winit calls; the minimized-root UI pass, DWM cloaking, the flyout
//! placement/dismissal and the ghost-free hidden start are implemented by
//! the shell unit (U1).

use super::system::RectI;
use super::OwnerHwnd;
use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::Arc;
use std::time::Instant;
use winit::window::Window;

/// The root viewport's OS window.
pub struct MainWindow {
    window: Arc<Window>,
    hwnd: isize,
    ctx: egui::Context,
}

impl MainWindow {
    /// Capture eframe's winit window and its HWND, and give the undecorated
    /// window the shadow and rounded corners a normal Win11 window has.
    pub fn from_creation_context(cc: &eframe::CreationContext<'_>) -> anyhow::Result<MainWindow> {
        let window = cc
            .winit_window()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("eframe did not expose the main window"))?;
        let hwnd = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
            Ok(_) => 0,
            Err(error) => anyhow::bail!("main window has no handle: {error}"),
        };
        #[cfg(windows)]
        {
            use winit::platform::windows::{CornerPreference, WindowExtWindows};
            window.set_undecorated_shadow(true);
            window.set_corner_preference(CornerPreference::Round);
        }
        Ok(MainWindow {
            window,
            hwnd,
            ctx: cc.egui_ctx.clone(),
        })
    }

    /// The raw HWND (0 if unknown).
    pub fn hwnd(&self) -> isize {
        self.hwnd
    }

    /// Owner for native dialogs. Callers go through `UiCtx::dialog_owner`,
    /// which pins the flyout first.
    pub fn owner(&self) -> OwnerHwnd {
        OwnerHwnd(self.hwnd)
    }

    /// Minimize (the caller has already run `will_hide()`).
    pub fn minimize(&self) {
        self.window.set_minimized(true);
    }

    /// Maximize ⇄ restore.
    pub fn toggle_maximize(&self) {
        self.window.set_maximized(!self.window.is_maximized());
    }

    /// Hand the current primary press to the OS move loop (title-bar drag).
    pub fn start_drag(&self) {
        if let Err(error) = self.window.drag_window() {
            log::debug!("window drag failed: {error}");
        }
    }

    /// Hand the current primary press to the OS sizing loop (Aero Snap works).
    pub fn begin_resize(&self, dir: egui::ResizeDirection) {
        use winit::window::ResizeDirection as W;
        let dir = match dir {
            egui::ResizeDirection::North => W::North,
            egui::ResizeDirection::South => W::South,
            egui::ResizeDirection::East => W::East,
            egui::ResizeDirection::West => W::West,
            egui::ResizeDirection::NorthEast => W::NorthEast,
            egui::ResizeDirection::SouthEast => W::SouthEast,
            egui::ResizeDirection::NorthWest => W::NorthWest,
            egui::ResizeDirection::SouthWest => W::SouthWest,
        };
        if let Err(error) = self.window.drag_resize_window(dir) {
            log::debug!("window resize failed: {error}");
        }
    }

    /// Close to tray (the caller has already run `will_hide()`).
    pub fn hide_to_tray(&self) {
        self.window.set_visible(false);
    }

    /// Restore, show and focus (tray click, "Open Witness", second instance).
    pub fn show_normal(&self) {
        self.window.set_minimized(false);
        self.window.set_visible(true);
        self.window.focus_window();
        self.ctx.request_repaint();
    }

    /// Visible on screen (not hidden to the tray). Minimized still counts.
    pub fn is_shown(&self) -> bool {
        self.window.is_visible().unwrap_or(true)
    }

    pub fn is_minimized(&self) -> bool {
        self.window.is_minimized().unwrap_or(false)
    }

    /// While minimized, briefly restore the root (cloaked, not activated) so
    /// one `ui()` pass can declare the captions viewport (design §4.3, R1).
    /// TODO(U1): cloak → SW_SHOWNOACTIVATE → repaint.
    pub fn ensure_ui_pass(&mut self) {}

    /// Undo `ensure_ui_pass` once the overlay exists (or after 2 s).
    /// TODO(U1): SW_SHOWMINNOACTIVE → uncloak.
    pub fn finish_ui_pass(&mut self) {}

    /// True between `ensure_ui_pass` and `finish_ui_pass`; the minimized-edge
    /// detector in `logic()` ignores transitions while it is set.
    pub fn ui_pass_in_progress(&self) -> bool {
        false
    }

    /// Whether the cursor is within the outer window rect ± `margin_px`.
    /// TODO(U1)
    pub fn cursor_within(&self, _margin_px: i32) -> bool {
        false
    }

    /// DWM-cloak (composed but invisible) or uncloak the window.
    /// TODO(U1): DwmSetWindowAttribute(DWMWA_CLOAK).
    pub fn set_cloaked(&self, _cloaked: bool) {}
}

/// What `FlyoutState::tick` asks the app to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlyoutTick {
    /// Focus was lost with the cursor away from the window: the app runs
    /// `will_hide()` and then `hide_to_tray()`.
    pub dismiss: bool,
}

/// Tray flyout: a left click shows the main window anchored above the tray;
/// losing focus dismisses it unless the user grabbed its border ("pin").
#[derive(Debug, Default)]
pub struct FlyoutState {
    mode: bool,
    shown_at: Option<Instant>,
}

impl FlyoutState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Show the window as a flyout for a tray click at physical (`x`, `y`).
    /// TODO(U1): size at the target monitor's DPI, `flyout_position`,
    /// physical `set_outer_position`, flyout mode, re-place check.
    pub fn open(&mut self, window: &MainWindow, _x: f64, _y: f64) {
        self.shown_at = Some(Instant::now());
        window.show_normal();
    }

    /// Leave flyout mode ("Open Witness", second instance).
    pub fn close_mode(&mut self) {
        self.mode = false;
    }

    /// Turn the flyout into a normal window (border grab, drag, dialog).
    pub fn pin(&mut self) {
        self.mode = false;
    }

    /// In flyout mode the window controls are hidden.
    pub fn is_popup(&self) -> bool {
        self.mode
    }

    /// Focus-loss dismissal and pinning; called every `logic()`.
    /// TODO(U1)
    pub fn tick(&mut self, _ctx: &egui::Context, _window: &MainWindow) -> FlyoutTick {
        FlyoutTick::default()
    }
}

/// Flyout placement: above-left of the click, clamped to the work area
/// (all physical pixels). `size_px` is the window's outer size at the
/// target monitor's DPI.
pub fn flyout_position(click: (i32, i32), size_px: (i32, i32), work: RectI) -> (i32, i32) {
    let (w, h) = size_px;
    let x = click.0 - w + 20;
    let y = click.1 - h - 12;
    let x = x.clamp(work.x, (work.x + work.w - w - 8).max(work.x));
    let y = y.min(work.y + work.h - h - 8).max(work.y);
    (x, y)
}

/// `--minimized` start without a ghost frame: eframe force-shows the root
/// after its first frame, so the window is cloaked in the creator and hidden
/// (then uncloaked) right after that first frame.
pub struct StartupVisibility {
    minimized: bool,
}

impl StartupVisibility {
    /// TODO(U1): cloak now when `minimized`.
    pub fn new(minimized: bool, _window: &MainWindow) -> Self {
        StartupVisibility { minimized }
    }

    /// TODO(U1): after the first rendered frame, hide then uncloak.
    pub fn tick(&mut self, _window: &MainWindow, _ctx: &egui::Context) {
        if self.minimized {
            self.minimized = false;
            log::debug!("--minimized start: hidden start not implemented yet (U1)");
        }
    }
}
