//! First-run setup: welcome, capture devices (with a level check), the
//! transcription models, a couple of preferences, done. Shown instead of the
//! views until finished or skipped (`UiPrefs::onboarded`).
//!
//! The shell asks [`Onboarding::is_active`] each pass and, while it is,
//! draws [`Onboarding::show`] in the content area below the top bar.

use super::UiCtx;
use crate::ui_state::UiPrefs;
use eframe::egui;

#[derive(Default)]
pub struct Onboarding {
    active: bool,
}

impl Onboarding {
    /// Decide whether setup runs this session (`prefs.onboarded` unset;
    /// the unit may also skip it for an existing library).
    pub fn new(prefs: &UiPrefs) -> Onboarding {
        let _ = prefs;
        Onboarding { active: false }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Draw the current step in the content area.
    pub fn show(&mut self, _ui: &mut egui::Ui, _cx: &mut UiCtx) {}
}
