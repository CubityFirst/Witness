//! App-owned settings writes (replaces settings.tsx `settingsWriteTail`).
//!
//! The view renders from an optimistic copy; writes are serialised with at
//! most one in flight and one queued (latest wins), and the queue lives in
//! the app, so toggling a checkbox twice and immediately leaving Settings
//! still persists the last value. A failed write reverts the optimistic
//! copy to the last confirmed settings (unless a newer save is pending) and
//! toasts "Could not save settings: …".

use crate::bridge::{OpDone, Tasks};
use crate::ui::toasts::Toasts;
use std::time::{Duration, Instant};
use witness_core::settings::Settings;
use witness_core::{AppCore, CoreResult};

/// How long the "saved ✓" flash stays.
pub const SAVED_FLASH: Duration = Duration::from_millis(1500);

pub struct SettingsStore {
    /// What the UI shows (includes unsaved edits).
    current: Settings,
    /// The last value known to be on disk.
    confirmed: Settings,
    /// Seq of the write in flight, and the value it is writing.
    in_flight: Option<(u64, Settings)>,
    /// The next value to write once the in-flight one finishes.
    queued: Option<(u64, Settings)>,
    /// Seq of the newest `save`.
    seq: u64,
    saved_at: Option<Instant>,
}

/// What a finished write changed, for `app.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WriteOutcome {
    /// The write succeeded and changed the configured data_dir
    /// (bump `shell.settings_tick`).
    pub data_dir_saved: bool,
}

impl SettingsStore {
    /// Seed from the core's configured settings (`AppCore::settings` is [ui-ok]).
    pub fn new(core: &AppCore) -> Self {
        Self::from_settings(core.settings())
    }

    fn from_settings(settings: Settings) -> Self {
        SettingsStore {
            current: settings.clone(),
            confirmed: settings,
            in_flight: None,
            queued: None,
            seq: 0,
            saved_at: None,
        }
    }

    /// The optimistic settings the UI renders from.
    pub fn current(&self) -> &Settings {
        &self.current
    }

    /// Save `next` (optimistically shown at once).
    pub fn save(&mut self, next: Settings, tasks: &Tasks) {
        if let Some((seq, settings)) = self.enqueue(next) {
            start_write(tasks, seq, settings);
        }
    }

    /// Queue `next`; returns the write to start now, if none is in flight.
    fn enqueue(&mut self, next: Settings) -> Option<(u64, Settings)> {
        self.seq += 1;
        self.current = next.clone();
        if self.in_flight.is_some() {
            // Latest wins: an older queued value is simply replaced.
            self.queued = Some((self.seq, next));
            return None;
        }
        self.in_flight = Some((self.seq, next.clone()));
        Some((self.seq, next))
    }

    /// A write finished (`OpDone::SettingsWritten`). Starts the queued write,
    /// if any.
    pub fn on_written(
        &mut self,
        seq: u64,
        result: CoreResult<()>,
        toasts: &mut Toasts,
        tasks: &Tasks,
    ) -> WriteOutcome {
        let (outcome, next) = self.finish(seq, result, toasts);
        if let Some((seq, settings)) = next {
            start_write(tasks, seq, settings);
        }
        outcome
    }

    fn finish(
        &mut self,
        seq: u64,
        result: CoreResult<()>,
        toasts: &mut Toasts,
    ) -> (WriteOutcome, Option<(u64, Settings)>) {
        let mut outcome = WriteOutcome::default();
        let written = match self.in_flight.take() {
            Some((in_flight, settings)) if in_flight == seq => settings,
            other => {
                log::warn!("unexpected settings write completion {seq}");
                self.in_flight = other;
                return (outcome, None);
            }
        };
        let latest = seq == self.seq;
        match result {
            Ok(()) => {
                outcome.data_dir_saved = written.data_dir != self.confirmed.data_dir;
                self.confirmed = written;
                if latest {
                    self.saved_at = Some(Instant::now());
                }
            }
            Err(error) => {
                if latest {
                    self.current = self.confirmed.clone();
                }
                toasts.error("Could not save settings", error);
            }
        }
        let next = self.queued.take();
        if let Some((seq, settings)) = &next {
            self.in_flight = Some((*seq, settings.clone()));
        }
        (outcome, next)
    }

    /// When the last successful save of the newest value finished, while
    /// the 1.5 s "saved ✓" flash should still show.
    pub fn saved_flash(&self) -> Option<Instant> {
        self.saved_at.filter(|at| at.elapsed() < SAVED_FLASH)
    }

    /// A write is in flight or queued.
    pub fn is_saving(&self) -> bool {
        self.in_flight.is_some() || self.queued.is_some()
    }
}

fn start_write(tasks: &Tasks, seq: u64, settings: Settings) {
    tasks.spawn_op("settings-write", move |c| OpDone::SettingsWritten {
        seq,
        result: c.update_settings(settings),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(auto_record: bool) -> Settings {
        let mut s: Settings = toml::from_str("").expect("default settings");
        s.auto_record = auto_record;
        s
    }

    #[test]
    fn one_in_flight_latest_queued_wins() {
        let mut store = SettingsStore::from_settings(settings(true));
        let mut toasts = Toasts::new();
        let first = store.enqueue(settings(false)).expect("starts at once");
        assert!(store.enqueue(settings(true)).is_none(), "queued behind");
        assert!(
            store.enqueue(settings(false)).is_none(),
            "replaces the queued one"
        );
        assert!(store.is_saving());
        assert!(!store.current().auto_record);

        let (_, next) = store.finish(first.0, Ok(()), &mut toasts);
        let next = next.expect("queued write starts");
        assert_eq!(next.0, 3);
        assert!(!next.1.auto_record);
        assert!(store.saved_flash().is_none(), "not the newest value yet");
        let (_, none) = store.finish(next.0, Ok(()), &mut toasts);
        assert!(none.is_none());
        assert!(!store.is_saving());
        assert!(store.saved_flash().is_some());
        assert!(toasts.is_empty());
    }

    #[test]
    fn failure_reverts_to_the_confirmed_value_and_toasts() {
        let mut store = SettingsStore::from_settings(settings(true));
        let mut toasts = Toasts::new();
        let (seq, _) = store.enqueue(settings(false)).unwrap();
        store.finish(seq, Err("disk full".into()), &mut toasts);
        assert!(store.current().auto_record, "reverted");
        assert!(!toasts.is_empty());
    }

    #[test]
    fn an_older_failure_keeps_the_newer_optimistic_value() {
        let mut store = SettingsStore::from_settings(settings(true));
        let mut toasts = Toasts::new();
        let (seq, _) = store.enqueue(settings(false)).unwrap();
        store.enqueue(settings(false));
        store.finish(seq, Err("busy".into()), &mut toasts);
        assert!(!store.current().auto_record, "newer save still pending");
    }

    #[test]
    fn data_dir_changes_are_reported() {
        let mut store = SettingsStore::from_settings(settings(true));
        let mut toasts = Toasts::new();
        let mut moved = settings(true);
        moved.data_dir = Some("D:\\Witness".into());
        let (seq, _) = store.enqueue(moved.clone()).unwrap();
        let (outcome, _) = store.finish(seq, Ok(()), &mut toasts);
        assert!(outcome.data_dir_saved);
        let (seq, _) = store.enqueue(moved).unwrap();
        let (outcome, _) = store.finish(seq, Ok(()), &mut toasts);
        assert!(!outcome.data_dir_saved);
    }
}
