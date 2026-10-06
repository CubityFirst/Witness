//! The custom title bar (map-ui-shell §4): record/stop, bookmark and CC
//! buttons, the status pill, navigation and the window controls; plus the
//! invisible resize edges and the health and model banners below it (§5).
//!
//! The bar reproduces the CSS `flex-wrap: wrap` row: each item (a button,
//! the pill, the nav group, the window-control group) is a wrap unit; the
//! nav group carries `margin-left: auto`, so on whichever line it lands it
//! and everything after it on that line sit at the right edge. Line breaks
//! are computed from the unit widths measured in the previous pass; when a
//! width change moves a unit to another line the pass is discarded and
//! re-run, so a stale layout is never shown.

use super::format::{capture_warning, fmt_elapsed};
use super::icons::{self, ph};
use super::theme::{
    self, narrow, ACCENT, BANNER_BG, BANNER_BORDER, BG, BG_RAISED, BORDER, OK, REC, REC_LIGHT,
    TEXT, TEXT_MUTED, WHITE,
};
use super::widgets::{self, PillKind, Politeness};
use super::{UiCtx, View};
use crate::bridge::OpDone;
use crate::model::ShellModel;
use crate::ui::toasts::Notice;
use eframe::egui::{
    self, Align, Color32, CornerRadius, CursorIcon, FontId, Frame, Layout, Margin, Order, Rect,
    ResizeDirection, RichText, Sense, Stroke,
};
use std::ops::Range;
use witness_core::events::ModelDownloadProgress;
use witness_core::settings::Engine;
use witness_core::{RecCmd, Trigger};

/// Window actions that `app.rs` performs (it runs `will_hide()` first).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TopBarOutput {
    pub minimize: bool,
    /// Close to tray.
    pub close: bool,
}

/// Presses this close to the window's top/left/right edge resize instead
/// of dragging (CSS `clientY<8 || clientX<8 || innerWidth-clientX<8`).
const EDGE_GRAB: f32 = 8.0;
/// Invisible resize zones: 6 pt edge strips, 12 pt corner squares.
const RESIZE_EDGE: f32 = 6.0;
const RESIZE_CORNER: f32 = 12.0;
/// `.topbar` row gap; the column gap is 12 (8 at the narrow width).
const ROW_GAP: f32 = 8.0;
/// `.nav` gap; `.window-controls` gap and margin-left.
const NAV_GAP: f32 = 4.0;
const CONTROLS_GAP: f32 = 2.0;
const CONTROLS_MARGIN: f32 = 4.0;
/// The record/stop glyph (`size={17}`).
const RECORD_ICON: f32 = 17.0;
/// Level meter: 48 pt wide, two 4 pt bars 2 pt apart, each ≥ 2 pt.
const METER_WIDTH: f32 = 48.0;
const METER_BAR: f32 = 4.0;
/// `.level-bar { transition: width 0.15s linear }`.
const METER_TRANSITION: f32 = 0.15;

// ---------------------------------------------------------------------------
// The bar

/// One flex item of the bar, in DOM order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Record,
    Bookmark,
    Captions,
    Pill,
    Nav,
    Controls,
}

impl Unit {
    /// First-frame size guesses (pt); replaced by measurements after one pass.
    fn estimate(self) -> egui::Vec2 {
        let width = match self {
            Unit::Record | Unit::Bookmark | Unit::Captions => 40.0,
            Unit::Pill => 60.0,
            Unit::Nav => 190.0,
            Unit::Controls => 100.0,
        };
        egui::vec2(width, 32.0)
    }
}

/// Unit sizes measured in the previous pass (egui temp data).
#[derive(Debug, Clone, Default)]
struct MeasuredWidths(Vec<(Unit, egui::Vec2)>);

impl MeasuredWidths {
    fn size(&self, unit: Unit) -> egui::Vec2 {
        self.0
            .iter()
            .find(|(u, _)| *u == unit)
            .map_or_else(|| unit.estimate(), |(_, size)| *size)
    }

    fn get(&self, unit: Unit) -> f32 {
        self.size(unit).x
    }

    /// The height of a line holding `units`: its tallest unit (CSS
    /// `align-items: center` centres every item on that line box).
    fn line_height(&self, units: &[Unit]) -> f32 {
        units.iter().map(|&u| self.size(u).y).fold(0.0, f32::max)
    }

    fn set(&mut self, unit: Unit, size: egui::Vec2) {
        match self.0.iter_mut().find(|(u, _)| *u == unit) {
            Some(slot) => slot.1 = size,
            None => self.0.push((unit, size)),
        }
    }
}

/// CSS flex-wrap line breaking: items keep their order, a line takes items
/// while they fit (`gap` between neighbours), and an item wider than the
/// whole row still gets a line of its own.
pub(crate) fn wrap_lines(widths: &[f32], available: f32, gap: f32) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut used = 0.0;
    for (i, &w) in widths.iter().enumerate() {
        if i == start {
            used = w;
        } else if used + gap + w > available + 0.5 {
            lines.push(start..i);
            start = i;
            used = w;
        } else {
            used += gap + w;
        }
    }
    if start < widths.len() {
        lines.push(start..widths.len());
    }
    lines
}

/// The units of the bar for this state, in DOM order.
fn bar_units(recording: bool, live_captions: bool, popup: bool) -> Vec<Unit> {
    let mut units = vec![Unit::Record];
    if recording {
        // Bookmarking works for any recording; the overlay button only
        // makes sense when a live-caption session actually started (the
        // core rejects it otherwise).
        units.push(Unit::Bookmark);
        if live_captions {
            units.push(Unit::Captions);
        }
    }
    units.push(Unit::Pill);
    units.push(Unit::Nav);
    if !popup {
        units.push(Unit::Controls);
    }
    units
}

/// The highlighted navigation item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavItem {
    Meetings,
    People,
    Settings,
}

/// What the bar shows this pass. Plain data, so the bar can be laid out
/// headless in tests; [`show`] builds it from the [`UiCtx`] and performs
/// the [`BarAction`]s the pass produced.
struct BarModel {
    units: Vec<Unit>,
    recording: bool,
    /// A start or stop is in flight (`ShellModel::recording_action`).
    busy: bool,
    pill: PillState,
    bookmark_tooltip: String,
    /// Microphone and system-audio RMS (0..1).
    levels: (f32, f32),
    /// Ease the level meter; never in a hidden window (a hidden root still
    /// runs full passes).
    animate_meter: bool,
    /// Meetings is "active" only on Meetings (not on Search or a
    /// transcript).
    active: Option<NavItem>,
}

