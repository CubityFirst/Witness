//! The custom title bar (map-ui-shell §4): record/stop, bookmark and CC
//! buttons, the status pill, navigation and the window controls; plus the
//! invisible resize edges and the health and model banners below it (§5).
//!
//! The bar is one 48 pt row that never wraps: the leading actions (record,
//! and while recording bookmark + CC), the status pill, a flexible gap,
//! the Meetings / People segmented control, the settings gear and the
//! window controls. Every item's width is known before anything is drawn
//! (fixed button sizes, texts measured with the fonts), so the layout is
//! decided up front: [`degrade`] picks the first [`Degrade`] level whose
//! row fits — full, then nav icon-only, then a compact pill (dot + time),
//! then a dot-only pill. The whole bar background drags the window.

use super::format::{capture_warning, fmt_elapsed};
use super::icons::{self, ph};
use super::theme::{self, pal};
use super::widgets::{self, PillKind, Politeness};
use super::{UiCtx, View};
use crate::bridge::OpDone;
use crate::model::ShellModel;
use crate::ui::toasts::Notice;
use eframe::egui::{
    self, Align, Color32, CornerRadius, CursorIcon, FontId, Frame, Layout, Margin, Order, Rect,
    ResizeDirection, RichText, Sense, Stroke,
};
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

/// The bar's height and side padding (the right side clears the 6 pt
/// resize strip, so the close button keeps its whole hit area).
const BAR_HEIGHT: f32 = 48.0;
const PAD_LEFT: f32 = 12.0;
const PAD_RIGHT: f32 = 6.0;
/// Square icon buttons (record, bookmark, CC, settings).
const BTN: f32 = 32.0;
/// Between the leading buttons; between the nav and the gear.
const TIGHT_GAP: f32 = 4.0;
/// Between groups, and the flexible gap's minimum.
const GAP: f32 = 8.0;
/// The window controls: three 32 pt buttons, edge to edge.
const WIN_BTN: f32 = 32.0;
const CONTROLS_WIDTH: f32 = WIN_BTN * 3.0;

/// The segmented nav: a 32 pt track with 2 pt inset, 28 pt segments.
const NAV_INSET: f32 = 2.0;
const SEG_HEIGHT: f32 = BTN - NAV_INSET * 2.0;
/// Segment padding, icon size and icon–label gap; icon-only segment width.
const SEG_PAD: f32 = 10.0;
const SEG_ICON: f32 = 16.0;
const SEG_ICON_GAP: f32 = 6.0;
const SEG_ICON_ONLY: f32 = 32.0;

/// The status pill (the kit's padding and height, see [`PillGeom`]):
/// items 6 pt apart; the leading dot / spinner box; the ⚠ glyph.
const PILL_GAP: f32 = 6.0;
const PILL_LEAD: f32 = 10.0;
const PILL_WARN: f32 = 14.0;
/// Level meter: 36 pt wide, two 4 pt bars 2 pt apart, each ≥ 2 pt, 8 pt
/// after the time.
const METER_WIDTH: f32 = 36.0;
const METER_BAR: f32 = 4.0;
const METER_GAP: f32 = 8.0;
/// `.level-bar { transition: width 0.15s linear }`.
const METER_TRANSITION: f32 = 0.15;

/// Pill and nav fonts.
fn pill_font() -> FontId {
    theme::regular(13.0)
}

fn nav_font() -> FontId {
    theme::regular(13.5)
}

// ---------------------------------------------------------------------------
// Debug hooks

/// What `WITNESS_DEBUG_FAKE_RECORDING` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fake {
    /// `=1`; `=warn` with a capture warning (pill ⚠ + the health banner).
    Recording { warning: bool },
    /// `=busy`: idle, transcribing with a long status text.
    Busy,
}

const FAKE_WARNING: &str = "Microphone device was lost during recording";

/// Debug builds only, for passive snapshots: `WITNESS_DEBUG_FAKE_RECORDING=1`
/// draws the bar as if recording (12:34, live captions on, some level),
/// `=warn` adds a capture warning and its banner, `=busy` shows a
/// transcription in progress instead. Real state is untouched and the
/// bar's recording actions are ignored while faking.
#[cfg(debug_assertions)]
fn fake_recording() -> Option<Fake> {
    static FAKE: std::sync::OnceLock<Option<Fake>> = std::sync::OnceLock::new();
    *FAKE.get_or_init(
        || match std::env::var("WITNESS_DEBUG_FAKE_RECORDING").as_deref() {
            Ok("1") => Some(Fake::Recording { warning: false }),
            Ok("warn") => Some(Fake::Recording { warning: true }),
            Ok("busy") => Some(Fake::Busy),
            _ => None,
        },
    )
}

#[cfg(not(debug_assertions))]
fn fake_recording() -> Option<Fake> {
    None
}

/// Debug builds only: `WITNESS_DEBUG_MODEL_BANNER=1` shows the model banner
/// even with the models installed (for snapshots).
#[cfg(debug_assertions)]
fn fake_model_banner() -> bool {
    static FAKE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FAKE.get_or_init(|| std::env::var_os("WITNESS_DEBUG_MODEL_BANNER").is_some_and(|v| v == "1"))
}

#[cfg(not(debug_assertions))]
fn fake_model_banner() -> bool {
    false
}

// ---------------------------------------------------------------------------
// The bar

/// The highlighted navigation item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavItem {
    Meetings,
    People,
    Settings,
}

impl NavItem {
    fn glyph(self) -> &'static str {
        match self {
            NavItem::Meetings => ph::WAVEFORM,
            NavItem::People => ph::USERS,
            NavItem::Settings => ph::GEAR_SIX,
        }
    }

    fn label(self) -> &'static str {
        match self {
            NavItem::Meetings => "Meetings",
            NavItem::People => "People",
            NavItem::Settings => "Settings",
        }
    }
}

/// What the bar shows this pass. Plain data, so the bar can be laid out
/// headless in tests; [`show`] builds it from the [`UiCtx`] and performs
/// the [`BarAction`]s the pass produced.
struct BarModel {
    recording: bool,
    /// A live-caption session runs (the CC button only makes sense then;
    /// the core rejects it otherwise).
    live_captions: bool,
    /// Tray-flyout mode: no window controls.
    popup: bool,
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
        let mut model = BarModel {
            recording,
            live_captions: recording && cx.shell.live_captions_active(),
            popup: cx.flyout.is_popup(),
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
        };
        match fake_recording() {
            Some(Fake::Recording { warning }) => {
                model.recording = true;
                model.live_captions = true;
                model.busy = false;
                model.pill = PillState::Recording {
                    elapsed: "12:34".into(),
                    warning: warning.then(|| FAKE_WARNING.to_owned()),
                };
                model.levels = (0.12, 0.05);
            }
            Some(Fake::Busy) => {
                model.recording = false;
                model.live_captions = false;
                model.busy = false;
                model.pill = PillState::Busy {
                    text: "Transcribing (diarize) 45% · 2 queued".into(),
                    pct: Some(45),
                };
            }
            None => {}
        }
        model
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

impl BarAction {
    /// Acts on the recording (ignored while the debug hook fakes one).
    fn touches_recording(self) -> bool {
        matches!(
            self,
            BarAction::Record
                | BarAction::Bookmark
                | BarAction::ShowCaptions
                | BarAction::OpenTranscript
        )
    }
}

/// One item of the bar (named in the layout tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Item {
    Record,
    Bookmark,
    Captions,
    Pill,
    Nav,
    Settings,
    Controls,
}

