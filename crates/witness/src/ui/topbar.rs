//! The custom title bar (map-ui-shell §4): record/stop, bookmark and CC
//! buttons, the status pill, navigation and the window controls; plus the
//! invisible resize edges and the health and model banners below it.
//!
//! STUB (F0.2): a minimal, usable bar — background drag, double-click
//! maximize, nav buttons and window buttons. The app-chrome unit (U7)
//! implements the rest.

use super::icons::ph;
use super::theme::{BG_RAISED, BORDER};
use super::widgets;
use super::{UiCtx, View};
use eframe::egui::{self, Margin, Sense, Stroke};

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

/// Draw the bar inside `Panel::top("topbar")`.
pub fn show(ui: &mut egui::Ui, cx: &mut UiCtx) -> TopBarOutput {
    let mut out = TopBarOutput::default();
    let screen = ui.ctx().content_rect();
    // The bar's own response is registered behind the widgets, so buttons
    // keep their clicks and only the background drags the window.
    let bar = ui.scope_builder(
        egui::UiBuilder::new().sense(Sense::click_and_drag()),
        |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                ui.label(egui::RichText::new("Witness").color(super::theme::TEXT_MUTED));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if !cx.flyout.is_popup() {
                        if widgets::win_btn(ui, ph::X, 14.0, "Close to tray", true).clicked() {
                            out.close = true;
                        }
                        if widgets::win_btn(ui, ph::SQUARE, 12.0, "Maximize", false).clicked() {
                            cx.window.toggle_maximize();
                        }
                        if widgets::win_btn(ui, ph::MINUS, 14.0, "Minimize", false).clicked() {
                            out.minimize = true;
                        }
                        ui.add_space(4.0);
                    }
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let settings = matches!(cx.view, View::Settings);
                    if widgets::nav_icon_btn(ui, ph::GEAR_SIX, settings, "Settings").clicked() {
                        cx.nav.go(View::Settings);
                    }
                    if widgets::nav_btn(ui, "People", matches!(cx.view, View::People)).clicked() {
                        cx.nav.go(View::People);
                    }
                    if widgets::nav_btn(ui, "Meetings", matches!(cx.view, View::Meetings)).clicked()
                    {
                        cx.go_meetings_clear_search();
                    }
                });
            });
        },
    );
    let response = bar.response;
    if response.drag_started_by(egui::PointerButton::Primary) {
        let near_edge = response.interact_pointer_pos().is_some_and(|p| {
            p.y - screen.top() < EDGE_GRAB
                || p.x - screen.left() < EDGE_GRAB
                || screen.right() - p.x < EDGE_GRAB
        });
        if !near_edge {
            cx.flyout.pin();
            cx.window.start_drag();
        }
    }
    if response.double_clicked() {
        cx.window.toggle_maximize();
    }
    out
}

/// The top bar's panel frame: raised fill, 10/14 padding. `app.rs` paints
/// the 1 px bottom border with [`paint_bottom_border`].
pub fn frame() -> egui::Frame {
    egui::Frame::new()
        .fill(BG_RAISED)
        .inner_margin(Margin::symmetric(14, 10))
}

/// The bar's 1 px bottom border.
pub fn paint_bottom_border(ui: &egui::Ui, rect: egui::Rect) {
    ui.painter().hline(
        rect.x_range(),
        rect.bottom() - 0.5,
        Stroke::new(1.0, BORDER),
    );
}

/// Invisible 6 px edge / 12 px corner resize zones. TODO(U7)
pub fn resize_edges(_ui: &mut egui::Ui, _cx: &mut UiCtx) {}

/// The capture-health warning under the bar while recording. TODO(U7)
pub fn health_banner(_ui: &mut egui::Ui, _cx: &mut UiCtx) {}

/// The first-run "Transcription models not installed." banner. TODO(U7)
pub fn model_banner(_ui: &mut egui::Ui, _cx: &mut UiCtx) {}