impl BarModel {
    fn of(cx: &UiCtx) -> BarModel {
        let recording = cx.shell.recording();
        BarModel {
            units: bar_units(
                recording,
                cx.shell.live_captions_active(),
                cx.flyout.is_popup(),
            ),
            recording,
            busy: cx.shell.recording_action,
            pill: PillState::of(cx.shell),
            bookmark_tooltip: bookmark_tooltip(cx.shell.bookmark_hotkey.as_deref()),
            levels: cx
                .shell
                .level
                .as_ref()
                .map_or((0.0, 0.0), |l| (l.mic_rms, l.loopback_rms)),
            animate_meter: cx.window.is_shown()
                && !cx.window.is_minimized()
                && icons::animations_enabled(),
            active: match cx.view {
                View::Meetings => Some(NavItem::Meetings),
                View::People => Some(NavItem::People),
                View::Settings => Some(NavItem::Settings),
                View::Transcript { .. } | View::Search { .. } => None,
            },
        }
    }
}

/// A click in the bar, performed by [`show`] after the pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BarAction {
    /// Record when idle, Stop while recording.
    Record,
    Bookmark,
    ShowCaptions,
    /// The recording pill: open the recording's transcript.
    OpenTranscript,
    Nav(NavItem),
    Minimize,
    Maximize,
    Close,
}

/// One pass of the bar.
struct DrawnBar {
    /// The background's click-and-drag response (the whole bar, padding
    /// included).
    response: egui::Response,
    actions: Vec<BarAction>,
    /// Where each unit landed (read by the layout tests).
    #[cfg(test)]
    rects: Vec<(Unit, Rect)>,
}

/// Whether a press at `press` drags the window: anywhere on the bar except
/// within 8 pt of the window's top, left or right edge (the resize border).
/// Tested on where the press began, not where the drag threshold was
/// crossed (CSS tested the mousedown position).
fn press_drags_window(press: egui::Pos2, screen: Rect) -> bool {
    !(press.y - screen.top() < EDGE_GRAB
        || press.x - screen.left() < EDGE_GRAB
        || screen.right() - press.x < EDGE_GRAB)
}

/// Draw the bar inside `Panel::top("topbar")` (framed by [`frame`]).
pub fn show(ui: &mut egui::Ui, cx: &mut UiCtx) -> TopBarOutput {
    let model = BarModel::of(cx);
    let bar = draw_bar(ui, &model);

    let mut out = TopBarOutput::default();
    for action in bar.actions {
        match action {
            BarAction::Record => cx.submit_rec(if model.recording {
                RecCmd::Stop { by_user: true }
            } else {
                RecCmd::Start(Trigger::Manual)
            }),
            // `app.rs` toasts "Bookmark added" or the error on the reply.
            BarAction::Bookmark => cx.submit_rec(RecCmd::BookmarkNow),
            // The core check is ⏳, so it runs as an op;
            // `OpDone::CaptionsCheck` shows the overlay or toasts "Could not
            // show the caption overlay: …".
            BarAction::ShowCaptions => {
                cx.tasks.spawn_op_or(
                    "captions-check",
                    |c| OpDone::CaptionsCheck(c.check_captions_overlay()),
                    |error| OpDone::CaptionsCheck(Err(error)),
                );
            }
            BarAction::OpenTranscript => {
                if let Some(meeting_id) = cx.shell.status.as_ref().and_then(|s| s.meeting_id) {
                    // Transcripts never nest: from a transcript, Back goes
                    // to where that transcript came from.
                    let from = match cx.view {
                        View::Transcript { from, .. } => from.clone(),
                        other => Some(Box::new(other.clone())),
                    };
                    cx.nav.go(View::Transcript {
                        meeting_id,
                        segment_id: None,
                        highlight: None,
                        from,
                    });
                }
            }
            // Clears the search box (cancelling a pending debounce) so the
            // box matches the unfiltered list.
            BarAction::Nav(NavItem::Meetings) => cx.go_meetings_clear_search(),
            BarAction::Nav(NavItem::People) => cx.nav.go(View::People),
            BarAction::Nav(NavItem::Settings) => cx.nav.go(View::Settings),
            // Minimize and close are performed by `app.rs`, which runs
            // `will_hide()` first.
            BarAction::Minimize => out.minimize = true,
            BarAction::Maximize => cx.window.toggle_maximize(),
            BarAction::Close => out.close = true,
        }
    }

    let response = bar.response;
    if response.drag_started_by(egui::PointerButton::Primary) {
        let screen = ui.ctx().content_rect();
        let press = ui.input(|i| i.pointer.press_origin());
        if press.is_some_and(|p| press_drags_window(p, screen)) {
            // Our own title-bar drag turns the tray flyout into a window.
            cx.flyout.pin();
            if let Err(error) = cx.window.start_drag() {
                cx.toasts.error("Could not move the window", error);
            }
        }
    }
    if response.double_clicked() {
        cx.window.toggle_maximize();
    }
    out
}