/// One pass of the bar.
struct DrawnBar {
    /// The background's click-and-drag response (the whole bar, padding
    /// included).
    response: egui::Response,
    actions: Vec<BarAction>,
    /// Where each item landed and the level used (read by the tests).
    #[cfg(test)]
    rects: Vec<(Item, Rect)>,
    #[cfg(test)]
    level: Degrade,
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
    let faking = fake_recording().is_some();

    let mut out = TopBarOutput::default();
    for action in bar.actions {
        if faking && action.touches_recording() {
            continue;
        }
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

// ---------------------------------------------------------------------------
// Degrade levels (pure layout arithmetic)

/// How far the bar has given way to the width it has, mildest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Degrade {
    /// Nav icon + label, the whole pill (a busy pill's full text).
    Full = 0,
    /// Nav icon-only; a busy pill's text may shorten ("Transcribing 45%").
    NavIcons = 1,
    /// … and the pill as dot + time (or a short text).
    PillCompact = 2,
    /// … and the pill as a dot (or spinner) only.
    PillDot = 3,
}

impl Degrade {
    const ALL: [Degrade; 4] = [
        Degrade::Full,
        Degrade::NavIcons,
        Degrade::PillCompact,
        Degrade::PillDot,
    ];

    fn nav_icons(self) -> bool {
        self != Degrade::Full
    }

    fn pill(self) -> PillMode {
        match self {
            Degrade::Full | Degrade::NavIcons => PillMode::Full,
            Degrade::PillCompact => PillMode::Compact,
            Degrade::PillDot => PillMode::Dot,
        }
    }
}

/// The widths the row is made of, for every variant an item can take.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RowWidths {
    /// Record (+ bookmark + CC), gaps included.
    lead: f32,
    /// The pill's width per [`Degrade`] level: whole, shortest full-mode
    /// text, compact, dot.
    pill: [f32; 4],
    /// The nav with labels, and icon-only.
    nav: [f32; 2],
    /// The gear and the window controls, with the gaps before them.
    trail: f32,
}

impl RowWidths {
    /// The whole row at `level`: padding, items, fixed gaps and the
    /// flexible gap at its minimum.
    fn total(&self, level: Degrade) -> f32 {
        PAD_LEFT
            + self.lead
            + GAP
            + self.pill[level as usize]
            + GAP
            + self.nav[usize::from(level.nav_icons())]
            + self.trail
            + PAD_RIGHT
    }
}

/// The mildest level whose row fits `available` (the bar's full width);
/// the last level if none does.
pub(crate) fn degrade(widths: &RowWidths, available: f32) -> Degrade {
    Degrade::ALL
        .into_iter()
        .find(|&level| widths.total(level) <= available + 0.5)
        .unwrap_or(Degrade::PillDot)
}

/// Leading buttons: record, plus bookmark and CC while recording.
fn lead_width(recording: bool, live_captions: bool) -> f32 {
    let extra = usize::from(recording) + usize::from(recording && live_captions);
    BTN + extra as f32 * (TIGHT_GAP + BTN)
}

/// The gear plus (outside the tray flyout) the window controls.
fn trail_width(popup: bool) -> f32 {
    TIGHT_GAP + BTN + if popup { 0.0 } else { GAP + CONTROLS_WIDTH }
}

/// One nav segment for a label `label_w` wide (or icon-only).
fn segment_width(icons_only: bool, label_w: f32) -> f32 {
    if icons_only {
        SEG_ICON_ONLY
    } else {
        SEG_PAD + SEG_ICON + SEG_ICON_GAP + label_w + SEG_PAD
    }
}

/// The segmented control holding segments for labels `label_ws` wide.
fn nav_width(icons_only: bool, label_ws: [f32; 2]) -> f32 {
    NAV_INSET * 2.0
        + label_ws
            .iter()
            .map(|&w| segment_width(icons_only, w))
            .sum::<f32>()
}

/// A text's width in `font`, with every digit measured as `0` so a ticking
/// timer or percentage keeps a steady width.
fn stable_width(ui: &egui::Ui, text: &str, font: FontId) -> f32 {
    let template: String = text
        .chars()
        .map(|c| if c.is_ascii_digit() { '0' } else { c })
        .collect();
    ui.painter()
        .layout_no_wrap(template, font, pal().text)
        .size()
        .x
        .ceil()
}

fn measure_row(ui: &egui::Ui, model: &BarModel) -> RowWidths {
    let measure = |text: &str| stable_width(ui, text, pill_font());
    let geom = PillGeom::of(ui);
    let full = PillParts::of(&model.pill, PillMode::Full, geom);
    let compact = PillParts::of(&model.pill, PillMode::Compact, geom);
    let dot = PillParts::of(&model.pill, PillMode::Dot, geom);
    let pill = [
        full.max_width(measure),
        full.min_width(measure),
        compact.min_width(measure),
        dot.min_width(measure),
    ];
    let labels = [NavItem::Meetings, NavItem::People].map(|item| {
        ui.painter()
            .layout_no_wrap(item.label().to_owned(), nav_font(), pal().text)
            .size()
            .x
            .ceil()
    });
    RowWidths {
        lead: lead_width(model.recording, model.live_captions),
        pill,
        nav: [nav_width(false, labels), nav_width(true, labels)],
        trail: trail_width(model.popup),
    }
}

