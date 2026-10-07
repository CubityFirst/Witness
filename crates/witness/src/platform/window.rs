//! The main (root) window: show/hide/minimize/drag/resize through winit and
//! Win32 directly — synchronous, in physical units, independent of the egui
//! pass state — plus the tray-flyout state machine and the `--minimized`
//! start (design §4.3).
//!
//! Two eframe facts shape this file (eframe 0.36.2 + winit 0.30.13):
//! - A *hidden* root still runs full egui passes, but a *minimized* root
//!   runs only `logic()`, so a deferred viewport (the captions overlay)
//!   cannot be created while minimized. `ensure_ui_pass` restores the root
//!   invisibly (DWM-cloaked, not activated) for the pass that declares it.
//!   Once the overlay exists eframe keeps running the minimized root's
//!   passes ("a visible descendant"), so the overlay keeps updating.
//! - eframe force-shows the root after its first painted frame, so an
//!   autostart (`--minimized`) launch cloaks the window before that frame
//!   and hides it right after (`StartupVisibility`): nothing ever appears.
//!
//! winit tracks the minimized state in its own flags; whenever a raw
//! `ShowWindow` changes it, `set_minimized(actual)` re-syncs those flags
//! (winit reads the real state first, so the call is a no-op on screen).

use super::system::{self, RectI};
use super::OwnerHwnd;
use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::dpi::PhysicalPosition;
use winit::window::Window;

/// Flyout size when the window's own size is unknown (logical px; the
/// Tauri fallback).
const FLYOUT_FALLBACK_SIZE: (f64, f64) = (460.0, 360.0);
/// Focus lost with the cursor this close to the window (physical px) is a
/// border grab, not a click elsewhere: pin instead of dismissing.
const FLYOUT_PIN_MARGIN: i32 = 16;
/// Moves/resizes this long after the flyout opened are the user's (earlier
/// ones are our own placement and Windows' DPI rescale).
const FLYOUT_SETTLE: Duration = Duration::from_millis(600);
/// How long after opening the flyout's rect is compared with the planned one
/// (a DPI change on show can resize it after the first frame).
const FLYOUT_REPLACE_WINDOW: Duration = Duration::from_millis(500);
/// Tolerance of the re-place check (physical px).
const FLYOUT_REPLACE_TOLERANCE: i32 = 2;
/// After `finish_ui_pass` re-minimizes the root, `logic()` sees the
/// minimized edge one or two passes later; it is ours, not the user's.
const UI_PASS_EDGE_GRACE: Duration = Duration::from_millis(500);
/// Poll for eframe's first-frame show every pass for this long after the
/// first `logic()` ...
const STARTUP_SHOW_TIMEOUT: Duration = Duration::from_secs(5);
/// ... and at this pace after that (the window stays cloaked meanwhile).
const STARTUP_LATE_POLL: Duration = Duration::from_secs(1);

/// An internal "restore for one pass" (see [`MainWindow::ensure_ui_pass`]).
#[derive(Debug, Clone, Copy)]
struct UiPass {
    /// The window was maximized before it was minimized; re-minimizing must
    /// keep "restore to maximized".
    restore_to_max: bool,
}