/// Lay out and draw the bar's units (flex-wrap, see the module docs).
fn draw_bar(ui: &mut egui::Ui, model: &BarModel) -> DrawnBar {
    let column_gap = if narrow(ui) { 8.0 } else { 12.0 };
    let units = &model.units;
    let mut actions = Vec::new();
    #[cfg(test)]
    let mut rects = Vec::new();

    let widths_id = ui.id().with("topbar-unit-widths");
    let mut measured: MeasuredWidths = ui.data(|d| d.get_temp(widths_id).unwrap_or_default());
    let widths_of = |m: &MeasuredWidths| -> Vec<f32> {
        units
            .iter()
            .map(|&u| {
                // `.window-controls { margin-left: 4px }` is part of its
                // outer size.
                m.get(u)
                    + if u == Unit::Controls {
                        CONTROLS_MARGIN
                    } else {
                        0.0
                    }
            })
            .collect()
    };

    // The bar's own response is registered behind the widgets, so buttons
    // keep their clicks and only the background drags the window. The
    // padding sits inside the sensing scope: like the web header's mouse
    // handlers, the whole bar drags, padding included.
    let bar = ui.scope_builder(
        egui::UiBuilder::new().sense(Sense::click_and_drag()),
        |ui| {
            Frame::new().inner_margin(BAR_PADDING).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                let available = ui.available_width();
                let lines = wrap_lines(&widths_of(&measured), available, column_gap);
                let heights_of = |m: &MeasuredWidths| -> Vec<f32> {
                    lines
                        .iter()
                        .map(|line| m.line_height(&units[line.clone()]))
                        .collect()
                };
                let heights = heights_of(&measured);
                ui.spacing_mut().item_spacing.y = ROW_GAP;
                for (line, &line_height) in lines.iter().zip(&heights) {
                    let line_units = &units[line.clone()];
                    // A line exactly as tall as its tallest unit, every unit
                    // centred on it (a `ui.horizontal` starts 30 pt tall and
                    // grows downward, leaving taller units off-centre).
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), line_height),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            ui.spacing_mut().item_spacing.x = column_gap;
                            let nav_at = line_units.iter().position(|u| *u == Unit::Nav);
                            for &unit in &line_units[..nav_at.unwrap_or(line_units.len())] {
                                if unit == Unit::Controls {
                                    // Wrapped without the nav, the controls start
                                    // their line, after their 4 px margin.
                                    ui.add_space(CONTROLS_MARGIN);
                                }
                                let rect = draw_unit(ui, unit, model, &mut actions);
                                measured.set(unit, rect.size());
                                #[cfg(test)]
                                rects.push((unit, rect));
                            }
                            if let Some(at) = nav_at {
                                // `margin-left: auto`: the rest of the line hugs
                                // the right edge (drawn right to left).
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.spacing_mut().item_spacing.x = column_gap + CONTROLS_MARGIN;
                                    for &unit in line_units[at..].iter().rev() {
                                        let rect = draw_unit(ui, unit, model, &mut actions);
                                        measured.set(unit, rect.size());
                                        #[cfg(test)]
                                        rects.push((unit, rect));
                                    }
                                });
                            }
                        },
                    );
                }
                // A new measurement moved a unit to another line or changed
                // a line's height: redo the pass so the stale layout is
                // never shown.
                let rewrapped = wrap_lines(&widths_of(&measured), available, column_gap) != lines;
                let resized = heights_of(&measured)
                    .iter()
                    .zip(&heights)
                    .any(|(a, b)| (a - b).abs() > 0.25);
                if rewrapped || resized {
                    ui.ctx().request_discard("top bar layout changed");
                }
            });
        },
    );
    ui.data_mut(|d| d.insert_temp(widths_id, measured));
    DrawnBar {
        response: bar.response,
        actions,
        #[cfg(test)]
        rects,
    }
}

/// Draw one wrap unit; returns its rect.
fn draw_unit(
    ui: &mut egui::Ui,
    unit: Unit,
    model: &BarModel,
    actions: &mut Vec<BarAction>,
) -> Rect {
    let mut act = |response: &egui::Response, action: BarAction| {
        if response.clicked() {
            actions.push(action);
        }
    };
    match unit {
        Unit::Record => {
            let response = record_button(ui, model);
            act(&response, BarAction::Record);
            response.rect
        }
        Unit::Bookmark => {
            let response = bookmark_button(ui, &model.bookmark_tooltip);
            act(&response, BarAction::Bookmark);
            response.rect
        }
        Unit::Captions => {
            let response = captions_button(ui);
            act(&response, BarAction::ShowCaptions);
            response.rect
        }
        Unit::Pill => {
            let response = status_pill(ui, model);
            act(&response, BarAction::OpenTranscript);
            response.rect
        }
        Unit::Nav => nav_group(ui, model.active, actions),
        Unit::Controls => window_controls(ui, actions),
    }
}

/// The order to draw a group's `n` items in: reversed inside a
/// right-to-left layout, so they still read left to right.
fn draw_order(ui: &egui::Ui, n: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    if ui.layout().main_dir() == egui::Direction::RightToLeft {
        order.reverse();
    }
    order
}

/// Record (idle) / Stop (recording) / spinner (stop in flight). Disabled
/// while a start or stop is in flight; both go through the ordered
/// rec-control worker, and a failure becomes an in-app error toast
/// (`app.rs`, on the reply).
fn record_button(ui: &mut egui::Ui, model: &BarModel) -> egui::Response {
    let recording = model.recording;
    let label = if recording {
        "Stop recording"
    } else {
        "Start recording"
    };
    let spinning = recording && model.busy;
    let content = if spinning {
        // Keeps the button's size; the spinner is painted over it.
        icons::icon(ph::CIRCLE_NOTCH, RECORD_ICON).color(Color32::TRANSPARENT)
    } else if recording {
        icons::icon(ph::STOP, RECORD_ICON)
    } else {
        icons::icon(ph::RECORD, RECORD_ICON)
    };
    ui.add_enabled_ui(!model.busy, |ui| {
        let response = widgets::btn_record(ui, recording, content, RECORD_ICON, label);
        if spinning {
            icons::paint_spinner(ui, response.rect, RECORD_ICON, WHITE);
        }
        response
    })
    .inner
}

/// "Bookmark this moment (CTRL+ALT+B)".
fn bookmark_button(ui: &mut egui::Ui, tooltip: &str) -> egui::Response {
    let response = widgets::btn_icon_only(ui, icons::icon(ph::BOOKMARK_SIMPLE, 16.0), 16.0)
        .on_hover_text(tooltip);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Bookmark this moment")
    });
    response
}

fn bookmark_tooltip(hotkey: Option<&str>) -> String {
    match hotkey {
        Some(combo) => format!("Bookmark this moment ({})", combo.to_uppercase()),
        None => "Bookmark this moment".to_owned(),
    }
}

/// Re-open (or focus) the captions overlay.
fn captions_button(ui: &mut egui::Ui) -> egui::Response {
    let label = "Show the caption overlay";
    let response = widgets::btn_icon_only(ui, icons::icon(ph::CLOSED_CAPTIONING, 16.0), 16.0)
        .on_hover_text(label);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    response
}

/// What the status pill shows (map-ui-shell §4, first match wins).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PillState {
    /// Recording with a stop in flight.
    Stopping,
    Recording {
        elapsed: String,
        warning: Option<String>,
    },
    Busy(String),
    Idle,
}