/// Lay out and draw the bar's items on one row (see the module docs).
fn draw_bar(ui: &mut egui::Ui, model: &BarModel) -> DrawnBar {
    let width = ui.available_width();
    let widths = measure_row(ui, model);
    let level = degrade(&widths, width);
    // The pill may grow into the flexible gap (a busy pill's full text).
    let pill_room = widths.pill[level as usize] + (width - widths.total(level)).max(0.0);
    let mut actions = Vec::new();
    #[cfg(test)]
    let mut rects = Vec::new();

    // The bar's own response is registered behind the widgets, so buttons
    // keep their clicks and only the background drags the window. The
    // padding sits inside the sensing scope: the whole bar drags.
    let bar = ui.scope_builder(
        egui::UiBuilder::new().sense(Sense::click_and_drag()),
        |ui| {
            let row = Rect::from_min_size(ui.max_rect().min, egui::vec2(width, BAR_HEIGHT));
            let mid = row.center().y;
            let slot = |x: f32, w: f32, h: f32| {
                Rect::from_min_size(egui::pos2(x, mid - h / 2.0), egui::vec2(w, h))
            };
            // Each item draws into its own slot (a child ui at that rect).
            let mut place =
                |ui: &mut egui::Ui,
                 item: Item,
                 rect: Rect,
                 draw: &mut dyn FnMut(&mut egui::Ui, &mut Vec<BarAction>)| {
                    ui.scope_builder(
                        egui::UiBuilder::new()
                            .id_salt(item)
                            .max_rect(rect)
                            .layout(Layout::left_to_right(Align::Center)),
                        |ui| {
                            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                            draw(ui, &mut actions);
                        },
                    );
                    #[cfg(test)]
                    rects.push((item, rect));
                };

            // Left to right: record (+ bookmark + CC), the pill.
            let mut x = row.left() + PAD_LEFT;
            let mut lead = vec![Item::Record];
            if model.recording {
                lead.push(Item::Bookmark);
                if model.live_captions {
                    lead.push(Item::Captions);
                }
            }
            for item in lead {
                place(ui, item, slot(x, BTN, BTN), &mut |ui, actions| {
                    let (response, action) = match item {
                        Item::Record => (record_button(ui, model), BarAction::Record),
                        Item::Bookmark => (
                            bookmark_button(ui, &model.bookmark_tooltip),
                            BarAction::Bookmark,
                        ),
                        _ => (captions_button(ui), BarAction::ShowCaptions),
                    };
                    if response.clicked() {
                        actions.push(action);
                    }
                });
                x += BTN + TIGHT_GAP;
            }
            x += GAP - TIGHT_GAP;
            let geom = PillGeom::of(ui);
            let pill = PillParts::of(&model.pill, level.pill(), geom);
            let measure = |text: &str| stable_width(ui, text, pill_font());
            let pill_text = pill.pick_text(pill_room, measure).map(str::to_owned);
            let pill_w = pill.width(pill_text.as_deref().map(measure));
            place(
                ui,
                Item::Pill,
                slot(x, pill_w, geom.height),
                &mut |ui, actions| {
                    let response = status_pill(ui, model, &pill, pill_text.as_deref(), pill_w);
                    if response.clicked() && model.recording {
                        actions.push(BarAction::OpenTranscript);
                    }
                },
            );

            // The right-hand group hugs the right edge: positions come right
            // to left, but the items are drawn left to right so Tab moves
            // through the bar in reading order (nav, gear, window controls).
            let mut right = row.right() - PAD_RIGHT;
            let controls = (!model.popup).then(|| {
                right -= CONTROLS_WIDTH + GAP;
                slot(right + GAP, CONTROLS_WIDTH, BTN)
            });
            right -= BTN;
            let gear = slot(right, BTN, BTN);
            let icons_only = level.nav_icons();
            let nav_w = widths.nav[usize::from(icons_only)];
            let nav = slot(right - TIGHT_GAP - nav_w, nav_w, BTN);
            place(ui, Item::Nav, nav, &mut |ui, actions| {
                nav_control(ui, model.active, icons_only, actions);
            });
            place(ui, Item::Settings, gear, &mut |ui, actions| {
                let selected = model.active == Some(NavItem::Settings);
                if settings_button(ui, selected).clicked() {
                    actions.push(BarAction::Nav(NavItem::Settings));
                }
            });
            if let Some(controls) = controls {
                place(ui, Item::Controls, controls, &mut |ui, actions| {
                    window_controls(ui, actions);
                });
            }
            ui.advance_cursor_after_rect(row);
        },
    );
    DrawnBar {
        response: bar.response,
        actions,
        #[cfg(test)]
        rects,
        #[cfg(test)]
        level,
    }
}

// ---------------------------------------------------------------------------
// Buttons

/// A bar button's fill and glyph colours, at rest and hovered / focused.
#[derive(Debug, Clone, Copy)]
struct Look {
    fill: Color32,
    hover_fill: Color32,
    glyph: Color32,
    hover_glyph: Color32,
}

/// Bookmark, CC and an unselected gear: no fill until hovered.
fn ghost() -> Look {
    let p = pal();
    Look {
        fill: Color32::TRANSPARENT,
        hover_fill: p.bg_hover,
        glyph: p.text_muted,
        hover_glyph: p.text,
    }
}

/// A 32 pt square bar button with `glyph` (`size` pt) centred, radius 8,
/// the keyboard focus ring and `label` as its accessible name. Dimmed and
/// inert while the ui is disabled.
fn bar_button(
    ui: &mut egui::Ui,
    glyph: Option<(&str, f32)>,
    label: &str,
    look: Look,
    selected: Option<bool>,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(BTN, BTN), Sense::click());
    let enabled = ui.is_enabled();
    response.widget_info(|| match selected {
        Some(selected) => {
            egui::WidgetInfo::selected(egui::WidgetType::Button, enabled, selected, label)
        }
        None => egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label),
    });
    if ui.is_rect_visible(rect) {
        let ring = response.has_focus() && widgets::focus_visible(ui.ctx());
        let hot = enabled && (response.hovered() || ring);
        let (fill, color) = if hot {
            (look.hover_fill, look.hover_glyph)
        } else {
            (look.fill, look.glyph)
        };
        let dim = |c: Color32| if enabled { c } else { c.gamma_multiply(0.55) };
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(theme::R_MD), dim(fill));
        if let Some((glyph, size)) = glyph {
            let galley =
                painter.layout_no_wrap(glyph.to_owned(), FontId::proportional(size), dim(color));
            painter.galley(rect.center() - galley.size() / 2.0, galley, dim(color));
        }
        if ring {
            focus_ring(ui, rect, theme::R_MD);
        }
    }
    response.on_hover_cursor(if enabled {
        CursorIcon::PointingHand
    } else {
        CursorIcon::NotAllowed
    })
}

/// The 2 pt FOCUS ring, 2 pt outside `rect`.
fn focus_ring(ui: &egui::Ui, rect: Rect, radius: u8) {
    ui.painter().rect_stroke(
        rect.expand(2.0),
        CornerRadius::same(radius.saturating_add(2)),
        Stroke::new(2.0, theme::pal().focus),
        egui::StrokeKind::Outside,
    );
}

/// Record (idle: a red disc on a soft red square) / Stop (recording: a
/// white square on solid red) / spinner (stop in flight). Disabled while a
/// start or stop is in flight; both go through the ordered rec-control
/// worker, and a failure becomes an in-app error toast (`app.rs`, on the
/// reply).
fn record_button(ui: &mut egui::Ui, model: &BarModel) -> egui::Response {
    let recording = model.recording;
    let label = if recording {
        "Stop recording"
    } else {
        "Start recording"
    };
    let look = if recording {
        Look {
            fill: pal().rec,
            hover_fill: pal().rec_hover,
            glyph: pal().white,
            hover_glyph: pal().white,
        }
    } else {
        Look {
            fill: theme::alpha(pal().rec, 0.16),
            hover_fill: theme::alpha(pal().rec, 0.28),
            glyph: pal().rec,
            hover_glyph: pal().rec_light,
        }
    };
    ui.add_enabled_ui(!model.busy, |ui| {
        let response = bar_button(ui, None, label, look, None);
        let rect = response.rect;
        let enabled = ui.is_enabled();
        let color = match (recording, enabled) {
            (true, _) => pal().white,
            (false, true) if response.hovered() => pal().rec_light,
            (false, true) => pal().rec,
            (false, false) => pal().rec.gamma_multiply(0.55),
        };
        let painter = ui.painter();
        if recording && model.busy {
            icons::paint_spinner(ui, rect, 16.0, pal().white);
        } else if recording {
            painter.rect_filled(
                Rect::from_center_size(rect.center(), egui::vec2(11.0, 11.0)),
                CornerRadius::same(2),
                color,
            );
        } else {
            painter.circle_filled(rect.center(), 6.5, color);
        }
        response.on_hover_text(label)
    })
    .inner
}

