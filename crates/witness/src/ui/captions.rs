//! The always-on-top live-caption overlay: a deferred viewport declared
//! every pass while open (design §4.3, map-ui-settings §3).
//!
//! The overlay window is undecorated, always on top, has no taskbar entry
//! and never takes focus when it opens (auto-record fires as the user joins
//! a Teams call). It shows the last 20 lines of the current meeting,
//! anchored at the bottom, so the window height alone decides how many are
//! visible. The whole surface drags the window; edge zones resize it;
//! Escape, × or Alt+F4 close it (recording and live captioning go on).
//!
//! Geometry persists to `data_dir/captions-overlay.toml` in **physical**
//! pixels, in the 0.2.x format plus an optional `scale` (the overlay's own
//! zoom). It is remembered in memory while the app runs, clamped to the
//! work area of the monitor under its centre on restore, and applied
//! exactly with `SetWindowPos` once the overlay's HWND exists (winit's
//! logical placement is off on mixed-DPI setups).
//!
//! The viewport callback runs on the main thread like the root's passes;
//! the state it shares with `CaptionsOverlay` sits behind a mutex only
//! because egui requires the callback to be `Send + Sync`. Window
//! operations that must not wait for a pass (hiding on close, re-showing
//! a not-yet-destroyed window) use Win32 directly on the overlay's HWND.

use super::theme::{self, ACCENT, CAPTION_BG, CAPTION_ERR, CAPTION_TEXT, OK_LIGHT, REC_LIGHT};
use super::topbar::edge_resize;
use super::widgets::{self, Politeness};
use crate::bridge::wake_root;
use crate::platform::system::{self, RectI};
use eframe::egui::{
    self, Color32, CornerRadius, CursorIcon, Key, Modifiers, Rect, Sense, Stroke, ViewportBuilder,
    ViewportCommand, ViewportId,
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use winit::dpi::PhysicalPosition;
use witness_core::events::LiveTranscript;

/// The overlay's window title (also how its HWND is found).
pub const TITLE: &str = "Witness — live captions";
const GEOMETRY_FILE: &str = "captions-overlay.toml";
/// Lines of history (`prev.slice(-19) + new`).
const MAX_LINES: usize = 20;
/// Initial / minimum inner size in logical pixels.
const DEFAULT_SIZE: egui::Vec2 = egui::vec2(560.0, 150.0);
const MIN_SIZE: egui::Vec2 = egui::vec2(320.0, 90.0);
/// The overlay's own zoom (Ctrl + `=` / `−` / `0`, Ctrl + wheel).
const SCALE_MIN: f32 = 0.6;
const SCALE_MAX: f32 = 2.5;
const ZOOM_STEP: f32 = 1.1;
/// How often the exact physical rect is (re)applied before giving up; a
/// move onto a monitor with another DPI makes Windows rescale once.
const PLACE_ATTEMPTS: u8 = 3;
/// Slack when comparing the actual window rect with the target.
const PLACE_TOLERANCE: i32 = 2;
/// `rgba(229,72,77,α)` tints: border .4, grip .12, error .15.
const BORDER_RED: Color32 = Color32::from_rgba_unmultiplied_const(0xe5, 0x48, 0x4d, 102);
const GRIP_BG: Color32 = Color32::from_rgba_unmultiplied_const(0xe5, 0x48, 0x4d, 31);
const ERROR_BG: Color32 = Color32::from_rgba_unmultiplied_const(0xe5, 0x48, 0x4d, 38);

/// The overlay's viewport id.
pub fn captions_id() -> ViewportId {
    ViewportId::from_hash_of("witness-captions")
}

/// Why the overlay is being opened (focus rules differ).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenReason {
    /// `CoreEvent::OpenCaptionsOverlay`: never takes focus.
    Auto,
    /// The CC button: focuses an already-open overlay.
    User,
}

// ---------------------------------------------------------------------------
// Geometry

/// Outer position and size in physical pixels (the 0.2.x file format) plus
/// the overlay's own zoom. Old files have no `scale` and still parse.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct OverlayGeometry {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    #[serde(default = "default_scale")]
    scale: f32,
}

fn default_scale() -> f32 {
    1.0
}

impl OverlayGeometry {
    fn from_rect(rect: RectI, scale: f32) -> OverlayGeometry {
        OverlayGeometry {
            x: rect.x,
            y: rect.y,
            width: rect.w.max(1) as u32,
            height: rect.h.max(1) as u32,
            scale,
        }
    }

