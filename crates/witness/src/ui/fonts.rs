//! Fonts. Segoe UI (+ Semibold and Segoe UI Symbol) and the Phosphor icon
//! font load eagerly from the OS at startup; they are never bundled
//! (licensing). Script fallbacks for CJK, Hangul, Indic, Thai, Ethiopic and
//! Canadian syllabics/Cherokee load lazily on a worker thread the first time
//! such text shows up, so an autostart never reads ~50 MB of fonts it does
//! not need. Text can show tofu for one frame before its font arrives.
//!
//! `seguiemj.ttf` is deliberately not loaded: egui 0.36 cannot render its
//! colour glyphs.

use super::theme::SEMIBOLD;
use crossbeam_channel::{Receiver, Sender};
use eframe::egui::{
    self, epaint::text::FontPriority, epaint::text::InsertFontFamily, FontData, FontDefinitions,
    FontFamily,
};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const SEGOE: &str = "segoe-ui";
const SEGOE_SEMIBOLD: &str = "segoe-ui-semibold";
const SEGOE_SYMBOL: &str = "segoe-ui-symbol";

fn fonts_dir() -> PathBuf {
    let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
    PathBuf::from(windir).join("Fonts")
}

fn read_font(file: &str) -> Option<Vec<u8>> {
    match std::fs::read(fonts_dir().join(file)) {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            log::debug!("font {file} unavailable: {error}");
            None
        }
    }
}

/// Build and install the eager font set: Segoe UI first in Proportional,
/// Phosphor second (so icon glyphs resolve from it, as egui-phosphor
/// expects), Segoe UI Symbol next, egui's defaults last. "semibold" is
/// Segoe UI Semibold followed by the same fallbacks. Without Segoe the
/// defaults are used as-is.
pub fn install_eager(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let mut eager_loaded = Vec::new();

    if let Some(bytes) = read_font("segoeui.ttf") {
        fonts
            .font_data
            .insert(SEGOE.into(), Arc::new(FontData::from_owned(bytes)));
        proportional(&mut fonts).insert(0, SEGOE.into());
        eager_loaded.push("segoeui");
    }
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    if let Some(bytes) = read_font("seguisym.ttf") {
        fonts
            .font_data
            .insert(SEGOE_SYMBOL.into(), Arc::new(FontData::from_owned(bytes)));
        let list = proportional(&mut fonts);
        let at = list.len().min(2);
        list.insert(at, SEGOE_SYMBOL.into());
        eager_loaded.push("seguisym");
    }

    // Semibold: its own face first, then everything Proportional has.
    let mut semibold = proportional(&mut fonts).clone();
    if let Some(bytes) = read_font("seguisb.ttf") {
        fonts
            .font_data
            .insert(SEGOE_SEMIBOLD.into(), Arc::new(FontData::from_owned(bytes)));
        semibold.insert(0, SEGOE_SEMIBOLD.into());
        eager_loaded.push("seguisb");
    }
    fonts
        .families
        .insert(FontFamily::Name(SEMIBOLD.into()), semibold);

    log::info!("fonts: {}", eager_loaded.join(", "));
    ctx.set_fonts(fonts);
}

fn proportional(fonts: &mut FontDefinitions) -> &mut Vec<String> {
    fonts.families.entry(FontFamily::Proportional).or_default()
}

/// A lazily loaded script fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Script {
    Cjk,
    Hangul,
    Indic,
    Thai,
    Ethiopic,
    Syllabics,
}

impl Script {
    /// Font files to try, in order, with the face index inside a `.ttc`.
    fn candidates(self) -> &'static [(&'static str, u32)] {
        match self {
            Script::Cjk => &[("msyh.ttc", 0), ("YuGothR.ttc", 0)],
            Script::Hangul => &[("malgun.ttf", 0)],
            Script::Indic => &[("Nirmala.ttc", 0)],
            Script::Thai => &[("LeelawUI.ttf", 0)],
            Script::Ethiopic => &[("ebrima.ttf", 0)],
            Script::Syllabics => &[("gadugi.ttf", 0)],
        }
    }

    /// The script a code point needs a fallback for, if any. Hebrew and
    /// Arabic are covered by Segoe UI itself.
    fn of(c: char) -> Option<Script> {
        let cp = c as u32;
        Some(match cp {
            0x0900..=0x0DFF => Script::Indic, // Devanagari … Sinhala
            0x0E00..=0x0E7F => Script::Thai,
            0x1100..=0x11FF | 0x3130..=0x318F | 0xA960..=0xA97F | 0xAC00..=0xD7FF => {
                Script::Hangul
            }
            0x1200..=0x139F | 0x2D80..=0x2DDF | 0xAB00..=0xAB2F => Script::Ethiopic,
            0x13A0..=0x13FF | 0x1400..=0x167F | 0x18B0..=0x18FF | 0xAB70..=0xABBF => {
                Script::Syllabics // Cherokee + Unified Canadian Aboriginal Syllabics
            }
            0x2E80..=0x2FDF // CJK radicals
            | 0x3000..=0x30FF // CJK punctuation, Hiragana, Katakana
            | 0x3190..=0x31FF
            | 0x3200..=0x4DBF // enclosed CJK, CJK ext. A
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFFEF // half/full-width forms
            | 0x20000..=0x3134F => Script::Cjk,
            _ => return None,
        })
    }
}

