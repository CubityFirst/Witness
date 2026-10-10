//! First-run setup: welcome, capture devices (with a live level check), the
//! transcription models, a couple of preferences, done. Shown instead of the
//! views until finished or skipped (`UiPrefs::onboarded`).
//!
//! The shell asks [`Onboarding::is_active`] each pass and, while it is,
//! draws [`Onboarding::show`] in the content area below the top bar.
//!
//! - **When it runs**: only while `prefs.onboarded` is unset *and* the
//!   library is empty (no meetings, none in the recycle bin) — someone
//!   upgrading with recordings has clearly set Witness up already, so for
//!   them the flag is set silently. The library is checked off the UI
//!   thread on the first pass; until the answer arrives the content area
//!   stays empty for that moment rather than flashing the wizard (or a
//!   view that is about to be replaced).
//! - **Nothing is lost by leaving early**: every choice is saved the moment
//!   it is made (devices and auto-record through `SettingsStore`, theme
//!   through `cx.prefs`, autostart through its own op) and a model download
//!   keeps going in the background, so Skip setup and Esc only end the
//!   wizard. Finishing or skipping sets `prefs.onboarded`; so does going
//!   to a view from the top bar (or a shortcut) while the wizard shows.
//! - **Layout**: one calm card (≤ [`CARD_MAX`] wide) centred in the content
//!   area under step dots, scrolling when the window is short; it reuses the
//!   Settings rows, so controls look and behave the same in both places.

use super::icons::{self, ph};
use super::theme::{self, pal};
use super::views::settings::{
    self, accent_row, device_row, download_active, download_fraction, download_progress_text,
    hotkey_label, keycap, note, start_model_download, status_note, theme_row, toggle, DeviceRow,
    OpRun, SystemLook,
};
use super::widgets::{self, Politeness};
use super::UiCtx;
use crate::bridge::Task;
use crate::platform::autostart;
use crate::ui::toasts::Notice;
use crate::ui_state::UiPrefs;
use eframe::egui::{self, Align, CornerRadius, Layout, Margin, RichText, Stroke};
use witness_core::audio_capture::TrackKind;
use witness_core::models::ModelInfo;
use witness_core::service::{AudioDevices, LevelCheck};
use witness_core::settings::{Engine, Settings};
use witness_core::CoreResult;

/// The card's maximum width.
const CARD_MAX: f32 = 520.0;
/// The Parakeet download's size, as the model banner states it.
const PARAKEET_DOWNLOAD: &str = "about 3.5 GB";

#[derive(Default)]
pub struct Onboarding {
    state: State,
}

#[derive(Default)]
enum State {
    /// Finished, skipped, or not needed this session.
    #[default]
    Off,
    /// `prefs.onboarded` is unset: checking whether the library is empty
    /// (the task starts on the first pass, which has the `UiCtx`).
    Deciding(Option<Task<CoreResult<bool>>>),
    Running(Box<Wizard>),
}

impl Onboarding {
    /// Decide whether setup may run this session: not once it was finished
    /// or skipped. Whether it does is settled by the library check in
    /// [`Onboarding::show`]. Debug builds honour
    /// `WITNESS_DEBUG_ONBOARDING=<step 1-5>`, which opens the wizard at that
    /// step whatever the prefs and the library say (snapshots).
    pub fn new(prefs: &UiPrefs) -> Onboarding {
        let state = match debug_step() {
            Some(step) => State::Running(Box::new(Wizard::new(step))),
            None if prefs.onboarded => State::Off,
            None => State::Deciding(None),
        };
        Onboarding { state }
    }

    /// The wizard from its first step, whatever the library holds
    /// (Settings → Run setup again).
    pub fn run_now() -> Onboarding {
        Onboarding {
            state: State::Running(Box::new(Wizard::new(Step::Welcome))),
        }
    }

    /// True while the wizard shows — and while the library check is still
    /// out, so no view flashes before it is replaced.
    pub fn is_active(&self) -> bool {
        !matches!(self.state, State::Off)
    }

    /// Draw the current step in the content area.
    pub fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        match &mut self.state {
            State::Off => {}
            State::Deciding(task) => {
                let task = task.get_or_insert_with(|| {
                    cx.tasks.spawn("onboarding-library-check", |c| {
                        Ok(c.list_meetings(0, 1)?.is_empty()
                            && c.list_deleted_meetings()?.is_empty())
                    })
                });
                let Some(result) = task.poll_flat() else {
                    return;
                };
                match decide(&result) {
                    Decision::Run => {
                        self.state = State::Running(Box::new(Wizard::new(Step::Welcome)));
                        self.show(ui, cx);
                    }
                    Decision::Skip { mark_onboarded } => {
                        if let Err(error) = &result {
                            log::warn!("first-run check failed; setup not shown: {error}");
                        }
                        if mark_onboarded {
                            cx.prefs.onboarded = true;
                        }
                        self.state = State::Off;
                        // The views draw from the next pass.
                        ui.ctx().request_repaint();
                    }
                }
            }
            State::Running(wizard) => {
                // The top bar (Meetings, People, Settings, the recording
                // pill) or a shortcut asked for a view this pass: the user
                // has left setup, as with Skip setup — otherwise those
                // controls would seem dead while the wizard covers the view.
                let navigated = cx.nav.pending.is_some();
                if navigated || wizard.show(ui, cx) {
                    cx.prefs.onboarded = true;
                    self.state = State::Off;
                    ui.ctx().request_repaint();
                }
            }
        }
    }
}