    fn rect(&self) -> RectI {
        RectI {
            x: self.x,
            y: self.y,
            w: self.width.min(i32::MAX as u32) as i32,
            h: self.height.min(i32::MAX as u32) as i32,
        }
    }

    fn center(&self) -> PhysicalPosition<i32> {
        PhysicalPosition::new(
            self.x.saturating_add((self.width / 2) as i32),
            self.y.saturating_add((self.height / 2) as i32),
        )
    }

    /// Keep the window inside `work` (the monitor's work area), so a
    /// display change cannot strand it offscreen; same rule as 0.2.x.
    fn clamped_to(self, work: RectI) -> OverlayGeometry {
        let width = (self.width.min(i32::MAX as u32) as i32).min(work.w).max(1);
        let height = (self.height.min(i32::MAX as u32) as i32).min(work.h).max(1);
        OverlayGeometry {
            x: self.x.clamp(work.x, (work.x + work.w - width).max(work.x)),
            y: self.y.clamp(work.y, (work.y + work.h - height).max(work.y)),
            width: width as u32,
            height: height as u32,
            scale: sanitize_scale(self.scale),
        }
    }

    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(GEOMETRY_FILE)
    }

    fn load(data_dir: &Path) -> Option<OverlayGeometry> {
        let text = std::fs::read_to_string(Self::path(data_dir)).ok()?;
        match toml::from_str::<OverlayGeometry>(&text) {
            Ok(geometry) => Some(geometry),
            Err(error) => {
                log::warn!("ignoring invalid {GEOMETRY_FILE}: {error}");
                None
            }
        }
    }

    /// Write atomically (temp file + rename); failures are only logged.
    fn save(&self, data_dir: &Path) {
        let text = match toml::to_string(self) {
            Ok(text) => text,
            Err(error) => {
                log::warn!("could not serialize overlay geometry: {error}");
                return;
            }
        };
        let path = Self::path(data_dir);
        let tmp = path.with_extension("toml.tmp");
        let result = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(error) = result {
            log::warn!("could not save overlay geometry: {error}");
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

fn sanitize_scale(scale: f32) -> f32 {
    if scale.is_finite() {
        scale.clamp(SCALE_MIN, SCALE_MAX)
    } else {
        1.0
    }
}

// ---------------------------------------------------------------------------
// Win32 on the overlay's HWND (eframe exposes no handle for child viewports)

#[cfg(windows)]
mod win {
    use super::RectI;
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, IsIconic, IsWindow, IsWindowVisible, ShowWindow, SW_HIDE, SW_SHOWNOACTIVATE,
    };

    fn hwnd(raw: isize) -> HWND {
        HWND(raw as *mut _)
    }

    /// Whether `raw` still names a window.
    pub fn exists(raw: isize) -> bool {
        // SAFETY: IsWindow accepts any value, including stale handles.
        unsafe { IsWindow(Some(hwnd(raw))).as_bool() }
    }

    pub fn is_visible(raw: isize) -> bool {
        // SAFETY: as above; a stale handle reads as not visible.
        exists(raw) && unsafe { IsWindowVisible(hwnd(raw)).as_bool() }
    }

    /// The outer rect in physical pixels; None when gone or minimized
    /// (a minimized window reports a parking position, not its place).
    pub fn rect(raw: isize) -> Option<RectI> {
        let mut r = RECT::default();
        // SAFETY: GetWindowRect writes one RECT and fails on a stale handle.
        unsafe {
            if IsIconic(hwnd(raw)).as_bool() {
                return None;
            }
            GetWindowRect(hwnd(raw), &mut r).ok()?;
        }
        Some(RectI {
            x: r.left,
            y: r.top,
            w: r.right - r.left,
            h: r.bottom - r.top,
        })
    }

    pub fn hide(raw: isize) {
        // SAFETY: ShowWindow tolerates a stale handle.
        unsafe {
            let _ = ShowWindow(hwnd(raw), SW_HIDE);
        }
    }

    pub fn show_no_activate(raw: isize) {
        // SAFETY: as above.
        unsafe {
            let _ = ShowWindow(hwnd(raw), SW_SHOWNOACTIVATE);
        }
    }
}

#[cfg(not(windows))]
mod win {
    use super::RectI;
    pub fn exists(_raw: isize) -> bool {
        false
    }
    pub fn is_visible(_raw: isize) -> bool {
        false
    }
    pub fn rect(_raw: isize) -> Option<RectI> {
        None
    }
    pub fn hide(_raw: isize) {}
    pub fn show_no_activate(_raw: isize) {}
}

fn close_enough(a: RectI, b: RectI) -> bool {
    (a.x - b.x).abs() <= PLACE_TOLERANCE
        && (a.y - b.y).abs() <= PLACE_TOLERANCE
        && (a.w - b.w).abs() <= PLACE_TOLERANCE
        && (a.h - b.h).abs() <= PLACE_TOLERANCE
}

// ---------------------------------------------------------------------------
// Shared state

/// What the parent and the viewport callback share.
struct CaptionsState {
    data_dir: PathBuf,
    /// The meeting the lines belong to (per-meeting reset).
    meeting_id: Option<i64>,
    lines: VecDeque<LiveTranscript>,
    /// Sticky error banner (cleared only for a new meeting).
    error: Option<String>,
    /// The latest physical outer rect seen while open.
    outer_rect: Option<RectI>,
    /// Geometry remembered for this run (0.2.x `OVERLAY_GEOMETRY`); wins
    /// over the file on the next open.
    remembered: Option<OverlayGeometry>,
    /// The overlay's own zoom.
    scale: f32,
    /// The Windows "Text size" setting, read on open (WebView2 applied it
    /// to the overlay too).
    text_scale: f32,
    /// Escape / × / Alt+F4: the window is hidden; the parent stops
    /// declaring the viewport on its next pass.
    close_requested: bool,
    /// The CC button re-opened an open overlay: focus it on its next pass.
    focus_pending: bool,
    /// The overlay window, once it exists (cleared when it is destroyed).
    hwnd: Option<isize>,
    /// The exact physical rect to apply, and how often it was applied.
    placement: Option<(RectI, u8)>,
}

impl CaptionsState {
    fn new(data_dir: &Path) -> CaptionsState {
        CaptionsState {
            data_dir: data_dir.to_path_buf(),
            meeting_id: None,
            lines: VecDeque::with_capacity(MAX_LINES),
            error: None,
            outer_rect: None,
            remembered: None,
            scale: 1.0,
            text_scale: 1.0,
            close_requested: false,
            focus_pending: false,
            hwnd: None,
            placement: None,
        }
    }

    fn clear_content(&mut self) {
        self.lines.clear();
        self.error = None;
    }

    fn push(&mut self, line: &LiveTranscript) {
        if self.meeting_id.is_some_and(|m| m != line.meeting_id) {
            return; // a straggler from another recording
        }
        while self.lines.len() >= MAX_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line.clone());
    }

    /// The overlay HWND (looked up by title + our PID until found).
    fn find_hwnd(&mut self) -> Option<isize> {
        if self.hwnd.is_none() {
            self.hwnd = system::find_own_window(TITLE);
        }
        self.hwnd
    }

    /// Snapshot the current geometry into memory and return it.
    fn capture(&mut self) -> Option<OverlayGeometry> {
        if let Some(rect) = self.hwnd.and_then(win::rect) {
            self.outer_rect = Some(rect);
        }
        let geometry = OverlayGeometry::from_rect(self.outer_rect?, self.scale);
        self.remembered = Some(geometry);
        Some(geometry)
    }

    /// Capture and write the geometry file.
    fn persist(&mut self, data_dir: &Path) {
        if let Some(geometry) = self.capture() {
            geometry.save(data_dir);
        }
    }
}