/// The root viewport's OS window.
pub struct MainWindow {
    window: Arc<Window>,
    hwnd: isize,
    ctx: egui::Context,
    /// Set between `ensure_ui_pass` and `finish_ui_pass`. A `Cell` because
    /// `show_normal(&self)` (tray, second instance) may cancel it.
    ui_pass: Cell<Option<UiPass>>,
    /// `finish_ui_pass` just re-minimized the window (edge grace period).
    ui_pass_finished_at: Cell<Option<Instant>>,
    /// A `--minimized` start is still waiting to hide the window (it is
    /// cloaked and `WS_EX_NOACTIVATE` meanwhile). `show_normal` cancels it:
    /// a show the user asked for (tray, second launch) always wins.
    startup_hide: Cell<bool>,
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
            ui_pass: Cell::new(None),
            ui_pass_finished_at: Cell::new(None),
            startup_hide: Cell::new(false),
        })
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
    pub fn start_drag(&self) -> Result<(), String> {
        self.window.drag_window().map_err(|error| {
            log::debug!("window drag failed: {error}");
            error.to_string()
        })
    }

    /// Hand the current primary press to the OS sizing loop (Aero Snap works).
    pub fn begin_resize(&self, dir: egui::ResizeDirection) -> Result<(), String> {
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
        self.window.drag_resize_window(dir).map_err(|error| {
            log::debug!("window resize failed: {error}");
            error.to_string()
        })
    }

    /// Close to tray (the caller has already run `will_hide()`).
    ///
    /// A hidden window is never left minimized: eframe treats a minimized
    /// root as invisible even when hidden, so a hidden *and* minimized root
    /// would run no `ui()` pass at all and the captions overlay could never
    /// open (spike U1-1). A minimized window is restored invisibly first.
    pub fn hide_to_tray(&self) {
        if self.is_minimized() {
            self.set_cloaked(true);
            self.restore_no_activate();
            self.window.set_visible(false);
            self.set_cloaked(false);
        } else {
            self.window.set_visible(false);
        }
    }

    /// Restore, show and focus (tray click, "Open Witness", second instance).
    /// A tray click or a second instance's `AllowSetForegroundWindow` grants
    /// the foreground right that `focus_window` (SetForegroundWindow) needs.
    pub fn show_normal(&self) {
        // The user wants the window now: an internal minimized pass ends
        // here without re-minimizing it.
        if self.ui_pass.take().is_some() {
            self.set_cloaked(false);
        }
        // Likewise a `--minimized` start that has not hidden the window yet
        // (a second launch or tray click during the first frames): it must
        // not hide it again, and the window must be visible and activatable.
        if self.startup_hide.take() {
            imp::set_no_activate(self.hwnd, false);
            self.set_cloaked(false);
            log::info!("--minimized start: shown on request before it was hidden");
        }
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

    /// The outer window rect in physical pixels.
    fn outer_rect(&self) -> Option<RectI> {
        let pos = self.window.outer_position().ok()?;
        let size = self.window.outer_size();
        Some(RectI {
            x: pos.x,
            y: pos.y,
            w: size.width as i32,
            h: size.height as i32,
        })
    }

    /// The outer size in logical pixels at the window's current DPI.
    fn outer_size_logical(&self) -> Option<(f64, f64)> {
        let size = self.window.outer_size();
        let scale = self.window.scale_factor();
        (size.width > 0 && size.height > 0 && scale > 0.0)
            .then(|| (size.width as f64 / scale, size.height as f64 / scale))
    }

    /// Move the window's top-left corner (physical pixels; winit, because
    /// `ViewportCommand::OuterPosition` converts with the wrong DPI on
    /// mixed-DPI setups).
    fn set_outer_position(&self, x: i32, y: i32) {
        self.window
            .set_outer_position(winit::dpi::Position::Physical(PhysicalPosition::new(x, y)));
    }

    /// Restore a minimized window without activating it (raw
    /// `SW_SHOWNOACTIVATE`), keeping winit's flags in sync.
    fn restore_no_activate(&self) {
        imp::show_window(self.hwnd, imp::ShowCmd::RestoreNoActivate);
        self.window.set_minimized(false);
    }

    /// While minimized, briefly restore the root (cloaked, not activated) so
    /// one `ui()` pass can declare the captions viewport (design §4.3, R1).
    /// A no-op unless the window is minimized. The app then calls
    /// [`finish_ui_pass`](Self::finish_ui_pass) once the overlay is visible.
    pub fn ensure_ui_pass(&mut self) {
        if self.ui_pass.get().is_some() || !self.is_minimized() {
            return;
        }
        let restore_to_max = imp::restores_to_maximized(self.hwnd);
        self.set_cloaked(true);
        self.restore_no_activate();
        self.ui_pass.set(Some(UiPass { restore_to_max }));
        self.ui_pass_finished_at.set(None);
        log::debug!("restored the minimized window (cloaked) for one UI pass");
        self.ctx.request_repaint();
    }

    /// Undo `ensure_ui_pass` once the overlay exists (or after 2 s):
    /// minimized again without activation, then uncloaked. If the user
    /// brought the window up in the meantime it is left as it is.
    pub fn finish_ui_pass(&mut self) {
        let Some(pass) = self.ui_pass.take() else {
            return;
        };
        if imp::is_foreground(self.hwnd) || !self.is_shown() {
            // Activated by the user (taskbar, Alt-Tab) or hidden to the tray
            // meanwhile: nothing to put back.
            log::debug!("UI pass ended by the user; the window keeps its new state");
        } else {
            imp::show_window(self.hwnd, imp::ShowCmd::MinimizeNoActivate);
            self.window.set_minimized(true);
            if pass.restore_to_max {
                imp::set_restore_to_maximized(self.hwnd);
            }
            self.ui_pass_finished_at.set(Some(Instant::now()));
        }
        self.set_cloaked(false);
    }

    /// True between `ensure_ui_pass` and `finish_ui_pass` (and briefly
    /// after, until `logic()` has seen the re-minimize); the minimized-edge
    /// detector ignores transitions while it is set.
    pub fn ui_pass_in_progress(&self) -> bool {
        self.ui_pass.get().is_some()
            || self
                .ui_pass_finished_at
                .get()
                .is_some_and(|at| at.elapsed() < UI_PASS_EDGE_GRACE)
    }

    /// Whether the cursor is within the outer window rect ± `margin_px`.
    pub fn cursor_within(&self, margin_px: i32) -> bool {
        self.outer_rect()
            .is_some_and(|r| r.contains_with_margin(system::cursor_pos(), margin_px))
    }

    /// Debug snapshot runs: never on screen and never activated.
    pub fn hide_for_snapshot(&self) {
        self.set_cloaked(true);
        imp::set_no_activate(self.hwnd, true);
    }

    /// DWM-cloak (composed but invisible) or uncloak the window.
    pub fn set_cloaked(&self, cloaked: bool) {
        imp::set_cloaked(self.hwnd, cloaked);
    }
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
/// Behaviour from 0.2.x (map-platform §3), placement per design §4.3.
#[derive(Debug, Default)]
pub struct FlyoutState {
    /// Flyout mode: window controls hidden, focus loss dismisses.
    mode: bool,
    shown_at: Option<Instant>,
    /// The tray click the flyout is anchored to (physical px).
    click: (i32, i32),
    /// Where the flyout was placed; checked against the real rect for a
    /// moment after showing (DPI rescale), then cleared.
    expected: Option<RectI>,
    /// The rect at the previous tick (user move/resize detection).
    last_rect: Option<RectI>,
    /// Focus at the previous tick (focus-loss edge).
    was_focused: bool,
}

impl FlyoutState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Show the window as a flyout for a tray click at physical (`x`, `y`):
    /// sized for the click's monitor DPI, placed above-left of the click and
    /// clamped to that monitor's work area.
    pub fn open(&mut self, window: &MainWindow, x: f64, y: f64) {
        let click = (x.round() as i32, y.round() as i32);
        let at = PhysicalPosition::new(click.0, click.1);
        // A minimized window ignores moves until it is restored: restore it
        // invisibly first so the placement applies to the real window.
        let cloaked = window.is_minimized();
        if cloaked {
            window.set_cloaked(true);
            window.restore_no_activate();
        }
        // Size at the *target* monitor's scale: showing the window there
        // makes Windows rescale it (WM_DPICHANGED) to exactly this size.
        let logical = window.outer_size_logical().unwrap_or(FLYOUT_FALLBACK_SIZE);
        let size = size_at_dpi(logical, system::dpi_at(at));
        let (fx, fy) = flyout_position(click, size, system::work_area_at(at));
        window.set_outer_position(fx, fy);

        self.mode = true;
        self.shown_at = Some(Instant::now());
        self.click = click;
        self.expected = Some(RectI {
            x: fx,
            y: fy,
            w: size.0,
            h: size.1,
        });
        self.last_rect = None;
        window.show_normal();
        if cloaked {
            window.set_cloaked(false);
        }
        log::debug!(
            "tray flyout at {fx},{fy} ({}x{}) for a click at {click:?}",
            size.0,
            size.1
        );
    }

    /// Leave flyout mode ("Open Witness", second instance).
    pub fn close_mode(&mut self) {
        self.mode = false;
        self.expected = None;
    }

    /// Turn the flyout into a normal window (border grab, drag, dialog).
    pub fn pin(&mut self) {
        if self.mode {
            log::debug!("tray flyout pinned");
        }
        self.mode = false;
        self.expected = None;
    }

    /// In flyout mode the window controls are hidden.
    pub fn is_popup(&self) -> bool {
        self.mode
    }

    /// Focus-loss dismissal and pinning; called every `logic()`.
    pub fn tick(&mut self, ctx: &egui::Context, window: &MainWindow) -> FlyoutTick {
        // The OS foreground window, not egui's `focused`: a window shown
        // again after a programmatic hide may be active without winit ever
        // reporting `Focused(true)` (spike U1-3), and then the focus-loss
        // edge would never fire. Losing the foreground still sends winit a
        // focus event, so a pass (and this tick) follows every loss.
        let focused = imp::is_foreground(window.hwnd);
        let mut rect = window.outer_rect();
        let mut out = FlyoutTick::default();
        if self.mode {
            let since = self.shown_at.map_or(Duration::MAX, |t| t.elapsed());

            // Re-place once if a DPI change on show resized the window away
            // from the planned rect (mixed-DPI monitors).
            if let Some(expected) = self.expected {
                if since >= FLYOUT_REPLACE_WINDOW {
                    self.expected = None;
                } else {
                    if let Some(actual) =
                        rect.filter(|r| rect_differs(expected, *r, FLYOUT_REPLACE_TOLERANCE))
                    {
                        let at = PhysicalPosition::new(self.click.0, self.click.1);
                        let (fx, fy) = flyout_position(
                            self.click,
                            (actual.w, actual.h),
                            system::work_area_at(at),
                        );
                        window.set_outer_position(fx, fy);
                        log::debug!(
                            "tray flyout re-placed after a rescale: planned {expected:?}, \
                             got {actual:?}, now at {fx},{fy}"
                        );
                        self.expected = None;
                        rect = window.outer_rect();
                    }
                    // Keep ticking until the check window closes.
                    ctx.request_repaint_after(Duration::from_millis(50));
                }
            }

            // Safety net for moves/resizes that bypass our own drag/resize
            // (Win+arrows, the system menu): those pin the flyout too.
            if since > FLYOUT_SETTLE
                && self.last_rect.is_some()
                && rect.is_some()
                && self.last_rect != rect
            {
                self.pin();
            }

            // Focus lost: a border grab (cursor on or next to the window)
            // pins; a click anywhere else dismisses, like the native volume
            // and Wi-Fi flyouts.
            if self.mode && self.was_focused && !focused {
                if window.cursor_within(FLYOUT_PIN_MARGIN) {
                    self.pin();
                } else {
                    log::debug!("tray flyout dismissed (focus lost)");
                    self.mode = false;
                    self.expected = None;
                    out.dismiss = true;
                }
            }
        }
        self.last_rect = rect;
        self.was_focused = focused;
        out
    }
}

