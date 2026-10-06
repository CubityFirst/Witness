//! In-app notices (the TSX `notify.tsx` host), distinct from OS toasts.
//!
//! Rules (map-ui-shell §7): the message is trimmed and empty ones are
//! ignored; a notice with the same kind and message refreshes the existing
//! one instead of stacking; at most 4 are visible (the oldest is dropped);
//! errors last 9 s and info/success 5 s; a notice with an action never
//! expires; hovering the region pauses expiry, and on leave every expiry is
//! shifted by the hovered time.

use super::theme::{self, ACCENT, BG_RAISED, BORDER, OK, REC, TEXT};
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

    /// Draw the region (bottom-right, above the confirm modal). Returns the
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

        let width = (ctx.content_rect().width() - 36.0).clamp(0.0, 400.0);
        let mut clicked_action = None;
        let mut dismissed = Vec::new();
        let area = egui::Area::new(egui::Id::new("witness-toasts"))
            .order(Order::Tooltip)
            .anchor(Align2::RIGHT_BOTTOM, egui::vec2(-18.0, -18.0))
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing.y = 8.0;
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

/// One toast; returns (clicked action, dismissed).
fn toast_ui(ui: &mut egui::Ui, toast: &Toast, width: f32) -> (Option<ToastAction>, bool) {
    let politeness = match toast.kind {
        NoticeKind::Error => Politeness::Assertive,
        _ => Politeness::Polite,
    };
    let mut action = None;
    let mut dismiss = false;
    let frame = egui::Frame::new()
        .fill(BG_RAISED)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin {
            left: 12,
            right: 10,
            top: 10,
            bottom: 10,
        })
        .shadow(ui.visuals().popup_shadow);
    let response = frame
        .show(ui, |ui| {
            ui.set_width(width - 22.0);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                // Right-hand controls first, so the message gets what is left.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    let close = ui
                        .add(
                            egui::Button::new(egui::RichText::new("×").size(18.0).color(TEXT))
                                .frame(false),
                        )
                        .on_hover_text("Dismiss notification");
                    close.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            true,
                            "Dismiss notification",
                        )
                    });
                    if close.clicked() {
                        dismiss = true;
                    }
                    if let Some((label, toast_action)) = &toast.action {
                        if widgets::btn_ghost(ui, label.as_str()).clicked() {
                            action = Some(toast_action.clone());
                        }
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&toast.message)
                                    .color(TEXT)
                                    .font(theme::regular(theme::BASE_SIZE)),
                            )
                            .wrap()
                            .selectable(true),
                        );
                    });
                });
            });
        })
        .response;
    // The 3 px coloured left border, following the rounded corners.
    widgets::paint_left_edge(ui, response.rect, 3.0, 8, toast.kind.accent());
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