fn lock(state: &Mutex<CaptionsState>) -> MutexGuard<'_, CaptionsState> {
    // The callback never panics while holding the lock in practice; if it
    // did, the panic hook exits the app anyway.
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// The overlay

pub struct CaptionsOverlay {
    data_dir: PathBuf,
    /// The parent declares the viewport while this is set.
    open: bool,
    state: Arc<Mutex<CaptionsState>>,
    /// Built once per open and then kept constant, so egui never re-applies
    /// a size or position over the user's.
    builder: Option<ViewportBuilder>,
    /// The (clamped) geometry this open restores.
    restore: Option<OverlayGeometry>,
    /// For repaint requests from `logic()`; set by the first `show`.
    ctx: Option<egui::Context>,
}

impl CaptionsOverlay {
    pub fn new(data_dir: &Path) -> Self {
        CaptionsOverlay {
            data_dir: data_dir.to_path_buf(),
            open: false,
            state: Arc::new(Mutex::new(CaptionsState::new(data_dir))),
            builder: None,
            restore: None,
            ctx: None,
        }
    }

    fn repaint(&self) {
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint_of(captions_id());
        }
    }

    /// Open (or re-open) for `meeting_id`. Lines and error are kept only
    /// for a re-open of the same meeting. A new window never takes focus;
    /// the CC button (`User`) focuses an overlay that is already open.
    pub fn open(&mut self, meeting_id: i64, why: OpenReason) {
        let mut st = lock(&self.state);
        if st.meeting_id != Some(meeting_id) {
            st.meeting_id = Some(meeting_id);
            st.clear_content();
        }
        if self.open && !st.close_requested {
            if why == OpenReason::User {
                st.focus_pending = true;
            }
            drop(st);
            self.repaint();
            return;
        }

        let restore = st
            .remembered
            .or_else(|| OverlayGeometry::load(&self.data_dir))
            .map(|g| g.clamped_to(system::work_area_at(g.center())));
        st.scale = restore.map_or(1.0, |g| g.scale);
        st.text_scale = system::text_scale_factor();
        st.placement = restore.map(|g| (g.rect(), 0));
        st.outer_rect = None;
        st.close_requested = false;
        st.focus_pending = false;
        // A window from an earlier open that was hidden but not destroyed
        // yet (the root ran no pass since, e.g. minimized): show it again.
        if let Some(hwnd) = st.hwnd {
            if win::exists(hwnd) {
                win::show_no_activate(hwnd);
            } else {
                st.hwnd = None;
            }
        }
        drop(st);
        log::debug!("captions overlay open for meeting {meeting_id} ({why:?})");
        self.restore = restore;
        self.builder = None;
        self.open = true;
        if let Some(ctx) = &self.ctx {
            wake_root(ctx);
        }
        self.repaint();
    }

    /// A new recording started: start empty ("Listening…").
    pub fn reset_for(&mut self, meeting_id: i64) {
        let mut st = lock(&self.state);
        st.meeting_id = Some(meeting_id);
        st.clear_content();
        drop(st);
        self.repaint();
    }

    /// Close (recording stopped, start rolled back, Quit): save the
    /// geometry and hide the window at once; the next root pass stops
    /// declaring the viewport, which destroys it. Keeps only the geometry.
    pub fn close(&mut self, data_dir: &Path) {
        let mut st = lock(&self.state);
        if self.open {
            // A user close has already saved (and hidden) it.
            if !st.close_requested {
                st.persist(data_dir);
            }
            if let Some(hwnd) = st.hwnd {
                win::hide(hwnd);
            }
            log::debug!("captions overlay closed");
        }
        st.meeting_id = None;
        st.clear_content();
        st.close_requested = false;
        st.focus_pending = false;
        st.placement = None;
        drop(st);
        self.open = false;
        self.builder = None;
        self.restore = None;
    }

    /// Append a caption line (keeps the last 20).
    pub fn push_line(&mut self, line: &LiveTranscript) {
        lock(&self.state).push(line);
        self.repaint();
    }

    /// The sticky error banner. `app.rs` calls this when live captions stop
    /// (`active == false`): `None` means "stopped without a reason", shown
    /// as "Live captions stopped" like the web overlay did. A new meeting
    /// clears it.
    pub fn set_error(&mut self, error: Option<String>) {
        lock(&self.state).error = Some(error.unwrap_or_else(|| "Live captions stopped".into()));
        self.repaint();
    }

    /// Open and not closed by the user.
    pub fn is_open(&self) -> bool {
        self.open && !lock(&self.state).close_requested
    }

    /// The overlay's OS window exists and is visible.
    pub fn is_visible(&self) -> bool {
        if !self.open {
            return false;
        }
        let mut st = lock(&self.state);
        !st.close_requested && st.find_hwnd().is_some_and(win::is_visible)
    }

    /// Debug builds only: `WITNESS_DEBUG_CAPTIONS=1` opens the overlay with
    /// sample lines at startup, so it can be checked without models or a
    /// live-caption session (the counterpart of the panic-hotkey hook).
    #[cfg(debug_assertions)]
    fn debug_demo(&mut self) {
        if std::env::var_os("WITNESS_DEBUG_CAPTIONS").is_none_or(|v| v != "1") {
            return;
        }
        const DEMO: i64 = -1;
        self.reset_for(DEMO);
        self.open(DEMO, OpenReason::Auto);
        let samples: [(&'static str, &str); 4] = [
            ("loopback", "Can everyone see the slides?"),
            ("mic", "Yes, the budget table is up."),
            (
                "loopback",
                "Great. Let's start with the third quarter numbers, which came in a little \
                 above forecast for every region except the north-east.",
            ),
            ("mic", "I'll share the breakdown after the call."),
        ];
        for (n, (track, text)) in samples.into_iter().enumerate() {
            self.push_line(&LiveTranscript {
                meeting_id: DEMO,
                track,
                start_ms: n as u64 * 1000,
                end_ms: n as u64 * 1000 + 900,
                text: text.into(),
            });
        }
    }

    /// Declare the deferred viewport (every root pass while open).
    pub fn show(&mut self, ctx: &egui::Context) {
        if self.ctx.is_none() {
            self.ctx = Some(ctx.clone());
            #[cfg(debug_assertions)]
            self.debug_demo();
        }
        if self.open && lock(&self.state).close_requested {
            // Closed by the user (geometry already saved, window hidden).
            self.open = false;
            self.builder = None;
            self.restore = None;
        }
        if !self.open {
            // Not declared this pass: egui destroys the window.
            let mut st = lock(&self.state);
            st.hwnd = None;
            st.placement = None;
            return;
        }
        let builder = self
            .builder
            .get_or_insert_with(|| overlay_builder(ctx, self.restore))
            .clone();
        let state = self.state.clone();
        ctx.show_viewport_deferred(captions_id(), builder, move |ui, _class| {
            overlay_ui(ui, &state);
        });
    }
}

