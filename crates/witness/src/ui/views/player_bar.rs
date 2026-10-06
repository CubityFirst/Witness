//! The transcript's player bar (map-ui-transcript §10.4): row 1 is the
//! current time, the scrubber with bookmark markers and the total time;
//! row 2 is ±10 s, play/pause, add bookmark, the speed menu, volume and
//! "Save a copy…".
//!
//! The bar only drives `witness_core::player::Player` (every call is cheap
//! and non-blocking); it never touches the database. Scrubbing previews the
//! dragged position and seeks at most every 100 ms, then once more on
//! release, so a drag does not flood the decoder with seeks. Volume and
//! speed persist in `UiPrefs` (they replaced the `witness-volume` /
//! `witness-speed` localStorage keys).

use crate::bridge::OpDone;
use crate::platform::dialogs::{self, SaveFileRequest};
use crate::ui::format::fmt_ts;
use crate::ui::icons::ph;
use crate::ui::theme::{
    self, ACCENT, BG, BG_HOVER, BG_RAISED, BORDER, FOCUS, TEXT, TEXT_MUTED, WARN,
};
use crate::ui::toasts::Notice;
use crate::ui::UiCtx;
use eframe::egui::{
    self, Align, CornerRadius, Frame, Key, Layout, Margin, Response, Sense, Stroke,
};
use std::time::{Duration, Instant};
use witness_core::player::PlayerSnapshot;
use witness_core::service::MeetingDetail;

/// The speed menu (HTML `playbackRate` options).
pub const SPEEDS: [f32; 6] = [0.75, 1.0, 1.25, 1.5, 1.75, 2.0];

/// While scrubbing, seek at most this often (the release always seeks).
const SCRUB_SEEK_INTERVAL: Duration = Duration::from_millis(100);
/// The ±10 s buttons.
const SKIP_BUTTON_MS: i64 = 10_000;
/// ←/→ on the focused scrubber (same step as the view's global keys).
const SKIP_KEY_MS: i64 = 5_000;
/// Scrubber row height: the 24 px marker hit targets fit inside it.
const SCRUB_HEIGHT: f32 = 24.0;
/// Seek-track thickness and thumb radius (Chromium's range input).
const TRACK_HEIGHT: f32 = 4.0;
const THUMB_RADIUS: f32 = 7.0;
/// Bookmark marker hit target (centred on the point) and dot size.
const MARKER_HIT: f32 = 24.0;
const MARKER_DOT: f32 = 4.0;

/// Per-transcript player bar state (scrub preview, throttling).
#[derive(Debug, Default)]
pub struct PlayerBarState {
    /// While the scrubber is held: the previewed position.
    scrub_ms: Option<u64>,
    /// When the last throttled scrub seek went out.
    last_scrub_seek: Option<Instant>,
}

/// What the bar asks the transcript view to do.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PlayerBarOutput {
    /// "Add bookmark" at this position.
    pub add_bookmark_at_ms: Option<i64>,
}

/// The nearest offered speed (a hand-edited `ui-state.toml` may hold any
/// value in 0.75–2; NaN falls back to 1×).
pub fn snap_speed(speed: f32) -> f32 {
    if !speed.is_finite() {
        return 1.0;
    }
    let mut best = 1.0;
    let mut best_distance = f32::INFINITY;
    for candidate in SPEEDS {
        let distance = (candidate - speed).abs();
        if distance < best_distance {
            best = candidate;
            best_distance = distance;
        }
    }
    best
}

/// "1×", "1.25×" … (the `<option>` labels).
fn speed_label(speed: f32) -> String {
    format!("{speed}×")
}

/// `fmtClock`: `m:ss`, or `h:mm:ss` from an hour.
fn clock(ms: u64) -> String {
    fmt_ts(i64::try_from(ms).unwrap_or(i64::MAX))
}

/// The bar's container: `.player-bar` (BG fill, 10/6 vertical padding).
pub fn bar_frame() -> Frame {
    Frame::new().fill(BG).inner_margin(Margin {
        left: 0,
        right: 0,
        top: 10,
        bottom: 6,
    })
}

