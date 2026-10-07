//! In-app notices (the TSX `notify.tsx` host), distinct from OS toasts.
//!
//! Rules (map-ui-shell §7): the message is trimmed and empty ones are
//! ignored; a notice with the same kind and message refreshes the existing
//! one instead of stacking; at most 4 are visible (the oldest is dropped);
//! errors last 9 s and info/success 5 s; a notice with an action never
//! expires; hovering the region pauses expiry, and on leave every expiry is
//! shifted by the hovered time. They stack bottom-centre, at most 420 pt
//! wide.

use super::icons::ph;
use super::theme::{self, ACCENT, ACCENT_HOVER, BG_RAISED, BORDER_STRONG, OK, REC, TEXT};
use super::widgets::{self, Politeness};
use eframe::egui::{self, Align2, Color32, CornerRadius, Margin, Order, Stroke};
use std::fmt::Display;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Error,
    Info,
    Success,
}

impl NoticeKind {
    fn lifetime(self) -> Duration {
        match self {
            NoticeKind::Error => Duration::from_secs(9),
            NoticeKind::Info | NoticeKind::Success => Duration::from_secs(5),
        }
    }

    fn accent(self) -> Color32 {
        match self {
            NoticeKind::Error => REC,
            NoticeKind::Info => ACCENT,
            NoticeKind::Success => OK,
        }
    }

    /// The kind's leading icon.
    fn glyph(self) -> &'static str {
        match self {
            NoticeKind::Error => ph::WARNING_CIRCLE,
            NoticeKind::Info => ph::INFO,
            NoticeKind::Success => ph::CHECK_CIRCLE,
        }
    }
}

/// What a toast's action button does; handled by `app.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToastAction {
    /// "Undo" on "Meeting moved to the recycle bin".
    RestoreMeeting(i64),
}

/// A notice to show, as produced by ops (`OpDone::Notice`) and effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub message: String,
    /// Button label + action. Notices with an action never expire.
    pub action: Option<(String, ToastAction)>,
}

impl Notice {
    /// "`message`: `err`" — `notifyError(message, error)`.
    pub fn error(message: &str, err: impl Display) -> Notice {
        Notice::error_plain(format!("{message}: {err}"))
    }

    /// An error notice shown verbatim.
    pub fn error_plain(message: impl Into<String>) -> Notice {
        Notice {
            kind: NoticeKind::Error,
            message: message.into(),
            action: None,
        }
    }

    pub fn info(message: impl Into<String>) -> Notice {
        Notice {
            kind: NoticeKind::Info,
            message: message.into(),
            action: None,
        }
    }

    pub fn success(message: impl Into<String>) -> Notice {
        Notice {
            kind: NoticeKind::Success,
            message: message.into(),
            action: None,
        }
    }

    /// Attach an action button.
    pub fn with_action(mut self, label: impl Into<String>, action: ToastAction) -> Notice {
        self.action = Some((label.into(), action));
        self
    }
}

struct Toast {
    id: u64,
    kind: NoticeKind,
    message: String,
    /// None = never expires (has an action).
    expires_at: Option<Instant>,
    action: Option<(String, ToastAction)>,
}

/// The app-level toast host.
#[derive(Default)]
pub struct Toasts {
    items: Vec<Toast>,
    next_id: u64,
    /// Set while the pointer is over the toast region.
    paused_since: Option<Instant>,
}

impl Toasts {
    pub fn new() -> Self {
        Self::default()
    }

    /// `notifyError(message, err)`: "message: err".
    pub fn error(&mut self, message: &str, err: impl Display) {
        self.push(Notice::error(message, err));
    }

    #[cfg(test)]
    pub fn info(&mut self, message: impl Into<String>) {
        self.push(Notice::info(message));
    }

    pub fn success(&mut self, message: impl Into<String>) {
        self.push(Notice::success(message));
    }

    /// Add (or refresh) a notice.
    pub fn push(&mut self, notice: Notice) {
        self.push_at(notice, Instant::now());
    }

