//! Global hotkeys with candidate chains, because other apps can own a combo
//! system-wide: record ctrl+alt+r → ctrl+alt+w → ctrl+alt+shift+r, bookmark
//! ctrl+alt+b → ctrl+alt+m. A combo the user picked in Settings (saved in
//! `ui-state.toml`) is tried before its chain. Registered on the main thread
//! (winit pumps the hotkey window); `Pressed` events only call
//! `sink.shell(..)`, and `WitnessApp::logic()` hands the action to the
//! core's ordered rec-control worker, so a slow start/stop never stalls the
//! message pump.
//!
//! The strings are parsed by global-hotkey's own parser, the one
//! tauri-plugin-global-shortcut used, so the bound combo strings (shown
//! uppercased in tooltips and Settings) are unchanged.

use crate::bridge::UiSink;
use crate::platform::ShellEvent;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

/// Record toggle candidates, first free one wins.
const RECORD_CHAIN: [&str; 3] = ["ctrl+alt+r", "ctrl+alt+w", "ctrl+alt+shift+r"];
/// Bookmark-this-moment candidates.
const BOOKMARK_CHAIN: [&str; 2] = ["ctrl+alt+b", "ctrl+alt+m"];

/// Which global hotkey.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    Record,
    Bookmark,
}

impl HotkeyAction {
    fn label(self) -> &'static str {
        match self {
            HotkeyAction::Record => "record",
            HotkeyAction::Bookmark => "bookmark",
        }
    }

    fn chain(self) -> &'static [&'static str] {
        match self {
            HotkeyAction::Record => &RECORD_CHAIN,
            HotkeyAction::Bookmark => &BOOKMARK_CHAIN,
        }
    }
}

/// The (record, bookmark) hotkey ids the event handler dispatches on.
type BoundIds = Arc<Mutex<(Option<u32>, Option<u32>)>>;

pub struct Hotkeys {
    /// None if the hotkey window could not be created (nothing bound).
    manager: Option<GlobalHotKeyManager>,
    record: Option<(String, HotKey)>,
    bookmark: Option<(String, HotKey)>,
    /// Shared with the event handler, which outlives a rebind.
    ids: BoundIds,
}