/// The overlay's window. Sizes are in points; egui multiplies them by the
/// zoom factor (the main window's zoom), so divide it out. A restored rect
/// is converted with the target monitor's DPI; the exact physical rect is
/// applied once the window exists.
fn overlay_builder(ctx: &egui::Context, restore: Option<OverlayGeometry>) -> ViewportBuilder {
    let zoom = ctx.zoom_factor().max(0.1);
    let mut builder = ViewportBuilder::default()
        .with_title(TITLE)
        .with_inner_size(DEFAULT_SIZE / zoom)
        .with_min_inner_size(MIN_SIZE / zoom)
        .with_decorations(false)
        .with_always_on_top()
        .with_taskbar(false)
        .with_active(false)
        .with_resizable(true)
        .with_drag_and_drop(false);
    if let Some(g) = restore {
        let points_per_pixel = 96.0 / system::dpi_at(g.center()).max(1) as f32 / zoom;
        builder = builder
            .with_position(egui::pos2(
                g.x as f32 * points_per_pixel,
                g.y as f32 * points_per_pixel,
            ))
            .with_inner_size(egui::vec2(
                g.width as f32 * points_per_pixel,
                g.height as f32 * points_per_pixel,
            ));
    }
    builder
}

/// Escape, × or an OS close (Alt+F4): save the geometry, hide the window
/// now (even if the root runs no pass soon), and let the parent drop it.
fn request_close(st: &mut CaptionsState, ctx: &egui::Context) {
    st.close_requested = true;
    let data_dir = st.data_dir.clone();
    st.persist(&data_dir);
    if let Some(hwnd) = st.hwnd {
        win::hide(hwnd);
    }
    wake_root(ctx);
}