/// Draw the bar (the `.player` card) for `detail`'s archived audio.
pub fn show(
    ui: &mut egui::Ui,
    cx: &mut UiCtx,
    detail: &MeetingDetail,
    st: &mut PlayerBarState,
) -> PlayerBarOutput {
    let mut out = PlayerBarOutput::default();
    let snap = cx.player.snapshot();
    sync_preferences(cx, &snap);

    Frame::new()
        .fill(BG_RAISED)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin {
            left: 12,
            right: 12,
            top: 10,
            bottom: 8,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::vec2(10.0, 6.0);
            time_row(ui, cx, detail, st, &snap);
            controls_row(ui, cx, detail, &snap, &mut out);
        });
    out
}

/// Keep the engine on the persisted volume and speed (re-applied after a
/// load, like the TSX effect keyed on `src`).
fn sync_preferences(cx: &mut UiCtx, snap: &PlayerSnapshot) {
    let speed = snap_speed(cx.prefs.speed);
    if (snap.speed - speed).abs() > 0.001 {
        cx.player.set_speed(speed);
    }
    let volume = cx.prefs.volume.clamp(0.0, 1.0);
    if (snap.volume - volume).abs() > 0.001 {
        cx.player.set_volume(volume);
    }
}

/// Paint a 12 pt muted time label in a `width` × `SCRUB_HEIGHT` box,
/// right- or left-aligned, and expose it to screen readers.
fn time_label(ui: &mut egui::Ui, text: String, min_width: f32, right_align: bool) {
    let galley = ui
        .painter()
        .layout_no_wrap(text.clone(), theme::regular(12.0), TEXT_MUTED);
    let width = galley.size().x.max(min_width);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, SCRUB_HEIGHT), Sense::hover());
    let x = if right_align {
        rect.right() - galley.size().x
    } else {
        rect.left()
    };
    let pos = egui::pos2(x, rect.center().y - galley.size().y / 2.0);
    ui.painter().galley(pos, galley, TEXT_MUTED);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &text));
}

/// Row 1: current time, scrubber + markers, total time.
fn time_row(
    ui: &mut egui::Ui,
    cx: &mut UiCtx,
    detail: &MeetingDetail,
    st: &mut PlayerBarState,
    snap: &PlayerSnapshot,
) {
    let current = st
        .scrub_ms
        .unwrap_or(snap.position_ms)
        .min(snap.duration_ms);
    let total = clock(snap.duration_ms);
    let total_width = ui
        .painter()
        .layout_no_wrap(total.clone(), theme::regular(12.0), TEXT_MUTED)
        .size()
        .x;
    ui.horizontal(|ui| {
        // `.player-time`: min-width 40, right-aligned.
        time_label(ui, clock(current), 40.0, true);
        let scrub_width =
            (ui.available_width() - total_width - ui.spacing().item_spacing.x).max(120.0);
        scrubber(ui, cx, detail, st, snap, scrub_width);
        time_label(ui, total, 0.0, false);
    });
}

