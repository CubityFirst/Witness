//! The in-app confirmation modal (`appConfirm` in confirm.tsx; WebView2
//! suppressed `window.confirm`, and the native app keeps the same look).
//!
//! Usage: on click, unless Shift is held (`ui.input(|i| i.modifiers.shift)`
//! — callers implement the skip), call [`Confirm::ask`] with an id of your
//! choosing; on later frames poll [`Confirm::answer`] with the same id.
//! A new request cancels a pending one, which then answers `false`.

use super::theme::{self, pal};
use super::widgets;
use eframe::egui::{self, CornerRadius, Margin, Stroke};
use std::collections::HashMap;

/// The modal's maximum width.
const MAX_WIDTH: f32 = 400.0;

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

    /// Close a showing request without recording an answer: the view that
    /// asked is being replaced, so nobody is left to act on a "yes".
    pub fn withdraw(&mut self) {
        self.pending = None;
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

    /// Draw the modal (call once per pass, after the views): a 12 pt
    /// rounded surface at most 400 pt wide (16 pt clear of the window
    /// edges), the message, a faint Shift hint, then Cancel and the danger
    /// button at the bottom right.
    pub fn show(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.pending.as_mut() else {
            return;
        };
        let width = (ctx.content_rect().width() - 2.0 * theme::S4).clamp(0.0, MAX_WIDTH);
        // Generous padding, a little tighter on the narrowest windows.
        let pad: i8 = if width < 360.0 { 20 } else { 24 };
        let frame = egui::Frame::new()
            .fill(pal().bg_raised)
            .stroke(Stroke::new(1.0, pal().border_strong))
            .corner_radius(CornerRadius::same(theme::R_LG))
            .inner_margin(Margin {
                left: pad,
                right: pad,
                top: pad - 2,
                bottom: pad - 4,
            })
            .shadow(ctx.global_style().visuals.window_shadow);
        let mut decision = None;
        let modal = egui::Modal::new(egui::Id::new("witness-confirm"))
            .frame(frame)
            .backdrop_color(pal().backdrop)
            .show(ctx, |ui| {
                // The frame adds its padding and the 1 pt stroke on each side.
                ui.set_width((width - 2.0 * (f32::from(pad) + 1.0)).max(0.0));
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&pending.message)
                            .color(pal().text)
                            .line_height(Some(theme::BASE_SIZE * 1.5)),
                    )
                    .wrap(),
                );
                ui.add_space(theme::S1 + 2.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new("Tip: hold Shift to skip this confirmation")
                            .font(theme::regular(12.0))
                            .color(pal().text_faint),
                    )
                    .wrap(),
                );
                ui.add_space(theme::S5 - 4.0);
                // `horizontal` bounds the row's height; a bare right-to-left
                // layout here would claim all the remaining (screen) height,
                // stretching the modal and pushing the message off the top.
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = theme::S2;
                        if widgets::btn_danger(ui, pending.ok_label.as_str()).clicked() {
                            decision = Some(true);
                        }
                        let cancel = widgets::btn(ui, "Cancel");
                        if pending.focus_cancel {
                            cancel.request_focus();
                            pending.focus_cancel = false;
                        }
                        if cancel.clicked() {
                            decision = Some(false);
                        }
                    });
                });
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
    fn withdraw_closes_without_an_answer() {
        let mut confirm = Confirm::new();
        let a = egui::Id::new("a");
        confirm.ask(a, "Delete?", "Delete");
        confirm.withdraw();
        assert!(!confirm.is_open());
        assert_eq!(confirm.answer(a), None);
    }

    #[test]
    fn modal_renders() {
        for width in [380.0, 460.0, 1000.0] {
            let ctx = theme::test_context();
            let mut confirm = Confirm::new();
            confirm.ask(
                egui::Id::new("x"),
                "Delete?\nThis cannot be undone.",
                "Delete",
            );
            for _ in 0..3 {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 620.0),
                    )),
                    ..Default::default()
                };
                let mut output = ctx.run_ui(input, |ui| confirm.show(ui.ctx()));
                output.textures_delta.clear(); // no renderer to upload the font atlas
            }
            assert!(confirm.is_open());
            // A compact card, not a screen-tall panel (the message stays on
            // screen above the buttons), clear of the window edges.
            let card = ctx
                .memory(|m| m.area_rect(egui::Id::new("witness-confirm")))
                .expect("modal area");
            assert!(card.height() < 260.0, "{card:?}");
            assert!(card.top() > 0.0, "{card:?}");
            assert!(card.width() <= MAX_WIDTH + 0.5, "{card:?}");
            assert!(card.left() >= theme::S4 - 0.5, "{width}: {card:?}");
            assert!(card.right() <= width - theme::S4 + 0.5, "{width}: {card:?}");
        }
    }
}
