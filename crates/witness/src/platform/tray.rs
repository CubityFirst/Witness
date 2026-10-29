//! The notification-area icon: menu (Open Witness / Record now ⇄ Stop
//! recording / status line / Quit), tooltip and the state glyph (idle,
//! recording, transcribing), inked for the taskbar's light or dark theme.
//! Created exactly once per process on the main thread, as the process's
//! first `TrayIcon` (uID 2, like 0.2.x), and never with a GUID (design §7):
//! Explorer keys the "always show" promotion on exe path + uID, and a GUID
//! would bind the icon to one exe path.
//!
//! The tray-icon / muda handlers run synchronously in the tray window's
//! wndproc on the main thread; they only forward a `ShellEvent` through the
//! sink, and `WitnessApp::logic()` does the work (works while hidden).

use crate::bridge::UiSink;
use crate::platform::{system, ShellEvent};
use anyhow::Context as _;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// The tray icon id (as in 0.2.x).
const TRAY_ID: &str = "witness-tray";
/// The shell uID the process's first tray icon is expected to get. tray-icon
/// draws it from a private process-wide counter starting at 1, but
/// `TrayIconBuilder::new()` spends the first value on a default
/// `TrayIconId` before `build()` takes the next one, so the first icon is
/// uID 2. Tauri 2 (tray-icon 0.24) went through the same builder, so
/// 0.2.x's icon was uID 2 as well, and Explorer's "always show" promotion
/// (exe path + uID) carries over. The real uID is probed, not assumed
/// ([`probe_uid`]): any earlier `TrayIconId::new_unique()` shifts it.
const EXPECTED_TRAY_UID: u32 = 2;
/// The uIDs probed when the icon is not at the expected one.
const PROBED_TRAY_UIDS: std::ops::RangeInclusive<u32> = 1..=16;

const MENU_OPEN: &str = "open";
const MENU_RECORD: &str = "record";
const MENU_STATUS: &str = "status";
const MENU_QUIT: &str = "quit";

/// What the tray glyph shows. Recording wins over transcribing (the
/// pipeline can run for an earlier meeting while a new one records).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayState {
    Idle,
    Recording,
    Transcribing,
}

impl TrayState {
    fn of(recording: bool, transcribing: bool) -> TrayState {
        match (recording, transcribing) {
            (true, _) => TrayState::Recording,
            (false, true) => TrayState::Transcribing,
            (false, false) => TrayState::Idle,
        }
    }
}

/// How the glyph is drawn: monochrome inked for a light or dark taskbar, or
/// the full-colour app tile (Settings → General → Tray icon).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayLook {
    Light,
    Dark,
    Colour,
}

impl TrayLook {
    fn of(colour: bool, light_taskbar: bool) -> TrayLook {
        match (colour, light_taskbar) {
            (true, _) => TrayLook::Colour,
            (false, true) => TrayLook::Light,
            (false, false) => TrayLook::Dark,
        }
    }
}

pub struct Tray {
    icon: TrayIcon,
    record_item: MenuItem,
    status_item: MenuItem,
    recording: bool,
    transcribing: bool,
    colour: bool,
    light_taskbar: bool,
    /// The glyph last set on the icon, so unchanged states skip the shell.
    shown: Option<(TrayState, TrayLook)>,
    warning: Option<String>,
    /// (owner HWND, uID) for the panic hook's NIM_DELETE.
    ids: (isize, u32),
    /// Keeps the context menu alive for the icon's lifetime.
    _menu: Menu,
}