/// "Bookmark this moment (CTRL+ALT+B)".
fn bookmark_button(ui: &mut egui::Ui, tooltip: &str) -> egui::Response {
    bar_button(
        ui,
        Some((ph::BOOKMARK_SIMPLE, 17.0)),
        "Bookmark this moment",
        ghost(),
        None,
    )
    .on_hover_text(tooltip)
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
    bar_button(
        ui,
        Some((ph::CLOSED_CAPTIONING, 18.0)),
        label,
        ghost(),
        None,
    )
    .on_hover_text(label)
}

/// The settings gear; accent-tinted while Settings is open.
fn settings_button(ui: &mut egui::Ui, selected: bool) -> egui::Response {
    let look = if selected {
        Look {
            fill: pal().accent_soft,
            hover_fill: pal().accent_soft,
            glyph: pal().accent_hover,
            hover_glyph: pal().accent_hover,
        }
    } else {
        ghost()
    };
    let item = NavItem::Settings;
    bar_button(
        ui,
        Some((item.glyph(), 18.0)),
        item.label(),
        look,
        Some(selected),
    )
    .on_hover_text(item.label())
}

/// The Meetings / People segmented control: a raised track, the current
/// page's segment accent-tinted. Icon + label, or icon-only (the label is
/// then the tooltip). Also the keyboard path: each segment is a button.
fn nav_control(
    ui: &mut egui::Ui,
    active: Option<NavItem>,
    icons_only: bool,
    actions: &mut Vec<BarAction>,
) {
    let track = ui.max_rect();
    ui.painter().rect(
        track,
        CornerRadius::same(theme::R_MD),
        pal().bg_raised,
        Stroke::new(1.0, pal().border),
        egui::StrokeKind::Inside,
    );
    ui.add_space(NAV_INSET);
    for item in [NavItem::Meetings, NavItem::People] {
        let response = nav_segment(ui, item, icons_only, active == Some(item));
        if response.clicked() {
            actions.push(BarAction::Nav(item));
        }
    }
}

fn nav_segment(
    ui: &mut egui::Ui,
    item: NavItem,
    icons_only: bool,
    selected: bool,
) -> egui::Response {
    let label = item.label();
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), nav_font(), pal().text);
    let width = segment_width(icons_only, galley.size().x.ceil());
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, SEG_HEIGHT), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, label)
    });
    if ui.is_rect_visible(rect) {
        let ring = response.has_focus() && widgets::focus_visible(ui.ctx());
        let hot = response.hovered() || ring;
        let fill = if selected {
            pal().accent_soft
        } else if hot {
            pal().bg_hover
        } else {
            Color32::TRANSPARENT
        };
        let (icon_color, text_color) = match (selected, hot) {
            (true, _) => (pal().accent_hover, pal().text),
            (false, true) => (pal().text, pal().text),
            (false, false) => (pal().text_muted, pal().text_muted),
        };
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(theme::R_SM), fill);
        let icon = painter.layout_no_wrap(
            item.glyph().to_owned(),
            FontId::proportional(SEG_ICON),
            icon_color,
        );
        let mid = rect.center().y;
        if icons_only {
            painter.galley(rect.center() - icon.size() / 2.0, icon, icon_color);
        } else {
            let x = rect.left() + SEG_PAD;
            let icon_x = x + (SEG_ICON - icon.size().x) / 2.0;
            painter.galley(
                egui::pos2(icon_x, mid - icon.size().y / 2.0),
                icon,
                icon_color,
            );
            let text_x = x + SEG_ICON + SEG_ICON_GAP;
            let text_y = mid - galley.size().y / 2.0;
            painter.galley(egui::pos2(text_x, text_y), galley, text_color);
        }
        if ring {
            focus_ring(ui, rect, theme::R_SM);
        }
    }
    let response = response.on_hover_cursor(CursorIcon::PointingHand);
    if icons_only {
        response.on_hover_text(label)
    } else {
        response
    }
}

/// Minimize, maximize (always the square glyph) and close-to-tray, edge to
/// edge. Hidden in tray-flyout mode.
fn window_controls(ui: &mut egui::Ui, actions: &mut Vec<BarAction>) {
    let buttons = [
        (BarAction::Minimize, ph::MINUS, 14.0, "Minimize", false),
        (BarAction::Maximize, ph::SQUARE, 12.0, "Maximize", false),
        (BarAction::Close, ph::X, 14.0, "Close to tray", true),
    ];
    for (action, glyph, size, label, close) in buttons {
        if widgets::win_btn(ui, glyph, size, label, close).clicked() {
            actions.push(action);
        }
    }
}

/// The top bar's panel frame: the window background, no padding (the bar
/// pads itself inside its drag area). `app.rs` paints the hairline bottom
/// border with [`paint_bottom_border`].
pub fn frame() -> egui::Frame {
    egui::Frame::new().fill(pal().window_bg)
}

/// The bar's 1 px bottom border.
pub fn paint_bottom_border(ui: &egui::Ui, rect: egui::Rect) {
    ui.painter().hline(
        rect.x_range(),
        rect.bottom() - 0.5,
        Stroke::new(1.0, pal().border),
    );
}

// ---------------------------------------------------------------------------
// The status pill

/// What the status pill shows (map-ui-shell §4, first match wins).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PillState {
    /// Recording with a stop in flight.
    Stopping,
    Recording {
        elapsed: String,
        warning: Option<String>,
    },
    /// Transcribing: the full status text and the rounded percentage when
    /// known (for the compact pill).
    Busy {
        text: String,
        pct: Option<u32>,
    },
    Idle,
}

/// A rounded, clamped percentage.
fn whole_pct(pct: f64) -> u32 {
    pct.round().clamp(0.0, 100.0) as u32
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
            return PillState::Busy {
                text: format!("Transcribing ({}) {}%{suffix}", p.stage, p.pct.round()),
                pct: Some(whole_pct(f64::from(p.pct))),
            };
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
            return PillState::Busy {
                text: format!("{text}{suffix}"),
                pct: status.processing_pct.map(|p| whole_pct(f64::from(p))),
            };
        }
        if queued > 0 {
            // Between jobs: the next queued meeting has no progress yet.
            return PillState::Busy {
                text: format!("Transcribing…{suffix}"),
                pct: None,
            };
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
            PillState::Busy { text, .. } => text.clone(),
            PillState::Idle => "Idle".into(),
        }
    }
}

/// How much of the pill shows (see [`Degrade`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PillMode {
    Full,
    Compact,
    Dot,
}

/// The pill's padding and height: the kit's (`widgets::pill_padding`
/// around a line of 13 pt text), shared by every pill state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PillGeom {
    pad: egui::Vec2,
    height: f32,
}

impl PillGeom {
    fn of(ui: &egui::Ui) -> PillGeom {
        let pad = widgets::pill_padding(ui);
        let text = ui
            .painter()
            .layout_no_wrap("0".to_owned(), pill_font(), pal().text)
            .size()
            .y;
        PillGeom {
            pad,
            height: text + pad.y * 2.0,
        }
    }
}