impl PillState {
    pub(crate) fn of(shell: &ShellModel) -> PillState {
        let recording = shell.recording();
        if recording && shell.recording_action {
            return PillState::Stopping;
        }
        if recording {
            return PillState::Recording {
                elapsed: fmt_elapsed(shell.level.as_ref().map_or(0, |l| l.elapsed_ms)),
                warning: shell.health.as_ref().and_then(capture_warning),
            };
        }
        let queued = shell.status.as_ref().map_or(0, |s| s.queue_len);
        let suffix = if queued > 0 {
            format!(" · {queued} queued")
        } else {
            String::new()
        };
        if let Some(p) = &shell.progress {
            return PillState::Busy(format!(
                "Transcribing ({}) {}%{suffix}",
                p.stage,
                p.pct.round()
            ));
        }
        if let Some(status) = shell
            .status
            .as_ref()
            .filter(|s| s.transcribing_meeting_id.is_some())
        {
            // Opened mid-job: the durable progress snapshot from the core.
            let text = match &status.processing_stage {
                None => "Transcribing…".to_owned(),
                Some(stage) => match status.processing_pct {
                    Some(pct) => format!("Transcribing ({stage}) {}%", pct.round()),
                    None => format!("Transcribing ({stage})"),
                },
            };
            return PillState::Busy(format!("{text}{suffix}"));
        }
        if queued > 0 {
            // Between jobs: the next queued meeting has no progress yet.
            return PillState::Busy(format!("Transcribing…{suffix}"));
        }
        PillState::Idle
    }

    /// The live-region text. The recording pill was not a live region in
    /// the web UI (its time changes every second), so it announces the
    /// state and any warning, not the time.
    fn announcement(&self) -> String {
        match self {
            PillState::Stopping => "Stopping…".into(),
            PillState::Recording { warning, .. } => match warning {
                Some(w) => format!("Recording. {w}"),
                None => "Recording".into(),
            },
            PillState::Busy(text) => text.clone(),
            PillState::Idle => "Idle".into(),
        }
    }
}

/// The status pill; only the recording pill is clickable.
fn status_pill(ui: &mut egui::Ui, model: &BarModel) -> egui::Response {
    widgets::announce(
        ui.ctx(),
        egui::Id::new("witness-status-pill-live"),
        &model.pill.announcement(),
        Politeness::Polite,
    );
    match &model.pill {
        PillState::Stopping => widgets::pill(ui, PillKind::Busy, "Stopping…"),
        PillState::Busy(text) => widgets::pill(ui, PillKind::Busy, text.as_str()),
        PillState::Idle => widgets::pill(ui, PillKind::Idle, "Idle"),
        PillState::Recording { elapsed, warning } => {
            recording_pill(ui, model, elapsed, warning.as_deref())
        }
    }
}

/// The level meter's tooltip and accessible name.
const METER_LABEL: &str = "Microphone and system audio levels";

/// The clickable recording pill: pulsing dot, "Recording" (wide only),
/// elapsed time, ⚠ on a capture warning, and the level meter (wide only).
/// A click opens the recording's transcript.
fn recording_pill(
    ui: &mut egui::Ui,
    model: &BarModel,
    elapsed: &str,
    warning: Option<&str>,
) -> egui::Response {
    let wide = !narrow(ui);
    let font = theme::regular(13.0);
    // Laid out by hand and allocated at its exact size (see
    // `widgets::pill_shell`): dot, texts and meter 8 pt apart, centred.
    let mut texts = Vec::new();
    if wide {
        texts.push("Recording");
    }
    texts.push(elapsed);
    if warning.is_some() {
        texts.push("⚠");
    }
    let galleys: Vec<_> = texts
        .iter()
        .map(|t| {
            ui.painter()
                .layout_no_wrap((*t).to_owned(), font.clone(), REC_LIGHT)
        })
        .collect();
    let meter_size = egui::vec2(METER_WIDTH, METER_BAR * 2.0 + 2.0);
    let mut widths = vec![8.0];
    widths.extend(galleys.iter().map(|g| g.size().x));
    if wide {
        widths.push(meter_size.x);
    }
    let gap = 8.0;
    let content = egui::vec2(
        widths.iter().sum::<f32>() + gap * (widths.len() - 1) as f32,
        galleys
            .iter()
            .map(|g| g.size().y)
            .fold(meter_size.y, f32::max),
    );
    let rect = widgets::pill_shell(ui, PillKind::Recording, content);
    let mid = rect.center().y;
    let mut x = rect.left() + widgets::pill_padding(ui).x;
    let alpha = icons::pulse_alpha(ui.ctx());
    ui.painter()
        .circle_filled(egui::pos2(x + 4.0, mid), 4.0, REC.gamma_multiply(alpha));
    x += 8.0 + gap;
    for galley in galleys {
        let size = galley.size();
        ui.painter()
            .galley(egui::pos2(x, mid - size.y / 2.0), galley, REC_LIGHT);
        x += size.x + gap;
    }
    let meter = wide.then(|| {
        let meter = Rect::from_min_size(egui::pos2(x, mid - meter_size.y / 2.0), meter_size);
        level_meter(ui, meter, model);
        meter
    });
    let response = ui
        .interact(rect, ui.id().with("recording-pill"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    // The pill's click response sits above the hover-only meter, so the
    // meter's own tooltip would never show: pick the text by where the
    // pointer is (the web meter's `title` won over the pill's inside it).
    let over_meter =
        meter.is_some_and(|meter| response.hover_pos().is_some_and(|p| meter.contains(p)));
    let tooltip = if over_meter {
        METER_LABEL
    } else {
        warning.unwrap_or("Open live captions")
    };
    let response = response.on_hover_text(tooltip);
    let name = format!(
        "{elapsed} recording. {}",
        warning.unwrap_or("Open live transcript")
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &name));
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(2.0),
            CornerRadius::same(255),
            Stroke::new(2.0, theme::FOCUS),
            egui::StrokeKind::Outside,
        );
    }
    response
}

/// Two stacked bars in `rect`: microphone (accent) and system audio (ok),
/// each `min(100, rms·300)` % of 48 pt, at least 2 pt, easing over 0.15 s.
fn level_meter(ui: &mut egui::Ui, rect: Rect, model: &BarModel) {
    let response = ui.interact(rect, ui.id().with("level-meter"), Sense::hover());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, METER_LABEL));
    let (mic, loopback) = model.levels;
    for (row, (rms, color)) in [(mic, ACCENT), (loopback, OK)].into_iter().enumerate() {
        let target = meter_width(rms);
        let width = if model.animate_meter {
            ui.ctx().animate_value_with_time(
                ui.id().with(("level-bar", row)),
                target,
                METER_TRANSITION,
            )
        } else {
            target
        };
        let top = rect.top() + row as f32 * (METER_BAR + 2.0);
        let bar = Rect::from_min_size(egui::pos2(rect.left(), top), egui::vec2(width, METER_BAR));
        ui.painter().rect_filled(bar, CornerRadius::same(2), color);
    }
}

