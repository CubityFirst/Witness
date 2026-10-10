//! The Settings chrome around the category pages.
//!
//! - **Wide** (content ≥ [`WIDE_MIN`]): a navigation rail on the left (one
//!   row per category: icon + name, the selected one a neutral `bg_active`
//!   fill, hover the usual left-edge pill), the selected category's page on
//!   the right — its title (with the "saved ✓" flash) fixed above its own
//!   scroller, the sections left-aligned in a column of at most
//!   `CONTENT_MAX`.
//! - **Narrow** (the 380–460 pt tray flyout included): an index of the
//!   categories (icon, name, a one-line summary of the current values,
//!   chevron); opening one shows its page under a Back button.

use super::{SettingsPage, SettingsState};
use crate::settings_store::SAVED_FLASH;
use crate::ui::icons::ph;
use crate::ui::theme::{self, pal};
use crate::ui::widgets::{self, KeyScroll, Politeness};
use crate::ui::UiCtx;
use eframe::egui::{
    self, Align, CornerRadius, Layout, Rect, RichText, Stroke, UiBuilder, WidgetInfo, WidgetType,
};

/// From this content width the rail sits beside the page.
pub(crate) const WIDE_MIN: f32 = 720.0;
/// The navigation rail's width.
const RAIL_WIDTH: f32 = 212.0;
/// Between the rail and the page.
const RAIL_GAP: f32 = theme::S5;
/// The page title row (the rail's first row lines up with it).
const HEADER_H: f32 = 36.0;
/// The index rows' icon tile.
const ICON_TILE: f32 = 32.0;
/// The index rows' chevron.
const CHEVRON: f32 = 14.0;

/// Whether a content area `width` pt wide shows the rail beside the page.
pub(crate) fn is_wide(width: f32) -> bool {
    width >= WIDE_MIN
}

/// `WITNESS_DEBUG_SETTINGS_PAGE`: a category slug opens that page, `index`
/// shows the (narrow) index; None when not understood.
#[cfg_attr(not(any(debug_assertions, test)), allow(dead_code))]
pub(super) fn parse_debug_page(spec: &str) -> Option<Option<SettingsPage>> {
    if spec.trim().eq_ignore_ascii_case("index") {
        return Some(None);
    }
    SettingsPage::from_slug(spec).map(Some)
}

impl SettingsState {
    /// The rail beside the selected page.
    pub(super) fn wide(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let full = ui.available_rect_before_wrap();
        let pad = widgets::pad_x(full.width());
        let rail = Rect::from_min_max(
            egui::pos2(full.left() + pad, full.top()),
            egui::pos2(full.left() + pad + RAIL_WIDTH, full.bottom()),
        );
        let page = Rect::from_min_max(egui::pos2(rail.right() + RAIL_GAP, full.top()), full.max);
        let mut rail_ui = ui.new_child(
            UiBuilder::new()
                .id_salt("settings-rail")
                .max_rect(rail)
                .layout(Layout::top_down(Align::Min)),
        );
        self.rail(&mut rail_ui, cx);
        let mut page_ui = ui.new_child(
            UiBuilder::new()
                .id_salt("settings-page")
                .max_rect(page)
                .layout(Layout::top_down(Align::Min)),
        );
        self.page_pane(&mut page_ui, cx, Some(pad));
        ui.advance_cursor_after_rect(full);
    }

    /// The index, or the open page.
    pub(super) fn narrow(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        if self.route.open {
            self.page_pane(ui, cx, None);
        } else {
            self.index(ui, cx);
        }
    }

