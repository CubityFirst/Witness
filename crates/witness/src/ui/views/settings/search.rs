//! Settings search: a static index of every setting (its category, title,
//! a short description and extra keywords such as "hotkey shortcut" or
//! "api key openai"), matched as the user types in the field at the top of
//! the rail / index.
//!
//! - **Matching**: every word typed must occur in the entry (title,
//!   keywords, category name or description); title hits rank above
//!   keyword hits above description hits, word starts above substrings.
//! - **Results** are grouped by category (the best group first), replacing
//!   the page (wide) or the category list (narrow). ↑/↓ move the selection
//!   while the field has focus, Enter opens it, Esc clears the field.
//! - **Opening** a result shows its category and scrolls the row (or the
//!   section) into view with a brief highlight that fades
//!   (`kit::show_anchor`).

use super::{row_anchor_id, section_anchor_id, show_anchor, SettingsPage, SettingsState};
use crate::ui::icons::{self, ph};
use crate::ui::theme::{self, pal};
use crate::ui::views::search::clear_button;
use crate::ui::widgets::{self, Politeness};
use crate::ui::UiCtx;
use eframe::egui::{self, Key, Margin, Modifiers, RichText, WidgetInfo, WidgetType};

/// The field's left padding, which holds the magnifier (as the shell's).
const ICON_PAD: i8 = 34;