impl Tray {
    /// Create the icon (full-colour or monochrome) and menu and route their
    /// events to `sink`.
    pub fn build(sink: UiSink, colour: bool) -> anyhow::Result<Tray> {
        let open = MenuItem::with_id(MENU_OPEN, "Open Witness", true, None);
        let record = MenuItem::with_id(MENU_RECORD, record_text(false), true, None);
        let status = MenuItem::with_id(MENU_STATUS, status_text(TrayState::Idle), false, None);
        let quit = MenuItem::with_id(MENU_QUIT, "Quit", true, None);
        let menu = Menu::with_items(&[
            &open,
            &PredefinedMenuItem::separator(),
            &record,
            &status,
            &PredefinedMenuItem::separator(),
            &quit,
        ])?;

        // Both handlers are process-wide once-cells; `build` runs at most
        // once per process (main.rs runs the app creator once).
        let menu_sink = sink.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let shell = match event.id().as_ref() {
                MENU_OPEN => ShellEvent::TrayMenuOpen,
                MENU_RECORD => ShellEvent::TrayMenuToggleRecord,
                MENU_QUIT => ShellEvent::TrayMenuQuit,
                _ => return,
            };
            menu_sink.shell(shell);
        }));
        let click_sink = sink.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let Some((x, y)) = flyout_click(&event) {
                click_sink.shell(ShellEvent::TrayClick { x, y });
            }
        }));

        // Windows' default for an unreadable mode is a dark taskbar.
        let light_taskbar = system::system_uses_light_theme().unwrap_or(false);
        watch_taskbar_theme(sink, light_taskbar);
        let look = TrayLook::of(colour, light_taskbar);
        let icon = TrayIconBuilder::new()
            .with_id(TRAY_ID)
            .with_icon(glyph(TrayState::Idle, look)?)
            .with_tooltip(tooltip(TrayState::Idle, None))
            .with_menu(Box::new(menu.clone()))
            // The menu opens on right-click only; a left click is the flyout.
            .with_menu_on_left_click(false)
            .build()?;

        let hwnd = icon.window_handle() as isize;
        let uid = match probe_uid(|uid| imp::icon_exists(hwnd, uid)) {
            Some(uid) => {
                if uid != EXPECTED_TRAY_UID {
                    log::warn!(
                        "tray icon is uID {uid}, not {EXPECTED_TRAY_UID}: Explorer may not \
                         keep 0.2.x's \"always show\" setting"
                    );
                }
                uid
            }
            None => {
                // Only a log: the panic hook's NIM_DELETE would just miss.
                log::warn!("tray icon uID not found; a crash may leave a ghost icon");
                EXPECTED_TRAY_UID
            }
        };
        let ids = (hwnd, uid);
        log::debug!("tray icon created (hwnd {hwnd:#x}, uID {uid})");
        Ok(Tray {
            icon,
            record_item: record,
            status_item: status,
            recording: false,
            transcribing: false,
            colour,
            light_taskbar,
            shown: Some((TrayState::Idle, look)),
            warning: None,
            ids,
            _menu: menu,
        })
    }

    /// Icon, tooltip and menu text for the recording state (0.2.x
    /// `tray::update`; it also clears any capture warning).
    pub fn set_recording(&mut self, recording: bool) {
        self.recording = recording;
        self.warning = None;
        self.record_item.set_text(record_text(recording));
        self.apply_state();
    }

    /// Whether the transcription pipeline is working (shown while not
    /// recording). Cheap to call on every pass: unchanged values do nothing.
    pub fn set_transcribing(&mut self, transcribing: bool) {
        if transcribing != self.transcribing {
            self.transcribing = transcribing;
            self.apply_state();
        }
    }

    /// Re-ink the monochrome glyph for the taskbar's theme.
    pub fn set_light_taskbar(&mut self, light: bool) {
        if light != self.light_taskbar {
            self.light_taskbar = light;
            self.apply_icon();
        }
    }

    /// Full-colour or monochrome. Cheap to call on every pass.
    pub fn set_colour(&mut self, colour: bool) {
        if colour != self.colour {
            self.colour = colour;
            self.apply_icon();
        }
    }

    fn state(&self) -> TrayState {
        TrayState::of(self.recording, self.transcribing)
    }

    fn apply_state(&mut self) {
        self.status_item.set_text(status_text(self.state()));
        self.apply_tooltip();
        self.apply_icon();
    }

    fn apply_icon(&mut self) {
        let wanted = (self.state(), TrayLook::of(self.colour, self.light_taskbar));
        if self.shown == Some(wanted) {
            return;
        }
        let swapped =
            glyph(wanted.0, wanted.1).and_then(|icon| Ok(self.icon.set_icon(Some(icon))?));
        match swapped {
            Ok(()) => self.shown = Some(wanted),
            Err(error) => log::warn!("tray icon swap failed: {error:#}"),
        }
    }

    /// Capture warning appended to the recording tooltip (None clears it).
    /// Ignored while idle, so a late warning cannot leave a "recording"
    /// tooltip behind after the stop.
    pub fn set_warning(&mut self, warning: Option<String>) {
        if !self.recording {
            return;
        }
        self.warning = warning;
        self.apply_tooltip();
    }

    fn apply_tooltip(&self) {
        let text = tooltip(self.state(), self.warning.as_deref());
        if let Err(error) = self.icon.set_tooltip(Some(text)) {
            log::warn!("tray tooltip failed: {error}");
        }
    }

    /// (owner HWND, uID) of the shell icon, for the panic hook's NIM_DELETE.
    pub fn notify_ids(&self) -> (isize, u32) {
        self.ids
    }
}