    fn push_at(&mut self, notice: Notice, now: Instant) {
        let message = notice.message.trim().to_string();
        if message.is_empty() {
            return;
        }
        let expires_at = match notice.action {
            Some(_) => None,
            None => Some(now + notice.kind.lifetime()),
        };
        if let Some(existing) = self
            .items
            .iter_mut()
            .find(|t| t.kind == notice.kind && t.message == message)
        {
            existing.expires_at = expires_at;
            existing.action = notice.action;
            return;
        }
        if self.items.len() >= 4 {
            let excess = self.items.len() - 3;
            self.items.drain(..excess);
        }
        self.next_id += 1;
        self.items.push(Toast {
            id: self.next_id,
            kind: notice.kind,
            message,
            expires_at,
            action: notice.action,
        });
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Drop expired notices (not while paused); returns the next expiry.
    fn expire(&mut self, now: Instant) -> Option<Instant> {
        if self.paused_since.is_some() {
            return None;
        }
        self.items
            .retain(|t| t.expires_at.is_none_or(|expires| expires > now));
        self.items.iter().filter_map(|t| t.expires_at).min()
    }

    fn set_hovered(&mut self, hovered: bool, now: Instant) {
        match (hovered, self.paused_since) {
            (true, None) => self.paused_since = Some(now),
            (false, Some(since)) => {
                self.paused_since = None;
                let delta = now.saturating_duration_since(since);
                for toast in &mut self.items {
                    if let Some(expires) = toast.expires_at.as_mut() {
                        *expires += delta;
                    }
                }
            }
            _ => {}
        }
    }

    /// Draw the region (bottom-centre, above the confirm modal). Returns the
    /// action whose button was clicked this frame (its notice is dismissed).
    pub fn show(&mut self, ctx: &egui::Context) -> Option<ToastAction> {
        let now = Instant::now();
        if let Some(next) = self.expire(now) {
            ctx.request_repaint_after(next.saturating_duration_since(now));
        }
        if self.items.is_empty() {
            self.paused_since = None;
            return None;
        }

        let width = toast_width(ctx.content_rect().width());
        let mut clicked_action = None;
        let mut dismissed = Vec::new();
        let area = egui::Area::new(egui::Id::new("witness-toasts"))
            .order(Order::Tooltip)
            .anchor(Align2::CENTER_BOTTOM, egui::vec2(0.0, -theme::S4))
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing.y = theme::S2;
                for toast in &self.items {
                    let (action, dismiss) = toast_ui(ui, toast, width);
                    if let Some(action) = action {
                        clicked_action = Some(action);
                        dismissed.push(toast.id);
                    }
                    if dismiss {
                        dismissed.push(toast.id);
                    }
                }
            });
        let hovered = area.response.contains_pointer()
            || ctx
                .pointer_hover_pos()
                .is_some_and(|p| area.response.rect.contains(p));
        self.set_hovered(hovered, now);
        if !dismissed.is_empty() {
            self.items.retain(|t| !dismissed.contains(&t.id));
            ctx.request_repaint();
        }
        clicked_action
    }
}

/// Toasts are at most this wide.
const MAX_WIDTH: f32 = 420.0;

/// The toasts' width in a window `window` pt wide: 16 pt clear of its
/// edges, at most [`MAX_WIDTH`].
fn toast_width(window: f32) -> f32 {
    (window - 2.0 * theme::S4).clamp(0.0, MAX_WIDTH)
}

/// The toast's padding: left, right, top / bottom.
const PAD_LEFT: f32 = 14.0;
const PAD_RIGHT: f32 = 8.0;
const PAD_Y: f32 = 8.0;
/// The band the icon, the controls and the first text line centre on.
const FIRST_LINE: f32 = 32.0;
/// The kind icon's size; the dismiss button's side (`widgets::icon_btn`
/// with a 14 pt glyph).
const ICON: f32 = 16.0;
const CLOSE: f32 = 28.0;

/// Where one toast's pieces go, from its content width and measurements
/// (pure, so the layout is testable): `(icon, message, action, close)`
/// rects relative to the content's top-left, and the content height.
fn toast_layout(
    width: f32,
    message: egui::Vec2,
    line_height: f32,
    action_w: Option<f32>,
) -> (egui::Rect, egui::Rect, Option<egui::Rect>, egui::Rect, f32) {
    use egui::{pos2, vec2, Rect};
    let text_top = ((FIRST_LINE - line_height) / 2.0).max(0.0);
    let height = FIRST_LINE.max(text_top * 2.0 + message.y);
    let icon = Rect::from_min_size(pos2(0.0, (FIRST_LINE - ICON) / 2.0), vec2(ICON, ICON));
    let close = Rect::from_min_size(
        pos2(width - CLOSE, (FIRST_LINE - CLOSE) / 2.0),
        vec2(CLOSE, CLOSE),
    );
    let mut right = close.left() - theme::S1;
    let action = action_w.map(|w| {
        right -= w;
        let rect = Rect::from_min_size(pos2(right, 0.0), vec2(w, FIRST_LINE));
        right -= theme::S1;
        rect
    });
    let left = ICON + theme::S3;
    let text = Rect::from_min_max(
        pos2(left, text_top),
        pos2((right - theme::S1).max(left), text_top + message.y),
    );
    (icon, text, action, close, height)
}