/// One pass of the overlay viewport.
fn overlay_ui(ui: &mut egui::Ui, state: &Mutex<CaptionsState>) {
    let ctx = ui.ctx().clone();
    let mut st = lock(state);

    let os_close = ctx.input(|i| i.viewport().close_requested());
    if os_close {
        // Answer Alt+F4 ourselves so egui does not close it on its own.
        ctx.send_viewport_cmd(ViewportCommand::CancelClose);
    }
    if st.close_requested {
        return; // hidden; the parent drops it on its next pass
    }

    place_window(&mut st, &ctx);

    if std::mem::take(&mut st.focus_pending) {
        ctx.send_viewport_cmd(ViewportCommand::Focus);
    }

    // The overlay's own zoom (the main window's zoom is root-only).
    let (zoom_in, zoom_out, zoom_reset, wheel, escape) = ctx.input_mut(|i| {
        (
            i.consume_key(Modifiers::COMMAND, Key::Equals)
                || i.consume_key(Modifiers::COMMAND, Key::Plus),
            i.consume_key(Modifiers::COMMAND, Key::Minus),
            i.consume_key(Modifiers::COMMAND, Key::Num0),
            i.zoom_delta(),
            i.key_pressed(Key::Escape),
        )
    });
    let mut scale = st.scale;
    if zoom_in {
        scale *= ZOOM_STEP;
    }
    if zoom_out {
        scale /= ZOOM_STEP;
    }
    if zoom_reset {
        scale = 1.0;
    }
    if wheel != 1.0 {
        scale *= wheel;
    }
    st.scale = sanitize_scale(scale);

    let resize = edge_resize(&ctx, "captions");
    if let Some(dir) = resize {
        ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
    }

    let close_clicked = paint(ui, &st, resize.is_some());
    if escape || os_close || close_clicked {
        request_close(&mut st, &ctx);
    }
}