/// Where an entry lives on its page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Anchor {
    /// A row, by the salt it is drawn with (`row` / `toggle`).
    Row(&'static str),
    /// A whole section, by its title.
    Section(&'static str),
}

impl Anchor {
    pub(super) fn id(self) -> egui::Id {
        match self {
            Anchor::Row(salt) => row_anchor_id(salt),
            Anchor::Section(title) => section_anchor_id(title),
        }
    }
}

/// One searchable setting.
#[derive(Debug)]
pub(super) struct Entry {
    pub page: SettingsPage,
    pub anchor: Anchor,
    pub title: &'static str,
    pub description: &'static str,
    /// Extra words that find it; never shown.
    pub keywords: &'static str,
}

const fn entry(
    page: SettingsPage,
    anchor: Anchor,
    title: &'static str,
    description: &'static str,
    keywords: &'static str,
) -> Entry {
    Entry {
        page,
        anchor,
        title,
        description,
        keywords,
    }
}

use Anchor::{Row, Section};
use SettingsPage as P;

/// Every setting, in page order.
pub(super) const INDEX: &[Entry] = &[
    // General
    entry(
        P::General,
        Row("appearance-theme"),
        "Theme",
        "System, light or dark.",
        "dark mode light mode appearance night day colours colors",
    ),
    entry(
        P::General,
        Row("appearance-accent"),
        "Accent colour",
        "Windows' accent colour or Witness blue.",
        "accent color colour highlight appearance blue",
    ),
    entry(
        P::General,
        Row("appearance-tray-icon"),
        "Tray icon",
        "Monochrome to match the taskbar, or the colour app icon.",
        "tray icon notification area system tray monochrome colour color taskbar",
    ),
    entry(
        P::General,
        Row("Start Witness with Windows"),
        "Start Witness with Windows",
        "Starts in the tray when you sign in.",
        "autostart startup boot login launch sign in tray",
    ),
    entry(
        P::General,
        Row("run-setup"),
        "Setup assistant",
        "Walk through devices, models and preferences again.",
        "onboarding wizard first run welcome setup again",
    ),
    // Recording
    entry(
        P::Recording,
        Row("Auto-record"),
        "Auto-record",
        "Records while a watched app is using the microphone.",
        "automatic teams detect detection meeting watcher call",
    ),
    entry(
        P::Recording,
        Row("watch-patterns"),
        "Watch patterns",
        "Apps whose microphone use counts as a meeting.",
        "teams zoom msteams detection apps process",
    ),
    entry(
        P::Recording,
        Row("mic-device"),
        "Microphone",
        "Recorded as you.",
        "mic input device headset audio me capture",
    ),
    entry(
        P::Recording,
        Row("loopback-device"),
        "Meeting audio",
        "Recorded as everyone else: the output Teams plays through.",
        "loopback speakers output device them chat goxlr headphones audio",
    ),
    entry(
        P::Recording,
        Row("refresh-devices"),
        "Refresh devices",
        "Re-scan after plugging a device in.",
        "rescan audio devices missing usb bluetooth",
    ),
    entry(
        P::Recording,
        Row("Recording hotkey"),
        "Recording hotkey",
        "Starts or stops recording from any app.",
        "shortcut keyboard global hotkey keys ctrl alt",
    ),
    entry(
        P::Recording,
        Row("Bookmark hotkey"),
        "Bookmark hotkey",
        "Marks the current moment of a recording.",
        "shortcut keyboard global hotkey keys ctrl alt mark",
    ),
    // Transcription
    entry(
        P::Transcription,
        Row("engine"),
        "Engine",
        "Parakeet or Whisper.",
        "parakeet whisper model asr speech recognition",
    ),
    entry(
        P::Transcription,
        Row("gpu"),
        "GPU",
        "Whether transcription runs on the graphics card.",
        "cuda nvidia graphics card acceleration cpu slow speed",
    ),
    entry(
        P::Transcription,
        Row("gpu-libs"),
        "GPU libraries",
        "Download or repair the CUDA runtime and cuDNN.",
        "cuda cudnn nvidia download repair dll libraries",
    ),
    entry(
        P::Transcription,
        Section("Engine"),
        "Speaker separation",
        "Up to 8 remote speakers (Nemotron 3 Diarization).",
        "diarization speakers nemotron who spoke",
    ),
    entry(
        P::Transcription,
        Row("Auto-transcribe"),
        "Auto-transcribe",
        "Transcribes each recording as soon as it stops.",
        "automatic transcript after recording",
    ),
    entry(
        P::Transcription,
        Row("Notify when transcribed"),
        "Notify when transcribed",
        "A Windows notification when a transcript is ready.",
        "toast notification alert popup done finished ready",
    ),
    entry(
        P::Transcription,
        Row("Live captions"),
        "Live captions",
        "Rough captions while recording; the finished transcript replaces them.",
        "subtitles realtime real-time cc live",
    ),
    entry(
        P::Transcription,
        Row("Open caption window automatically"),
        "Open caption window automatically",
        "Always on top, opened when recording starts.",
        "floating overlay captions always on top subtitles cc default",
    ),
    entry(
        P::Transcription,
        Row("junk-phrases"),
        "Discarded noise phrases",
        "Lines that say only one of these are dropped.",
        "filter hallucinations junk phantom lines thank you",
    ),
    // AI assistant
    entry(
        P::Ai,
        Row("Use an AI service"),
        "Use an AI service",
        "Off by default; sends transcript text, never audio.",
        "ai assistant openai llm gpt chatgpt enable summaries privacy",
    ),
    entry(
        P::Ai,
        Row("ai-base-url"),
        "Base URL",
        "Any OpenAI-compatible API.",
        "endpoint server url openai ollama localhost lm studio",
    ),
    entry(
        P::Ai,
        Row("ai-api-key"),
        "API key",
        "Kept in Windows Credential Manager.",
        "api key token secret openai credential password remove",
    ),
    entry(
        P::Ai,
        Row("ai-model"),
        "Model",
        "The model id as the service names it.",
        "gpt llama model name llm",
    ),
    entry(
        P::Ai,
        Row("ai-models"),
        "Available models",
        "Asks the service which models it offers.",
        "list load models",
    ),
    entry(
        P::Ai,
        Row("ai-test"),
        "Test connection",
        "Sends one short test message.",
        "check verify ping connection",
    ),
    entry(
        P::Ai,
        Row("Title new meetings automatically"),
        "Title new meetings automatically",
        "After each transcription.",
        "auto title name rename meetings ai",
    ),
    entry(
        P::Ai,
        Row("Write meeting notes automatically"),
        "Write meeting notes automatically",
        "A summary, key points, decisions and action items.",
        "summary notes minutes action items auto ai",
    ),
    // Voices
    entry(
        P::Voices,
        Row("threshold"),
        "Voice match strictness",
        "How alike two voices must sound to label a speaker.",
        "threshold speaker identification voice id sensitivity auto-label",
    ),
    entry(
        P::Voices,
        Section("Voice prints"),
        "Voice prints",
        "The people Witness recognizes by voice.",
        "people speakers enrolled forget delete voice identification",
    ),
    // Storage
    entry(
        P::Storage,
        Row("data-dir"),
        "Data directory",
        "Where the database, recordings and models live.",
        "folder location path move disk drive data",
    ),
    entry(
        P::Storage,
        Row("backup"),
        "Backup",
        "A verified copy of the database, recordings and settings.",
        "export copy archive snapshot save restore",
    ),
    entry(
        P::Storage,
        Section("Models"),
        "Models",
        "Download or repair the transcription models.",
        "download parakeet whisper diarization nemotron repair",
    ),
    entry(
        P::Storage,
        Row("model-cache"),
        "Delete model cache",
        "Free the disk space the models use.",
        "remove models disk space free clean cache",
    ),
    // About
    entry(
        P::About,
        Row("update-version"),
        "Version and updates",
        "Check for and install new versions.",
        "update upgrade version release new install about",
    ),
    entry(
        P::About,
        Row("Check for updates at startup"),
        "Check for updates at startup",
        "Looks for a new version shortly after launch.",
        "automatic update check",
    ),
    entry(
        P::About,
        Section("Privacy"),
        "Privacy",
        "What stays on this computer and what uses the network.",
        "data local offline network security encryption",
    ),
    entry(
        P::About,
        Row("privacy-data"),
        "Where your data lives",
        "The folder that holds everything.",
        "folder path location data",
    ),
    entry(
        P::About,
        Row("diagnostics-actions"),
        "Diagnostic report",
        "Copy or export a privacy-safe report.",
        "diagnostics debug log report troubleshooting support bug",
    ),
];

/// One category's results: indices into [`INDEX`], best first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Group {
    pub page: SettingsPage,
    pub hits: Vec<usize>,
}

