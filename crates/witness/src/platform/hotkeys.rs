//! Global hotkeys with candidate chains, because other apps can own a combo
//! system-wide: record ctrl+alt+r → ctrl+alt+w → ctrl+alt+shift+r, bookmark
//! ctrl+alt+b → ctrl+alt+m. Registered on the main thread (winit pumps the
//! hotkey window); `Pressed` events only call `sink.shell(..)`, and
//! `WitnessApp::logic()` hands the action to the core's ordered rec-control
//! worker, so a slow start/stop never stalls the message pump.
//!
//! The strings are parsed by global-hotkey's own parser, the one
//! tauri-plugin-global-shortcut used, so the bound combo strings (shown
//! uppercased in tooltips and Settings) are unchanged.

use crate::bridge::UiSink;
use crate::platform::ShellEvent;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::str::FromStr;

/// Record toggle candidates, first free one wins.
const RECORD_CHAIN: [&str; 3] = ["ctrl+alt+r", "ctrl+alt+w", "ctrl+alt+shift+r"];
/// Bookmark-this-moment candidates.
const BOOKMARK_CHAIN: [&str; 2] = ["ctrl+alt+b", "ctrl+alt+m"];

pub struct Hotkeys {
    /// None if the hotkey window could not be created (nothing bound).
    manager: Option<GlobalHotKeyManager>,
    record: Option<(String, HotKey)>,
    bookmark: Option<(String, HotKey)>,
}

impl Hotkeys {
    /// Bind the first free combo of each chain and route `Pressed` events
    /// to `sink`. Runs once per process (the event handler is a once-cell).
    pub fn register(sink: UiSink) -> Hotkeys {
        let manager = match GlobalHotKeyManager::new() {
            Ok(manager) => manager,
            Err(error) => {
                log::error!("global hotkeys unavailable: {error}");
                return Hotkeys {
                    manager: None,
                    record: None,
                    bookmark: None,
                };
            }
        };
        let record = bind_first(&RECORD_CHAIN, "record", |hotkey| manager.register(*hotkey));
        let bookmark = bind_first(&BOOKMARK_CHAIN, "bookmark", |hotkey| {
            manager.register(*hotkey)
        });

        let record_id = record.as_ref().map(|(_, hotkey)| hotkey.id());
        let bookmark_id = bookmark.as_ref().map(|(_, hotkey)| hotkey.id());
        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            if let Some(shell) = shell_event(event, record_id, bookmark_id) {
                sink.shell(shell);
            }
        }));
        Hotkeys {
            manager: Some(manager),
            record,
            bookmark,
        }
    }

    /// Nothing bound (debug snapshot runs leave the combos to real instances).
    pub fn none() -> Hotkeys {
        Hotkeys {
            manager: None,
            record: None,
            bookmark: None,
        }
    }

    /// The record combo that bound (e.g. "ctrl+alt+r"), None if all taken.
    pub fn record_combo(&self) -> Option<String> {
        self.record.as_ref().map(|(combo, _)| combo.clone())
    }

    /// The bookmark combo that bound, None if all taken.
    pub fn bookmark_combo(&self) -> Option<String> {
        self.bookmark.as_ref().map(|(combo, _)| combo.clone())
    }
}

impl Drop for Hotkeys {
    /// `UnregisterHotKey` for what was bound (exit paths drop this before
    /// `process::exit`); the manager then destroys its window.
    fn drop(&mut self) {
        let Some(manager) = &self.manager else { return };
        for (combo, hotkey) in self.record.iter().chain(self.bookmark.iter()) {
            if let Err(error) = manager.unregister(*hotkey) {
                log::debug!("unregistering hotkey {combo}: {error}");
            }
        }
    }
}

/// Walk `chain` and keep the first combo that parses and registers.
/// Logs exactly like 0.2.x: `{label} hotkey bound: {c}` /
/// `hotkey {c} unavailable: {e}`.
fn bind_first<E: std::fmt::Display>(
    chain: &[&str],
    label: &str,
    mut register: impl FnMut(&HotKey) -> Result<(), E>,
) -> Option<(String, HotKey)> {
    for candidate in chain {
        let hotkey = match HotKey::from_str(candidate) {
            Ok(hotkey) => hotkey,
            Err(error) => {
                log::warn!("hotkey {candidate} unavailable: {error}");
                continue;
            }
        };
        match register(&hotkey) {
            Ok(()) => {
                log::info!("{label} hotkey bound: {candidate}");
                return Some((candidate.to_string(), hotkey));
            }
            Err(error) => log::warn!("hotkey {candidate} unavailable: {error}"),
        }
    }
    None
}

/// Witness acts on key-down only (`MOD_NOREPEAT` already drops repeats).
fn shell_event(
    event: GlobalHotKeyEvent,
    record_id: Option<u32>,
    bookmark_id: Option<u32>,
) -> Option<ShellEvent> {
    if event.state() != HotKeyState::Pressed {
        return None;
    }
    if Some(event.id()) == record_id {
        Some(ShellEvent::HotkeyRecord)
    } else if Some(event.id()) == bookmark_id {
        Some(ShellEvent::HotkeyBookmark)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_candidate_parses() {
        for combo in RECORD_CHAIN.iter().chain(BOOKMARK_CHAIN.iter()) {
            assert!(HotKey::from_str(combo).is_ok(), "{combo}");
        }
        // The combos are distinct keys, so their ids never collide.
        let ids: std::collections::HashSet<u32> = RECORD_CHAIN
            .iter()
            .chain(BOOKMARK_CHAIN.iter())
            .map(|c| HotKey::from_str(c).unwrap().id())
            .collect();
        assert_eq!(ids.len(), RECORD_CHAIN.len() + BOOKMARK_CHAIN.len());
    }

    #[test]
    fn the_first_free_candidate_wins() {
        let taken = HotKey::from_str("ctrl+alt+r").unwrap();
        let mut tried = Vec::new();
        let bound = bind_first(&RECORD_CHAIN, "record", |hotkey| {
            tried.push(hotkey.id());
            if *hotkey == taken {
                Err("HotKey already registered")
            } else {
                Ok(())
            }
        });
        assert_eq!(bound.map(|(combo, _)| combo).as_deref(), Some("ctrl+alt+w"));
        assert_eq!(tried.len(), 2);

        let none = bind_first(&BOOKMARK_CHAIN, "bookmark", |_| Err("taken"));
        assert!(none.is_none());
    }

    #[test]
    fn only_presses_of_bound_combos_become_shell_events() {
        let record = HotKey::from_str("ctrl+alt+r").unwrap().id();
        let bookmark = HotKey::from_str("ctrl+alt+b").unwrap().id();
        let event = |id, state| GlobalHotKeyEvent { id, state };
        assert_eq!(
            shell_event(
                event(record, HotKeyState::Pressed),
                Some(record),
                Some(bookmark)
            ),
            Some(ShellEvent::HotkeyRecord)
        );
        assert_eq!(
            shell_event(
                event(bookmark, HotKeyState::Pressed),
                Some(record),
                Some(bookmark)
            ),
            Some(ShellEvent::HotkeyBookmark)
        );
        assert_eq!(
            shell_event(
                event(record, HotKeyState::Released),
                Some(record),
                Some(bookmark)
            ),
            None
        );
        assert_eq!(
            shell_event(event(record, HotKeyState::Pressed), None, Some(bookmark)),
            None
        );
    }
}