    fn rail(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        // The field lines up with the page title beside it.
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), HEADER_H),
            Layout::top_down(Align::Min).with_main_align(Align::Center),
            |ui| {
                ui.set_min_size(egui::vec2(ui.available_width(), HEADER_H));
                self.search_field(ui, cx);
            },
        );
        ui.add_space(theme::S2);
        // While searching no category is selected: the results are shown.
        let selected = (!self.searching()).then_some(self.route.page);
        let mut picked = None;
        egui::ScrollArea::vertical()
            .id_salt("settings-rail-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for page in SettingsPage::ALL {
                    if rail_item(ui, page, Some(page) == selected).clicked() {
                        picked = Some(page);
                    }
                }
                ui.add_space(theme::S4);
            });
        if let Some(page) = picked {
            // A category picked while searching ends the search.
            self.search_text.clear();
            self.open_page(cx, page);
        }
    }

    /// Narrow windows: every category with its summary.
    fn index(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        widgets::padded_column(ui, |ui| {
            page_header(ui, cx, "Settings", false);
            ui.add_space(theme::S1);
            self.search_field(ui, cx);
            ui.add_space(theme::S1);
        });
        let summaries = SettingsPage::ALL.map(|page| self.summary(cx, page));
        let mut picked = None;
        let searching = self.searching();
        let out = widgets::scroll_column(ui, "settings-index", KeyScroll { space: true }, |ui| {
            if searching {
                self.search_results(ui, cx);
                return;
            }
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add_space(theme::S2);
            for (page, summary) in SettingsPage::ALL.into_iter().zip(&summaries) {
                if index_row(ui, page, summary).clicked() {
                    picked = Some(page);
                }
            }
        });
        scrolled_hairline(ui, &out);
        if let Some(page) = picked {
            self.open_page(cx, page);
        }
    }

    /// A page: its title (Back first when narrow) above its own scroller.
    /// `wide` is the right padding beside the rail, None when narrow.
    fn page_pane(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx, wide: Option<f32>) {
        let page = self.route.page;
        if let (Some(pad), true) = (wide, self.searching()) {
            self.results_pane(ui, cx, pad);
            return;
        }
        let back = match wide {
            Some(pad) => {
                left_column(ui, pad, |ui| page_header(ui, cx, page.title(), false));
                false
            }
            None => widgets::padded_column(ui, |ui| page_header(ui, cx, page.title(), true)),
        };
        let out = egui::ScrollArea::vertical()
            .id_salt(("settings-page", page.slug()))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                widgets::keyboard_scroll(ui, KeyScroll { space: true });
                match wide {
                    Some(pad) => left_column(ui, pad, |ui| self.page_body(ui, cx, page)),
                    None => widgets::padded_column(ui, |ui| self.page_body(ui, cx, page)),
                }
                ui.add_space(theme::S5);
            });
        scrolled_hairline(ui, &out);
        #[cfg(debug_assertions)]
        self.debug_scroll(ui.ctx(), &out);
        if back {
            self.close_page(cx);
        }
    }
}

impl SettingsState {
    /// Wide windows while searching: the results where the page was.
    fn results_pane(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx, pad: f32) {
        left_column(ui, pad, |ui| page_header(ui, cx, "Search results", false));
        let out = egui::ScrollArea::vertical()
            .id_salt("settings-results")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                left_column(ui, pad, |ui| self.search_results(ui, cx));
                ui.add_space(theme::S5);
            });
        scrolled_hairline(ui, &out);
    }
}

/// A column of at most `CONTENT_MAX` at the left of the page area, `pad`
/// short of its right edge (where the floating scroll bar sits).
fn left_column<R>(ui: &mut egui::Ui, pad: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let width = (ui.available_width() - pad).clamp(0.0, theme::CONTENT_MAX);
    ui.vertical(|ui| {
        ui.set_width(width);
        add(ui)
    })
    .inner
}

/// A hairline under the fixed header once the content scrolls beneath it.
fn scrolled_hairline<R>(ui: &egui::Ui, out: &egui::scroll_area::ScrollAreaOutput<R>) {
    if out.state.offset.y > 0.5 {
        ui.painter().hline(
            ui.max_rect().x_range(),
            out.inner_rect.top(),
            Stroke::new(1.0, pal().border),
        );
    }
}