/// One toast — a raised 12 pt surface: kind icon, the (selectable, wrapping)
/// message, the action and ×. Returns (clicked action, dismissed).
fn toast_ui(ui: &mut egui::Ui, toast: &Toast, width: f32) -> (Option<ToastAction>, bool) {
    let politeness = match toast.kind {
        NoticeKind::Error => Politeness::Assertive,
        _ => Politeness::Polite,
    };
    let mut action = None;
    let mut dismiss = false;
    // The frame adds its padding and the 1 pt stroke on each side.
    let content_w = (width - PAD_LEFT - PAD_RIGHT - 2.0).max(0.0);
    let font = theme::regular(theme::BASE_SIZE);
    // The action button's width (`widgets::btn_ghost` padding).
    let action_w = toast.action.as_ref().map(|(label, _)| {
        let pad = if theme::narrow(ui) { 20.0 } else { 28.0 };
        ui.painter()
            .layout_no_wrap(label.clone(), font.clone(), TEXT)
            .size()
            .x
            .ceil()
            + pad
    });
    let line_height = ui
        .painter()
        .layout_no_wrap("0".to_owned(), font.clone(), TEXT)
        .size()
        .y;
    // Measure the message at the width it gets (no action yet: the
    // message rect's width does not depend on its height).
    let (_, probe, _, _, _) = toast_layout(content_w, egui::Vec2::ZERO, line_height, action_w);
    let galley = ui
        .painter()
        .layout(toast.message.clone(), font, TEXT, probe.width().max(1.0));
    let (icon, text, action_rect, close, height) =
        toast_layout(content_w, galley.size(), line_height, action_w);

    let frame = egui::Frame::new()
        .fill(BG_RAISED)
        .stroke(Stroke::new(1.0, BORDER_STRONG))
        .corner_radius(CornerRadius::same(theme::R_LG))
        .inner_margin(Margin {
            left: PAD_LEFT as i8,
            right: PAD_RIGHT as i8,
            top: PAD_Y as i8,
            bottom: PAD_Y as i8,
        })
        .shadow(ui.visuals().popup_shadow);
    frame.show(ui, |ui| {
        let (_, content) = ui.allocate_space(egui::vec2(content_w, height));
        let at = |r: egui::Rect| r.translate(content.min.to_vec2());
        let glyph = ui.painter().layout_no_wrap(
            toast.kind.glyph().to_owned(),
            egui::FontId::proportional(ICON),
            toast.kind.accent(),
        );
        let icon = at(icon);
        ui.painter().galley(
            icon.center() - glyph.size() / 2.0,
            glyph,
            toast.kind.accent(),
        );
        // Each control in its own slot; the message left-aligned in its own.
        let mut slot =
            |rect: egui::Rect, layout: egui::Layout, add: &mut dyn FnMut(&mut egui::Ui)| {
                ui.scope_builder(
                    egui::UiBuilder::new().max_rect(at(rect)).layout(layout),
                    |ui| add(ui),
                );
            };
        slot(text, egui::Layout::top_down(egui::Align::Min), &mut |ui| {
            ui.add(egui::Label::new(galley.clone()).selectable(true));
        });
        if let (Some(rect), Some((label, toast_action))) = (action_rect, &toast.action) {
            slot(
                rect,
                egui::Layout::right_to_left(egui::Align::Center),
                &mut |ui| {
                    let text = egui::RichText::new(label.as_str()).color(ACCENT_HOVER);
                    if widgets::btn_ghost(ui, text).clicked() {
                        action = Some(toast_action.clone());
                    }
                },
            );
        }
        slot(
            close,
            egui::Layout::right_to_left(egui::Align::Center),
            &mut |ui| {
                if widgets::icon_btn(ui, ph::X, 14.0, "Dismiss notification").clicked() {
                    dismiss = true;
                }
            },
        );
    });
    widgets::announce(
        ui.ctx(),
        egui::Id::new(("witness-toast", toast.id)),
        &toast.message,
        politeness,
    );
    (action, dismiss)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(t: &Toasts) -> Vec<&str> {
        t.items.iter().map(|t| t.message.as_str()).collect()
    }

    #[test]
    fn trims_and_ignores_empty_messages() {
        let mut t = Toasts::new();
        t.info("   ");
        t.info("  Saved  ");
        assert_eq!(messages(&t), ["Saved"]);
    }

    #[test]
    fn toasts_are_at_most_420_wide_and_clear_the_edges() {
        assert_eq!(toast_width(380.0), 348.0);
        assert_eq!(toast_width(460.0), 420.0);
        assert_eq!(toast_width(1000.0), 420.0);
        assert_eq!(toast_width(10.0), 0.0);
    }

    #[test]
    fn toast_pieces_sit_side_by_side_on_the_first_line() {
        for action in [None, Some(60.0)] {
            // One line, then a long wrapped message.
            for message in [egui::vec2(120.0, 19.0), egui::vec2(250.0, 57.0)] {
                let (icon, text, action, close, height) =
                    toast_layout(326.0, message, 19.0, action);
                let mut row = vec![icon, text, close];
                row.extend(action);
                row.sort_by(|a, b| a.left().total_cmp(&b.left()));
                for pair in row.windows(2) {
                    assert!(pair[0].right() <= pair[1].left(), "{row:?}");
                }
                assert!(close.right() <= 326.0 && icon.left() >= 0.0);
                // Icon, × and the first text line share one centre line.
                let first_line = text.top() + 19.0 / 2.0;
                assert!((icon.center().y - first_line).abs() < 0.01);
                assert!((close.center().y - first_line).abs() < 0.01);
                // The toast grows with the message, never below one band.
                assert!(height >= FIRST_LINE && height >= text.bottom(), "{height}");
            }
        }
    }

    #[test]
    fn toasts_render_bottom_centre_inside_the_window() {
        for width in [380.0, 1000.0] {
            let ctx = theme::test_context();
            let mut t = Toasts::new();
            t.push(Notice::error(
                "Could not save settings",
                "the disk is full and a very long reason follows it",
            ));
            t.push(Notice::success("Moved").with_action("Undo", ToastAction::RestoreMeeting(1)));
            for _ in 0..3 {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 620.0),
                    )),
                    ..Default::default()
                };
                let mut output = ctx.run_ui(input, |ui| {
                    t.show(ui.ctx());
                });
                output.textures_delta.clear();
            }
            let area = ctx
                .memory(|m| m.area_rect(egui::Id::new("witness-toasts")))
                .expect("toast area");
            assert!(area.width() <= MAX_WIDTH + 0.5, "{area:?}");
            assert!(area.left() >= theme::S4 - 0.5, "{width}: {area:?}");
            assert!(area.right() <= width - theme::S4 + 0.5, "{width}: {area:?}");
            assert!((area.center().x - width / 2.0).abs() < 1.0, "{area:?}");
            assert!(
                (area.bottom() - (620.0 - theme::S4)).abs() < 1.0,
                "{area:?}"
            );
        }
    }

    #[test]
    fn error_joins_message_and_cause() {
        assert_eq!(
            Notice::error("Could not save settings", "disk full").message,
            "Could not save settings: disk full"
        );
    }

    #[test]
    fn dedupes_on_kind_and_message() {
        let now = Instant::now();
        let mut t = Toasts::new();
        t.push_at(Notice::info("Same"), now);
        t.push_at(Notice::error_plain("Same"), now);
        t.push_at(Notice::info("Same"), now + Duration::from_secs(3));
        assert_eq!(messages(&t), ["Same", "Same"]);
        // The refreshed info notice now expires 5 s after the second push.
        assert_eq!(t.items[0].expires_at, Some(now + Duration::from_secs(8)));
    }

    #[test]
    fn keeps_at_most_four_dropping_the_oldest() {
        let mut t = Toasts::new();
        for i in 0..6 {
            t.info(format!("n{i}"));
        }
        assert_eq!(messages(&t), ["n2", "n3", "n4", "n5"]);
    }

    #[test]
    fn lifetimes_and_sticky_actions() {
        let now = Instant::now();
        let mut t = Toasts::new();
        t.push_at(Notice::error_plain("e"), now);
        t.push_at(Notice::success("s"), now);
        t.push_at(
            Notice::success("Meeting moved to the recycle bin")
                .with_action("Undo", ToastAction::RestoreMeeting(7)),
            now,
        );
        assert_eq!(
            t.expire(now + Duration::from_secs(6)),
            Some(now + Duration::from_secs(9))
        );
        assert_eq!(messages(&t), ["e", "Meeting moved to the recycle bin"]);
        assert_eq!(t.expire(now + Duration::from_secs(10)), None);
        assert_eq!(messages(&t), ["Meeting moved to the recycle bin"]);
    }

    #[test]
    fn hover_pauses_and_shifts_expiry() {
        let now = Instant::now();
        let mut t = Toasts::new();
        t.push_at(Notice::info("i"), now);
        t.set_hovered(true, now + Duration::from_secs(1));
        assert_eq!(t.expire(now + Duration::from_secs(30)), None);
        assert_eq!(messages(&t), ["i"], "nothing expires while hovered");
        t.set_hovered(false, now + Duration::from_secs(31));
        // 30 s of hover pushed the 5 s expiry to 35 s.
        assert_eq!(
            t.expire(now + Duration::from_secs(34)),
            Some(now + Duration::from_secs(35))
        );
        t.expire(now + Duration::from_secs(36));
        assert!(t.is_empty());
    }
}