/// Find the HWND, apply the exact physical rect (a few attempts), and
/// track the outer rect for saving.
fn place_window(st: &mut CaptionsState, ctx: &egui::Context) {
    let Some(hwnd) = st.find_hwnd() else {
        if st.placement.is_some() {
            ctx.request_repaint(); // the window is still being created
        }
        return;
    };
    if let Some((target, attempts)) = st.placement {
        let actual = win::rect(hwnd);
        if actual.is_some_and(|r| close_enough(r, target)) {
            st.placement = None;
        } else if attempts >= PLACE_ATTEMPTS {
            log::warn!(
                "captions overlay placement did not settle: want {target:?}, got {actual:?}"
            );
            st.placement = None;
        } else {
            system::set_window_rect(hwnd, target);
            st.placement = Some((target, attempts + 1));
            ctx.request_repaint();
        }
    }
    if let Some(rect) = win::rect(hwnd) {
        st.outer_rect = Some(rect);
    }
}

/// Paint the overlay; returns whether × was clicked. A primary press
/// anywhere else starts the OS move loop (unless it began a resize).
fn paint(ui: &mut egui::Ui, st: &CaptionsState, resizing: bool) -> bool {
    let ctx = ui.ctx().clone();
    let rect = ui.max_rect();
    // CSS px scale: the overlay's zoom × Windows text size, independent of
    // the main window's zoom (which egui applies to every viewport).
    let k = st.scale * st.text_scale / ctx.zoom_factor().max(0.1);
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, CornerRadius::ZERO, CAPTION_BG);

    // The surface: drags the window (registered first, so × stays on top).
    let surface = ui
        .interact(
            rect,
            ui.id().with("captions-surface"),
            Sense::click_and_drag(),
        )
        .on_hover_cursor(CursorIcon::Move);
    surface
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, "Live captions"));

    // Grip: "● Witness — live captions (drag to move)" and ×.
    let pad_x = 10.0 * k;
    let close_galley =
        painter.layout_no_wrap("×".to_owned(), theme::regular(18.0 * k), CAPTION_TEXT);
    let close_size = close_galley.size() + egui::vec2(8.0 * k, 2.0 * k);
    let title_width = (rect.width() - close_size.x - 2.0 * pad_x - 8.0 * k).max(10.0);
    let mut title_job = egui::text::LayoutJob::simple_singleline(
        "● Witness — live captions (drag to move)".to_owned(),
        theme::regular(11.0 * k),
        REC_LIGHT,
    );
    title_job.wrap = egui::text::TextWrapping::truncate_at_width(title_width);
    let title_galley = painter.layout_job(title_job);
    let grip_height = title_galley.size().y.max(close_size.y) + 8.0 * k;
    let grip = Rect::from_min_size(rect.min, egui::vec2(rect.width(), grip_height));
    let radius = (8.0 * k).round().clamp(0.0, 255.0) as u8;
    painter.rect_filled(
        grip,
        CornerRadius {
            nw: radius,
            ne: radius,
            sw: 0,
            se: 0,
        },
        GRIP_BG,
    );
    painter.galley(
        egui::pos2(
            grip.left() + pad_x,
            grip.center().y - title_galley.size().y / 2.0,
        ),
        title_galley,
        REC_LIGHT,
    );
    let close_rect = Rect::from_center_size(
        egui::pos2(grip.right() - pad_x - close_size.x / 2.0, grip.center().y),
        close_size,
    );
    let close = ui
        .interact(close_rect, ui.id().with("captions-close"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("Close captions (Escape)");
    close.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Close live captions")
    });
    painter.galley(
        close_rect.center() - close_galley.size() / 2.0,
        close_galley,
        CAPTION_TEXT,
    );

    // Error banner (sticky).
    let mut top = grip.bottom();
    if let Some(error) = &st.error {
        let mut job = egui::text::LayoutJob::simple(
            error.clone(),
            theme::regular(11.0 * k),
            CAPTION_ERR,
            (rect.width() - 2.0 * pad_x).max(10.0),
        );
        job.wrap.max_rows = 3;
        let galley = painter.layout_job(job);
        let band = Rect::from_min_size(
            egui::pos2(rect.left(), top),
            egui::vec2(rect.width(), galley.size().y + 10.0 * k),
        );
        painter.rect_filled(band, CornerRadius::ZERO, ERROR_BG);
        painter.galley(
            egui::pos2(band.left() + pad_x, band.top() + 5.0 * k),
            galley,
            CAPTION_ERR,
        );
        top = band.bottom();
    }

    // Lines, newest at the bottom; older ones are pushed up and clipped.
    let area = Rect::from_min_max(egui::pos2(rect.left(), top), rect.max);
    let lines_painter = painter.with_clip_rect(area.intersect(rect));
    let (pad_x, pad_y) = (12.0 * k, 8.0 * k);
    let wrap_width = (area.width() - 2.0 * pad_x).max(10.0);
    let size = 15.0 * k;
    let format = |font_id, color| egui::TextFormat {
        font_id,
        color,
        line_height: Some(size * 1.4),
        ..Default::default()
    };
    let mut y = area.bottom() - pad_y;
    if st.lines.is_empty() {
        let mut job = egui::text::LayoutJob::default();
        job.append(
            "Listening…",
            0.0,
            format(theme::regular(size), theme::TEXT_MUTED),
        );
        job.wrap.max_width = wrap_width;
        let galley = lines_painter.layout_job(job);
        y -= galley.size().y;
        lines_painter.galley(egui::pos2(area.left() + pad_x, y), galley, CAPTION_TEXT);
    }
    for line in st.lines.iter().rev() {
        if y < area.top() {
            break;
        }
        let (who, color) = if line.track == "mic" {
            ("Me", ACCENT)
        } else {
            ("Them", OK_LIGHT)
        };
        let mut job = egui::text::LayoutJob::default();
        job.append(who, 0.0, format(theme::semibold(size), color));
        job.append(
            &format!(" {}", line.text),
            0.0,
            format(theme::regular(size), CAPTION_TEXT),
        );
        job.wrap.max_width = wrap_width;
        let galley = lines_painter.layout_job(job);
        y -= galley.size().y;
        lines_painter.galley(egui::pos2(area.left() + pad_x, y), galley, CAPTION_TEXT);
        y -= 4.0 * k;
    }
    if let Some(newest) = st.lines.back() {
        let who = if newest.track == "mic" { "Me" } else { "Them" };
        widgets::announce(
            &ctx,
            egui::Id::new("witness-captions-log"),
            &format!("{who} {}", newest.text),
            Politeness::Polite,
        );
    }

    // 1 px rounded border inside the square, opaque window.
    painter.rect_stroke(
        rect.shrink(0.5),
        CornerRadius::same(radius),
        Stroke::new(1.0, BORDER_RED),
        egui::StrokeKind::Inside,
    );

    let pressed = ctx.input(|i| i.pointer.primary_pressed());
    if pressed && !resizing && surface.contains_pointer() && !close.contains_pointer() {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }
    close.clicked()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "witness-captions-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn line(meeting_id: i64, n: u64, track: &'static str) -> LiveTranscript {
        LiveTranscript {
            meeting_id,
            track,
            start_ms: n,
            end_ms: n + 1,
            text: format!("line {n}"),
        }
    }

    fn lines(overlay: &CaptionsOverlay) -> Vec<u64> {
        lock(&overlay.state)
            .lines
            .iter()
            .map(|l| l.start_ms)
            .collect()
    }

    #[test]
    fn old_geometry_files_still_parse() {
        let old = "x = -1910\ny = 40\nwidth = 560\nheight = 150\n";
        let g: OverlayGeometry = toml::from_str(old).unwrap();
        assert_eq!(
            (g.x, g.y, g.width, g.height, g.scale),
            (-1910, 40, 560, 150, 1.0)
        );
        let dir = temp_dir();
        let saved = OverlayGeometry { scale: 1.5, ..g };
        saved.save(&dir);
        assert_eq!(OverlayGeometry::load(&dir), Some(saved));
        let text = std::fs::read_to_string(dir.join(GEOMETRY_FILE)).unwrap();
        assert!(text.contains("width = 560"), "{text}");
        std::fs::write(dir.join(GEOMETRY_FILE), "garbage").unwrap();
        assert_eq!(OverlayGeometry::load(&dir), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn restore_is_clamped_to_the_work_area() {
        let work = RectI {
            x: 0,
            y: 0,
            w: 1920,
            h: 1040,
        };
        let g = |x, y, width, height| OverlayGeometry {
            x,
            y,
            width,
            height,
            scale: 1.0,
        };
        // Inside: unchanged.
        assert_eq!(
            g(100, 100, 560, 150).clamped_to(work),
            g(100, 100, 560, 150)
        );
        // Off the right/bottom edge: pulled back in.
        assert_eq!(
            g(1800, 1000, 560, 150).clamped_to(work),
            g(1360, 890, 560, 150)
        );
        // Off the left/top (a monitor that went away): pulled in.
        assert_eq!(g(-3000, -50, 560, 150).clamped_to(work), g(0, 0, 560, 150));
        // Bigger than the work area: shrunk to it.
        assert_eq!(g(10, 10, 4000, 3000).clamped_to(work), g(0, 0, 1920, 1040));
        // Scale is sanitized.
        let mut wild = g(0, 0, 560, 150);
        wild.scale = 9.0;
        assert_eq!(wild.clamped_to(work).scale, SCALE_MAX);
        wild.scale = f32::NAN;
        assert_eq!(wild.clamped_to(work).scale, 1.0);
    }

    #[test]
    fn keeps_the_last_twenty_lines_of_its_meeting() {
        let dir = temp_dir();
        let mut overlay = CaptionsOverlay::new(&dir);
        overlay.reset_for(4);
        for n in 0..25 {
            overlay.push_line(&line(4, n, "mic"));
        }
        overlay.push_line(&line(3, 99, "mic")); // another meeting
        assert_eq!(lines(&overlay), (5..25).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn every_new_meeting_starts_empty() {
        let dir = temp_dir();
        let mut overlay = CaptionsOverlay::new(&dir);
        overlay.reset_for(1);
        overlay.open(1, OpenReason::Auto);
        assert!(overlay.is_open());
        overlay.push_line(&line(1, 1, "mic"));
        overlay.set_error(None);
        assert_eq!(
            lock(&overlay.state).error.as_deref(),
            Some("Live captions stopped")
        );
        // A re-open for the same meeting keeps the lines.
        overlay.open(1, OpenReason::User);
        assert_eq!(lines(&overlay), vec![1]);
        assert!(
            lock(&overlay.state).focus_pending,
            "CC focuses an open overlay"
        );

        // Recording start resets lines and error.
        overlay.reset_for(2);
        assert!(lines(&overlay).is_empty());
        assert!(lock(&overlay.state).error.is_none());
        overlay.push_line(&line(2, 5, "loopback"));

        // A backend close keeps nothing but the geometry.
        overlay.close(&dir);
        assert!(!overlay.is_open());
        assert!(lines(&overlay).is_empty());
        // Opening for another meeting starts empty, without focus.
        overlay.open(3, OpenReason::User);
        assert!(lines(&overlay).is_empty());
        assert!(
            !lock(&overlay.state).focus_pending,
            "a new window never takes focus"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_user_close_saves_the_geometry_and_drops_the_viewport() {
        let dir = temp_dir();
        let mut overlay = CaptionsOverlay::new(&dir);
        overlay.open(7, OpenReason::Auto);
        let ctx = theme::test_context();
        {
            let mut st = lock(&overlay.state);
            st.outer_rect = Some(RectI {
                x: 50,
                y: 60,
                w: 600,
                h: 160,
            });
            st.scale = 1.2;
            request_close(&mut st, &ctx);
        }
        assert!(!overlay.is_open());
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| overlay.show(ui.ctx()));
        output.textures_delta.clear();
        assert!(!overlay.open);
        let saved = OverlayGeometry::load(&dir).unwrap();
        assert_eq!(
            (saved.x, saved.y, saved.width, saved.height),
            (50, 60, 600, 160)
        );
        assert!((saved.scale - 1.2).abs() < 1e-6);
        // The next open restores it (from memory).
        overlay.open(7, OpenReason::Auto);
        assert_eq!(
            lock(&overlay.state).scale,
            saved.clamped_to(system::work_area_at(saved.center())).scale
        );
        assert!(overlay.restore.is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn overlay_renders_without_panicking() {
        // A bare context embeds deferred viewports as egui windows, which
        // runs the overlay callback here.
        let dir = temp_dir();
        let mut overlay = CaptionsOverlay::new(&dir);
        overlay.reset_for(1);
        overlay.open(1, OpenReason::Auto);
        let ctx = theme::test_context();
        for frame in 0..3 {
            if frame == 1 {
                overlay.push_line(&line(1, 1, "mic"));
                overlay.push_line(&line(1, 2, "loopback"));
                overlay.set_error(Some("model missing".into()));
            }
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| overlay.show(ui.ctx()));
            output.textures_delta.clear();
        }
        assert!(overlay.is_open());
        let _ = std::fs::remove_dir_all(dir);
    }
}