/// What the library check means for this session.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Run,
    /// `mark_onboarded`: the library has meetings, so this is no first run;
    /// remember that. A failed check only skips setup this time.
    Skip {
        mark_onboarded: bool,
    },
}

/// `library_empty` is the check's result: an empty library runs setup.
fn decide(library_empty: &CoreResult<bool>) -> Decision {
    match library_empty {
        Ok(true) => Decision::Run,
        Ok(false) => Decision::Skip {
            mark_onboarded: true,
        },
        Err(_) => Decision::Skip {
            mark_onboarded: false,
        },
    }
}

/// `WITNESS_DEBUG_ONBOARDING=<1-5>` (debug builds only).
fn debug_step() -> Option<Step> {
    #[cfg(debug_assertions)]
    if let Some(spec) = std::env::var_os("WITNESS_DEBUG_ONBOARDING") {
        let step = spec
            .to_str()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .and_then(Step::number);
        if step.is_none() {
            log::warn!("WITNESS_DEBUG_ONBOARDING={spec:?} not understood; ignored");
        }
        return step;
    }
    None
}

/// `WITNESS_DEBUG_ONBOARDING_FAKE=1` (debug builds only): the Audio step
/// shows level meters at fixed levels (no device is opened) and the Models
/// step a download in progress, for snapshots of those states.
fn debug_fake() -> bool {
    #[cfg(debug_assertions)]
    if std::env::var_os("WITNESS_DEBUG_ONBOARDING_FAKE").is_some_and(|v| v == "1") {
        return true;
    }
    false
}

// ----------------------------------------------------------------------
// Steps

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Welcome,
    Audio,
    Models,
    Preferences,
    Done,
}

impl Step {
    const ALL: [Step; 5] = [
        Step::Welcome,
        Step::Audio,
        Step::Models,
        Step::Preferences,
        Step::Done,
    ];

    fn index(self) -> usize {
        Step::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    /// The step numbered `n` (1-based), as the debug hook names them.
    #[cfg_attr(not(any(debug_assertions, test)), allow(dead_code))]
    fn number(n: usize) -> Option<Step> {
        n.checked_sub(1).and_then(|i| Step::ALL.get(i).copied())
    }

    fn next(self) -> Option<Step> {
        Step::ALL.get(self.index() + 1).copied()
    }

    fn prev(self) -> Option<Step> {
        self.index().checked_sub(1).map(|i| Step::ALL[i])
    }
}

/// What the footer (or a key) asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nav {
    Back,
    Next,
    /// Skip setup / Esc: end the wizard where it is.
    Skip,
}

/// Where a [`Nav`] leads: another step, or the end of setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Stay,
    Go(Step),
    Finish,
}

fn flow(step: Step, nav: Nav) -> Flow {
    match nav {
        Nav::Skip => Flow::Finish,
        Nav::Next => step.next().map_or(Flow::Finish, Flow::Go),
        Nav::Back => step.prev().map_or(Flow::Stay, Flow::Go),
    }
}

/// Esc skips setup only when it is not already doing something else:
/// closing an open dropdown, leaving a text field, or answering a modal.
fn esc_skips(popup_open: bool, typing: bool, modal_open: bool) -> bool {
    !popup_open && !typing && !modal_open
}

// ----------------------------------------------------------------------
// The wizard

struct Wizard {
    step: Step,
    /// The step whose loads were started (they restart on every change).
    mounted: Option<Step>,

    devices: Option<Task<CoreResult<AudioDevices>>>,
    device_list: Option<AudioDevices>,
    devices_error: Option<String>,
    levels: Option<LevelTest>,

    models: Option<Task<Vec<ModelInfo>>>,
    model_list: Option<Vec<ModelInfo>>,
    seen_download_tick: Option<u64>,

    autostart_read: Option<Task<Result<bool, String>>>,
    /// None until read (the switch is disabled meanwhile).
    autostart: Option<bool>,
    autostart_write: Option<OpRun<bool>>,
    look_task: Option<Task<SystemLook>>,
    look: Option<SystemLook>,
    /// Debug snapshots: fake levels and download progress ([`debug_fake`]).
    fake: bool,
}

impl Wizard {
    fn new(step: Step) -> Wizard {
        Wizard {
            step,
            mounted: None,
            devices: None,
            device_list: None,
            devices_error: None,
            levels: None,
            models: None,
            model_list: None,
            seen_download_tick: None,
            autostart_read: None,
            autostart: None,
            autostart_write: None,
            look_task: None,
            look: None,
            fake: debug_fake(),
        }
    }