/// The pill's leading mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lead {
    /// None (the idle pill is the kit's plain text pill).
    None,
    /// The pulsing red dot (recording).
    RecDot,
    /// A warn-coloured spinner (transcribing, stopping).
    Spinner,
    /// ⚠ in place of the dot (dot-only recording pill with a warning).
    Warning,
}

/// The pieces of a pill, left to right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Piece {
    Lead,
    Text,
    Warning,
    Meter,
}

/// What one pill variant is made of.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PillParts {
    kind: PillKind,
    lead: Lead,
    /// Text candidates, preferred first; the first that fits is shown.
    /// Empty: no text.
    texts: Vec<String>,
    /// A ⚠ after the text.
    warning: bool,
    /// The level meter at the end.
    meter: bool,
    geom: PillGeom,
}

impl PillParts {
    pub(crate) fn of(state: &PillState, mode: PillMode, geom: PillGeom) -> PillParts {
        let parts = |kind, lead, texts: Vec<String>| PillParts {
            kind,
            lead,
            texts,
            warning: false,
            meter: false,
            geom,
        };
        let dot = mode == PillMode::Dot;
        match state {
            // Short enough never to need degrading.
            PillState::Idle => parts(PillKind::Idle, Lead::None, vec!["Idle".into()]),
            PillState::Stopping if dot => parts(PillKind::Busy, Lead::Spinner, vec![]),
            PillState::Stopping => parts(PillKind::Busy, Lead::Spinner, vec!["Stopping…".into()]),
            PillState::Busy { text, pct } => {
                let texts = match mode {
                    // The whole text, or "Transcribing 45%" when it does
                    // not fit.
                    PillMode::Full => {
                        let medium = match pct {
                            Some(p) => format!("Transcribing {p}%"),
                            None => "Transcribing…".to_owned(),
                        };
                        if *text == medium {
                            vec![medium]
                        } else {
                            vec![text.clone(), medium]
                        }
                    }
                    PillMode::Compact => pct.iter().map(|p| format!("{p}%")).collect(),
                    PillMode::Dot => vec![],
                };
                parts(PillKind::Busy, Lead::Spinner, texts)
            }
            PillState::Recording { elapsed, warning } => {
                if dot {
                    let lead = if warning.is_some() {
                        Lead::Warning
                    } else {
                        Lead::RecDot
                    };
                    parts(PillKind::Recording, lead, vec![])
                } else {
                    PillParts {
                        warning: warning.is_some(),
                        meter: mode == PillMode::Full,
                        ..parts(PillKind::Recording, Lead::RecDot, vec![elapsed.clone()])
                    }
                }
            }
        }
    }

    /// Just the leading mark: a round pill.
    fn dot_only(&self) -> bool {
        self.lead != Lead::None && self.texts.is_empty() && !self.warning && !self.meter
    }

    /// Each piece's left edge (from the content's left edge) and width,
    /// for a text `text_w` wide (or none).
    fn pieces(&self, text_w: Option<f32>) -> Vec<(Piece, f32, f32)> {
        let mut pieces = Vec::new();
        let mut x = 0.0;
        let mut push = |piece: Piece, gap: f32, w: f32| {
            if !pieces.is_empty() {
                x += gap;
            }
            pieces.push((piece, x, w));
            x += w;
        };
        if self.lead != Lead::None {
            push(Piece::Lead, PILL_GAP, PILL_LEAD);
        }
        if let Some(w) = text_w {
            push(Piece::Text, PILL_GAP, w);
        }
        if self.warning {
            push(Piece::Warning, PILL_GAP, PILL_WARN);
        }
        if self.meter {
            push(Piece::Meter, METER_GAP, METER_WIDTH);
        }
        pieces
    }

    /// The pill's width showing a text `text_w` wide (or none).
    pub(crate) fn width(&self, text_w: Option<f32>) -> f32 {
        if self.dot_only() {
            return self.geom.height;
        }
        let content = self.pieces(text_w).last().map_or(0.0, |&(_, x, w)| x + w);
        content + self.geom.pad.x * 2.0
    }

    /// The widest this variant gets (its first text candidate).
    pub(crate) fn max_width(&self, measure: impl Fn(&str) -> f32) -> f32 {
        self.width(self.texts.first().map(|t| measure(t)))
    }

    /// The narrowest this variant gets (its last text candidate).
    pub(crate) fn min_width(&self, measure: impl Fn(&str) -> f32) -> f32 {
        self.width(self.texts.last().map(|t| measure(t)))
    }

    /// The first text candidate that fits in `room` (else the last).
    pub(crate) fn pick_text(&self, room: f32, measure: impl Fn(&str) -> f32) -> Option<&str> {
        self.texts
            .iter()
            .find(|t| self.width(Some(measure(t))) <= room + 0.5)
            .or(self.texts.last())
            .map(String::as_str)
    }
}

/// The level meter's tooltip and accessible name.
const METER_LABEL: &str = "Microphone and system audio levels";

/// The status pill (`parts`, showing `text`, `width` wide). Only the
/// recording pill is clickable (it opens the recording's transcript); the
/// others are labels whose tooltip has the full status when it is cut.
fn status_pill(
    ui: &mut egui::Ui,
    model: &BarModel,
    parts: &PillParts,
    text: Option<&str>,
    width: f32,
) -> egui::Response {
    widgets::announce(
        ui.ctx(),
        egui::Id::new("witness-status-pill-live"),
        &model.pill.announcement(),
        Politeness::Polite,
    );
    if model.pill == PillState::Idle {
        return widgets::pill(ui, PillKind::Idle, "Idle");
    }
    let geom = parts.geom;
    let content = egui::vec2(width - geom.pad.x * 2.0, geom.height - geom.pad.y * 2.0);
    let rect = widgets::pill_shell(ui, parts.kind, content.max(egui::Vec2::ZERO));
    let (_, color) = parts.kind.colors();
    let painter = ui.painter().clone();
    let mid = rect.center().y;
    let text_w = text.map(|t| stable_width(ui, t, pill_font()));
    let mut meter = None;
    for (piece, x, w) in parts.pieces(text_w) {
        // A dot-only pill centres its mark.
        let x = if parts.dot_only() {
            rect.center().x - w / 2.0
        } else {
            rect.left() + geom.pad.x + x
        };
        let center = egui::pos2(x + w / 2.0, mid);
        let warning_glyph = || {
            let glyph = painter.layout_no_wrap(
                ph::WARNING.to_owned(),
                FontId::proportional(PILL_WARN),
                pal().warn,
            );
            painter.galley(center - glyph.size() / 2.0, glyph, pal().warn);
        };
        match piece {
            Piece::Lead => match parts.lead {
                Lead::RecDot => {
                    let alpha = icons::pulse_alpha(ui.ctx());
                    painter.circle_filled(center, 4.0, pal().rec.gamma_multiply(alpha));
                }
                Lead::Spinner => {
                    let lead = Rect::from_center_size(center, egui::vec2(w, w));
                    icons::paint_spinner(ui, lead, 13.0, pal().warn);
                }
                Lead::Warning => warning_glyph(),
                Lead::None => {}
            },
            Piece::Text => {
                let text = text.unwrap_or_default().to_owned();
                let galley = painter.layout_no_wrap(text, pill_font(), color);
                painter.galley(egui::pos2(x, mid - galley.size().y / 2.0), galley, color);
            }
            Piece::Warning => warning_glyph(),
            Piece::Meter => {
                let rect = Rect::from_center_size(center, egui::vec2(w, METER_BAR * 2.0 + 2.0));
                level_meter(ui, rect, model);
                meter = Some(rect);
            }
        }
    }

    match &model.pill {
        PillState::Recording { elapsed, warning } => {
            recording_pill_response(ui, rect, elapsed, warning.as_deref(), text.is_some(), meter)
        }
        other => {
            let full = other.announcement();
            let response = ui.interact(rect, ui.id().with("status-pill"), Sense::hover());
            response
                .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &full));
            if text != Some(full.as_str()) {
                response.on_hover_text(full)
            } else {
                response
            }
        }
    }
}

