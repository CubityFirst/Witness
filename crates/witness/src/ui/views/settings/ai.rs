//! Settings → AI assistant: the opt-in connection to an OpenAI-compatible
//! service that titles meetings and writes their notes (`service/ai.rs`).
//!
//! - The master switch "Use an AI service" is off by default, and its
//!   description says exactly what is sent and where. While it is off the
//!   core refuses every AI request (the test and model-list buttons are
//!   disabled here as well).
//! - Base URL and model are ordinary settings (`[ai]` in the settings
//!   file), saved on blur / Enter through the `SettingsStore` like the other
//!   text fields; the base URL is validated as typed (https, or http only
//!   for localhost / 127.0.0.1 / [::1]).
//! - The API key never enters the settings: the masked field is written to
//!   Windows Credential Manager (an op, so its toast survives leaving
//!   Settings) on Enter / blur and then cleared, egui's undo history
//!   included. Settings only learns whether a key is stored, never reads
//!   it back. An unsaved key typed into the field is never persisted on
//!   leave.
//! - "Load models" and "Test connection" are view-owned tasks that use the
//!   typed values (and a typed key before it is saved).

use super::{
    button, fill, footnote, note, notice_done, row, save, section, status_note, title, toggle,
    OpRun, Slot, FIELD_WIDTH,
};
use crate::bridge::Task;
use crate::ui::icons::ph;
use crate::ui::theme::pal;
use crate::ui::toasts::Notice;
use crate::ui::widgets;
use crate::ui::UiCtx;
use eframe::egui;
use witness_core::ai_client::{wipe, ApiKey, BaseUrl};
use witness_core::settings::AiSettings;
use witness_core::CoreResult;

const MASTER_HELP: &str = "Off by default. While on, Witness sends a meeting\u{2019}s transcript text, speaker names and title \u{2014} never audio \u{2014} to the service below, and nowhere else. A server on this computer (localhost) keeps everything local.";

fn base_id() -> egui::Id {
    egui::Id::new("ai-base-url")
}

fn key_id() -> egui::Id {
    egui::Id::new("ai-api-key")
}

fn model_id() -> egui::Id {
    egui::Id::new("ai-model")
}

/// A text field's draft over a stored setting (re-synced when the stored
/// value moves away while the field is clean).
#[derive(Debug, Default)]
struct Draft {
    text: String,
    source: Option<String>,
    dirty: bool,
}

impl Draft {
    fn sync(&mut self, stored: &str) {
        if !self.dirty && self.source.as_deref() != Some(stored) {
            self.text = stored.to_string();
            self.source = Some(stored.to_string());
        }
    }

    /// The value to save, once, if the field was typed into.
    fn take_commit(&mut self) -> Option<String> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        self.source = None;
        Some(self.text.trim().to_string())
    }
}

/// What the section's widgets did this pass.
#[derive(Default)]
struct Out {
    enabled: bool,
    base: Option<egui::Response>,
    key: Option<egui::Response>,
    toggle_key: bool,
    remove_key: bool,
    model: Option<egui::Response>,
    picked: Option<String>,
    load_models: bool,
    test: bool,
    auto_title: bool,
    auto_notes: bool,
}

#[derive(Default)]
pub struct AiSection {
    mounted: bool,
    base: Draft,
    model: Draft,
    /// The key being typed (never loaded from the store).
    key_text: String,
    show_key: bool,
    /// The host of the key in Credential Manager: None until known,
    /// Some(None) = no key.
    key_host: Option<Option<String>>,
    key_load: Option<Task<CoreResult<Option<String>>>>,
    key_write: Option<OpRun<Option<String>>>,
    models: Vec<String>,
    models_task: Option<Task<CoreResult<Vec<String>>>>,
    models_error: Option<String>,
    test_task: Option<Task<CoreResult<String>>>,
    test_result: Option<Result<String, String>>,
    /// Debug builds: `WITNESS_DEBUG_AI_SETTINGS=on|error|other`.
    #[cfg(debug_assertions)]
    debug: Option<String>,
}