/// The icon's shell uID: the expected one first, then the rest of the
/// probed range. Each tray icon has its own hidden owner window, so any uID
/// the shell knows for that window is ours.
fn probe_uid(exists: impl Fn(u32) -> bool) -> Option<u32> {
    std::iter::once(EXPECTED_TRAY_UID)
        .chain(PROBED_TRAY_UIDS.filter(|&uid| uid != EXPECTED_TRAY_UID))
        .find(|&uid| exists(uid))
}

/// The physical click position for a left click (button up) or a left
/// double click: both open the flyout, as in 0.2.x.
fn flyout_click(event: &TrayIconEvent) -> Option<(f64, f64)> {
    match event {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            position,
            ..
        }
        | TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            position,
            ..
        } => Some((position.x, position.y)),
        _ => None,
    }
}

fn record_text(recording: bool) -> &'static str {
    if recording {
        "Stop recording"
    } else {
        "Record now"
    }
}

fn status_text(state: TrayState) -> &'static str {
    match state {
        TrayState::Idle => "Idle",
        TrayState::Recording => "● Recording",
        TrayState::Transcribing => "Transcribing…",
    }
}

/// The capture warning only shows while recording.
fn tooltip(state: TrayState, warning: Option<&str>) -> String {
    match (state, warning) {
        (TrayState::Idle, _) => "Witness · idle".into(),
        (TrayState::Transcribing, _) => "Witness · transcribing".into(),
        (TrayState::Recording, None) => "Witness · recording".into(),
        (TrayState::Recording, Some(text)) => format!("Witness · recording · {text}"),
    }
}

/// Sends `ShellEvent::TaskbarTheme` whenever Windows' default mode flips,
/// from a thread parked on the registry (no polling while hidden).
fn watch_taskbar_theme(sink: UiSink, mut light: bool) {
    let spawned = std::thread::Builder::new()
        .name("taskbar-theme".into())
        .spawn(move || {
            system::watch_personalize(|| {
                let now = system::system_uses_light_theme().unwrap_or(false);
                if now != light {
                    light = now;
                    sink.shell(ShellEvent::TaskbarTheme { light });
                }
            });
        });
    if let Err(error) = spawned {
        log::warn!("taskbar theme watcher: {error}");
    }
}

/// `icons/tray-<state>-<light|dark|colour>.ico`, drawn by
/// `assets/brand/icons.mjs` with grid-fitted frames for each small-icon size
/// up to 300 % scaling.
fn glyph_ico(state: TrayState, look: TrayLook) -> &'static [u8] {
    use {TrayLook as L, TrayState as S};
    match (state, look) {
        (S::Idle, L::Light) => include_bytes!("../../icons/tray-idle-light.ico"),
        (S::Idle, L::Dark) => include_bytes!("../../icons/tray-idle-dark.ico"),
        (S::Idle, L::Colour) => include_bytes!("../../icons/tray-idle-colour.ico"),
        (S::Recording, L::Light) => include_bytes!("../../icons/tray-recording-light.ico"),
        (S::Recording, L::Dark) => include_bytes!("../../icons/tray-recording-dark.ico"),
        (S::Recording, L::Colour) => include_bytes!("../../icons/tray-recording-colour.ico"),
        (S::Transcribing, L::Light) => include_bytes!("../../icons/tray-transcribing-light.ico"),
        (S::Transcribing, L::Dark) => include_bytes!("../../icons/tray-transcribing-dark.ico"),
        (S::Transcribing, L::Colour) => {
            include_bytes!("../../icons/tray-transcribing-colour.ico")
        }
    }
}