/// A level bar's width in points for an RMS level (0..1).
pub(crate) fn meter_width(rms: f32) -> f32 {
    let percent = if rms.is_finite() {
        (rms * 300.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    (percent / 100.0 * METER_WIDTH).max(2.0)
}

/// Meetings, People and the Settings gear; returns the group's rect.
fn nav_group(ui: &mut egui::Ui, active: Option<NavItem>, actions: &mut Vec<BarAction>) -> Rect {
    group(ui, NAV_GAP, |ui, rect| {
        for item in draw_order(ui, 3) {
            let (item, response) = match item {
                0 => (
                    NavItem::Meetings,
                    widgets::nav_btn(ui, "Meetings", active == Some(NavItem::Meetings)),
                ),
                1 => (
                    NavItem::People,
                    widgets::nav_btn(ui, "People", active == Some(NavItem::People)),
                ),
                _ => (
                    NavItem::Settings,
                    widgets::nav_icon_btn(
                        ui,
                        ph::GEAR_SIX,
                        active == Some(NavItem::Settings),
                        "Settings",
                    ),
                ),
            };
            *rect = rect.union(response.rect);
            if response.clicked() {
                actions.push(BarAction::Nav(item));
            }
        }
    })
}

/// A group of buttons `gap` apart in the line's own layout and full height,
/// so each centres on the line (a nested `ui.horizontal` starts 30 pt tall
/// and grows downward). Returns the union of what `add` reports: the
/// buttons themselves, never the line-tall frame around them (that would
/// feed back into the line height).
fn group(ui: &mut egui::Ui, gap: f32, add: impl FnOnce(&mut egui::Ui, &mut Rect)) -> Rect {
    let mut rect = Rect::NOTHING;
    let layout = *ui.layout();
    ui.allocate_ui_with_layout(ui.available_size(), layout, |ui| {
        ui.spacing_mut().item_spacing.x = gap;
        add(ui, &mut rect);
    });
    rect
}

/// Minimize, maximize (always the square glyph) and close-to-tray; returns
/// the group's rect. Hidden in tray-flyout mode.
fn window_controls(ui: &mut egui::Ui, actions: &mut Vec<BarAction>) -> Rect {
    group(ui, CONTROLS_GAP, |ui, rect| {
        for item in draw_order(ui, 3) {
            let (action, response) = match item {
                0 => (
                    BarAction::Minimize,
                    widgets::win_btn(ui, ph::MINUS, 14.0, "Minimize", false),
                ),
                1 => (
                    BarAction::Maximize,
                    widgets::win_btn(ui, ph::SQUARE, 12.0, "Maximize", false),
                ),
                _ => (
                    BarAction::Close,
                    widgets::win_btn(ui, ph::X, 14.0, "Close to tray", true),
                ),
            };
            *rect = rect.union(response.rect);
            if response.clicked() {
                actions.push(action);
            }
        }
    })
}

/// `.topbar { padding: 10px 14px }`, applied inside the bar's drag scope.
const BAR_PADDING: Margin = Margin::symmetric(14, 10);

/// The top bar's panel frame: the raised fill only. The padding is part of
/// the bar itself ([`BAR_PADDING`]), so the drag area covers it. `app.rs`
/// paints the 1 px bottom border with [`paint_bottom_border`].
pub fn frame() -> egui::Frame {
    egui::Frame::new().fill(BG_RAISED)
}

/// The bar's 1 px bottom border.
pub fn paint_bottom_border(ui: &egui::Ui, rect: egui::Rect) {
    ui.painter().hline(
        rect.x_range(),
        rect.bottom() - 0.5,
        Stroke::new(1.0, BORDER),
    );
}

// ---------------------------------------------------------------------------
// Resize edges (shared with the captions overlay)

/// The eight resize zones of a window whose content fills `rect`: 12 pt
/// corner squares and 6 pt edge strips between them (no overlap).
pub(crate) fn resize_zones(rect: Rect) -> [(Rect, ResizeDirection); 8] {
    let (e, c) = (RESIZE_EDGE, RESIZE_CORNER);
    let (l, r, t, b) = (rect.left(), rect.right(), rect.top(), rect.bottom());
    let zone = |x0: f32, y0: f32, x1: f32, y1: f32| {
        Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1.max(x0), y1.max(y0)))
    };
    [
        (zone(l, t, l + c, t + c), ResizeDirection::NorthWest),
        (zone(r - c, t, r, t + c), ResizeDirection::NorthEast),
        (zone(l, b - c, l + c, b), ResizeDirection::SouthWest),
        (zone(r - c, b - c, r, b), ResizeDirection::SouthEast),
        (zone(l + c, t, r - c, t + e), ResizeDirection::North),
        (zone(l + c, b - e, r - c, b), ResizeDirection::South),
        (zone(l, t + c, l + e, b - c), ResizeDirection::West),
        (zone(r - e, t + c, r, b - c), ResizeDirection::East),
    ]
}

fn resize_cursor(dir: ResizeDirection) -> CursorIcon {
    match dir {
        ResizeDirection::North | ResizeDirection::South => CursorIcon::ResizeVertical,
        ResizeDirection::East | ResizeDirection::West => CursorIcon::ResizeHorizontal,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::ResizeNwSe,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::ResizeNeSw,
    }
}