/// The recording pill's click target: tooltip, accessible name, focus.
fn recording_pill_response(
    ui: &mut egui::Ui,
    rect: Rect,
    elapsed: &str,
    warning: Option<&str>,
    time_shown: bool,
    meter: Option<Rect>,
) -> egui::Response {
    let response = ui
        .interact(rect, ui.id().with("recording-pill"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    // The pill's click response sits above the hover-only meter, so the
    // meter's own tooltip would never show: pick the text by where the
    // pointer is (the web meter's `title` won over the pill's inside it).
    let over_meter =
        meter.is_some_and(|meter| response.hover_pos().is_some_and(|p| meter.contains(p)));
    let tooltip = if over_meter {
        METER_LABEL.to_owned()
    } else {
        let hint = warning.unwrap_or("Open live captions");
        if time_shown {
            hint.to_owned()
        } else {
            // A dot-only pill: the time lives in the tooltip.
            format!("Recording {elapsed} · {hint}")
        }
    };
    let response = response.on_hover_text(tooltip);
    let name = format!(
        "{elapsed} recording. {}",
        warning.unwrap_or("Open live transcript")
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &name));
    if response.has_focus() && widgets::focus_visible(ui.ctx()) {
        focus_ring(ui, rect, 253);
    }
    response
}

/// Two stacked bars in `rect` on a faint track: microphone (accent) and
/// system audio (ok), each `min(100, rms·300)` % of the meter, at least
/// 2 pt, easing over 0.15 s.
fn level_meter(ui: &mut egui::Ui, rect: Rect, model: &BarModel) {
    let response = ui.interact(rect, ui.id().with("level-meter"), Sense::hover());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, METER_LABEL));
    let (mic, loopback) = model.levels;
    for (row, (rms, color)) in [(mic, pal().accent), (loopback, pal().ok)]
        .into_iter()
        .enumerate()
    {
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
        let track = Rect::from_min_size(
            egui::pos2(rect.left(), top),
            egui::vec2(METER_WIDTH, METER_BAR),
        );
        ui.painter()
            .rect_filled(track, CornerRadius::same(2), theme::alpha(pal().text, 0.08));
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
/// selectable), in the content column.
pub fn health_banner(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let warning = match fake_recording() {
        Some(Fake::Recording { warning: true }) => Some(FAKE_WARNING.to_owned()),
        Some(_) => None,
        None if cx.shell.recording() => cx.shell.health.as_ref().and_then(capture_warning),
        None => None,
    };
    let Some(warning) = warning else {
        return;
    };
    egui::Panel::top("health-banner")
        .frame(Frame::new().fill(pal().bg).inner_margin(Margin {
            left: 0,
            right: 0,
            top: 12,
            bottom: 0,
        }))
        .resizable(false)
        .show_separator_line(false)
        .show(ui, |ui| {
            widgets::padded_column(ui, |ui| {
                widgets::inline_warning(ui, &warning);
            });
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

/// The model banner's text: a semibold lead, then the explanation.
const BANNER_TITLE: &str = "Transcription models not installed.";
const BANNER_BODY: &str = "Recording works, but nothing gets transcribed. One-time download, \
                           ~3.5 GB (Parakeet + speaker models).";
/// The text column's minimum width beside the actions; narrower, the
/// actions go under the text.
const BANNER_TEXT_MIN: f32 = 280.0;
const BANNER_ICON: f32 = 20.0;

/// Whether [`model_banner`] draws anything this pass.
pub fn has_model_banner(cx: &UiCtx) -> bool {
    cx.shell.models_missing || fake_model_banner()
}

/// The first-run "Transcription models not installed." banner: Download
/// now (or the download progress) and Later, which persists the dismissal.
/// A raised surface in the content column; the caller adds the gap below.
pub fn model_banner(ui: &mut egui::Ui, cx: &mut UiCtx) {
    if !has_model_banner(cx) {
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
        &format!("{BANNER_TITLE} {BANNER_BODY}"),
        Politeness::Polite,
    );
    Frame::new()
        .fill(pal().bg_raised)
        .stroke(Stroke::new(1.0, pal().border))
        .corner_radius(CornerRadius::same(theme::R_LG))
        .inner_margin(Margin::symmetric(16, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            banner_contents(ui, cx, progress.as_deref());
        });
}

/// The banner's actions' width: Later (ghost) + Download now (primary) or
/// the progress text, 8 apart (button padding as `widgets::btn_padding`).
fn banner_actions_width(ui: &egui::Ui, progress: Option<&str>) -> f32 {
    let width = |text: &str, font: FontId| {
        ui.painter()
            .layout_no_wrap(text.to_owned(), font, pal().text)
            .size()
            .x
    };
    let pad = if theme::narrow(ui) { 20.0 } else { 28.0 };
    let primary = match progress {
        Some(text) => width(text, theme::regular(13.0)),
        None => width("Download now", theme::semibold(theme::BASE_SIZE)) + pad,
    };
    primary + theme::S2 + width("Later", theme::regular(theme::BASE_SIZE)) + pad
}

fn banner_contents(ui: &mut egui::Ui, cx: &mut UiCtx, progress: Option<&str>) {
    let actions_w = banner_actions_width(ui, progress).ceil();
    let text_lead = BANNER_ICON + theme::S3;
    let side_by_side = ui.available_width() >= text_lead + BANNER_TEXT_MIN + theme::S4 + actions_w;

    // Icon, then the title over the muted explanation, both wrapping.
    let text = |ui: &mut egui::Ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = theme::S3;
            ui.add_sized(
                [BANNER_ICON, 20.0],
                egui::Label::new(icons::icon(ph::DOWNLOAD_SIMPLE, 18.0).color(pal().accent)),
            );
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.add(
                    egui::Label::new(
                        RichText::new(BANNER_TITLE)
                            .font(theme::semibold(theme::BASE_SIZE))
                            .color(pal().text),
                    )
                    .wrap(),
                );
                ui.add(
                    egui::Label::new(
                        RichText::new(BANNER_BODY)
                            .font(theme::regular(13.0))
                            .color(pal().text_muted),
                    )
                    .wrap(),
                );
            });
        });
    };
    // Right to left: the primary action outermost, Later beside it.
    let mut actions = |ui: &mut egui::Ui| {
        ui.spacing_mut().item_spacing.x = theme::S2;
        match progress {
            Some(text) => {
                ui.label(
                    RichText::new(text)
                        .font(theme::regular(13.0))
                        .color(pal().text_muted),
                );
                widgets::announce(
                    ui.ctx(),
                    egui::Id::new("witness-model-download-live"),
                    text,
                    Politeness::Polite,
                );
            }
            None => {
                if widgets::btn_primary(ui, "Download now").clicked() {
                    cx.tasks.spawn_op("model-download", |c| OpDone::Notice {
                        notice: c
                            .download_models(Engine::Parakeet)
                            .err()
                            .map(|error| Notice::error("Could not start model download", error)),
                        refresh: false,
                    });
                }
            }
        }
        if widgets::btn_ghost(ui, "Later").clicked() {
            cx.prefs.model_banner_dismissed = true;
            cx.shell.models_missing = false;
        }
    };

    if side_by_side {
        ui.horizontal(|ui| {
            let text_w = ui.available_width() - actions_w - theme::S4;
            ui.allocate_ui_with_layout(egui::vec2(text_w, 0.0), Layout::top_down(Align::Min), text);
            ui.with_layout(Layout::right_to_left(Align::Center), &mut actions);
        });
    } else {
        text(ui);
        ui.add_space(theme::S1);
        // `horizontal` bounds the row's height; a bare right-to-left layout
        // would claim all the remaining height.
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), &mut actions);
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

    fn widths(pill: [f32; 4]) -> RowWidths {
        RowWidths {
            lead: 32.0,
            pill,
            nav: [190.0, 68.0],
            trail: trail_width(false),
        }
    }

    #[test]
    fn degrade_levels_go_nav_then_pill_compact_then_dot() {
        let w = widths([120.0, 120.0, 70.0, 24.0]);
        let full = w.total(Degrade::Full);
        assert_eq!(degrade(&w, 2000.0), Degrade::Full);
        assert_eq!(degrade(&w, full), Degrade::Full, "exactly full fits");
        assert_eq!(degrade(&w, full - 1.0), Degrade::NavIcons);
        // The nav saves 122 pt before the pill gives anything up.
        assert_eq!(w.total(Degrade::NavIcons), full - 122.0);
        // A busy pill's whole text needs the full level; icon-only nav
        // may shorten it.
        let busy = widths([260.0, 140.0, 60.0, 24.0]);
        assert_eq!(
            busy.total(Degrade::Full) - busy.total(Degrade::NavIcons),
            122.0 + 120.0
        );
        assert_eq!(
            degrade(&w, w.total(Degrade::NavIcons) - 1.0),
            Degrade::PillCompact
        );
        assert_eq!(
            degrade(&w, w.total(Degrade::PillCompact) - 1.0),
            Degrade::PillDot
        );
        // Nothing fits: the last level (it never wraps).
        assert_eq!(degrade(&w, 10.0), Degrade::PillDot);
        // Levels only ever narrow the row.
        let totals: Vec<f32> = Degrade::ALL.iter().map(|&l| w.total(l)).collect();
        assert!(totals.windows(2).all(|p| p[1] <= p[0]), "{totals:?}");
    }

    #[test]
    fn row_widths_follow_the_state() {
        assert_eq!(lead_width(false, true), BTN);
        assert_eq!(lead_width(true, false), BTN * 2.0 + TIGHT_GAP);
        assert_eq!(lead_width(true, true), BTN * 3.0 + TIGHT_GAP * 2.0);
        assert!(
            trail_width(true) < trail_width(false),
            "flyout: no controls"
        );
        assert_eq!(nav_width(true, [60.0, 45.0]), NAV_INSET * 2.0 + 64.0);
        assert!(nav_width(false, [60.0, 45.0]) > nav_width(true, [60.0, 45.0]));
    }

    /// A fixed pill geometry (10 / 5 padding, 27 pt tall).
    const GEOM: PillGeom = PillGeom {
        pad: egui::vec2(10.0, 5.0),
        height: 27.0,
    };

    fn parts_of(state: &PillState, mode: PillMode) -> PillParts {
        PillParts::of(state, mode, GEOM)
    }

    /// 7 pt per character: enough to reason about widths without fonts.
    fn fake_measure(text: &str) -> f32 {
        text.chars().count() as f32 * 7.0
    }

    #[test]
    fn pill_variants_shrink_and_pick_the_text_that_fits() {
        let rec = PillState::Recording {
            elapsed: "12:34".into(),
            warning: None,
        };
        let full = parts_of(&rec, PillMode::Full);
        let compact = parts_of(&rec, PillMode::Compact);
        let dot = parts_of(&rec, PillMode::Dot);
        assert!(full.meter && !compact.meter);
        assert_eq!(compact.texts, ["12:34"]);
        assert!(dot.dot_only());
        assert_eq!(dot.min_width(fake_measure), GEOM.height, "a round dot");
        assert!(full.min_width(fake_measure) > compact.min_width(fake_measure));

        // A warning: ⚠ after the time, or in place of the dot.
        let warned = PillState::Recording {
            elapsed: "1:02:03".into(),
            warning: Some("Microphone device was lost during recording".into()),
        };
        assert!(parts_of(&warned, PillMode::Compact).warning);
        assert_eq!(parts_of(&warned, PillMode::Dot).lead, Lead::Warning);

        // Busy: the whole text if it fits, else "Transcribing 45%"; then
        // "45%"; then the spinner alone.
        let busy = PillState::Busy {
            text: "Transcribing (diarize) 45% · 2 queued".into(),
            pct: Some(45),
        };
        let parts = parts_of(&busy, PillMode::Full);
        let whole = parts.width(Some(fake_measure(&parts.texts[0])));
        assert_eq!(
            parts.pick_text(whole, fake_measure),
            Some("Transcribing (diarize) 45% · 2 queued")
        );
        assert_eq!(
            parts.pick_text(whole - 1.0, fake_measure),
            Some("Transcribing 45%")
        );
        assert_eq!(parts.pick_text(0.0, fake_measure), Some("Transcribing 45%"));
        assert_eq!(parts_of(&busy, PillMode::Compact).texts, ["45%".to_owned()]);
        let unknown = PillState::Busy {
            text: "Transcribing…".into(),
            pct: None,
        };
        assert_eq!(parts_of(&unknown, PillMode::Full).texts, ["Transcribing…"]);
        assert!(parts_of(&unknown, PillMode::Compact).dot_only());

        let idle = parts_of(&PillState::Idle, PillMode::Compact);
        assert_eq!(idle.texts, ["Idle"]);
        // Idle is the plain kit pill in every mode.
        assert_eq!(parts_of(&PillState::Idle, PillMode::Dot), idle);
        assert_eq!(idle.width(Some(28.0)), 48.0);
    }

    fn bar_model(pill: PillState, captions: bool, popup: bool) -> BarModel {
        let recording = matches!(pill, PillState::Recording { .. } | PillState::Stopping);
        BarModel {
            recording,
            live_captions: recording && captions,
            popup,
            busy: pill == PillState::Stopping,
            pill,
            bookmark_tooltip: bookmark_tooltip(Some("ctrl+alt+b")),
            levels: (0.1, 0.05),
            animate_meter: false,
            active: Some(NavItem::Meetings),
        }
    }

    fn recording(elapsed: &str, warning: bool) -> PillState {
        PillState::Recording {
            elapsed: elapsed.into(),
            warning: warning.then(|| FAKE_WARNING.to_owned()),
        }
    }

    /// Every state the bar can be in, the widest variants included.
    fn scenarios() -> Vec<(&'static str, BarModel)> {
        let busy = PillState::Busy {
            text: "Transcribing (diarize) 100% · 12 queued".into(),
            pct: Some(100),
        };
        vec![
            ("idle", bar_model(PillState::Idle, false, false)),
            ("busy", bar_model(busy.clone(), false, false)),
            ("busy flyout", bar_model(busy, false, true)),
            ("stopping", bar_model(PillState::Stopping, true, false)),
            (
                "recording",
                bar_model(recording("12:34", false), false, false),
            ),
            (
                "recording cc",
                bar_model(recording("12:34", false), true, false),
            ),
            (
                "long recording cc warn",
                bar_model(recording("10:02:34", true), true, false),
            ),
            (
                "recording flyout",
                bar_model(recording("12:34", true), true, true),
            ),
        ]
    }

    /// Run the real bar in a `width` × 620 window for a few frames, like
    /// `app.rs` does; returns the last pass's bar rect, item rects and
    /// level.
    fn lay_out_bar(width: f32, model: &BarModel) -> (Rect, Vec<(Item, Rect)>, Degrade) {
        let ctx = theme::test_context();
        let mut last = (Rect::NOTHING, Vec::new(), Degrade::Full);
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
                        last = (bar.response.rect, bar.rects, bar.level);
                    });
            });
            output.textures_delta.clear();
        }
        last
    }

    fn rect_of(rects: &[(Item, Rect)], item: Item) -> Rect {
        rects.iter().find(|(i, _)| *i == item).unwrap().1
    }

    #[test]
    fn the_bar_is_one_row_that_fits_at_every_width() {
        for width in [380.0, 420.0, 460.0, 640.0, 1000.0] {
            for (name, model) in scenarios() {
                let (bar, rects, level) = lay_out_bar(width, &model);
                let at = format!("{name} at {width} ({level:?}): {rects:?}");
                // The drag area is the whole 48 pt bar, padding included.
                assert_eq!(bar.min, egui::Pos2::ZERO, "{at}");
                assert_eq!(bar.width(), width, "{at}");
                assert!((bar.height() - BAR_HEIGHT).abs() < 0.01, "{at}");
                // Every item inside the bar, centred on its middle line…
                for (item, rect) in &rects {
                    assert!(rect.left() >= PAD_LEFT - 0.01, "{item:?} {at}");
                    assert!(rect.right() <= width - PAD_RIGHT + 0.01, "{item:?} {at}");
                    assert!(
                        (rect.center().y - bar.center().y).abs() < 0.01,
                        "{item:?} {at}"
                    );
                }
                // …drawn left to right (the Tab order), never overlapping
                // (at least the minimum gap before the nav).
                for pair in rects.windows(2) {
                    let (a, b) = (pair[0].1, pair[1].1);
                    assert!(a.right() <= b.left() + 0.01, "order / overlap: {at}");
                }
                let pill = rect_of(&rects, Item::Pill);
                let nav = rect_of(&rects, Item::Nav);
                assert!(nav.left() - pill.right() >= GAP - 0.01, "{at}");
                assert_eq!(
                    rects.iter().any(|(i, _)| *i == Item::Controls),
                    !model.popup,
                    "{at}"
                );
            }
        }
    }

    #[test]
    fn wide_bars_show_everything_and_narrow_ones_degrade_in_order() {
        for (name, model) in scenarios() {
            let (_, _, wide) = lay_out_bar(1000.0, &model);
            assert_eq!(wide, Degrade::Full, "{name}");
        }
        let idle = bar_model(PillState::Idle, false, false);
        assert_eq!(lay_out_bar(460.0, &idle).2, Degrade::Full, "labels at 460");
        assert_eq!(lay_out_bar(380.0, &idle).2, Degrade::NavIcons);
        // Recording with CC at the minimum width: down to the dot.
        let rec = bar_model(recording("12:34", false), true, false);
        let (_, rects, level) = lay_out_bar(380.0, &rec);
        assert_eq!(level, Degrade::PillDot);
        let pill = rect_of(&rects, Item::Pill);
        assert!(
            (pill.width() - pill.height()).abs() < 0.01,
            "round: {pill:?}"
        );
        // Without CC there is room for the time.
        let rec = bar_model(recording("12:34", false), false, false);
        assert!(lay_out_bar(380.0, &rec).2 <= Degrade::PillCompact);
        // Window controls hug the right edge, the record button the left.
        let (_, rects, _) = lay_out_bar(640.0, &rec);
        assert!((rect_of(&rects, Item::Controls).right() - (640.0 - PAD_RIGHT)).abs() < 0.01);
        assert!((rect_of(&rects, Item::Record).left() - PAD_LEFT).abs() < 0.01);
    }

    #[test]
    fn presses_near_the_top_left_and_right_edges_do_not_drag() {
        let screen = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(460.0, 620.0));
        assert!(press_drags_window(egui::pos2(200.0, 20.0), screen));
        assert!(press_drags_window(egui::pos2(200.0, 8.0), screen));
        assert!(press_drags_window(egui::pos2(200.0, 40.0), screen));
        assert!(!press_drags_window(egui::pos2(200.0, 7.0), screen));
        assert!(!press_drags_window(egui::pos2(5.0, 30.0), screen));
        assert!(!press_drags_window(egui::pos2(455.0, 30.0), screen));
    }

    #[test]
    fn pill_follows_its_priority_order() {
        let busy = |text: &str, pct: Option<u32>| PillState::Busy {
            text: text.into(),
            pct,
        };
        let mut shell = ShellModel::new();
        assert_eq!(PillState::of(&shell), PillState::Idle);

        let mut s = status(false);
        s.queue_len = 2;
        shell.set_status(s.clone());
        assert_eq!(
            PillState::of(&shell),
            busy("Transcribing… · 2 queued", None)
        );

        s.transcribing_meeting_id = Some(3);
        shell.set_status(s.clone());
        assert_eq!(
            PillState::of(&shell),
            busy("Transcribing… · 2 queued", None)
        );
        s.processing_stage = Some("asr".into());
        shell.set_status(s.clone());
        assert_eq!(
            PillState::of(&shell),
            busy("Transcribing (asr) · 2 queued", None)
        );
        s.processing_pct = Some(44.6);
        shell.set_status(s.clone());
        assert_eq!(
            PillState::of(&shell),
            busy("Transcribing (asr) 45% · 2 queued", Some(45))
        );

        shell.progress = Some(TranscriptionProgress {
            meeting_id: 3,
            stage: "diarize",
            pct: 12.2,
        });
        assert_eq!(
            PillState::of(&shell),
            busy("Transcribing (diarize) 12% · 2 queued", Some(12))
        );
        s.queue_len = 0;
        shell.set_status(s);
        assert_eq!(
            PillState::of(&shell),
            busy("Transcribing (diarize) 12%", Some(12))
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
        assert!((meter_width(0.1) - 0.3 * METER_WIDTH).abs() < 1e-3);
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