impl Hotkeys {
    /// Bind the user's combo (if any), else the first free combo of each
    /// chain, and route `Pressed` events to `sink`. Runs once per process
    /// (the event handler is a once-cell).
    pub fn register(sink: UiSink, record: Option<&str>, bookmark: Option<&str>) -> Hotkeys {
        let manager = match GlobalHotKeyManager::new() {
            Ok(manager) => manager,
            Err(error) => {
                log::error!("global hotkeys unavailable: {error}");
                return Hotkeys::none();
            }
        };
        let bind = |action: HotkeyAction, custom: Option<&str>| {
            bind_first(&candidates(action, custom), action.label(), |hotkey| {
                manager.register(*hotkey)
            })
        };
        let record = bind(HotkeyAction::Record, record);
        let bookmark = bind(HotkeyAction::Bookmark, bookmark);

        let ids = BoundIds::default();
        let handler_ids = ids.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            let (record_id, bookmark_id) = *lock(&handler_ids);
            if let Some(shell) = shell_event(event, record_id, bookmark_id) {
                sink.shell(shell);
            }
        }));
        let hotkeys = Hotkeys {
            manager: Some(manager),
            record,
            bookmark,
            ids,
        };
        hotkeys.publish_ids();
        hotkeys
    }

    /// Nothing bound (debug snapshot runs leave the combos to real instances).
    pub fn none() -> Hotkeys {
        Hotkeys {
            manager: None,
            record: None,
            bookmark: None,
            ids: BoundIds::default(),
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

    /// Settings: bind `action` to `combo`, or back to its default chain
    /// (None). On failure the previous combo stays bound.
    pub fn rebind(&mut self, action: HotkeyAction, combo: Option<&str>) -> Result<(), String> {
        let Some(manager) = &self.manager else {
            return Err("global hotkeys are unavailable".into());
        };
        let (slot, other) = match action {
            HotkeyAction::Record => (&mut self.record, &self.bookmark),
            HotkeyAction::Bookmark => (&mut self.bookmark, &self.record),
        };
        let result = rebind_slot(slot, other.as_ref(), action, combo, |hotkey, on| {
            if on {
                manager.register(hotkey).map_err(|e| e.to_string())
            } else {
                manager.unregister(hotkey).map_err(|e| e.to_string())
            }
        });
        self.publish_ids();
        result
    }

    /// Hand the bound ids to the event handler.
    fn publish_ids(&self) {
        let id = |bound: &Option<(String, HotKey)>| bound.as_ref().map(|(_, hotkey)| hotkey.id());
        *lock(&self.ids) = (id(&self.record), id(&self.bookmark));
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

/// `rebind` against a `toggle(hotkey, register?)` seam (tests fake it).
fn rebind_slot(
    slot: &mut Option<(String, HotKey)>,
    other: Option<&(String, HotKey)>,
    action: HotkeyAction,
    combo: Option<&str>,
    mut toggle: impl FnMut(HotKey, bool) -> Result<(), String>,
) -> Result<(), String> {
    if let Some(combo) = combo {
        let hotkey = HotKey::from_str(combo).map_err(|e| format!("{combo}: {e}"))?;
        if other.is_some_and(|(_, bound)| *bound == hotkey) {
            return Err(format!("{combo} is already the other hotkey"));
        }
    }
    let chain = match combo {
        Some(combo) => vec![combo],
        None => action.chain().to_vec(),
    };
    // Bind the new combo before releasing the old one, so a failure never
    // leaves the action unbound; the current combo counts as free.
    let current = slot.as_ref().map(|(_, hotkey)| *hotkey);
    let Some(bound) = bind_first(&chain, action.label(), |hotkey| {
        if Some(*hotkey) == current {
            Ok(())
        } else {
            toggle(*hotkey, true)
        }
    }) else {
        return Err(match combo {
            Some(combo) => format!("{combo} is taken by another app"),
            None => "every default combination is taken by another app".into(),
        });
    };
    if let Some((old_combo, old)) = slot.take() {
        if old != bound.1 {
            if let Err(error) = toggle(old, false) {
                log::debug!("unregistering hotkey {old_combo}: {error}");
            }
        }
    }
    *slot = Some(bound);
    Ok(())
}

/// The user's combo (if any) first, then the default chain.
fn candidates(action: HotkeyAction, custom: Option<&str>) -> Vec<&str> {
    let mut chain: Vec<&str> = custom.into_iter().collect();
    chain.extend(action.chain().iter().filter(|c| Some(**c) != custom));
    chain
}

/// A poisoned lock only means a holder panicked; the ids are still usable.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

    fn bound(combo: &str) -> Option<(String, HotKey)> {
        Some((combo.to_string(), HotKey::from_str(combo).unwrap()))
    }

    #[test]
    fn a_user_combo_is_tried_before_the_chain() {
        assert_eq!(
            candidates(HotkeyAction::Record, Some("ctrl+alt+w")),
            vec!["ctrl+alt+w", "ctrl+alt+r", "ctrl+alt+shift+r"]
        );
        assert_eq!(candidates(HotkeyAction::Bookmark, None), BOOKMARK_CHAIN);
    }

    #[test]
    fn rebinding_swaps_the_combo_or_keeps_the_old_one() {
        let taken = HotKey::from_str("ctrl+alt+k").unwrap();
        let mut registered: Vec<u32> = vec![HotKey::from_str("ctrl+alt+r").unwrap().id()];
        let released = std::cell::Cell::new(0);
        let mut toggle = |hotkey: HotKey, on: bool| {
            if !on {
                released.set(released.get() + 1);
                registered.retain(|id| *id != hotkey.id());
                Ok(())
            } else if hotkey == taken {
                Err("HotKey already registered".to_string())
            } else {
                registered.push(hotkey.id());
                Ok(())
            }
        };
        let other = bound("ctrl+alt+b");

        let mut slot = bound("ctrl+alt+r");
        rebind_slot(
            &mut slot,
            other.as_ref(),
            HotkeyAction::Record,
            Some("ctrl+shift+f9"),
            &mut toggle,
        )
        .unwrap();
        assert_eq!(slot.as_ref().unwrap().0, "ctrl+shift+f9");

        assert_eq!(released.get(), 1);

        // Taken elsewhere: the previous combo was never released.
        let error = rebind_slot(
            &mut slot,
            other.as_ref(),
            HotkeyAction::Record,
            Some("ctrl+alt+k"),
            &mut toggle,
        )
        .unwrap_err();
        assert!(error.contains("taken"), "{error}");
        assert_eq!(slot.as_ref().unwrap().0, "ctrl+shift+f9");
        assert_eq!(released.get(), 1);

        // The other hotkey's combo is refused without touching anything.
        let error = rebind_slot(
            &mut slot,
            other.as_ref(),
            HotkeyAction::Record,
            Some("alt+ctrl+b"),
            &mut toggle,
        )
        .unwrap_err();
        assert!(error.contains("other hotkey"), "{error}");
        assert!(rebind_slot(
            &mut slot,
            None,
            HotkeyAction::Record,
            Some("ctrl+alt+nope"),
            &mut toggle
        )
        .is_err());
        assert_eq!(slot.as_ref().unwrap().0, "ctrl+shift+f9");

        // Reset walks the default chain again.
        rebind_slot(
            &mut slot,
            other.as_ref(),
            HotkeyAction::Record,
            None,
            &mut toggle,
        )
        .unwrap();
        assert_eq!(slot.as_ref().unwrap().0, "ctrl+alt+r");
        assert_eq!(
            registered,
            vec![HotKey::from_str("ctrl+alt+r").unwrap().id()]
        );
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
