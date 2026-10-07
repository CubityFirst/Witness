//! The Ctrl+K command palette: jump to any meeting or person, search
//! transcripts, or run an action (record, settings, theme…), from anywhere.
//!
//! The shell owns one [`CommandPalette`], toggles it on Ctrl+K and draws it
//! above the views with the same [`UiCtx`] they get; everything it does goes
//! through `cx` (navigation, recording control, prefs, tasks).

use super::UiCtx;
use eframe::egui;

#[derive(Default)]
pub struct CommandPalette {
    open: bool,
}

impl CommandPalette {
    pub fn new() -> CommandPalette {
        CommandPalette::default()
    }

    /// Open (Ctrl+K pressed while closed) or close it.
    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Draw it when open, above everything but the confirm modal.
    pub fn show(&mut self, _ctx: &egui::Context, _cx: &mut UiCtx) {}
}