    /// One pass. True once setup is finished or skipped.
    fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) -> bool {
        // Keys first: a dropdown that Esc closes this pass is still open
        // here, so Esc only skips when there is nothing else to dismiss.
        let mut nav = self.keys(ui.ctx(), cx);
        if self.mounted != Some(self.step) {
            self.mount(cx);
        }
        self.poll(cx);

        let measured = egui::Id::new("onboarding-content-height");
        let last_height: Option<f32> = ui.data(|d| d.get_temp(measured));
        let out = egui::ScrollArea::vertical()
            .id_salt(("onboarding", self.step.index()))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let viewport = ui.clip_rect().height();
                // Centred while it fits (a little above centre reads as
                // centred), from the top with breathing room otherwise.
                let top = last_height.map_or(theme::S5, |h| ((viewport - h) * 0.42).max(theme::S4));
                ui.add_space(top);
                let start = ui.cursor().top();
                widgets::padded_column(ui, |ui| {
                    widgets::centered_column(ui, CARD_MAX, |ui| {
                        step_dots(ui, self.step);
                        widgets::gap(ui, theme::S4);
                        if let Some(choice) = card(ui, |ui| self.step_body(ui, cx)) {
                            nav = Some(choice);
                        }
                    })
                });
                let height = ui.cursor().top() - start;
                ui.add_space(theme::S5);
                height
            });
        let height = out.inner;
        if last_height.is_none_or(|h| (h - height).abs() > 0.5) {
            ui.data_mut(|d| d.insert_temp(measured, height));
            // Re-run the pass centred on the new height (no visible jump).
            ui.ctx().request_discard("onboarding card measured");
            ui.ctx().request_repaint();
        }

        let Some(nav) = nav else {
            return false;
        };
        match flow(self.step, nav) {
            Flow::Stay => false,
            Flow::Go(step) => {
                self.step = step;
                // A level check never outlives its step.
                self.levels = None;
                ui.ctx().request_repaint();
                false
            }
            Flow::Finish => true,
        }
    }

    /// Enter = Next while nothing has focus (a focused button takes Enter
    /// itself); Esc = Skip setup when it has nothing else to close.
    fn keys(&self, ctx: &egui::Context, cx: &UiCtx) -> Option<Nav> {
        let popup_open = egui::Popup::is_any_open(ctx);
        let typing = ctx.egui_wants_keyboard_input();
        let nothing_focused = ctx.memory(|m| m.focused().is_none());
        let modal = cx.confirm.is_open();
        ctx.input_mut(|i| {
            if esc_skips(popup_open, typing, modal)
                && i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
            {
                Some(Nav::Skip)
            } else if nothing_focused
                && !popup_open
                && !modal
                && i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
            {
                Some(Nav::Next)
            } else {
                None
            }
        })
    }

    /// Start the loads the current step shows.
    fn mount(&mut self, cx: &mut UiCtx) {
        self.mounted = Some(self.step);
        match self.step {
            Step::Audio => self.refresh_devices(cx),
            Step::Models => {
                self.seen_download_tick = Some(cx.shell.download_done_tick);
                self.models = Some(cx.tasks.spawn("model-status", |c| c.model_status()));
            }
            Step::Preferences => {
                if self.autostart.is_none() && self.autostart_read.is_none() {
                    // A registry read; off the UI thread like Settings'.
                    self.autostart_read = Some(
                        cx.tasks
                            .spawn("autostart-read", |_| autostart::is_enabled()),
                    );
                }
                if self.look.is_none() && self.look_task.is_none() {
                    self.look_task = Some(SystemLook::load(cx));
                }
            }
            Step::Welcome | Step::Done => {}
        }
    }

    fn refresh_devices(&mut self, cx: &UiCtx) {
        self.devices_error = None;
        self.devices = Some(cx.tasks.spawn("list-devices", |c| c.list_audio_devices()));
    }

    fn poll(&mut self, cx: &mut UiCtx) {
        if let Some(result) = take(&mut self.devices) {
            match result.and_then(|inner| inner) {
                Ok(devices) => self.device_list = Some(devices),
                Err(error) => self.devices_error = Some(error),
            }
        }
        if let Some(result) = take(&mut self.models) {
            match result {
                Ok(models) => self.model_list = Some(models),
                Err(error) => cx.toasts.error("Could not load model status", error),
            }
        }
        // A finished download (success or failure) changes the rows.
        if self
            .seen_download_tick
            .is_some_and(|seen| seen != cx.shell.download_done_tick)
        {
            self.seen_download_tick = Some(cx.shell.download_done_tick);
            self.models = Some(cx.tasks.spawn("model-status", |c| c.model_status()));
        }
        if let Some(result) = take(&mut self.autostart_read) {
            match result.and_then(|inner| inner) {
                Ok(enabled) => self.autostart = Some(enabled),
                Err(error) => cx.toasts.error("Could not read autostart status", error),
            }
        }
        // A failed write was toasted by the op; the switch stays as it was.
        if let Some(Some(enabled)) = OpRun::finished(&mut self.autostart_write) {
            self.autostart = Some(enabled);
        }
        if let Some(result) = take(&mut self.look_task) {
            self.look = Some(result.unwrap_or_default());
        }
        if let Some(levels) = &mut self.levels {
            levels.poll(cx);
        }
    }

    /// The current step's content and footer; returns the footer's choice.
    fn step_body(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) -> Option<Nav> {
        let forward = match self.step {
            Step::Welcome => self.welcome(ui),
            Step::Audio => self.audio(ui, cx),
            Step::Models => self.models_step(ui, cx),
            Step::Preferences => self.preferences(ui, cx),
            Step::Done => self.done(ui, cx),
        };
        widgets::gap(ui, theme::S5);
        footer(ui, self.step, forward)
    }

    // ------------------------------------------------------------------
    // Step bodies. Each returns its footer's forward button.

    fn welcome(&mut self, ui: &mut egui::Ui) -> Forward {
        heading(
            ui,
            ph::WAVEFORM,
            "Welcome to Witness",
            "Witness records your meetings and turns them into searchable transcripts that know who said what.",
        );
        widgets::gap(ui, theme::S5);
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = theme::S4;
            feature(
                ui,
                ph::MICROPHONE,
                "Records the call",
                "Your microphone and the meeting audio as separate tracks — automatically when a Teams call starts, or with one click.",
            );
            feature(
                ui,
                ph::TEXT_ALIGN_LEFT,
                "Transcribes on this PC",
                "With speaker labels and live captions, and it learns the voices you name.",
            );
            feature(
                ui,
                ph::LOCK_SIMPLE,
                "Stays on this PC",
                "Audio, transcripts and voice prints never leave your computer. The network is only used to download models and check for updates.",
            );
        });
        Forward::primary("Get started")
    }

    fn audio(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) -> Forward {
        heading(
            ui,
            ph::MICROPHONE,
            "Audio sources",
            "Witness records two tracks: your microphone, and the meeting audio your PC plays.",
        );
        widgets::gap(ui, theme::S4);
        let current = cx.settings.current().clone();
        let devices = self.device_list.as_ref();
        let failed = self.devices_error.is_some();
        if self.fake && self.levels.is_none() {
            self.levels = Some(LevelTest::fake(cx));
        }
        let levels = self.levels.as_ref();
        let (mic, loopback) = ui
            .scope(|ui| {
                ui.spacing_mut().item_spacing.y = theme::S3;
                let mic = device_row(
                    ui,
                    &DeviceRow {
                        label: "Microphone",
                        id: "onboarding-mic-device",
                        default_label: "Default input device",
                        missing_badge: "configured microphone is not connected",
                        help: None,
                        configured: current.mic_device.as_deref(),
                        devices: devices.map(|d| d.capture.as_slice()),
                        list_failed: failed,
                    },
                );
                if let Some(levels) = levels {
                    level_meter(ui, &levels.mic, "Say something.");
                }
                widgets::gap(ui, theme::S4);
                let loopback = device_row(
                    ui,
                    &DeviceRow {
                        label: "Meeting audio",
                        id: "onboarding-loopback-device",
                        default_label: "Default output device",
                        missing_badge: "configured output is not connected",
                        help: Some("The output Teams plays the call through."),
                        configured: current.loopback_device.as_deref(),
                        devices: devices.map(|d| d.render.as_slice()),
                        list_failed: failed,
                    },
                );
                if let Some(levels) = levels {
                    level_meter(ui, &levels.loopback, "Play something through this output.");
                }
                (mic, loopback)
            })
            .inner;
        if let Some(error) = &self.devices_error {
            widgets::inline_warning(ui, &format!("Audio devices could not be listed: {error}"));
        }
        if let Some(value) = mic {
            save(cx, |s| s.mic_device = value);
        }
        if let Some(value) = loopback {
            save(cx, |s| s.loopback_device = value);
        }

        widgets::gap(ui, theme::S4);
        let running = self.levels.as_ref().is_some_and(|l| !l.finished());
        let (test, refresh) = ui
            .horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(theme::S2, theme::S2);
                let label = match &self.levels {
                    Some(_) if running => "Stop test",
                    Some(_) => "Test again",
                    None => "Test levels",
                };
                let test = widgets::btn(ui, label).clicked();
                let refresh = widgets::btn_ghost(ui, "Refresh devices").clicked();
                (test, refresh)
            })
            .inner;
        if test {
            self.levels = (!running).then(|| LevelTest::start(cx));
        }
        if refresh {
            self.refresh_devices(cx);
        }

        widgets::gap(ui, theme::S4);
        tip(
            ui,
            ph::LIGHTBULB,
            "In Teams, set Settings → Devices → Speaker to a dedicated output — say your headset's \"Chat\" channel — and pick the same output here. Recordings then hold just the meeting, not music or game audio. A headset also keeps the call from echoing into your microphone.",
        );
        Forward::primary("Next")
    }

    fn models_step(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) -> Forward {
        let engine = cx.settings.current().engine;
        let size = match engine {
            Engine::Parakeet => format!(" of {PARAKEET_DOWNLOAD}"),
            Engine::Whisper => String::new(),
        };
        heading(
            ui,
            ph::DOWNLOAD_SIMPLE,
            "Transcription models",
            &format!(
                "Transcription runs on this PC, so Witness needs its speech models: a one-time download{size} from Hugging Face. Recording works without them; only transcription waits."
            ),
        );
        widgets::gap(ui, theme::S4);

        let mut dl = cx.shell.download.clone();
        let models: Vec<&ModelInfo> = self
            .model_list
            .iter()
            .flatten()
            .filter(|m| m.engine == engine)
            .collect();
        let loaded = self.model_list.is_some();
        let mut installed = loaded && !models.is_empty() && models.iter().all(|m| m.present);
        if self.fake {
            installed = false;
            dl = models.first().map(|m| fake_download(&m.id));
        }
        let active = download_active(dl.as_ref());

        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = theme::S3;
            if !loaded {
                widgets::skeleton_rows(ui, 2);
            }
            for model in &models {
                let state = if model.present && !self.fake {
                    ModelLine::Installed
                } else if dl
                    .as_ref()
                    .is_some_and(|d| active && d.model_id == model.id)
                {
                    ModelLine::Downloading
                } else {
                    ModelLine::Missing
                };
                model_line(ui, model, state);
            }
        });
        widgets::gap(ui, theme::S4);

        let failed = dl.as_ref().and_then(|d| d.error.clone());
        let mut download = false;
        if installed {
            status_line(
                ui,
                ph::CHECK_CIRCLE,
                pal().ok_light,
                "All set: the models are installed.",
            );
        } else if let Some(progress) = dl.as_ref().filter(|_| active) {
            let name = models
                .iter()
                .find(|m| m.id == progress.model_id)
                .map_or("models", |m| m.display_name.as_str());
            let text = format!("Downloading {name} — {}", download_progress_text(progress));
            status_note(ui, pal().text, &text);
            settings::progress_bar(ui, download_fraction(progress).unwrap_or(0.0));
            widgets::announce(
                ui.ctx(),
                egui::Id::new("onboarding-download-live"),
                &text,
                Politeness::Polite,
            );
            note(
                ui,
                "You can carry on: the download continues in the background.",
            );
        } else if loaded {
            if let Some(error) = &failed {
                status_note(ui, pal().rec_light, &format!("Download failed: {error}"));
            }
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(theme::S3, theme::S2);
                let label = if failed.is_some() {
                    "Retry download"
                } else {
                    "Download now"
                };
                download = widgets::btn_primary(ui, label).clicked();
                ui.label(
                    RichText::new("or later, in Settings → Storage")
                        .font(theme::regular(12.5))
                        .color(pal().text_muted),
                );
            });
        }
        if download {
            // A retry clears the failed state first, as Settings does.
            cx.shell.download = None;
            start_model_download(cx, engine, "Could not download models".into());
        }

        if loaded && !installed && !active {
            Forward::later()
        } else {
            Forward::primary("Next")
        }
    }

    fn preferences(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) -> Forward {
        heading(
            ui,
            ph::SLIDERS_HORIZONTAL,
            "Preferences",
            "A few choices to start with. They are all in Settings later.",
        );
        widgets::gap(ui, theme::S4);
        let mut auto_record = cx.settings.current().auto_record;
        let mut start = self.autostart.unwrap_or(false);
        let start_ready = self.autostart.is_some() && self.autostart_write.is_none();
        let look = self.look;
        let (auto, autostart) = ui
            .scope(|ui| {
                ui.spacing_mut().item_spacing.y = theme::S4;
                let auto = toggle(
                    ui,
                    true,
                    &mut auto_record,
                    "Auto-record meetings",
                    "Start recording when a Teams call is detected; stop when it ends.",
                )
                .changed();
                let autostart = toggle(
                    ui,
                    start_ready,
                    &mut start,
                    "Start with Windows",
                    "Opens minimized to the tray, ready for the first call of the day.",
                )
                .changed();
                theme_row(ui, cx.prefs);
                accent_row(ui, cx.prefs, look.as_ref());
                (auto, autostart)
            })
            .inner;
        if auto {
            save(cx, |s| s.auto_record = auto_record);
        }
        if autostart {
            // The switch shows the new value once the registry write
            // succeeds; a failure toasts.
            self.autostart_write =
                Some(OpRun::spawn(
                    cx,
                    "autostart-write",
                    move |_, slot| match autostart::set_enabled(start) {
                        Ok(()) => {
                            settings::fill(slot, start);
                            settings::quiet()
                        }
                        Err(error) => settings::notice_done(Notice::error(
                            "Could not update autostart",
                            error,
                        )),
                    },
                ));
        }
        Forward::primary("Next")
    }

    fn done(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) -> Forward {
        heading(
            ui,
            ph::CHECK_CIRCLE,
            "You're all set",
            "Witness is watching for meetings. Closing the window keeps it running in the tray.",
        );
        widgets::gap(ui, theme::S5);
        let record = cx.shell.record_hotkey.as_deref().map(hotkey_label);
        let tips = [
            (
                Some("CTRL + K".to_string()),
                "Command palette: jump to any meeting or person, or run an action.",
            ),
            match record {
                Some(combo) => (Some(combo), "Start or stop recording from any app."),
                None => (
                    None,
                    "Recording hotkey unavailable: other apps hold every candidate combo. The record button and the tray still work.",
                ),
            },
            (Some("CTRL + F".to_string()), "Find in the open transcript."),
        ];
        // One key column, as wide as the widest cap, so the descriptions
        // line up.
        let keys = tips
            .iter()
            .filter_map(|(combo, _)| combo.as_deref())
            .map(|combo| keycap_width(ui, combo))
            .fold(0.0, f32::max);
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = theme::S3;
            for (combo, what) in &tips {
                shortcut(ui, keys, combo.as_deref(), what);
            }
        });
        widgets::gap(ui, theme::S4);
        tip(
            ui,
            ph::LIGHTBULB,
            "Rename a speaker in any transcript and Witness learns their voice: future meetings label them automatically.",
        );
        Forward::primary("Start using Witness")
    }
}

