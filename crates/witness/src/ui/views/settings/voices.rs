//! Settings → Voices: how strictly speakers are matched to enrolled voices,
//! and the enrolled voice prints (forget one per row).

use super::{
    footnote, note, notice_done, quiet, row, save, section, title, SettingsState, Slot, FIELD_WIDTH,
};
use crate::ui::icons::ph;
use crate::ui::theme::{self, pal};
use crate::ui::toasts::Notice;
use crate::ui::widgets;
use crate::ui::UiCtx;
use eframe::egui;

/// The voice-match slider's UI range (the backend accepts 0–1).
pub(super) const THRESHOLD_MIN: f32 = 0.40;
pub(super) const THRESHOLD_MAX: f32 = 0.80;
pub(super) const THRESHOLD_STEP: f32 = 0.05;
/// What the voice-match slider trades off.
const THRESHOLD_HELP: &str = "How alike two voices must sound to label a speaker automatically. Lower labels more, with more mistakes.";

impl SettingsState {
    /// "3 voices enrolled · strictness 0.60".
    pub(super) fn voices_summary(&self, cx: &UiCtx) -> String {
        let threshold = self
            .local_threshold
            .unwrap_or(cx.settings.current().speaker_match_threshold);
        voices_summary(self.people_loaded.then_some(self.people.len()), threshold)
    }

    pub(super) fn voices_page(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let stored = cx.settings.current().speaker_match_threshold;
        let local_threshold = &mut self.local_threshold;
        let threshold = section(ui, "Voice matching", |ui| {
            threshold_row(ui, local_threshold, stored)
        });
        footnote(
            ui,
            "Speakers matched by voice show a ≈ before their name; renaming them confirms or corrects the match.",
        );
        if let Some(threshold) = threshold {
            save(cx, |s| s.speaker_match_threshold = threshold);
        }
        self.people_section(ui, cx);
    }

    fn people_section(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let people = &self.people;
        let forget = section(ui, "Voice prints", |ui| {
            if people.is_empty() {
                note(
                    ui,
                    "No one enrolled yet. Rename a speaker in any transcript to teach Witness their voice; future meetings then label them automatically.",
                );
                return None;
            }
            let mut forget = None;
            for (i, person) in people.iter().enumerate() {
                if i > 0 {
                    widgets::group_divider(ui);
                }
                let clicked = row(
                    ui,
                    ("person", person.id),
                    Slot::Fit,
                    |ui| {
                        let id = title(ui, &person.name);
                        note(ui, &speech_learned(person.sample_seconds));
                        id
                    },
                    |ui, _| {
                        widgets::icon_btn_danger(
                            ui,
                            ph::TRASH,
                            15.0,
                            "Forget this voice (Shift-click skips confirmation)",
                        )
                        .clicked()
                    },
                );
                if clicked {
                    let shift = ui.input(|i| i.modifiers.shift);
                    forget = Some((person.id, person.name.clone(), shift));
                }
            }
            forget
        });
        if let Some((id, name, shift)) = forget {
            if shift {
                self.forget(cx, id, name);
            } else {
                cx.confirm.ask(
                    forget_confirm_id(id),
                    format!("Forget {name}'s voice?\nExisting transcripts keep their names."),
                    "Forget voice",
                );
                self.confirm_forget = Some((id, name));
            }
        }
    }

    pub(super) fn forget(&mut self, cx: &mut UiCtx, person_id: i64, name: String) {
        let handle = cx
            .tasks
            .spawn_op("forget-voice", move |c| match c.delete_person(person_id) {
                Ok(()) => quiet(),
                Err(error) => {
                    notice_done(Notice::error(&format!("Could not forget {name}"), error))
                }
            });
        // Hide the row now; the reload after the op restores it on failure.
        self.people.retain(|p| p.id != person_id);
        self.forgetting.push(handle);
    }
}

pub(super) fn forget_confirm_id(person_id: i64) -> egui::Id {
    egui::Id::new(("settings-forget-voice", person_id))
}

/// The voice-match slider row: the value readout, then the slider. Returns
/// the threshold to save once an edit is finished.
fn threshold_row(ui: &mut egui::Ui, local: &mut Option<f32>, stored: f32) -> Option<f32> {
    row(
        ui,
        "threshold",
        Slot::Fill(FIELD_WIDTH),
        |ui| {
            let id = title(ui, "Voice match strictness");
            note(ui, THRESHOLD_HELP);
            id
        },
        |ui, _| {
            let shown_before = local.unwrap_or(stored);
            let mut value = shown_before;
            let enabled = ui.is_enabled();
            // The readout's place; painted after the slider moved.
            let (readout, _) = ui.allocate_exact_size(
                egui::vec2(34.0, ui.spacing().interact_size.y),
                egui::Sense::hover(),
            );
            let width = ui.available_width().clamp(60.0, 360.0);
            let response = widgets::slider(
                ui,
                &mut value,
                THRESHOLD_MIN..=THRESHOLD_MAX,
                THRESHOLD_STEP as f64,
                Some(width),
            );
            let shown = value;
            response.widget_info(|| {
                egui::WidgetInfo::slider(
                    enabled,
                    shown as f64,
                    format!("Voice match strictness: {}", threshold_value_text(shown)),
                )
            });
            let interacted = response.is_pointer_button_down_on()
                || response.dragged()
                || response.drag_stopped()
                || response.clicked()
                || (response.has_focus()
                    && ui.input(|i| {
                        i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::ArrowRight)
                    }));
            if let Some(edit) = threshold_edit(shown_before, value, response.changed(), interacted)
            {
                *local = Some(edit);
            }
            // Commit once the pointer lets go (a keyboard change commits at
            // once), like the range input's `onChange`.
            let mut commit = None;
            if let Some(value) = *local {
                if !response.dragged() && !response.is_pointer_button_down_on() {
                    *local = None;
                    commit = (value != stored).then_some(value);
                }
            }
            ui.painter().text(
                egui::pos2(readout.left(), response.rect.center().y),
                egui::Align2::LEFT_CENTER,
                threshold_readout(local.or(commit).unwrap_or(stored)),
                theme::regular(theme::BASE_SIZE),
                pal().text,
            );
            commit
        },
    )
}