/// The glyph at the shell's small-icon size (16 px at 100 %), taken from its
/// own frame rather than scaled, so the bars stay on whole pixels.
fn glyph(state: TrayState, look: TrayLook) -> anyhow::Result<Icon> {
    let size = imp::small_icon_size().map_or(16, |(width, _)| width);
    png_icon(ico_frame(glyph_ico(state, look), size)?)
}

/// The PNG frame of a PNG-compressed .ico for `size`: the smallest frame at
/// least that big, else the largest.
fn ico_frame(ico: &[u8], size: u32) -> anyhow::Result<&[u8]> {
    let le32 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
    let count = usize::from(u16::from_le_bytes(
        ico.get(4..6).context("ico header")?.try_into()?,
    ));
    let mut frames = Vec::with_capacity(count);
    for i in 0..count {
        let entry = ico.get(6 + 16 * i..22 + 16 * i).context("ico directory")?;
        let side = if entry[0] == 0 {
            256
        } else {
            u32::from(entry[0])
        };
        let (len, at) = (le32(&entry[8..12]), le32(&entry[12..16]));
        frames.push((side, ico.get(at..at + len).context("ico frame")?));
    }
    frames
        .iter()
        .filter(|(side, _)| *side >= size)
        .min_by_key(|(side, _)| *side)
        .or_else(|| frames.iter().max_by_key(|(side, _)| *side))
        .map(|(_, png)| *png)
        .context("empty ico")
}

fn png_icon(bytes: &[u8]) -> anyhow::Result<Icon> {
    let data = eframe::icon_data::from_png_bytes(bytes)?;
    Ok(Icon::from_rgba(data.rgba, data.width, data.height)?)
}