/// `poll` a load slot; clears it once the result is in.
fn take<T>(slot: &mut Option<Task<T>>) -> Option<Result<T, String>> {
    let result = slot.as_mut()?.poll()?;
    *slot = None;
    Some(result)
}

/// Save `edit` applied to the store's current (optimistic) settings.
fn save(cx: &mut UiCtx, edit: impl FnOnce(&mut Settings)) {
    let mut next = cx.settings.current().clone();
    edit(&mut next);
    cx.settings.save(next, cx.tasks);
}

// ----------------------------------------------------------------------
// Level test

/// The Audio step's level check: one capture per track on the configured
/// devices, restarted when a picker changes, dropped with the step (or
/// after `level_check::MAX_DURATION`, by the core).
struct LevelTest {
    mic: Probe,
    loopback: Probe,
}

/// One track's check: the device it runs on and the check (or its start).
struct Probe {
    kind: TrackKind,
    device: Option<String>,
    starting: Option<Task<LevelCheck>>,
    check: Option<LevelCheck>,
    error: Option<String>,
    /// Debug snapshots: a fixed level, no device opened.
    fake: Option<f32>,
}

impl Probe {
    fn start(cx: &UiCtx, kind: TrackKind, device: Option<String>) -> Probe {
        let wanted = device.clone();
        Probe {
            kind,
            device,
            // A capture start spawns threads: never on the UI thread.
            starting: Some(
                cx.tasks
                    .spawn("level-check", move |c| c.start_level_check(kind, wanted)),
            ),
            check: None,
            error: None,
            fake: None,
        }
    }