/// The seek slider (`<input type=range step=0.1>`) plus the bookmark
/// markers on top of it. Markers are registered after the track, so they
/// win the click where they overlap it.
fn scrubber(
    ui: &mut egui::Ui,
    cx: &mut UiCtx,
    detail: &MeetingDetail,
    st: &mut PlayerBarState,
    snap: &PlayerSnapshot,
    width: f32,
) {
    let duration = snap.duration_ms;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, SCRUB_HEIGHT), Sense::click_and_drag());
    // The thumb's centre travels between these x positions.
    let track = egui::Rangef::new(rect.left() + THUMB_RADIUS, rect.right() - THUMB_RADIUS);
    let to_ms = |x: f32| -> u64 {
        if duration == 0 || track.span() <= 0.0 {
            return 0;
        }
        let t = ((x - track.min) / track.span()).clamp(0.0, 1.0);
        (f64::from(t) * duration as f64).round() as u64
    };
    let to_x = |ms: u64| -> f32 {
        if duration == 0 {
            return track.min;
        }
        let t = (ms as f64 / duration as f64).clamp(0.0, 1.0) as f32;
        track.min + t * track.span()
    };

    // Pointer: preview while held, throttled seeks, a final seek on release.
    let now = Instant::now();
    let held = response.is_pointer_button_down_on();
    if duration > 0 {
        if held {
            if let Some(pos) = response.interact_pointer_pos() {
                let ms = to_ms(pos.x);
                st.scrub_ms = Some(ms);
                let due = st
                    .last_scrub_seek
                    .is_none_or(|at| now.duration_since(at) >= SCRUB_SEEK_INTERVAL);
                if due {
                    cx.player.seek_ms(ms);
                    st.last_scrub_seek = Some(now);
                }
            }
        }
        if response.drag_stopped() || response.clicked() {
            if let Some(ms) = st.scrub_ms {
                cx.player.seek_ms(ms);
            }
        }
    }
    if !held {
        st.scrub_ms = None;
        st.last_scrub_seek = None;
    }

    // Keyboard on the focused scrubber: ←/→ ±5 s, Home/End.
    if response.has_focus() && duration > 0 {
        let (left, right, home, end) = ui.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, Key::ArrowLeft),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowRight),
                i.consume_key(egui::Modifiers::NONE, Key::Home),
                i.consume_key(egui::Modifiers::NONE, Key::End),
            )
        });
        if left {
            cx.player.skip_ms(-SKIP_KEY_MS);
        }
        if right {
            cx.player.skip_ms(SKIP_KEY_MS);
        }
        if home {
            cx.player.seek_ms(0);
        }
        if end {
            cx.player.seek_ms(duration);
        }
    }

    let value = st.scrub_ms.unwrap_or(snap.position_ms).min(duration);
    response.widget_info(|| egui::WidgetInfo::slider(true, value as f64 / 1000.0, "Seek"));

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let cy = rect.center().y;
        let bar = egui::Rect::from_x_y_ranges(
            track.min..=track.max,
            (cy - TRACK_HEIGHT / 2.0)..=(cy + TRACK_HEIGHT / 2.0),
        );
        painter.rect_filled(bar, CornerRadius::same(2), BORDER);
        let x = to_x(value);
        let filled = egui::Rect::from_x_y_ranges(bar.left()..=x, bar.y_range());
        painter.rect_filled(filled, CornerRadius::same(2), ACCENT);
        let hot = response.hovered() || held;
        let radius = if hot {
            THUMB_RADIUS
        } else {
            THUMB_RADIUS - 1.0
        };
        painter.circle_filled(egui::pos2(x, cy), radius, ACCENT);
        if response.has_focus() {
            painter.rect_stroke(
                rect.expand(2.0),
                CornerRadius::same(6),
                Stroke::new(2.0, FOCUS),
                egui::StrokeKind::Outside,
            );
        }
    }
    if duration > 0 {
        response.on_hover_cursor(egui::CursorIcon::PointingHand);
    }

    // Bookmark markers (only once the duration is known): 24×24
    // transparent targets around a 4 px warn dot.
    if duration == 0 {
        return;
    }
    for bookmark in &detail.bookmarks {
        let at = u64::try_from(bookmark.at_ms).unwrap_or(0);
        let center = egui::pos2(to_x(at), rect.center().y);
        let hit = egui::Rect::from_center_size(center, egui::vec2(MARKER_HIT, MARKER_HIT));
        let marker = ui.interact(
            hit,
            ui.id().with(("seek-marker", bookmark.id)),
            Sense::click(),
        );
        let time = clock(at);
        let (tooltip, label) = if bookmark.note.is_empty() {
            (time.clone(), format!("Go to bookmark at {time}"))
        } else {
            (
                format!("{time} — {}", bookmark.note),
                format!("Go to bookmark at {time}: {}", bookmark.note),
            )
        };
        marker.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
        ui.painter().circle_filled(center, MARKER_DOT / 2.0, WARN);
        if marker.has_focus() {
            ui.painter().rect_stroke(
                hit,
                CornerRadius::same(4),
                Stroke::new(2.0, FOCUS),
                egui::StrokeKind::Outside,
            );
        }
        let marker = marker
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(tooltip);
        if marker.clicked() {
            // Markers seek without playing.
            cx.player.seek_ms(at);
        }
    }
}