impl AiSection {
    pub fn new() -> Self {
        #[allow(unused_mut)]
        let mut section = AiSection::default();
        #[cfg(debug_assertions)]
        {
            section.debug = std::env::var("WITNESS_DEBUG_AI_SETTINGS")
                .ok()
                .filter(|v| !v.is_empty());
        }
        section
    }

    /// Save a base URL / model draft that has not been blurred yet (the
    /// view is being left). A typed API key is deliberately not saved.
    pub fn commit_drafts(&mut self, cx: &mut UiCtx) {
        if let Some(base) = self.base.take_commit() {
            save(cx, |s| s.ai.base_url = base);
        }
        if let Some(model) = self.model.take_commit() {
            save(cx, |s| s.ai.model = model);
        }
    }

    fn mount(&mut self, cx: &UiCtx) {
        self.mounted = true;
        #[cfg(debug_assertions)]
        if self.debug.is_some() {
            self.key_host = Some(Some("api.openai.com".into()));
            return;
        }
        self.key_load = Some(cx.tasks.spawn("ai-key-status", |c| c.ai_key_host()));
    }

    fn poll(&mut self) {
        if let Some(result) = self.key_load.as_mut().and_then(Task::poll_flat) {
            self.key_load = None;
            match result {
                Ok(host) => self.key_host = Some(host),
                Err(error) => log::warn!("could not check the stored AI key: {error}"),
            }
        }
        // A failed write keeps the old state (the op toasted the error).
        if let Some(Some(stored)) = OpRun::finished(&mut self.key_write) {
            self.key_host = Some(stored);
        }
        if let Some(result) = self.models_task.as_mut().and_then(Task::poll_flat) {
            self.models_task = None;
            match result {
                Ok(models) if models.is_empty() => {
                    self.models_error = Some("The service listed no models.".into());
                }
                Ok(models) => {
                    self.models = models;
                    self.models_error = None;
                }
                Err(error) => self.models_error = Some(error),
            }
        }
        if let Some(result) = self.test_task.as_mut().and_then(Task::poll_flat) {
            self.test_task = None;
            self.test_result = Some(result);
        }
    }

    /// The settings to render: the store's, or a debug sample.
    fn shown_settings(&mut self, cx: &UiCtx) -> AiSettings {
        let stored = cx.settings.current().ai.clone();
        #[cfg(debug_assertions)]
        if let Some(mode) = self.debug.clone() {
            let error = mode == "error";
            if self.models.is_empty() && !error {
                self.models = vec!["example-large".into(), "example-small".into()];
            }
            if self.test_result.is_none() {
                self.test_result = Some(if error {
                    Err("The AI service rejected the API key (401). Check the key in Settings \u{2192} AI assistant.".into())
                } else {
                    Ok("Connected: example-small answered.".into())
                });
            }
            return AiSettings {
                enabled: true,
                base_url: match mode.as_str() {
                    "error" => "http://ai.example.com/v1".into(),
                    // A base on another host than the saved key.
                    "other" => "https://ai.example.com/v1".into(),
                    _ => "https://api.openai.com/v1".into(),
                },
                model: "example-small".into(),
                auto_title: !error,
                auto_notes: true,
            };
        }
        stored
    }