    fn fake(kind: TrackKind, device: Option<String>, level: f32) -> Probe {
        Probe {
            kind,
            device,
            starting: None,
            check: None,
            error: None,
            fake: Some(level),
        }
    }

    fn poll(&mut self) {
        if let Some(result) = take(&mut self.starting) {
            match result {
                Ok(check) => self.check = Some(check),
                Err(error) => self.error = Some(error),
            }
        }
    }

    /// The meter fraction (0 while starting).
    fn level(&self) -> f32 {
        if let Some(level) = self.fake {
            return level;
        }
        self.check.as_ref().map_or(0.0, LevelCheck::level)
    }

    fn error(&self) -> Option<String> {
        self.error
            .clone()
            .or_else(|| self.check.as_ref().and_then(LevelCheck::error))
    }

    fn finished(&self) -> bool {
        self.error.is_some() || self.check.as_ref().is_some_and(LevelCheck::finished)
    }
}

impl LevelTest {
    fn start(cx: &UiCtx) -> LevelTest {
        let settings = cx.settings.current();
        LevelTest {
            mic: Probe::start(cx, TrackKind::Mic, settings.mic_device.clone()),
            loopback: Probe::start(cx, TrackKind::Loopback, settings.loopback_device.clone()),
        }
    }

    /// Debug snapshots: a talking microphone and a silent output.
    fn fake(cx: &UiCtx) -> LevelTest {
        let settings = cx.settings.current();
        LevelTest {
            mic: Probe::fake(TrackKind::Mic, settings.mic_device.clone(), 0.62),
            loopback: Probe::fake(TrackKind::Loopback, settings.loopback_device.clone(), 0.0),
        }
    }