/// Lower-case words of `text` (letters and digits).
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
}

/// How well `token` (lower-case) matches `text`: a word start, else a
/// substring.
fn hit(text: &str, token: &str, start: i32, inside: i32) -> Option<i32> {
    if words(text).any(|w| w.starts_with(token)) {
        Some(start)
    } else if text.to_lowercase().contains(token) {
        Some(inside)
    } else {
        None
    }
}

/// `entry`'s score for the query `tokens`; None unless every token occurs.
fn score(entry: &Entry, query: &str, tokens: &[String]) -> Option<i32> {
    let mut total = 0;
    for token in tokens {
        total += hit(entry.title, token, 30, 15)
            .or_else(|| hit(entry.keywords, token, 12, 6))
            .or_else(|| hit(entry.page.title(), token, 8, 4))
            .or_else(|| hit(entry.description, token, 5, 3))?;
    }
    if entry.title.to_lowercase().starts_with(query) {
        total += 20;
    }
    Some(total)
}

/// The settings matching `query`, grouped by category.
pub(super) fn search(query: &str) -> Vec<Group> {
    let query = query.trim().to_lowercase();
    let tokens: Vec<String> = query.split_whitespace().map(str::to_owned).collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(usize, i32)> = INDEX
        .iter()
        .enumerate()
        .filter_map(|(i, e)| score(e, &query, &tokens).map(|s| (i, s)))
        .collect();
    // Best first; ties keep page order.
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut groups: Vec<Group> = Vec::new();
    for (i, _) in scored {
        let page = INDEX[i].page;
        match groups.iter_mut().find(|g| g.page == page) {
            Some(group) => group.hits.push(i),
            None => groups.push(Group {
                page,
                hits: vec![i],
            }),
        }
    }
    groups
}

/// The results in display order.
pub(super) fn flatten(groups: &[Group]) -> Vec<usize> {
    groups.iter().flat_map(|g| g.hits.iter().copied()).collect()
}

/// Move a selection among `count` results, wrapping.
pub(super) fn step(selected: usize, count: usize, delta: i32) -> usize {
    if count == 0 {
        return 0;
    }
    (selected as i64 + delta as i64).rem_euclid(count as i64) as usize
}

fn field_id() -> egui::Id {
    egui::Id::new("settings-search")
}

impl SettingsState {
    /// A query is being shown.
    pub(super) fn searching(&self) -> bool {
        !self.search_text.trim().is_empty()
    }