/// Which scripts `text` needs (cheap: ASCII is skipped outright).
fn scripts_in(text: &str) -> impl Iterator<Item = Script> + '_ {
    text.chars()
        .filter(|c| !c.is_ascii())
        .filter_map(Script::of)
}

struct Loaded {
    script: Script,
    name: String,
    bytes: Vec<u8>,
    index: u32,
}

/// Lazy script-fallback loader. Views call [`FontLoader::note_texts`] when
/// data arrives (titles, segments, hits, live lines, names, notes);
/// `WitnessApp::logic()` calls [`FontLoader::install_pending`].
pub struct FontLoader {
    /// Scripts already requested (each family is loaded at most once).
    requested: Mutex<HashSet<Script>>,
    tx: Sender<Loaded>,
    rx: Receiver<Loaded>,
}

impl Default for FontLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl FontLoader {
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        FontLoader {
            requested: Mutex::new(HashSet::new()),
            tx,
            rx,
        }
    }

    /// Scan `texts` for scripts that need a fallback font and start loading
    /// each missing one on a worker thread. Cheap for already-seen scripts.
    pub fn note_texts<'a>(&self, ctx: &egui::Context, texts: impl IntoIterator<Item = &'a str>) {
        let mut wanted = HashSet::new();
        {
            let requested = self.requested.lock().unwrap();
            for text in texts {
                for script in scripts_in(text) {
                    if !requested.contains(&script) {
                        wanted.insert(script);
                    }
                }
            }
        }
        for script in wanted {
            self.load(ctx, script);
        }
    }

    fn load(&self, ctx: &egui::Context, script: Script) {
        if !self.requested.lock().unwrap().insert(script) {
            return;
        }
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("font-load".into())
            .spawn(move || {
                for &(file, index) in script.candidates() {
                    if let Some(bytes) = read_font(file) {
                        log::info!("loaded fallback font {file} for {script:?}");
                        let _ = tx.send(Loaded {
                            script,
                            name: format!("fallback-{file}"),
                            bytes,
                            index,
                        });
                        ctx.request_repaint();
                        return;
                    }
                }
                log::info!("no fallback font installed for {script:?}");
            });
        if let Err(error) = spawned {
            log::warn!("could not start the font loader: {error}");
        }
    }

    /// Add every font loaded since the last call, as the lowest-priority
    /// fallback of both Proportional and semibold text. Called in `logic()`.
    pub fn install_pending(&self, ctx: &egui::Context) {
        while let Ok(loaded) = self.rx.try_recv() {
            let mut data = FontData::from_owned(loaded.bytes);
            data.index = loaded.index;
            log::debug!("installing fallback font for {:?}", loaded.script);
            ctx.add_font(egui::epaint::text::FontInsert::new(
                &loaded.name,
                data,
                vec![
                    InsertFontFamily {
                        family: FontFamily::Proportional,
                        priority: FontPriority::Lowest,
                    },
                    InsertFontFamily {
                        family: FontFamily::Name(SEMIBOLD.into()),
                        priority: FontPriority::Lowest,
                    },
                ],
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scripts(text: &str) -> Vec<Script> {
        let mut found: Vec<Script> = scripts_in(text).collect();
        found.dedup();
        found
    }

    #[test]
    fn latin_hebrew_and_arabic_need_no_fallback() {
        assert!(scripts("Weekly sync — café ñ ü").is_empty());
        assert!(scripts("שלום עולם").is_empty());
        assert!(scripts("مرحبا").is_empty());
    }

    #[test]
    fn scripts_are_classified() {
        assert_eq!(scripts("会议"), vec![Script::Cjk]);
        assert_eq!(scripts("ミーティング"), vec![Script::Cjk]);
        assert_eq!(scripts("회의"), vec![Script::Hangul]);
        assert_eq!(scripts("बैठक"), vec![Script::Indic]);
        assert_eq!(scripts("ประชุม"), vec![Script::Thai]);
        assert_eq!(scripts("ስብሰባ"), vec![Script::Ethiopic]);
        assert_eq!(scripts("ᏣᎳᎩ"), vec![Script::Syllabics]);
        assert_eq!(scripts("ᐃᓄᒃᑎᑐᑦ"), vec![Script::Syllabics]);
    }

    #[test]
    fn each_script_is_requested_once() {
        let loader = FontLoader::new();
        let ctx = egui::Context::default();
        loader.note_texts(&ctx, ["회의", "또 회의"]);
        loader.note_texts(&ctx, ["회의"]);
        assert_eq!(loader.requested.lock().unwrap().len(), 1);
    }
}
