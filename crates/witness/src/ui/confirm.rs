//! The in-app confirmation modal (`appConfirm` in confirm.tsx; WebView2
//! suppressed `window.confirm`, and the native app keeps the same look).
//!
//! Usage: on click, unless Shift is held (`ui.input(|i| i.modifiers.shift)`
//! — callers implement the skip), call [`Confirm::ask`] with an id of your
//! choosing; on later frames poll [`Confirm::answer`] with the same id.
//! A new request cancels a pending one, which then answers `false`.

use super::theme::{self, BACKDROP, BG_RAISED, BORDER, TEXT, TEXT_MUTED};
use super::widgets;
use eframe::egui::{self, CornerRadius, Margin, Stroke};
use std::collections::HashMap;

struct Pending {
    id: egui::Id,
    message: String,
    ok_label: String,
    /// Focus Cancel on the first frame (Enter = cancel, never delete).
    focus_cancel: bool,
}

#[derive(Default)]
pub struct Confirm {
    pending: Option<Pending>,
    answers: HashMap<egui::Id, bool>,
}

impl Confirm {
    pub fn new() -> Self {
        Self::default()
    }

    /// Show the modal with `message` (`\n` breaks are kept) and a danger
    /// button labelled `ok_label`.
    pub fn ask(&mut self, id: egui::Id, message: impl Into<String>, ok_label: &str) {
        if let Some(previous) = self.pending.take() {
            self.answers.insert(previous.id, false);
        }
        self.answers.remove(&id);
        self.pending = Some(Pending {
            id,
            message: message.into(),
            ok_label: ok_label.to_owned(),
            focus_cancel: true,
        });
    }

    /// The answer for `id`, exactly once.
    pub fn answer(&mut self, id: egui::Id) -> Option<bool> {
        self.answers.remove(&id)
    }

    /// Whether a request is showing.
    pub fn is_open(&self) -> bool {
        self.pending.is_some()
    }

    fn resolve(&mut self, value: bool) {
        if let Some(pending) = self.pending.take() {
            self.answers.insert(pending.id, value);
        }
    }

    /// Draw the modal (call once per pass, after the views).
    pub fn show(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.pending.as_mut() else {
            return;
        };
        let width = (ctx.content_rect().width() - 48.0).clamp(0.0, 380.0);
        let frame = egui::Frame::new()
            .fill(BG_RAISED)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin {
                left: 20,
                right: 20,
                top: 18,
                bottom: 14,
            })
            .shadow(ctx.global_style().visuals.window_shadow);
        let mut decision = None;
        let modal = egui::Modal::new(egui::Id::new("witness-confirm"))
            .frame(frame)
            .backdrop_color(BACKDROP)
            .show(ctx, |ui| {
                ui.set_width(width - 40.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&pending.message)
                            .color(TEXT)
                            .line_height(Some(theme::BASE_SIZE * 1.5)),
                    )
                    .wrap(),
                );
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::btn_danger(ui, pending.ok_label.as_str()).clicked() {
                        decision = Some(true);
                    }
                    let cancel = widgets::btn_ghost(ui, "Cancel");
                    if pending.focus_cancel {
                        cancel.request_focus();
                        pending.focus_cancel = false;
                    }
                    if cancel.clicked() {
                        decision = Some(false);
                    }
                });
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("Tip: hold Shift to skip this confirmation")
                        .color(TEXT_MUTED),
                );
            });
        // Escape or a backdrop click cancels.
        if decision.is_none() && modal.should_close() {
            decision = Some(false);
        }
        if let Some(value) = decision {
            self.resolve(value);
            ctx.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_request_cancels_the_pending_one() {
        let mut confirm = Confirm::new();
        let (a, b) = (egui::Id::new("a"), egui::Id::new("b"));
        confirm.ask(a, "Move \"x\" to the recycle bin?", "Move to bin");
        assert!(confirm.is_open());
        confirm.ask(b, "Empty bin?", "Empty bin");
        assert_eq!(confirm.answer(a), Some(false));
        assert_eq!(confirm.answer(a), None, "answers are taken once");
        assert_eq!(confirm.answer(b), None, "b is still pending");
        confirm.resolve(true);
        assert_eq!(confirm.answer(b), Some(true));
        assert!(!confirm.is_open());
    }

    #[test]
    fn modal_renders() {
        let ctx = theme::test_context();
        let mut confirm = Confirm::new();
        confirm.ask(
            egui::Id::new("x"),
            "Delete?\nThis cannot be undone.",
            "Delete",
        );
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| confirm.show(ui.ctx()));
        output.textures_delta.clear(); // no renderer to upload the font atlas
        assert!(confirm.is_open());
    }
}
