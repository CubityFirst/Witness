//! The notification-area icon: menu (Open Witness / Record now ⇄ Stop
//! recording / status line / Quit), tooltip and the recording icon swap.
//! Created exactly once per process on the main thread, as the process's
//! first `TrayIcon` (uID 2, like 0.2.x), and never with a GUID (design §7):
//! Explorer keys the "always show" promotion on exe path + uID, and a GUID
//! would bind the icon to one exe path.
//!
//! The tray-icon / muda handlers run synchronously in the tray window's
//! wndproc on the main thread; they only forward a `ShellEvent` through the
//! sink, and `WitnessApp::logic()` does the work (works while hidden).

use crate::bridge::UiSink;
use crate::platform::ShellEvent;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// The tray icon id (as in 0.2.x).
const TRAY_ID: &str = "witness-tray";
/// The exe's icon resource (build.rs embeds `icons/icon.ico` under this ID,
/// the same one tauri-build used).
const APP_ICON_RESOURCE: u16 = 32512;
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

pub struct Tray {
    icon: TrayIcon,
    record_item: MenuItem,
    status_item: MenuItem,
    idle_icon: Icon,
    /// None if `tray-rec.png` could not be decoded (the idle icon stays).
    recording_icon: Option<Icon>,
    recording: bool,
    warning: Option<String>,
    /// (owner HWND, uID) for the panic hook's NIM_DELETE.
    ids: (isize, u32),
    /// Keeps the context menu alive for the icon's lifetime.
    _menu: Menu,
}

impl Tray {
    /// Create the icon and menu and route their events to `sink`.
    pub fn build(sink: UiSink) -> anyhow::Result<Tray> {
        let open = MenuItem::with_id(MENU_OPEN, "Open Witness", true, None);
        let record = MenuItem::with_id(MENU_RECORD, record_text(false), true, None);
        let status = MenuItem::with_id(MENU_STATUS, status_text(false), false, None);
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
        let click_sink = sink;
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let Some((x, y)) = flyout_click(&event) {
                click_sink.shell(ShellEvent::TrayClick { x, y });
            }
        }));

        let idle_icon = app_icon()?;
        let recording_icon = match recording_icon() {
            Ok(icon) => Some(icon),
            Err(error) => {
                log::warn!("tray recording icon: {error:#}");
                None
            }
        };
        let icon = TrayIconBuilder::new()
            .with_id(TRAY_ID)
            .with_icon(idle_icon.clone())
            .with_tooltip(tooltip(false, None))
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
            idle_icon,
            recording_icon,
            recording: false,
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
        self.status_item.set_text(status_text(recording));
        self.apply_tooltip();
        let icon = match (&self.recording_icon, recording) {
            (Some(rec), true) => rec.clone(),
            _ => self.idle_icon.clone(),
        };
        if let Err(error) = self.icon.set_icon(Some(icon)) {
            log::warn!("tray icon swap failed: {error}");
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
        let text = tooltip(self.recording, self.warning.as_deref());
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

fn status_text(recording: bool) -> &'static str {
    if recording {
        "● Recording"
    } else {
        "Idle"
    }
}

fn tooltip(recording: bool, warning: Option<&str>) -> String {
    match (recording, warning) {
        (false, _) => "Witness — idle".into(),
        (true, None) => "Witness — recording".into(),
        (true, Some(text)) => format!("Witness — recording — {text}"),
    }
}

/// The exe's own icon (resource 32512), so Windows picks the right small
/// size; the bundled PNG if the resource is missing.
fn app_icon() -> anyhow::Result<Icon> {
    match Icon::from_resource(APP_ICON_RESOURCE, None) {
        Ok(icon) => Ok(icon),
        Err(error) => {
            log::warn!("tray icon resource {APP_ICON_RESOURCE}: {error}; using the PNG");
            png_icon(include_bytes!("../../icons/32x32.png"))
        }
    }
}

/// The red recording icon (`tray-rec.png`, 128×128 RGBA).
fn recording_icon() -> anyhow::Result<Icon> {
    png_icon(include_bytes!("../../icons/tray-rec.png"))
}

fn png_icon(bytes: &[u8]) -> anyhow::Result<Icon> {
    let data = eframe::icon_data::from_png_bytes(bytes)?;
    Ok(Icon::from_rgba(data.rgba, data.width, data.height)?)
}

#[cfg(windows)]
mod imp {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tray_icon::{Rect, TrayIconId};

    #[test]
    fn texts_match_0_2_x() {
        assert_eq!(record_text(false), "Record now");
        assert_eq!(record_text(true), "Stop recording");
        assert_eq!(status_text(false), "Idle");
        assert_eq!(status_text(true), "● Recording");
        assert_eq!(tooltip(false, None), "Witness — idle");
        assert_eq!(tooltip(false, Some("x")), "Witness — idle");
        assert_eq!(tooltip(true, None), "Witness — recording");
        assert_eq!(
            tooltip(true, Some("Microphone disconnected")),
            "Witness — recording — Microphone disconnected"
        );
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
    fn bundled_icons_decode() {
        assert!(recording_icon().is_ok());
        assert!(png_icon(include_bytes!("../../icons/32x32.png")).is_ok());
    }
}