    /// "Off" / "On · api.openai.com · gpt-4o".
    pub fn summary(&self, cx: &UiCtx) -> String {
        summary(&cx.settings.current().ai)
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        if !self.mounted {
            self.mount(cx);
        }
        self.poll();
        let ai = self.shown_settings(cx);
        self.base.sync(&ai.base_url);
        self.model.sync(&ai.model);

        let base_check = BaseUrl::parse(&self.base.text);
        let has_model = !self.model.text.trim().is_empty();
        let on = ai.enabled;
        let configured = on && base_check.is_ok() && has_model;
        let key_host = self.key_host.clone();
        let key_busy = self.key_write.is_some();
        let loading = self.models_task.is_some();
        let testing = self.test_task.is_some();
        let models = self.models.clone();
        let models_error = self.models_error.clone();
        let test_result = self.test_result.clone();
        let show_key = self.show_key;
        let current_model = self.model.text.trim().to_string();
        let (base_text, model_text, key_text) = (
            &mut self.base.text,
            &mut self.model.text,
            &mut self.key_text,
        );

        let mut out = Out {
            enabled: on,
            auto_title: ai.auto_title,
            auto_notes: ai.auto_notes,
            ..Out::default()
        };
        let mut enabled = on;
        let mut auto_title = ai.auto_title;
        let mut auto_notes = ai.auto_notes;
        section(ui, "Connection", |ui| {
            out.enabled =
                toggle(ui, true, &mut enabled, "Use an AI service", MASTER_HELP).changed();
            widgets::group_divider(ui);

            out.base = Some(row(
                ui,
                "ai-base-url",
                Slot::Fill(FIELD_WIDTH),
                |ui| {
                    let id = title(ui, "Base URL");
                    note(
                        ui,
                        "Any OpenAI-compatible API, e.g. https://api.openai.com/v1, or http://localhost:11434/v1 for a local server.",
                    );
                    match &base_check {
                        Err(error) => {
                            status_note(ui, pal().rec_light, error);
                        }
                        Ok(base) if base.is_local() => {
                            status_note(
                                ui,
                                pal().ok_light,
                                "On this computer: nothing leaves your PC.",
                            );
                        }
                        Ok(base) => {
                            let when = if on { "" } else { "When on, " };
                            let goes = if on {
                                "Meeting text goes"
                            } else {
                                "meeting text goes"
                            };
                            status_note(
                                ui,
                                pal().text_muted,
                                &format!("{when}{goes} to {}.", base.host()),
                            );
                        }
                    }
                    id
                },
                |ui, label| {
                    let field = widgets::text_input(
                        ui,
                        base_id(),
                        egui::TextEdit::singleline(base_text).desired_width(f32::INFINITY),
                    );
                    let response = ui.add(field).labelled_by(label);
                    widgets::text_edit_menu(&response);
                    response
                },
            ));
            widgets::group_divider(ui);

            out.key = Some(row(
                ui,
                "ai-api-key",
                Slot::Fill(FIELD_WIDTH),
                |ui| {
                    let id = title(ui, "API key");
                    note(
                        ui,
                        "Kept in Windows Credential Manager, never in Witness\u{2019}s files. Local servers usually need none.",
                    );
                    match &key_host {
                        Some(Some(host)) => {
                            status_note(ui, pal().ok_light, &format!("A key is saved for {host}."));
                            if let Some(warning) = key_mismatch(host, base_check.as_ref().ok()) {
                                status_note(ui, pal().rec_light, &warning);
                            }
                        }
                        Some(None) => {
                            status_note(ui, pal().text_faint, "No key saved.");
                        }
                        None => {}
                    }
                    id
                },
                |ui, label| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let width = (ui.available_width() - 32.0).max(80.0);
                    let hint = if matches!(key_host, Some(Some(_))) {
                        "Saved \u{2014} type to replace"
                    } else {
                        "Paste the key, then Enter"
                    };
                    let field = widgets::text_input(
                        ui,
                        key_id(),
                        egui::TextEdit::singleline(key_text)
                            .password(!show_key)
                            .hint_text(hint)
                            .desired_width(width),
                    );
                    let response = ui.add_enabled(!key_busy, field).labelled_by(label);
                    let (glyph, tip) = if show_key {
                        (ph::EYE_SLASH, "Hide the key")
                    } else {
                        (ph::EYE, "Show the key")
                    };
                    out.toggle_key = widgets::icon_btn(ui, glyph, 16.0, tip).clicked();
                    response
                },
            ));
            widgets::group_divider(ui);

            out.model = Some(row(
                ui,
                "ai-model",
                Slot::Fill(FIELD_WIDTH),
                |ui| {
                    let id = title(ui, "Model");
                    note(ui, "Required. The model id as the service names it.");
                    id
                },
                |ui, label| {
                    let field = widgets::text_input(
                        ui,
                        model_id(),
                        egui::TextEdit::singleline(model_text)
                            .hint_text("Model id")
                            .desired_width(f32::INFINITY),
                    );
                    let response = ui.add(field).labelled_by(label);
                    widgets::text_edit_menu(&response);
                    response
                },
            ));
            widgets::group_divider(ui);

            let slot = if models.is_empty() {
                Slot::Fit
            } else {
                Slot::Fill(FIELD_WIDTH)
            };
            row(
                ui,
                "ai-models",
                slot,
                |ui| {
                    let id = title(ui, "Available models");
                    note(ui, "Asks the service which models it offers.");
                    if let Some(error) = &models_error {
                        status_note(ui, pal().rec_light, error);
                    }
                    id
                },
                |ui, label| {
                    if models.is_empty() {
                        let text = if loading {
                            "Loading\u{2026}"
                        } else {
                            "Load models"
                        };
                        out.load_models = button(ui, on && !loading, text).clicked();
                    } else {
                        out.picked = model_picker(ui, label, &models, &current_model);
                    }
                },
            );
            widgets::group_divider(ui);

            row(
                ui,
                "ai-test",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "Test connection");
                    note(ui, "Sends one short test message with the settings above.");
                    match &test_result {
                        Some(Ok(message)) => {
                            status_note(ui, pal().ok_light, message);
                        }
                        Some(Err(error)) => {
                            status_note(ui, pal().rec_light, error);
                        }
                        None => {}
                    }
                    id
                },
                |ui, _| {
                    let text = if testing { "Testing\u{2026}" } else { "Test" };
                    out.test = button(ui, on && !testing && has_model, text).clicked();
                },
            );
            // The destructive action sits apart, at the end of the group.
            if let Some(Some(host)) = &key_host {
                widgets::group_divider(ui);
                out.remove_key = row(
                    ui,
                    "ai-key-remove",
                    Slot::Fit,
                    |ui| {
                        let id = title(ui, "Saved API key");
                        note(ui, &format!("For {host}, in Windows Credential Manager."));
                        id
                    },
                    |ui, _| {
                        ui.add_enabled_ui(!key_busy, |ui| widgets::btn_danger(ui, "Remove"))
                            .inner
                            .clicked()
                    },
                );
            }
        });

        section(ui, "Automatic", |ui| {
            out.auto_title = toggle(
                ui,
                configured,
                &mut auto_title,
                "Title new meetings automatically",
                "After each transcription. Only replaces a title Witness or Teams chose \u{2014} never one you typed.",
            )
            .changed();
            widgets::group_divider(ui);
            out.auto_notes = toggle(
                ui,
                configured,
                &mut auto_notes,
                "Write meeting notes automatically",
                "A summary, key points, decisions and action items after each transcription.",
            )
            .changed();
        });
        footnote(
            ui,
            if on {
                "AI text can be wrong \u{2014} check summaries before relying on them. The service\u{2019}s own data policy applies to what it receives."
            } else {
                "Nothing is sent anywhere while \u{201c}Use an AI service\u{201d} is off \u{2014} not even a connection test."
            },
        );

        self.apply(ui.ctx(), cx, out, enabled, auto_title, auto_notes);
    }

    fn apply(
        &mut self,
        ctx: &egui::Context,
        cx: &mut UiCtx,
        out: Out,
        enabled: bool,
        auto_title: bool,
        auto_notes: bool,
    ) {
        #[cfg(debug_assertions)]
        if self.debug.is_some() {
            return; // a sample, never saved
        }
        // A key typed but not saved yet still counts for test / list.
        let typed_key = ApiKey::new(&self.key_text);
        if out.enabled {
            save(cx, |s| s.ai.enabled = enabled);
            self.test_result = None;
        }
        if let Some(base) = &out.base {
            if base.changed() {
                self.base.dirty = true;
                self.test_result = None;
                self.models_error = None;
            }
            if base.lost_focus() {
                if let Some(value) = self.base.take_commit() {
                    let value = value.trim_end_matches('/').to_string();
                    save(cx, |s| s.ai.base_url = value);
                }
            }
        }
        if let Some(model) = &out.model {
            if model.changed() {
                self.model.dirty = true;
                self.test_result = None;
            }
            if model.lost_focus() {
                if let Some(value) = self.model.take_commit() {
                    save(cx, |s| s.ai.model = value);
                }
            }
        }
        if let Some(picked) = out.picked {
            self.model.text = picked.clone();
            self.model.dirty = false;
            self.model.source = None;
            self.test_result = None;
            save(cx, |s| s.ai.model = picked);
        }
        if out.toggle_key {
            self.show_key = !self.show_key;
        }
        if let Some(key) = &out.key {
            if key.changed() {
                self.test_result = None;
            }
            if key.lost_focus() && typed_key.is_some() {
                self.save_key(ctx, cx);
            }
        }
        if out.remove_key {
            self.key_write = Some(OpRun::spawn(cx, "ai-key-remove", |c, slot| {
                match c.set_ai_key("", "") {
                    Ok(()) => {
                        fill(slot, None);
                        notice_done(Notice::success("API key removed"))
                    }
                    Err(error) => notice_done(Notice::error("Could not remove the API key", error)),
                }
            }));
        }
        if out.load_models && self.models_task.is_none() {
            let base = self.base.text.trim().to_string();
            let key = typed_key.clone();
            self.models_error = None;
            self.models_task = Some(cx.tasks.spawn("ai-models", move |c| {
                c.ai_list_models(&base, key.as_ref().map(ApiKey::expose))
            }));
        }
        if out.test && self.test_task.is_none() {
            let base = self.base.text.trim().to_string();
            let model = self.model.text.trim().to_string();
            let key = typed_key;
            self.test_result = None;
            self.test_task = Some(cx.tasks.spawn("ai-test", move |c| {
                c.ai_test_connection(&base, &model, key.as_ref().map(ApiKey::expose))
            }));
        }
        if out.auto_title {
            save(cx, |s| s.ai.auto_title = auto_title);
        }
        if out.auto_notes {
            save(cx, |s| s.ai.auto_notes = auto_notes);
        }
    }

    /// Hand the typed key to Credential Manager and forget it here (the
    /// field, and egui's undo history of it).
    fn save_key(&mut self, ctx: &egui::Context, cx: &mut UiCtx) {
        let Some(key) = ApiKey::new(&self.key_text) else {
            return;
        };
        // The key is bound to the host of the base URL shown right now.
        let base = self.base.text.trim().to_string();
        let host = BaseUrl::parse(&base)
            .ok()
            .map(|b| b.host().to_ascii_lowercase());
        wipe(&mut self.key_text);
        self.show_key = false;
        if let Some(mut state) = egui::text_edit::TextEditState::load(ctx, key_id()) {
            state.clear_undoer();
            state.cursor.set_char_range(None);
            state.store(ctx, key_id());
        }
        self.key_write = Some(OpRun::spawn(cx, "ai-key-save", move |c, slot| {
            match c.set_ai_key(key.expose(), &base) {
                Ok(()) => {
                    fill(slot, host);
                    notice_done(Notice::success(
                        "API key saved in Windows Credential Manager",
                    ))
                }
                Err(error) => notice_done(Notice::error("Could not save the API key", error)),
            }
        }));
    }
}