    fn poll(&mut self, cx: &UiCtx) {
        // A new pick restarts that track's check on the new device.
        let settings = cx.settings.current();
        for (probe, configured) in [
            (&mut self.mic, &settings.mic_device),
            (&mut self.loopback, &settings.loopback_device),
        ] {
            if probe.fake.is_none() && probe.device != *configured {
                *probe = Probe::start(cx, probe.kind, configured.clone());
            }
            probe.poll();
        }
    }

    fn finished(&self) -> bool {
        self.mic.finished() && self.loopback.finished()
    }
}

/// Debug snapshots: a download a third of the way through its first file.
fn fake_download(model_id: &str) -> witness_core::events::ModelDownloadProgress {
    witness_core::events::ModelDownloadProgress {
        model_id: model_id.to_owned(),
        file: "encoder-model.onnx".into(),
        downloaded_bytes: 820_000_000,
        total_bytes: Some(2_440_000_000),
        file_index: 1,
        file_count: 4,
        done: false,
        error: None,
    }
}

// ----------------------------------------------------------------------
// Pieces

/// The footer's forward button.
struct Forward {
    text: &'static str,
    primary: bool,
}

impl Forward {
    fn primary(text: &'static str) -> Self {
        Forward {
            text,
            primary: true,
        }
    }

    /// "Later": moving on without doing the step's main action, which
    /// then holds the accent instead.
    fn later() -> Self {
        Forward {
            text: "Later",
            primary: false,
        }
    }
}

/// Five dots above the card: done steps tinted, the current one a wider
/// accent pill. Exposed as "Step n of 5".
fn step_dots(ui: &mut egui::Ui, step: Step) {
    const DOT: f32 = 6.0;
    const CURRENT: f32 = 20.0;
    const GAP: f32 = 6.0;
    let count = Step::ALL.len();
    let gaps = (count - 1) as f32;
    let width = CURRENT + gaps * DOT + gaps * GAP;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), DOT), egui::Sense::hover());
    let label = format!("Step {} of {count}", step.index() + 1);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label));
    let p = pal();
    let mut x = rect.center().x - width / 2.0;
    for i in 0..count {
        let current = i == step.index();
        let w = if current { CURRENT } else { DOT };
        let color = if current {
            p.accent
        } else if i < step.index() {
            theme::alpha(p.accent, 0.45)
        } else {
            p.border_strong
        };
        let dot = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(w, DOT));
        ui.painter()
            .rect_filled(dot, CornerRadius::same(255), color);
        x += w + GAP;
    }
}