/// The title row: an optional Back button, the title (20 semibold) and the
/// "saved ✓" flash. Returns whether Back was clicked.
fn page_header(ui: &mut egui::Ui, cx: &mut UiCtx, title: &str, back: bool) -> bool {
    let width = ui.available_width();
    ui.allocate_ui_with_layout(
        egui::vec2(width, HEADER_H),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_min_size(egui::vec2(width, HEADER_H));
            ui.spacing_mut().item_spacing.x = theme::S2;
            let back = back && widgets::icon_btn(ui, ph::ARROW_LEFT, 18.0, "Back").clicked();
            ui.label(
                RichText::new(title)
                    .font(theme::semibold(20.0))
                    .color(pal().text),
            );
            if let Some(at) = cx.settings.saved_flash() {
                ui.add_space(theme::S1);
                ui.label(RichText::new("saved ✓").size(12.5).color(pal().ok));
                widgets::announce(
                    ui.ctx(),
                    egui::Id::new("settings-saved-flash"),
                    "saved ✓",
                    Politeness::Polite,
                );
                let left = SAVED_FLASH.saturating_sub(at.elapsed());
                ui.ctx().request_repaint_after(left);
            }
            back
        },
    )
    .inner
}

/// A rail row: the category's icon and name; the selected one filled.
fn rail_item(ui: &mut egui::Ui, page: SettingsPage, selected: bool) -> egui::Response {
    let (row, ()) = widgets::list_row(
        ui,
        ("settings-rail", page.slug()),
        selected,
        |ui, hovered| {
            let font = if selected {
                theme::semibold(14.0)
            } else {
                theme::regular(14.0)
            };
            let height = ui.fonts_mut(|f| f.row_height(&font)).max(20.0);
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), height),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = theme::S3;
                    let glyph = if selected || hovered {
                        pal().text
                    } else {
                        pal().text_muted
                    };
                    ui.label(RichText::new(page.icon()).size(17.0).color(glyph));
                    widgets::elide(ui, page.title(), font, pal().text);
                },
            );
        },
    );
    row.widget_info(|| {
        WidgetInfo::selected(WidgetType::SelectableLabel, true, selected, page.title())
    });
    row
}

/// An index row: icon tile, name over the summary, chevron.
fn index_row(ui: &mut egui::Ui, page: SettingsPage, summary: &str) -> egui::Response {
    let (row, ()) = widgets::list_row(ui, ("settings-index", page.slug()), false, |ui, hovered| {
        let title_font = theme::regular(14.0);
        let meta_font = theme::regular(12.5);
        let text_h = ui.fonts_mut(|f| f.row_height(&title_font) + f.row_height(&meta_font)) + 1.0;
        let height = text_h.max(ICON_TILE);
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), height),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = theme::S3;
                icon_tile(ui, page.icon());
                let text_w = (ui.available_width() - CHEVRON - theme::S3).max(0.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(text_w, text_h),
                    Layout::top_down(Align::Min),
                    |ui| {
                        ui.set_width(text_w);
                        ui.spacing_mut().item_spacing.y = 1.0;
                        widgets::elide(ui, page.title(), title_font, pal().text);
                        widgets::elide(ui, summary, meta_font, pal().text_muted);
                    },
                );
                let color = if hovered {
                    pal().text
                } else {
                    pal().text_faint
                };
                ui.label(RichText::new(ph::CARET_RIGHT).size(CHEVRON).color(color));
            },
        );
    });
    let name = format!("{}, {summary}", page.title());
    row.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
    row
}

/// A category icon on a soft rounded tile.
fn icon_tile(ui: &mut egui::Ui, glyph: &str) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ICON_TILE, ICON_TILE), egui::Sense::hover());
    let p = pal();
    ui.painter()
        .rect_filled(rect, CornerRadius::same(theme::R_MD), p.bg_hover);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(18.0),
        p.text,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rail_shows_from_720_pt() {
        // The tray flyout and other narrow windows get the index.
        for width in [380.0, 460.0, 640.0, 719.5] {
            assert!(!is_wide(width), "{width}");
        }
        for width in [720.0, 800.0, 1200.0] {
            assert!(is_wide(width), "{width}");
        }
        // The rail plus a page still fits a 380 pt row at the threshold.
        const { assert!(WIDE_MIN - RAIL_WIDTH - RAIL_GAP - 2.0 * theme::S5 >= 380.0) };
    }

    #[test]
    fn debug_page_hook_grammar() {
        assert_eq!(parse_debug_page("index"), Some(None));
        assert_eq!(parse_debug_page("Voices"), Some(Some(SettingsPage::Voices)));
        assert_eq!(parse_debug_page("nope"), None);
    }
}