/// Snap a slider value to the 0.05 grid inside 0.40–0.80 (no float noise
/// like 0.6500001 reaches the settings file).
fn snap_threshold(value: f32) -> f32 {
    let clamped = value.clamp(THRESHOLD_MIN, THRESHOLD_MAX);
    let snapped = (clamped / THRESHOLD_STEP).round() * THRESHOLD_STEP;
    (snapped * 100.0).round() / 100.0
}

/// The user's slider edit, if any. egui's `Slider` (clamping `Always` plus
/// `step_by`) rounds an off-grid value such as a hand-edited 0.62 to the
/// step as it draws and reports that as a change; the range input never
/// rewrote the value untouched. So a change counts as an edit when the
/// pointer or keyboard was on the slider this pass (`interacted` — even a
/// click on the 0.60 the handle already shows over a stored 0.62), or,
/// without such input (e.g. a screen reader's increment), when it moved to
/// a different grid position than the one shown.
fn threshold_edit(shown: f32, value: f32, changed: bool, interacted: bool) -> Option<f32> {
    let snapped = snap_threshold(value);
    (changed && (interacted || snapped != snap_threshold(shown))).then_some(snapped)
}

/// The value shown beside the slider.
fn threshold_readout(t: f32) -> String {
    format!("{t:.2}")
}

/// The slider's accessible value text (not shown visually).
fn threshold_value_text(t: f32) -> String {
    let word = if t < 0.55 {
        "lenient"
    } else if t < 0.7 {
        "balanced"
    } else {
        "strict"
    };
    format!("{t:.2} · {word}")
}

fn speech_learned(sample_seconds: f64) -> String {
    format!("{} min of speech learned", (sample_seconds / 60.0).round())
}

/// The Voices category's summary: who is enrolled and how strict matching is.
fn voices_summary(enrolled: Option<usize>, threshold: f32) -> String {
    let strictness = format!("strictness {}", threshold_readout(threshold));
    match enrolled {
        None => strictness,
        Some(0) => format!("No voices enrolled · {strictness}"),
        Some(1) => format!("1 voice enrolled · {strictness}"),
        Some(n) => format!("{n} voices enrolled · {strictness}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_counts_enrolled_voices() {
        assert_eq!(voices_summary(None, 0.6), "strictness 0.60");
        assert_eq!(
            voices_summary(Some(0), 0.65),
            "No voices enrolled · strictness 0.65"
        );
        assert_eq!(
            voices_summary(Some(1), 0.6),
            "1 voice enrolled · strictness 0.60"
        );
        assert_eq!(
            voices_summary(Some(3), 0.6),
            "3 voices enrolled · strictness 0.60"
        );
    }

    #[test]
    fn threshold_snaps_and_describes() {
        assert_eq!(speech_learned(150.0), "3 min of speech learned");
        assert_eq!(snap_threshold(0.650_000_1), 0.65);
        assert_eq!(snap_threshold(0.1), 0.4);
        assert_eq!(snap_threshold(0.93), 0.8);
        assert_eq!(snap_threshold(0.62), 0.6);
        assert_eq!(threshold_readout(0.6), "0.60");
        assert_eq!(threshold_value_text(0.5), "0.50 · lenient");
        assert_eq!(threshold_value_text(0.55), "0.55 · balanced");
        assert_eq!(threshold_value_text(0.7), "0.70 · strict");
    }

    #[test]
    fn slider_rounding_an_off_grid_value_is_not_an_edit() {
        // egui snapped a hand-edited 0.62 to 0.60 while drawing and
        // reported a change: not the user's doing, so nothing is saved.
        assert_eq!(threshold_edit(0.62, 0.6, true, false), None);
        assert_eq!(threshold_edit(0.6, 0.6, true, false), None);
        assert_eq!(threshold_edit(0.62, 0.65, false, false), None);
        assert_eq!(threshold_edit(0.62, 0.65, false, true), None);
        // Real edits, also away from an off-grid start.
        assert_eq!(threshold_edit(0.62, 0.650_000_1, true, true), Some(0.65));
        assert_eq!(threshold_edit(0.62, 0.55, true, true), Some(0.55));
        assert_eq!(threshold_edit(0.6, 0.8, true, true), Some(0.8));
        // A non-pointer change (screen-reader increment) to a new position.
        assert_eq!(threshold_edit(0.6, 0.65, true, false), Some(0.65));
    }

    #[test]
    fn clicking_the_shown_grid_value_over_an_off_grid_one_saves_it() {
        // The handle shows 0.60 over a stored 0.62; clicking it there is the
        // user picking 0.60.
        assert_eq!(threshold_edit(0.62, 0.6, true, true), Some(0.6));
    }

    /// The real row, untouched, over an off-grid stored value: egui's
    /// slider rewrites it to the grid and reports a change, which must not
    /// be saved.
    #[test]
    fn untouched_slider_over_an_off_grid_value_saves_nothing() {
        let mut local = None;
        let mut saved = None;
        super::super::kit::run_at(640.0, |ui| {
            saved = saved.or(threshold_row(ui, &mut local, 0.62));
        });
        assert_eq!((saved, local), (None, None));
    }
}