/// Invisible resize zones for the current viewport of an undecorated
/// window (the OS draws no resize border). Each zone is a tiny
/// `Order::Foreground` area, so it wins the hit test over whatever lies
/// beneath (title bar, player bar) without blocking anything else. Returns
/// the direction to hand to the OS sizing loop when the primary button
/// went down on a zone this pass.
pub(crate) fn edge_resize(ctx: &egui::Context, id_salt: &str) -> Option<ResizeDirection> {
    let rect = ctx.content_rect();
    let pressed = ctx.input(|i| i.pointer.primary_pressed());
    let mut hit = None;
    for (index, (zone, dir)) in resize_zones(rect).into_iter().enumerate() {
        if zone.width() <= 0.0 || zone.height() <= 0.0 {
            continue;
        }
        let area = egui::Area::new(egui::Id::new((id_salt, "resize-zone", index)))
            .order(Order::Foreground)
            .fixed_pos(zone.min)
            .movable(false)
            .constrain(false)
            .fade_in(false)
            .show(ctx, |ui| {
                ui.allocate_exact_size(zone.size(), Sense::drag()).1
            });
        let response = area.inner.on_hover_cursor(resize_cursor(dir));
        if pressed && response.contains_pointer() {
            hit = Some(dir);
        }
    }
    hit
}

/// Invisible 6 pt edge / 12 pt corner resize zones on all four sides of
/// the main window; a press hands off to the OS sizing loop (Aero Snap
/// works). Not while maximized. A resize pins the tray flyout.
pub fn resize_edges(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let ctx = ui.ctx().clone();
    if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
        return;
    }
    if let Some(dir) = edge_resize(&ctx, "main") {
        cx.flyout.pin();
        if let Err(error) = cx.window.begin_resize(dir) {
            cx.toasts.error("Could not resize the window", error);
        }
    }
}

// ---------------------------------------------------------------------------
// Banners

/// The capture-health warning under the bar while recording (role=alert,
/// selectable, 8/14 margin).
pub fn health_banner(ui: &mut egui::Ui, cx: &mut UiCtx) {
    if !cx.shell.recording() {
        return;
    }
    let Some(warning) = cx.shell.health.as_ref().and_then(capture_warning) else {
        return;
    };
    egui::Panel::top("health-banner")
        .frame(Frame::new().fill(BG).inner_margin(Margin::symmetric(14, 8)))
        .resizable(false)
        .show_separator_line(false)
        .show(ui, |ui| {
            widgets::inline_warning(ui, &warning);
            widgets::announce(
                ui.ctx(),
                egui::Id::new("witness-health-alert"),
                &warning,
                Politeness::Assertive,
            );
        });
}

/// `file <i> of <n> — <pct>%`, or `… — downloading…` without a total.
pub(crate) fn download_text(p: &ModelDownloadProgress) -> String {
    let amount = match p.total_bytes.filter(|t| *t > 0) {
        Some(total) => format!(
            "{}%",
            ((p.downloaded_bytes as f64 / total as f64) * 100.0).round()
        ),
        None => "downloading…".to_owned(),
    };
    format!("file {} of {} — {amount}", p.file_index, p.file_count)
}

/// `.model-banner span:first-child { flex: 1; min-width: 200px }`, gap 12.
const BANNER_TEXT_MIN: f32 = 200.0;
const BANNER_GAP: f32 = 12.0;
/// The model banner's text: a bold lead, then the explanation.
const BANNER_TITLE: &str = "Transcription models not installed.";
const BANNER_BODY: &str = " Recording works, but nothing gets transcribed. One-time download, \
                           ~3.5 GB (Parakeet + speaker models).";

/// The first-run "Transcription models not installed." banner: Download
/// now (or the download progress) and Later, which persists the dismissal.
pub fn model_banner(ui: &mut egui::Ui, cx: &mut UiCtx) {
    if !cx.shell.models_missing {
        return;
    }
    let progress = cx
        .shell
        .download
        .as_ref()
        .filter(|d| d.error.is_none())
        .map(download_text);
    // `.model-banner` was `role="status"`: announced when it appears.
    widgets::announce(
        ui.ctx(),
        egui::Id::new("witness-model-banner"),
        &format!("{BANNER_TITLE}{BANNER_BODY}"),
        Politeness::Polite,
    );
    widgets::centered_column(ui, 900.0, |ui| {
        Frame::new()
            .fill(BANNER_BG)
            .stroke(Stroke::new(1.0, BANNER_BORDER))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                banner_contents(ui, cx, progress.as_deref());
            });
    });
    ui.add_space(14.0);
}

fn banner_text(width: f32) -> egui::text::LayoutJob {
    let format = |font_id: FontId| egui::TextFormat {
        font_id,
        color: TEXT,
        ..Default::default()
    };
    let mut job = egui::text::LayoutJob::default();
    job.append(BANNER_TITLE, 0.0, format(theme::semibold(13.0)));
    job.append(BANNER_BODY, 0.0, format(theme::regular(13.0)));
    job.wrap.max_width = width;
    job
}