/// A logical size converted to physical pixels at `dpi` (96 = 100 %).
fn size_at_dpi(logical: (f64, f64), dpi: u32) -> (i32, i32) {
    let scale = dpi.max(1) as f64 / 96.0;
    (
        (logical.0 * scale).round() as i32,
        (logical.1 * scale).round() as i32,
    )
}

/// True when the position or size of `a` and `b` differ by more than
/// `tolerance` pixels.
fn rect_differs(a: RectI, b: RectI, tolerance: i32) -> bool {
    (a.x - b.x).abs() > tolerance
        || (a.y - b.y).abs() > tolerance
        || (a.w - b.w).abs() > tolerance
        || (a.h - b.h).abs() > tolerance
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
///
/// eframe's show is a plain `SW_SHOW`, which would also *activate* the
/// invisible window and take the keyboard focus from whatever the user is
/// doing while Windows starts. `WS_EX_NOACTIVATE` is set for that one show
/// and cleared again with the hide.
///
/// The pending state lives in [`MainWindow`] (`startup_hide`), so that
/// `show_normal` (tray, second launch) cancels it: the window then stays up.
/// Until eframe's show is seen the window stays cloaked, however long that
/// takes; a cloaked window is neither on screen nor on the taskbar.
pub struct StartupVisibility {
    /// Waiting for eframe's first-frame show.
    pending: bool,
    /// The first `tick`, i.e. the first `logic()` (eframe shows the window
    /// after that pass's paint). The timeout counts from here, not from the
    /// creator: a slow cold start must not run it out before the first frame.
    first_tick: Option<Instant>,
    /// The timeout warning was logged.
    warned: bool,
}

impl StartupVisibility {
    /// With `minimized`, cloak the (not yet shown) window now.
    pub fn new(minimized: bool, window: &MainWindow) -> Self {
        if minimized {
            window.set_cloaked(true);
            imp::set_no_activate(window.hwnd, true);
            window.startup_hide.set(true);
        }
        StartupVisibility {
            pending: minimized,
            first_tick: None,
            warned: false,
        }
    }

    /// After the first rendered frame (eframe has shown the cloaked window),
    /// hide it to the tray and uncloak it.
    pub fn tick(&mut self, window: &MainWindow, ctx: &egui::Context) {
        if !self.pending {
            return;
        }
        if !window.startup_hide.get() {
            // `show_normal` took over: the user asked for the window.
            self.pending = false;
            return;
        }
        let first_tick = *self.first_tick.get_or_insert_with(Instant::now);
        if window.window.is_visible() == Some(true) {
            window.hide_to_tray();
            imp::set_no_activate(window.hwnd, false);
            window.set_cloaked(false);
            window.startup_hide.set(false);
            self.pending = false;
            log::info!("started minimized to the tray");
        } else if first_tick.elapsed() <= STARTUP_SHOW_TIMEOUT {
            ctx.request_repaint();
        } else {
            // Unexpected (eframe always shows the root after its first
            // frame): stay cloaked and keep looking at a slow pace, so a late
            // show is still hidden properly and never flashes.
            if !self.warned {
                self.warned = true;
                log::warn!(
                    "--minimized start: no first-frame show within {STARTUP_SHOW_TIMEOUT:?}; \
                     the window stays cloaked until it is shown"
                );
            }
            ctx.request_repaint_after(STARTUP_LATE_POLL);
        }
    }
}

#[cfg(windows)]
mod imp {
    use windows::core::BOOL;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CLOAK};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowLongPtrW, GetWindowPlacement, SetWindowLongPtrW,
        SetWindowPlacement, ShowWindow, GWL_EXSTYLE, SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE,
        WINDOWPLACEMENT, WPF_RESTORETOMAXIMIZED, WS_EX_NOACTIVATE,
    };

    fn hwnd(raw: isize) -> HWND {
        HWND(raw as *mut _)
    }

    #[derive(Debug, Clone, Copy)]
    pub enum ShowCmd {
        /// `SW_SHOWNOACTIVATE`: restore a minimized window, keep focus away.
        RestoreNoActivate,
        /// `SW_SHOWMINNOACTIVE`: minimize without activating anything.
        MinimizeNoActivate,
    }

    pub fn show_window(raw: isize, cmd: ShowCmd) {
        if raw == 0 {
            return;
        }
        let cmd = match cmd {
            ShowCmd::RestoreNoActivate => SW_SHOWNOACTIVATE,
            ShowCmd::MinimizeNoActivate => SW_SHOWMINNOACTIVE,
        };
        // SAFETY: ShowWindow tolerates any handle (it fails on a stale one);
        // its return value is the previous visibility, not an error.
        let _ = unsafe { ShowWindow(hwnd(raw), cmd) };
    }

    pub fn set_cloaked(raw: isize, cloaked: bool) {
        if raw == 0 {
            return;
        }
        let value = BOOL::from(cloaked);
        // SAFETY: DWMWA_CLOAK reads one BOOL from a live stack value; a
        // process may cloak its own top-level windows.
        let result = unsafe {
            DwmSetWindowAttribute(
                hwnd(raw),
                DWMWA_CLOAK,
                &value as *const BOOL as *const _,
                std::mem::size_of::<BOOL>() as u32,
            )
        };
        if let Err(error) = result {
            log::warn!("DWM cloak({cloaked}) failed: {error}");
        }
    }

    pub fn is_foreground(raw: isize) -> bool {
        // SAFETY: no arguments; returns a handle or null.
        raw != 0 && unsafe { GetForegroundWindow() }.0 as isize == raw
    }

    fn placement(raw: isize) -> Option<WINDOWPLACEMENT> {
        let mut wp = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        // SAFETY: a correctly sized out-parameter.
        unsafe { GetWindowPlacement(hwnd(raw), &mut wp) }
            .ok()
            .map(|_| wp)
    }

    pub fn restores_to_maximized(raw: isize) -> bool {
        raw != 0 && placement(raw).is_some_and(|wp| wp.flags.contains(WPF_RESTORETOMAXIMIZED))
    }

    /// Mark a minimized window to restore maximized again.
    pub fn set_restore_to_maximized(raw: isize) {
        let Some(mut wp) = placement(raw) else { return };
        wp.flags |= WPF_RESTORETOMAXIMIZED;
        wp.showCmd = SW_SHOWMINNOACTIVE.0 as u32;
        // SAFETY: the placement was just read from the same window.
        if let Err(error) = unsafe { SetWindowPlacement(hwnd(raw), &wp) } {
            log::debug!("SetWindowPlacement failed: {error}");
        }
    }

    /// Add or remove `WS_EX_NOACTIVATE`.
    pub fn set_no_activate(raw: isize, on: bool) {
        if raw == 0 {
            return;
        }
        // SAFETY: reading and writing our own window's extended style.
        unsafe {
            let style = GetWindowLongPtrW(hwnd(raw), GWL_EXSTYLE);
            let bit = WS_EX_NOACTIVATE.0 as isize;
            let new = if on { style | bit } else { style & !bit };
            if new != style {
                SetWindowLongPtrW(hwnd(raw), GWL_EXSTYLE, new);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    #[derive(Debug, Clone, Copy)]
    pub enum ShowCmd {
        RestoreNoActivate,
        MinimizeNoActivate,
    }
    pub fn show_window(_raw: isize, _cmd: ShowCmd) {}
    pub fn set_cloaked(_raw: isize, _cloaked: bool) {}
    pub fn is_foreground(_raw: isize) -> bool {
        false
    }
    pub fn restores_to_maximized(_raw: isize) -> bool {
        false
    }
    pub fn set_restore_to_maximized(_raw: isize) {}
    pub fn set_no_activate(_raw: isize, _on: bool) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The primary monitor of the dev box: 2560×1440, 48 px taskbar.
    const WORK: RectI = RectI {
        x: 0,
        y: 0,
        w: 2560,
        h: 1392,
    };

    #[test]
    fn flyout_sits_above_left_of_the_click() {
        // Clicked well above the taskbar (the overflow flyout): no clamping.
        assert_eq!(flyout_position((2000, 1200), (460, 620), WORK), (1560, 568));
        // A tray icon in the taskbar: y is clamped 8 px above the work area.
        assert_eq!(
            flyout_position((2300, 1415), (460, 620), WORK),
            (2300 - 460 + 20, 1392 - 620 - 8)
        );
    }

    #[test]
    fn flyout_is_clamped_to_the_work_area() {
        // Right edge: keeps 8 px from the work area's right side.
        assert_eq!(
            flyout_position((2555, 1000), (460, 620), WORK).0,
            2560 - 460 - 8
        );
        // Left edge: never left of the work area.
        assert_eq!(flyout_position((100, 1000), (460, 620), WORK).0, 0);
        // Top: a click near the top puts the window at the top.
        assert_eq!(flyout_position((1000, 100), (460, 620), WORK).1, 0);
        // Bottom: 8 px above the taskbar.
        assert_eq!(
            flyout_position((1000, 1430), (460, 620), WORK).1,
            1392 - 620 - 8
        );
    }

    #[test]
    fn flyout_clamps_on_offset_and_tiny_monitors() {
        // A secondary monitor left of the primary (negative coordinates).
        let left = RectI {
            x: -2560,
            y: 0,
            w: 2560,
            h: 1392,
        };
        assert_eq!(flyout_position((-10, 1415), (460, 620), left), (-468, 764));
        // A work area smaller than the window: pinned to its top-left.
        let tiny = RectI {
            x: 100,
            y: 50,
            w: 300,
            h: 200,
        };
        assert_eq!(flyout_position((350, 240), (460, 620), tiny), (100, 50));
    }

    #[test]
    fn flyout_size_follows_the_target_monitor_dpi() {
        assert_eq!(size_at_dpi((460.0, 620.0), 96), (460, 620));
        assert_eq!(size_at_dpi((460.0, 620.0), 144), (690, 930));
        assert_eq!(size_at_dpi((460.0, 620.0), 120), (575, 775));
        // A 150 % secondary monitor's tray, opened from a 100 % primary.
        let work = RectI {
            x: 2560,
            y: 0,
            w: 2560,
            h: 1392,
        };
        let size = size_at_dpi((460.0, 620.0), 144);
        assert_eq!(
            flyout_position((4900, 1415), size, work),
            (4900 - 690 + 20, 1392 - 930 - 8)
        );
    }

    #[test]
    fn rect_difference_tolerance() {
        let a = RectI {
            x: 10,
            y: 10,
            w: 460,
            h: 620,
        };
        assert!(!rect_differs(a, a, 2));
        assert!(!rect_differs(a, RectI { x: 12, h: 618, ..a }, 2));
        assert!(rect_differs(a, RectI { w: 690, ..a }, 2));
        assert!(rect_differs(a, RectI { y: 7, ..a }, 2));
    }
}