#[cfg(windows)]
mod imp {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON, SM_CYSMICON};

    /// The shell's small-icon size (16 px at 100 %, 24 px at 150 %), or
    /// None (the default size) if the metrics are unavailable.
    pub fn small_icon_size() -> Option<(u32, u32)> {
        // SAFETY: plain metric queries.
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) };
        (w > 0 && h > 0).then_some((w as u32, h as u32))
    }

    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{Shell_NotifyIconW, NIM_MODIFY, NOTIFYICONDATAW};

    /// Whether the shell knows an icon with this owner window and uID: a
    /// `NIM_MODIFY` that changes nothing succeeds only for an existing icon
    /// (unlike `Shell_NotifyIconGetRect`, which fails for icons hidden in
    /// the Windows 11 overflow).
    pub fn icon_exists(hwnd: isize, uid: u32) -> bool {
        let data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: HWND(hwnd as *mut _),
            uID: uid,
            ..Default::default()
        };
        // SAFETY: a correctly sized struct with no flags set: nothing is
        // read beyond hWnd + uID and nothing is modified.
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool()
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn icon_exists(_hwnd: isize, _uid: u32) -> bool {
        true
    }
    pub fn small_icon_size() -> Option<(u32, u32)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tray_icon::{Rect, TrayIconId};

    #[test]
    fn texts_match_0_2_x() {
        assert_eq!(record_text(false), "Record now");
        assert_eq!(record_text(true), "Stop recording");
        assert_eq!(status_text(TrayState::Idle), "Idle");
        assert_eq!(status_text(TrayState::Recording), "● Recording");
        assert_eq!(tooltip(TrayState::Idle, None), "Witness · idle");
        assert_eq!(tooltip(TrayState::Idle, Some("x")), "Witness · idle");
        assert_eq!(tooltip(TrayState::Recording, None), "Witness · recording");
        assert_eq!(
            tooltip(TrayState::Recording, Some("Microphone disconnected")),
            "Witness · recording · Microphone disconnected"
        );
    }

    #[test]
    fn transcribing_shows_only_while_not_recording() {
        assert_eq!(TrayState::of(false, false), TrayState::Idle);
        assert_eq!(TrayState::of(false, true), TrayState::Transcribing);
        assert_eq!(TrayState::of(true, true), TrayState::Recording);
        assert_eq!(status_text(TrayState::Transcribing), "Transcribing…");
        assert_eq!(
            tooltip(TrayState::Transcribing, Some("x")),
            "Witness · transcribing"
        );
    }

    #[test]
    fn colour_overrides_the_taskbar_theme() {
        assert_eq!(TrayLook::of(false, true), TrayLook::Light);
        assert_eq!(TrayLook::of(false, false), TrayLook::Dark);
        assert_eq!(TrayLook::of(true, true), TrayLook::Colour);
        assert_eq!(TrayLook::of(true, false), TrayLook::Colour);
    }

    #[test]
    fn ico_frame_takes_the_smallest_frame_at_least_the_size() {
        let ico = glyph_ico(TrayState::Idle, TrayLook::Light);
        let side = |size| {
            let png = ico_frame(ico, size).unwrap();
            eframe::icon_data::from_png_bytes(png).unwrap().width
        };
        assert_eq!(side(16), 16);
        assert_eq!(side(20), 20);
        assert_eq!(side(30), 32);
        assert_eq!(side(64), 48);
        assert!(ico_frame(&[0, 0, 1, 0, 1, 0], 16).is_err());
    }

    #[test]
    fn only_left_button_up_and_double_click_open_the_flyout() {
        let position = tray_icon::dpi::PhysicalPosition::new(2300.0, 1415.0);
        let click = |button, button_state| TrayIconEvent::Click {
            id: TrayIconId::new(TRAY_ID),
            position,
            rect: Rect::default(),
            button,
            button_state,
        };
        assert_eq!(
            flyout_click(&click(MouseButton::Left, MouseButtonState::Up)),
            Some((2300.0, 1415.0))
        );
        assert_eq!(
            flyout_click(&click(MouseButton::Left, MouseButtonState::Down)),
            None
        );
        assert_eq!(
            flyout_click(&click(MouseButton::Right, MouseButtonState::Up)),
            None
        );
        let double = TrayIconEvent::DoubleClick {
            id: TrayIconId::new(TRAY_ID),
            position,
            rect: Rect::default(),
            button: MouseButton::Left,
        };
        assert_eq!(flyout_click(&double), Some((2300.0, 1415.0)));
    }

    #[test]
    fn uid_probe_prefers_the_expected_uid_and_finds_others() {
        assert_eq!(probe_uid(|_| true), Some(EXPECTED_TRAY_UID));
        assert_eq!(probe_uid(|uid| uid == 1), Some(1));
        assert_eq!(probe_uid(|uid| uid == 7), Some(7));
        assert_eq!(probe_uid(|_| false), None);
        // The expected uID is probed once, first.
        let probed = std::cell::RefCell::new(Vec::new());
        assert_eq!(
            probe_uid(|uid| {
                probed.borrow_mut().push(uid);
                false
            }),
            None
        );
        let probed = probed.into_inner();
        assert_eq!(probed[0], EXPECTED_TRAY_UID);
        assert_eq!(probed.len(), PROBED_TRAY_UIDS.count());
    }

    #[test]
    fn every_tray_glyph_decodes_at_every_scale() {
        for state in [
            TrayState::Idle,
            TrayState::Recording,
            TrayState::Transcribing,
        ] {
            for look in [TrayLook::Light, TrayLook::Dark, TrayLook::Colour] {
                for size in [16, 20, 24, 28, 32, 40, 48] {
                    let png = ico_frame(glyph_ico(state, look), size).unwrap();
                    let data = eframe::icon_data::from_png_bytes(png).unwrap();
                    assert_eq!(
                        (data.width, data.height),
                        (size, size),
                        "{state:?} {look:?}"
                    );
                    assert!(png_icon(png).is_ok());
                }
            }
        }
    }
}