impl Drop for AiSection {
    /// A key typed but never saved is overwritten when Settings closes.
    fn drop(&mut self) {
        wipe(&mut self.key_text);
    }
}

/// The AI assistant category's summary.
fn summary(ai: &AiSettings) -> String {
    if !ai.enabled {
        return "Off — nothing is sent anywhere".into();
    }
    let host = match BaseUrl::parse(&ai.base_url) {
        Ok(base) if base.is_local() => "local server".to_string(),
        Ok(base) => base.host().to_string(),
        Err(_) => return "On · the base URL needs fixing".into(),
    };
    match ai.model.trim() {
        "" => format!("On · {host} · choose a model"),
        model => format!("On · {host} · {model}"),
    }
}

/// The warning when the saved key belongs to another host than the base
/// URL shown (it will not be sent there).
fn key_mismatch(key_host: &str, base: Option<&BaseUrl>) -> Option<String> {
    let base = base?;
    let host = base.host().to_ascii_lowercase();
    if host == key_host {
        return None;
    }
    Some(if base.is_local() {
        format!("It is not sent to {host} (a local server gets no key).")
    } else {
        format!("It is not sent to {host} \u{2014} enter the key for {host}.")
    })
}

/// The loaded model list as a select; returns a newly picked id.
fn model_picker(
    ui: &mut egui::Ui,
    label: egui::Id,
    models: &[String],
    current: &str,
) -> Option<String> {
    let selected = if models.iter().any(|m| m == current) {
        current.to_string()
    } else {
        format!("Choose from {} models\u{2026}", models.len())
    };
    let width = ui.available_width();
    let mut picked = None;
    widgets::select(ui, widgets::SelectKind::TextInput, |ui| {
        egui::ComboBox::from_id_salt("ai-model-picker")
            .width(width)
            .selected_text(selected)
            .truncate()
            .height(320.0)
            .show_ui(ui, |ui| {
                for model in models {
                    let is_current = model == current;
                    if ui.selectable_label(is_current, model.as_str()).clicked() && !is_current {
                        picked = Some(model.clone());
                    }
                }
            })
            .response
            .labelled_by(label);
    });
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_for_another_host_is_flagged() {
        let openai = BaseUrl::parse("https://API.openai.com/v1").unwrap();
        assert_eq!(key_mismatch("api.openai.com", Some(&openai)), None);
        let other = BaseUrl::parse("https://ai.example.com/v1").unwrap();
        assert!(key_mismatch("api.openai.com", Some(&other))
            .unwrap()
            .contains("enter the key for ai.example.com"));
        let local = BaseUrl::parse("http://localhost:11434/v1").unwrap();
        assert!(key_mismatch("api.openai.com", Some(&local))
            .unwrap()
            .contains("local server gets no key"));
        assert_eq!(key_mismatch("api.openai.com", None), None);
    }

    #[test]
    fn summary_names_the_service_and_model() {
        let mut ai = AiSettings {
            enabled: false,
            base_url: "https://api.openai.com/v1".into(),
            model: "example-small".into(),
            auto_title: true,
            auto_notes: true,
        };
        assert_eq!(summary(&ai), "Off — nothing is sent anywhere");
        ai.enabled = true;
        assert_eq!(summary(&ai), "On · api.openai.com · example-small");
        ai.base_url = "http://localhost:11434/v1".into();
        ai.model = " ".into();
        assert_eq!(summary(&ai), "On · local server · choose a model");
        ai.base_url = "http://ai.example.com/v1".into();
        assert_eq!(summary(&ai), "On · the base URL needs fixing");
    }

    #[test]
    fn drafts_resync_only_while_clean_and_commit_once() {
        let mut draft = Draft::default();
        draft.sync("https://a/v1");
        assert_eq!(draft.text, "https://a/v1");
        draft.text = " https://b/v1 ".into();
        draft.dirty = true;
        draft.sync("https://other/v1");
        assert_eq!(draft.text, " https://b/v1 ", "typing is never clobbered");
        assert_eq!(draft.take_commit().as_deref(), Some("https://b/v1"));
        assert_eq!(draft.take_commit(), None, "once");
        draft.sync("https://b/v1");
        assert_eq!(draft.text, "https://b/v1");
    }
}