    /// The search field, full width. ↑/↓ and Enter act on the results
    /// while it has focus; Escape clears it.
    pub(super) fn search_field(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let id = field_id();
        let focused = ui.memory(|m| m.has_focus(id));
        let groups = search(&self.search_text);
        let flat = flatten(&groups);
        // Taken before the field sees them (a single-line field would move
        // its cursor on ↑/↓).
        let (up, down, enter) = if focused {
            ui.input_mut(|i| {
                (
                    i.consume_key(Modifiers::NONE, Key::ArrowUp),
                    i.consume_key(Modifiers::NONE, Key::ArrowDown),
                    i.consume_key(Modifiers::NONE, Key::Enter),
                )
            })
        } else {
            (false, false, false)
        };
        let has_text = !self.search_text.is_empty();
        let margin = Margin {
            left: ICON_PAD,
            right: if has_text { ICON_PAD } else { 12 },
            top: 7,
            bottom: 7,
        };
        let hint = if ui.available_width() < 200.0 {
            "Find a setting"
        } else {
            "Find a setting (Ctrl+F)"
        };
        let edit = egui::TextEdit::singleline(&mut self.search_text)
            .hint_text(RichText::new(hint).color(pal().text_faint))
            .font(theme::regular(14.0))
            .return_key(None::<egui::KeyboardShortcut>)
            .desired_width(f32::INFINITY);
        let edit = widgets::framed_text_edit(ui, id, edit, pal().bg_raised, theme::R_MD, margin);
        let response = ui.add(edit);
        icons::paint_rotated_glyph(
            ui.painter(),
            egui::pos2(
                response.rect.left() + f32::from(ICON_PAD) / 2.0 + 1.0,
                response.rect.center().y,
            ),
            ph::MAGNIFYING_GLASS,
            15.0,
            if focused {
                pal().text_muted
            } else {
                pal().text_faint
            },
            0.0,
        );
        widgets::text_edit_menu(&response);
        ui.ctx().accesskit_node_builder(id, |node| {
            node.set_label("Search settings");
        });
        if std::mem::take(&mut self.search_focus) {
            response.request_focus();
        }
        let mut changed = response.changed();
        let escape = (focused || response.has_focus() || response.lost_focus())
            && ui.input(|i| i.key_pressed(Key::Escape));
        if escape && has_text {
            self.search_text.clear();
            changed = true;
            response.request_focus();
        }
        if has_text && clear_button(ui, response.rect, id.with("clear")) {
            self.search_text.clear();
            changed = true;
            response.request_focus();
        }
        if changed {
            self.search_selected = 0;
        }
        self.search_selected = self.search_selected.min(flat.len().saturating_sub(1));
        if up {
            self.search_selected = step(self.search_selected, flat.len(), -1);
        }
        if down {
            self.search_selected = step(self.search_selected, flat.len(), 1);
        }
        if enter && !changed {
            if let Some(&index) = flat.get(self.search_selected) {
                self.open_hit(ui.ctx(), cx, index, false);
            }
        }
        if self.searching() {
            let count = match flat.len() {
                0 => "No settings found".to_string(),
                1 => "1 setting found".to_string(),
                n => format!("{n} settings found"),
            };
            widgets::announce(
                ui.ctx(),
                egui::Id::new("settings-search-count"),
                &count,
                Politeness::Polite,
            );
        }
    }

    /// The grouped results (or why there are none).
    pub(super) fn search_results(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let groups = search(&self.search_text);
        if groups.is_empty() {
            ui.add_space(theme::S3);
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "No settings match \u{201c}{}\u{201d}.",
                        self.search_text.trim()
                    ))
                    .color(pal().text),
                )
                .wrap(),
            );
            ui.add(
                egui::Label::new(
                    RichText::new("Try another word, for example hotkey, GPU or backup.")
                        .font(theme::regular(12.5))
                        .color(pal().text_muted),
                )
                .wrap(),
            );
            return;
        }
        let mut position = 0;
        let mut picked = None;
        for group in &groups {
            widgets::section_header(ui, group.page.title());
            ui.spacing_mut().item_spacing.y = 2.0;
            for &index in &group.hits {
                let selected = position == self.search_selected;
                if result_row(ui, &INDEX[index], selected).clicked() {
                    picked = Some(index);
                }
                position += 1;
            }
        }
        if let Some(index) = picked {
            self.open_hit(ui.ctx(), cx, index, false);
        }
    }

    /// Open the category of `INDEX[index]` and point at the setting.
    pub(super) fn open_hit(
        &mut self,
        ctx: &egui::Context,
        cx: &mut UiCtx,
        index: usize,
        frozen: bool,
    ) {
        let entry = &INDEX[index];
        self.search_text.clear();
        self.search_selected = 0;
        // Keys go to the page again (Space scrolls it).
        ctx.memory_mut(|m| m.surrender_focus(field_id()));
        self.open_page(cx, entry.page);
        // The page's first pass draws it (and scrolls to it).
        show_anchor(ctx, entry.anchor.id(), frozen);
        widgets::announce(
            ctx,
            egui::Id::new("settings-search-count"),
            &format!("{} in {}", entry.title, entry.page.title()),
            Politeness::Polite,
        );
    }
}