/// Row 2: transport on the left; speed, volume and "Save a copy…" on the
/// right (`justify-content: space-between`). When both clusters do not fit
/// on one line, the right one wraps below, still right-aligned.
fn controls_row(
    ui: &mut egui::Ui,
    cx: &mut UiCtx,
    detail: &MeetingDetail,
    snap: &PlayerSnapshot,
    out: &mut PlayerBarOutput,
) {
    // Both clusters' widths from last pass (first pass: estimates).
    let width_id = ui.id().with("player-cluster-widths");
    let (left_width, right_width): (f32, f32) = ui
        .ctx()
        .data(|d| d.get_temp(width_id))
        .unwrap_or((150.0, 240.0));
    let one_line = ui.available_width() >= left_width + right_width + 10.0;

    let mut widths = (left_width, right_width);
    let row = ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        widths.0 = left_cluster(ui, cx, snap, out);
        if one_line {
            widths.1 = right_cluster(ui, cx, detail);
        }
    });
    if !one_line {
        ui.allocate_ui_with_layout(
            egui::vec2(row.response.rect.width().max(ui.available_width()), 0.0),
            Layout::right_to_left(Align::Center),
            |ui| widths.1 = right_cluster(ui, cx, detail),
        );
    }
    ui.ctx().data_mut(|d| d.insert_temp(width_id, widths));
}

/// ±10 s, play/pause, add bookmark. Returns the cluster's width.
fn left_cluster(
    ui: &mut egui::Ui,
    cx: &mut UiCtx,
    snap: &PlayerSnapshot,
    out: &mut PlayerBarOutput,
) -> f32 {
    let start = ui.cursor().left();
    if player_btn(
        ui,
        ph::REWIND,
        15.0,
        "Back 10 seconds",
        "Back 10 s (←: 5 s)",
        false,
    )
    .clicked()
    {
        cx.player.skip_ms(-SKIP_BUTTON_MS);
    }
    let (glyph, label, tip) = if snap.playing {
        (ph::PAUSE, "Pause", "Pause (space)")
    } else {
        (ph::PLAY, "Play", "Play (space)")
    };
    if player_btn(ui, glyph, 18.0, label, tip, true).clicked() {
        cx.player.toggle();
    }
    if player_btn(
        ui,
        ph::FAST_FORWARD,
        15.0,
        "Forward 10 seconds",
        "Forward 10 s (→: 5 s)",
        false,
    )
    .clicked()
    {
        cx.player.skip_ms(SKIP_BUTTON_MS);
    }
    if player_btn(
        ui,
        ph::BOOKMARK_SIMPLE,
        15.0,
        "Add bookmark at the current position",
        "Add bookmark here",
        false,
    )
    .clicked()
    {
        out.add_bookmark_at_ms = Some(i64::try_from(snap.position_ms).unwrap_or(i64::MAX));
    }
    ui.cursor().left() - start
}