/// The raised card around a step: hairline border, a soft shadow, roomy
/// padding (less on narrow windows).
fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let p = pal();
    let pad = if widgets::is_compact(ui) { 20 } else { 28 };
    egui::Frame::new()
        .fill(p.bg_raised)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(theme::R_LG))
        .shadow(egui::Shadow {
            offset: [0, 8],
            blur: 28,
            spread: 0,
            color: egui::Color32::from_black_alpha(if p.dark { 90 } else { 16 }),
        })
        .inner_margin(Margin::symmetric(pad, 24))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.x = theme::S2;
            add(ui)
        })
        .inner
}

/// A step's icon tile, title and lead paragraph.
fn heading(ui: &mut egui::Ui, glyph: &str, title: &str, lead: &str) {
    let p = pal();
    let (tile, _) = ui.allocate_exact_size(egui::vec2(40.0, 40.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(tile, CornerRadius::same(10), p.accent_soft);
    ui.painter().text(
        tile.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(22.0),
        theme::ink(p.accent),
    );
    widgets::gap(ui, theme::S4);
    ui.add(
        egui::Label::new(
            RichText::new(title)
                .font(theme::semibold(20.0))
                .color(p.text),
        )
        .wrap(),
    );
    widgets::gap(ui, theme::S2);
    ui.add(egui::Label::new(RichText::new(lead).color(p.text_muted)).wrap());
}

/// A welcome feature: an accent glyph, a semibold title over a muted line.
fn feature(ui: &mut egui::Ui, glyph: &str, title: &str, body: &str) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = theme::S3;
        ui.add_sized(
            [20.0, 20.0],
            egui::Label::new(icons::icon(glyph, 18.0).color(theme::ink(pal().accent))),
        );
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add(
                egui::Label::new(
                    RichText::new(title)
                        .font(theme::semibold(theme::BASE_SIZE))
                        .color(pal().text),
                )
                .wrap(),
            );
            ui.add(
                egui::Label::new(
                    RichText::new(body)
                        .font(theme::regular(13.0))
                        .color(pal().text_muted),
                )
                .wrap(),
            );
        });
    });
}

/// An inset hint: page-background fill, an amber glyph, wrapping text.
fn tip(ui: &mut egui::Ui, glyph: &str, text: &str) {
    egui::Frame::new()
        .fill(pal().bg)
        .corner_radius(CornerRadius::same(theme::R_MD))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = theme::S2;
                ui.add_sized(
                    [18.0, 18.0],
                    egui::Label::new(icons::icon(glyph, 16.0).color(pal().warn)),
                );
                ui.add(
                    egui::Label::new(
                        RichText::new(text)
                            .font(theme::regular(12.5))
                            .color(pal().text_muted),
                    )
                    .wrap(),
                );
            });
        });
}

/// A coloured glyph and text on one line (the installed state).
fn status_line(ui: &mut egui::Ui, glyph: &str, color: egui::Color32, text: &str) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = theme::S2;
        ui.label(icons::icon(glyph, 16.0).color(color));
        ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
    });
}

/// A model line's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelLine {
    Installed,
    Downloading,
    Missing,
}

/// One model: its name over the installed size, "downloading…" or "not
/// downloaded yet".
fn model_line(ui: &mut egui::Ui, model: &ModelInfo, line: ModelLine) {
    let p = pal();
    let (glyph, color, state) = match line {
        ModelLine::Installed => {
            let state = match model.size_mb {
                Some(mb) => format!("installed · {} MB", mb.round()),
                None => "installed".into(),
            };
            (ph::CHECK_CIRCLE, p.ok_light, state)
        }
        ModelLine::Downloading => (
            ph::ARROW_CIRCLE_DOWN,
            theme::ink(p.accent),
            "downloading…".into(),
        ),
        ModelLine::Missing => (ph::CIRCLE_DASHED, p.text_faint, "not downloaded yet".into()),
    };
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = theme::S2;
        ui.add_sized(
            [18.0, 18.0],
            egui::Label::new(icons::icon(glyph, 16.0).color(color)),
        );
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            widgets::elide(
                ui,
                &model.display_name,
                theme::regular(theme::BASE_SIZE),
                p.text,
            );
            ui.label(
                RichText::new(state)
                    .font(theme::regular(12.0))
                    .color(p.text_muted),
            );
        });
    });
}

/// A live level bar under a device picker, with a hint while it is quiet
/// and the reason when the device could not be opened.
fn level_meter(ui: &mut egui::Ui, probe: &Probe, quiet_hint: &str) {
    let p = pal();
    if let Some(error) = probe.error() {
        status_note(ui, p.rec_light, &format!("Level check failed: {error}"));
        return;
    }
    let level = probe.level();
    let running = !probe.finished();
    let (glyph, name) = match probe.kind {
        TrackKind::Mic => (ph::MICROPHONE, "Microphone level"),
        TrackKind::Loopback => (ph::SPEAKER_HIGH, "Meeting audio level"),
    };
    // Close under its picker, the hint tight under the bar.
    widgets::gap(ui, theme::S2);
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        meter_row(ui, glyph, name, level);
        let hint = if !running {
            Some("Test finished.")
        } else if level < 0.05 {
            Some(quiet_hint)
        } else {
            None
        };
        if let Some(hint) = hint {
            ui.label(
                RichText::new(hint)
                    .font(theme::regular(12.0))
                    .color(p.text_faint),
            );
        }
    });
    if running {
        // Smooth while the window shows (the shell schedules the frames).
        icons::request_animation_frame(ui.ctx());
    }
}