/// A result: the setting's title over its description.
fn result_row(ui: &mut egui::Ui, entry: &Entry, selected: bool) -> egui::Response {
    let (row, ()) = widgets::list_row(ui, ("settings-hit", entry.title), selected, |ui, _| {
        ui.spacing_mut().item_spacing.y = 1.0;
        widgets::elide(ui, entry.title, theme::regular(14.0), pal().text);
        widgets::elide(
            ui,
            entry.description,
            theme::regular(12.5),
            pal().text_muted,
        );
    });
    if selected {
        row.scroll_to_me(None);
    }
    let name = format!("{}, {}", entry.title, entry.page.title());
    row.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(query: &str) -> Vec<&'static str> {
        flatten(&search(query))
            .into_iter()
            .map(|i| INDEX[i].title)
            .collect()
    }

    fn pages(query: &str) -> Vec<SettingsPage> {
        search(query).into_iter().map(|g| g.page).collect()
    }

    #[test]
    fn titles_keywords_and_descriptions_all_find_settings() {
        assert_eq!(titles("theme")[0], "Theme");
        // Keywords.
        assert_eq!(
            titles("shortcut"),
            vec!["Recording hotkey", "Bookmark hotkey"]
        );
        assert_eq!(titles("cuda"), vec!["GPU", "GPU libraries"]);
        assert_eq!(titles("cudnn"), vec!["GPU libraries"]);
        assert_eq!(titles("openai key")[0], "API key");
        assert_eq!(titles("dark mode")[0], "Theme");
        // A description word.
        assert!(titles("tray").contains(&"Start Witness with Windows"));
        // Case and surrounding space do not matter.
        assert_eq!(titles("  MICRO "), titles("micro"));
        assert_eq!(titles("micro")[0], "Microphone");
    }

    #[test]
    fn every_word_must_match() {
        assert_eq!(titles("live captions")[0], "Live captions");
        assert!(titles("captions xyzzy").is_empty());
        assert!(search("").is_empty());
        assert!(search("   ").is_empty());
        assert!(search("qqzz").is_empty());
    }

    #[test]
    fn results_group_by_category_best_first() {
        // "model" is the AI assistant's Model row by title, Storage's
        // Models section by title, and Transcription's engine by keyword.
        let groups = search("model");
        let first = &groups[0];
        assert!(matches!(first.page, P::Ai | P::Storage), "{groups:?}");
        assert!(pages("model").contains(&P::Transcription));
        // No category appears twice; within one, the best hit leads.
        let mut seen = pages("model");
        seen.dedup();
        assert_eq!(seen.len(), groups.len());
        let ai = groups.iter().find(|g| g.page == P::Ai).unwrap();
        assert_eq!(INDEX[ai.hits[0]].title, "Model");
    }

    #[test]
    fn a_category_name_finds_its_settings() {
        assert!(pages("voices").contains(&P::Voices));
        assert!(titles("storage").contains(&"Backup"));
    }

    #[test]
    fn the_index_is_consistent() {
        // Every category has entries; anchors are unique per page.
        for page in SettingsPage::ALL {
            assert!(INDEX.iter().any(|e| e.page == page), "{page:?}");
        }
        // Entries are listed in page order (the tie-break relies on it).
        for pair in INDEX.windows(2) {
            let order = |p| SettingsPage::ALL.iter().position(|q| *q == p);
            assert!(
                order(pair[0].page) <= order(pair[1].page),
                "{}",
                pair[1].title
            );
        }
        for (i, a) in INDEX.iter().enumerate() {
            for b in &INDEX[i + 1..] {
                assert!(a.title != b.title, "duplicate result title {}", a.title);
            }
        }
    }

    #[test]
    fn selection_wraps() {
        assert_eq!(step(0, 4, -1), 3);
        assert_eq!(step(3, 4, 1), 0);
        assert_eq!(step(1, 4, 1), 2);
        assert_eq!(step(0, 0, 1), 0);
    }
}