/// Speed, volume and "Save a copy…", laid out right to left from the row's
/// right edge (so the widgets are added in reverse). Returns its width.
fn right_cluster(ui: &mut egui::Ui, cx: &mut UiCtx, detail: &MeetingDetail) -> f32 {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if player_btn(
            ui,
            ph::DOWNLOAD_SIMPLE,
            18.0,
            "Save a copy",
            "Save a copy…",
            false,
        )
        .clicked()
        {
            save_copy(cx, detail.meeting.id);
        }

        // Volume: muted speaker glyph + an 80 px slider, step 0.05.
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.spacing_mut().slider_width = 80.0;
            let mut volume = cx.prefs.volume.clamp(0.0, 1.0);
            let slider = egui::Slider::new(&mut volume, 0.0..=1.0)
                .step_by(0.05)
                .show_value(false)
                .trailing_fill(true);
            let response = ui.add(slider).on_hover_text("Volume");
            response.widget_info(|| {
                egui::WidgetInfo::slider(true, f64::from(volume) * 100.0, "Volume")
            });
            if response.changed() {
                cx.prefs.volume = volume;
                cx.player.set_volume(volume);
            }
            ui.label(
                egui::RichText::new(ph::SPEAKER_HIGH)
                    .size(18.0)
                    .color(TEXT_MUTED),
            )
            .on_hover_text("Volume");
        });

        // Speed (`.player-speed` select).
        let before = snap_speed(cx.prefs.speed);
        let mut speed = before;
        egui::ComboBox::from_id_salt("player-speed")
            .selected_text(egui::RichText::new(speed_label(speed)).size(12.0))
            .width(56.0)
            .show_ui(ui, |ui| {
                for option in SPEEDS {
                    ui.selectable_value(
                        &mut speed,
                        option,
                        egui::RichText::new(speed_label(option)).size(12.0),
                    );
                }
            })
            .response
            .on_hover_text("Playback speed");
        if (speed - before).abs() > f32::EPSILON {
            cx.prefs.speed = speed;
            cx.player.set_speed(speed);
        }
        ui.min_rect().width()
    })
    .inner
}

/// "Save a copy…": check the archive exists BEFORE any dialog (the old
/// `export_audio` command's order), then the save dialog, then the copy —
/// all in one op on a worker, so its error toast survives leaving the
/// transcript. Cancelling the dialog is silent.
fn save_copy(cx: &mut UiCtx, meeting_id: i64) {
    let owner = cx.dialog_owner();
    cx.tasks.spawn_op("export-audio", move |core| {
        let result = (|| {
            core.audio_path(meeting_id)?;
            let stem = core.suggested_file_stem(meeting_id)?;
            let request = SaveFileRequest {
                file_name: format!("{stem}.opus"),
                filter_name: "Ogg Opus audio".into(),
                extensions: vec!["opus".into()],
            };
            match dialogs::save_file_blocking(owner, request) {
                Some(dest) => core.export_audio(meeting_id, &dest),
                None => Ok(()),
            }
        })();
        OpDone::Notice {
            notice: result
                .err()
                .map(|error| Notice::error("Could not export the audio", error)),
            refresh: false,
        }
    });
}

/// `.player-btn`: a transparent glyph button (6 px padding, radius 6) whose
/// glyph turns accent on hover; `accent` = `.player-play`. `label` is the
/// accessible name, `tooltip` the CSS `title`.
fn player_btn(
    ui: &mut egui::Ui,
    glyph: &str,
    size: f32,
    label: &str,
    tooltip: &str,
    accent: bool,
) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(glyph.to_owned(), theme::regular(size), TEXT);
    let side = galley.size().x.max(size) + 12.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(side, side), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    if ui.is_rect_visible(rect) {
        let hot = response.hovered() || response.has_focus();
        let painter = ui.painter();
        if hot {
            painter.rect_filled(rect, CornerRadius::same(6), BG_HOVER);
        }
        let color = if hot || accent { ACCENT } else { TEXT };
        painter.galley(rect.center() - galley.size() / 2.0, galley, color);
        if response.has_focus() {
            painter.rect_stroke(
                rect.expand(2.0),
                CornerRadius::same(8),
                Stroke::new(2.0, FOCUS),
                egui::StrokeKind::Outside,
            );
        }
    }
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tooltip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speeds_snap_to_the_menu() {
        assert_eq!(snap_speed(1.0), 1.0);
        assert_eq!(snap_speed(1.3), 1.25);
        assert_eq!(snap_speed(0.1), 0.75);
        assert_eq!(snap_speed(9.0), 2.0);
        assert_eq!(snap_speed(f32::NAN), 1.0);
    }

    #[test]
    fn speed_labels_match_the_options() {
        let labels: Vec<String> = SPEEDS.iter().map(|s| speed_label(*s)).collect();
        assert_eq!(labels, ["0.75×", "1×", "1.25×", "1.5×", "1.75×", "2×"]);
    }

    #[test]
    fn clock_formats_like_fmt_clock() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(61_999), "1:01");
        assert_eq!(clock(3_600_000), "1:00:00");
    }
}