/// The meter itself: a muted glyph and a 6 pt bar, accent up to `level`.
fn meter_row(ui: &mut egui::Ui, glyph: &str, name: &str, level: f32) {
    let p = pal();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = theme::S2;
        ui.label(icons::icon(glyph, 14.0).color(p.text_muted));
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
        response.widget_info(|| egui::WidgetInfo::slider(false, f64::from(level), name));
        let radius = CornerRadius::same(3);
        ui.painter().rect_filled(rect, radius, p.border);
        let mut filled = rect;
        filled.set_width(rect.width() * level.clamp(0.0, 1.0));
        if filled.width() > 0.5 {
            ui.painter().rect_filled(filled, radius, p.accent);
        }
    });
}

/// The width [`keycap`] draws `combo` at (12 pt monospace, 8 pt padding).
fn keycap_width(ui: &egui::Ui, combo: &str) -> f32 {
    let text = ui
        .painter()
        .layout_no_wrap(combo.to_owned(), egui::FontId::monospace(12.0), pal().text)
        .size()
        .x;
    text + 2.0 * 8.0 + 2.0
}

/// A keyboard tip: the combo as a key cap in a `keys`-wide column (a dash
/// when there is none), its description beside it.
fn shortcut(ui: &mut egui::Ui, keys: f32, combo: Option<&str>, what: &str) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = theme::S3;
        let keys = keys.min(ui.available_width() * 0.45);
        ui.allocate_ui_with_layout(egui::vec2(keys, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(keys);
            match combo {
                Some(combo) => keycap(ui, combo),
                None => {
                    ui.label(RichText::new("—").color(pal().text_faint));
                }
            }
        });
        ui.add(
            egui::Label::new(
                RichText::new(what)
                    .font(theme::regular(13.0))
                    .color(pal().text_muted),
            )
            .wrap(),
        );
    });
}

/// Skip setup on the left (until the last step), then Back and the
/// forward button on the right.
fn footer(ui: &mut egui::Ui, step: Step, forward: Forward) -> Option<Nav> {
    let mut nav = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = theme::S2;
        if step != Step::Done && widgets::btn_ghost(ui, "Skip setup").clicked() {
            nav = Some(Nav::Skip);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let next = if forward.primary {
                widgets::btn_primary(ui, forward.text)
            } else {
                widgets::btn(ui, forward.text)
            };
            if next.clicked() {
                nav = Some(Nav::Next);
            }
            if step.prev().is_some() && widgets::btn(ui, "Back").clicked() {
                nav = Some(Nav::Back);
            }
        });
    });
    nav
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_runs_only_on_a_first_run() {
        assert!(!Onboarding::default().is_active());
        if debug_step().is_none() {
            // Finished or skipped before: never again.
            let done = UiPrefs {
                onboarded: true,
                ..UiPrefs::default()
            };
            assert!(!Onboarding::new(&done).is_active());
            // Not yet: active (deciding) until the library check answers.
            let fresh = Onboarding::new(&UiPrefs::default());
            assert!(fresh.is_active());
            assert!(matches!(fresh.state, State::Deciding(None)));
        }

        assert_eq!(decide(&Ok(true)), Decision::Run);
        // An existing library was set up long ago: remember that.
        assert_eq!(
            decide(&Ok(false)),
            Decision::Skip {
                mark_onboarded: true
            }
        );
        // A failed check skips setup this time only.
        assert_eq!(
            decide(&Err("database is locked".into())),
            Decision::Skip {
                mark_onboarded: false
            }
        );
    }

    #[test]
    fn steps_walk_forward_and_back() {
        assert_eq!(Step::number(1), Some(Step::Welcome));
        assert_eq!(Step::number(5), Some(Step::Done));
        assert_eq!(Step::number(0), None);
        assert_eq!(Step::number(6), None);

        let mut step = Step::Welcome;
        let mut seen = vec![step];
        while let Flow::Go(next) = flow(step, Nav::Next) {
            step = next;
            seen.push(step);
        }
        assert_eq!(seen, Step::ALL);
        assert_eq!(flow(Step::Done, Nav::Next), Flow::Finish);

        assert_eq!(flow(Step::Welcome, Nav::Back), Flow::Stay);
        assert_eq!(flow(Step::Models, Nav::Back), Flow::Go(Step::Audio));
        assert_eq!(flow(Step::Done, Nav::Back), Flow::Go(Step::Preferences));
        for step in Step::ALL {
            assert_eq!(flow(step, Nav::Skip), Flow::Finish, "{step:?}");
        }
    }

    #[test]
    fn esc_skips_only_with_nothing_else_to_close() {
        assert!(esc_skips(false, false, false));
        assert!(!esc_skips(true, false, false), "closes the dropdown");
        assert!(!esc_skips(false, true, false), "leaves the text field");
        assert!(!esc_skips(false, false, true), "answers the modal");
    }

    #[test]
    fn the_wizard_starts_where_asked() {
        let wizard = Wizard::new(Step::Models);
        assert_eq!(wizard.step, Step::Models);
        assert_eq!(wizard.mounted, None, "loads start on the first pass");
        assert!(wizard.levels.is_none());
    }
}