fn banner_contents(ui: &mut egui::Ui, cx: &mut UiCtx, progress: Option<&str>) {
    let small = theme::regular(13.0);
    // Measure the fixed items to break lines the way CSS flex-wrap does.
    let text_width = |ui: &egui::Ui, text: &str, font: FontId| {
        ui.painter()
            .layout_no_wrap(text.to_owned(), font, TEXT)
            .size()
            .x
    };
    let button_pad = if narrow(ui) { 20.0 } else { 28.0 };
    let middle = match progress {
        Some(text) => text_width(ui, text, small.clone()),
        None => text_width(ui, "Download now", theme::regular(theme::BASE_SIZE)) + button_pad,
    };
    let later = text_width(ui, "Later", theme::regular(theme::BASE_SIZE)) + button_pad;
    let available = ui.available_width();
    let lines = wrap_lines(&[BANNER_TEXT_MIN, middle, later], available, BANNER_GAP);

    ui.spacing_mut().item_spacing = egui::vec2(BANNER_GAP, 8.0);
    for line in lines {
        // flex: 1 — the text takes whatever its line's other items leave.
        let others: f32 = line
            .clone()
            .filter(|&i| i != 0)
            .map(|i| if i == 1 { middle } else { later } + BANNER_GAP)
            .sum();
        ui.horizontal(|ui| {
            for item in line {
                match item {
                    0 => {
                        let width = (available - others).max(BANNER_TEXT_MIN.min(available));
                        ui.allocate_ui(egui::vec2(width, 0.0), |ui| {
                            ui.set_width(width);
                            ui.add(egui::Label::new(banner_text(width)).wrap());
                        });
                    }
                    1 => match progress {
                        Some(text) => {
                            ui.label(RichText::new(text).font(small.clone()).color(TEXT_MUTED));
                            widgets::announce(
                                ui.ctx(),
                                egui::Id::new("witness-model-download-live"),
                                text,
                                Politeness::Polite,
                            );
                        }
                        None => {
                            if widgets::btn(ui, "Download now").clicked() {
                                cx.tasks.spawn_op("model-download", |c| OpDone::Notice {
                                    notice: c.download_models(Engine::Parakeet).err().map(
                                        |error| {
                                            Notice::error("Could not start model download", error)
                                        },
                                    ),
                                    refresh: false,
                                });
                            }
                        }
                    },
                    _ => {
                        if widgets::btn_ghost(ui, "Later").clicked() {
                            cx.prefs.model_banner_dismissed = true;
                            cx.shell.models_missing = false;
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use witness_core::events::{RecordingLevel, TranscriptionProgress, WatcherStatus};
    use witness_core::recorder::RecordingHealth;
    use witness_core::service::AppStatus;

    fn status(recording: bool) -> AppStatus {
        AppStatus {
            recording,
            meeting_id: recording.then_some(7),
            recording_since: None,
            live_captions: false,
            transcribing_meeting_id: None,
            queue_len: 0,
            processing_stage: None,
            processing_pct: None,
            watcher: WatcherStatus {
                enabled: true,
                teams_key_found: true,
                mic_in_use: false,
                suppressed: false,
            },
        }
    }

    #[test]
    fn wrap_lines_behaves_like_flex_wrap() {
        assert_eq!(wrap_lines(&[10.0, 10.0, 10.0], 100.0, 8.0), vec![0..3]);
        assert_eq!(
            wrap_lines(&[40.0, 40.0, 40.0], 100.0, 8.0),
            vec![0..2, 2..3]
        );
        // An item wider than the row still gets its own line.
        assert_eq!(wrap_lines(&[150.0, 10.0], 100.0, 8.0), vec![0..1, 1..2]);
        assert_eq!(wrap_lines(&[], 100.0, 8.0), Vec::<Range<usize>>::new());
        // Exactly full fits.
        assert_eq!(wrap_lines(&[46.0, 46.0], 100.0, 8.0), vec![0..2]);
    }

    fn bar_model(recording: bool) -> BarModel {
        BarModel {
            units: bar_units(recording, recording, false),
            recording,
            busy: false,
            pill: if recording {
                PillState::Recording {
                    elapsed: "12:34".into(),
                    warning: None,
                }
            } else {
                PillState::Idle
            },
            bookmark_tooltip: bookmark_tooltip(Some("ctrl+alt+b")),
            levels: (0.1, 0.05),
            animate_meter: false,
            active: Some(NavItem::Meetings),
        }
    }

    /// Run the real bar (measure + discard included) in a `width` × 620
    /// window for a few frames, like `app.rs` does; returns the last pass.
    fn lay_out_bar(width: f32, model: &BarModel) -> (Rect, Vec<(Unit, Rect)>) {
        let ctx = theme::test_context();
        let mut last = (Rect::NOTHING, Vec::new());
        for _ in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 620.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                egui::Panel::top("topbar")
                    .frame(frame())
                    .resizable(false)
                    .show_separator_line(false)
                    .show(ui, |ui| {
                        let bar = draw_bar(ui, model);
                        last = (bar.response.rect, bar.rects);
                    });
            });
            output.textures_delta.clear();
        }
        last
    }

    fn rect_of(rects: &[(Unit, Rect)], unit: Unit) -> Rect {
        rects.iter().find(|(u, _)| *u == unit).unwrap().1
    }

    #[test]
    fn the_bar_wraps_at_460_while_recording_but_not_when_idle() {
        let (bar, idle) = lay_out_bar(460.0, &bar_model(false));
        assert_eq!(idle.len(), 4, "{idle:?}");
        let row = rect_of(&idle, Unit::Record);
        for (unit, rect) in &idle {
            assert!(
                rect.top() < row.bottom() && rect.bottom() > row.top(),
                "{unit:?} wrapped: {idle:?}"
            );
        }
        // Window controls hug the right edge, inside the 14 pt padding.
        assert!((rect_of(&idle, Unit::Controls).right() - (460.0 - 14.0)).abs() < 1.0);
        // The drag area is the whole bar, padding included.
        assert_eq!(bar.left(), 0.0);
        assert_eq!(bar.top(), 0.0);
        assert_eq!(bar.right(), 460.0);
        assert!((bar.bottom() - (rect_of(&idle, Unit::Nav).bottom() + 10.0)).abs() < 1.0);

        let (_, rec) = lay_out_bar(460.0, &bar_model(true));
        assert_eq!(rec.len(), 6, "{rec:?}");
        let first = rect_of(&rec, Unit::Record).top();
        for unit in [Unit::Bookmark, Unit::Captions, Unit::Pill, Unit::Nav] {
            assert!(
                (rect_of(&rec, unit).top() - first).abs() < 8.0,
                "{unit:?} should stay on the first line: {rec:?}"
            );
        }
        let controls = rect_of(&rec, Unit::Controls);
        assert!(
            controls.top() > rect_of(&rec, Unit::Nav).bottom(),
            "the window controls wrap to a second line: {rec:?}"
        );
        // Only the nav has `margin-left: auto`: alone on their line, the
        // controls start it, after the 14 pt padding and their 4 pt margin.
        assert!(
            (controls.left() - (14.0 + CONTROLS_MARGIN)).abs() < 1.0,
            "{controls:?}"
        );

        // Wide enough, the recording bar is one line again.
        let (_, wide) = lay_out_bar(900.0, &bar_model(true));
        let (row, controls) = (rect_of(&wide, Unit::Record), rect_of(&wide, Unit::Controls));
        assert!(controls.top() < row.bottom(), "{wide:?}");
    }

    #[test]
    fn bar_units_centre_on_one_line_and_pills_hug_their_text() {
        for (width, recording) in [(460.0, false), (460.0, true), (900.0, true)] {
            let (bar, rects) = lay_out_bar(width, &bar_model(recording));
            let line = rect_of(&rects, Unit::Record);
            for (unit, rect) in &rects {
                if rect.top() >= line.bottom() {
                    continue; // wrapped to the second line
                }
                assert!(
                    (rect.center().y - line.center().y).abs() < 0.6,
                    "{unit:?} off the line's centre at {width}: {rects:?}"
                );
            }
            // `.pill`: 13 px text + 5 px padding (~27), not the row height.
            let pill = rect_of(&rects, Unit::Pill);
            assert!((26.0..=28.5).contains(&pill.height()), "pill {pill:?}");
            // `.btn-with-icon`: 17 px SVG + 12 padding + 2 border = 31.
            assert!((line.height() - 31.0).abs() < 0.6, "record {line:?}");
            if recording {
                let bookmark = rect_of(&rects, Unit::Bookmark);
                assert!((bookmark.height() - 30.0).abs() < 0.6, "{bookmark:?}");
            }
            // One line: 10 + the 33 pt nav buttons + 10 (0.2.x: ~54 px).
            if width > 800.0 || !recording {
                assert!(bar.height() < 55.0, "bar {bar:?} at {width}");
            }
        }
    }

    #[test]
    fn presses_near_the_top_left_and_right_edges_do_not_drag() {
        let screen = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(460.0, 620.0));
        assert!(press_drags_window(egui::pos2(200.0, 20.0), screen));
        assert!(press_drags_window(egui::pos2(200.0, 8.0), screen));
        assert!(press_drags_window(egui::pos2(200.0, 50.0), screen));
        assert!(!press_drags_window(egui::pos2(200.0, 7.0), screen));
        assert!(!press_drags_window(egui::pos2(5.0, 30.0), screen));
        assert!(!press_drags_window(egui::pos2(455.0, 30.0), screen));
    }

    #[test]
    fn pill_follows_its_priority_order() {
        let mut shell = ShellModel::new();
        assert_eq!(PillState::of(&shell), PillState::Idle);

        let mut s = status(false);
        s.queue_len = 2;
        shell.set_status(s.clone());
        assert_eq!(
            PillState::of(&shell),
            PillState::Busy("Transcribing… · 2 queued".into())
        );

        s.transcribing_meeting_id = Some(3);
        shell.set_status(s.clone());
        assert_eq!(
            PillState::of(&shell),
            PillState::Busy("Transcribing… · 2 queued".into())
        );
        s.processing_stage = Some("asr".into());
        shell.set_status(s.clone());
        assert_eq!(
            PillState::of(&shell),
            PillState::Busy("Transcribing (asr) · 2 queued".into())
        );
        s.processing_pct = Some(44.6);
        shell.set_status(s.clone());
        assert_eq!(
            PillState::of(&shell),
            PillState::Busy("Transcribing (asr) 45% · 2 queued".into())
        );

        shell.progress = Some(TranscriptionProgress {
            meeting_id: 3,
            stage: "diarize",
            pct: 12.2,
        });
        assert_eq!(
            PillState::of(&shell),
            PillState::Busy("Transcribing (diarize) 12% · 2 queued".into())
        );
        s.queue_len = 0;
        shell.set_status(s);
        assert_eq!(
            PillState::of(&shell),
            PillState::Busy("Transcribing (diarize) 12%".into())
        );

        shell.set_status(status(true));
        shell.level = Some(RecordingLevel {
            mic_rms: 0.1,
            loopback_rms: 0.0,
            elapsed_ms: 75_000,
        });
        assert_eq!(
            PillState::of(&shell),
            PillState::Recording {
                elapsed: "1:15".into(),
                warning: None
            }
        );
        let mut health = RecordingHealth::default();
        health.mic.device_loss_count = 1;
        shell.health = Some(health);
        let recording = PillState::of(&shell);
        assert_eq!(
            recording,
            PillState::Recording {
                elapsed: "1:15".into(),
                warning: Some("Microphone device was lost during recording".into())
            }
        );
        assert_eq!(
            recording.announcement(),
            "Recording. Microphone device was lost during recording"
        );
        shell.recording_action = true;
        assert_eq!(PillState::of(&shell), PillState::Stopping);
        assert_eq!(PillState::Stopping.announcement(), "Stopping…");
    }

    #[test]
    fn meter_width_is_clamped() {
        assert_eq!(meter_width(0.0), 2.0);
        assert_eq!(meter_width(1.0), METER_WIDTH);
        assert!((meter_width(0.1) - 14.4).abs() < 1e-3);
        assert_eq!(meter_width(f32::NAN), 2.0);
    }

    #[test]
    fn resize_zones_cover_edges_and_corners_without_overlap() {
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(460.0, 620.0));
        let zones = resize_zones(rect);
        let at = |x: f32, y: f32| -> Vec<ResizeDirection> {
            zones
                .iter()
                .filter(|(z, _)| z.contains(egui::pos2(x, y)))
                .map(|(_, d)| *d)
                .collect()
        };
        assert_eq!(at(2.0, 2.0), vec![ResizeDirection::NorthWest]);
        assert_eq!(at(450.0, 2.0), vec![ResizeDirection::NorthEast]);
        assert_eq!(at(2.0, 615.0), vec![ResizeDirection::SouthWest]);
        assert_eq!(at(458.0, 619.0), vec![ResizeDirection::SouthEast]);
        assert_eq!(at(200.0, 3.0), vec![ResizeDirection::North]);
        assert_eq!(at(200.0, 617.0), vec![ResizeDirection::South]);
        assert_eq!(at(3.0, 300.0), vec![ResizeDirection::West]);
        assert_eq!(at(457.0, 300.0), vec![ResizeDirection::East]);
        assert!(at(200.0, 300.0).is_empty());
        assert!(at(200.0, 7.0).is_empty(), "below the 6 pt top strip");
    }

    #[test]
    fn bookmark_tooltip_shows_the_bound_combo() {
        assert_eq!(
            bookmark_tooltip(Some("ctrl+alt+b")),
            "Bookmark this moment (CTRL+ALT+B)"
        );
        assert_eq!(bookmark_tooltip(None), "Bookmark this moment");
    }

    #[test]
    fn download_progress_text() {
        let mut p = ModelDownloadProgress {
            model_id: "parakeet".into(),
            file: "encoder.onnx".into(),
            downloaded_bytes: 50,
            total_bytes: Some(200),
            file_index: 2,
            file_count: 5,
            done: false,
            error: None,
        };
        assert_eq!(download_text(&p), "file 2 of 5 — 25%");
        p.total_bytes = None;
        assert_eq!(download_text(&p), "file 2 of 5 — downloading…");
        p.total_bytes = Some(0);
        assert_eq!(download_text(&p), "file 2 of 5 — downloading…");
    }

    #[test]
    fn edge_resize_registers_zones_without_panicking() {
        let ctx = theme::test_context();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            assert_eq!(edge_resize(ui.ctx(), "test"), None);
        });
        output.textures_delta.clear();
    }
}
