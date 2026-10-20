//! App-owned settings writes (replaces settings.tsx `settingsWriteTail`).
//!
//! The view renders from an optimistic copy; writes are serialised with at
//! most one in flight and one queued (latest wins), and the queue lives in
//! the app, so toggling a checkbox twice and immediately leaving Settings
//! still persists the last value. A failed write reverts the optimistic
//! copy to the last confirmed settings (unless a newer save is pending) and
//! toasts "Could not save settings: …".
//!
//! Writes go through a shared gate that remembers the newest seq persisted,
//! so the exit path can write the latest value synchronously
//! (`flush_for_exit`) without an older in-flight write landing after it.

use crate::bridge::{OpDone, Tasks};
use crate::ui::toasts::Toasts;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use witness_core::settings::Settings;
use witness_core::{AppCore, CoreResult};

/// How long the "saved ✓" flash stays.
pub const SAVED_FLASH: Duration = Duration::from_millis(1500);

pub struct SettingsStore {
    /// What the UI shows (includes unsaved edits).
    current: Settings,
    /// The last value known to be on disk, and the seq that wrote it.
    confirmed: Settings,
    confirmed_seq: u64,
    /// Seq of the write in flight, and the value it is writing.
    in_flight: Option<(u64, Settings)>,
    /// The next value to write once the in-flight one finishes.
    queued: Option<(u64, Settings)>,
    /// Seq of the newest `save`.
    seq: u64,
    saved_at: Option<Instant>,
    /// The newest seq handed to `update_settings`; held while writing so
    /// writes are serialised and an older one never overwrites a newer one.
    gate: Arc<Mutex<u64>>,
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
            confirmed_seq: 0,
            in_flight: None,
            queued: None,
            seq: 0,
            saved_at: None,
            gate: Arc::new(Mutex::new(0)),
        }
    }

    /// Re-seed from the stored settings, as settings.tsx did on every mount
    /// (`get_settings`), so the view shows what `update_settings` actually
    /// persisted (trimmed, de-duplicated junk phrases …). Skipped while a
    /// write is pending: the optimistic value is newer than the store.
    pub fn reload(&mut self, core: &AppCore) {
        if !self.is_saving() {
            let stored = core.settings();
            self.current = stored.clone();
            self.confirmed = stored;
        }
    }

    /// The optimistic settings the UI renders from.
    pub fn current(&self) -> &Settings {
        &self.current
    }

    /// Save `next` (optimistically shown at once).
    pub fn save(&mut self, next: Settings, tasks: &Tasks) {
        if let Some((seq, settings)) = self.enqueue(next) {
            start_write(tasks, &self.gate, seq, settings);
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
            start_write(tasks, &self.gate, seq, settings);
        }
        outcome
    }

    /// Exit paths only (tray Quit, Restart, Install update): write the
    /// newest pending value now, waiting at most `timeout`. A queued write
    /// otherwise starts from `on_written`, which never runs once the
    /// process has exited. The write runs on a helper thread (the UI thread
    /// never does disk I/O); the gate orders it after any in-flight write,
    /// which skips itself if it had not reached the gate yet.
    pub fn flush_for_exit(&mut self, core: &Arc<AppCore>, timeout: Duration) {
        if !self.is_saving() {
            return;
        }
        let (seq, settings) = (self.seq, self.current.clone());
        self.queued = None;
        let (tx, rx) = crossbeam_channel::bounded(1);
        let core = core.clone();
        let gate = self.gate.clone();
        let spawned = std::thread::Builder::new()
            .name("settings-exit-write".into())
            .spawn(move || {
                let result = write_through_gate(&gate, seq, || core.update_settings(settings));
                let _ = tx.send(result);
            });
        if let Err(error) = spawned {
            log::error!("could not save settings before exiting: {error}");
            return;
        }
        match rx.recv_timeout(timeout) {
            Ok(Ok(())) => {
                // If the exit is then abandoned (restart spawn failed), the
                // older in-flight write's completion must not roll this back.
                self.confirmed = settings_normalized(&self.current);
                self.confirmed_seq = seq;
            }
            Ok(Err(error)) => log::error!("could not save settings before exiting: {error}"),
            Err(_) => log::warn!("the settings write did not finish within {timeout:?}"),
        }
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
                // What `update_settings` persisted. `current` keeps the
                // typed value (no fighting a text field mid-edit); `reload`
                // adopts the stored one on the next Settings mount.
                if seq > self.confirmed_seq {
                    let written = settings_normalized(&written);
                    outcome.data_dir_saved = written.data_dir != self.confirmed.data_dir;
                    self.confirmed = written;
                    self.confirmed_seq = seq;
                }
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

fn start_write(tasks: &Tasks, gate: &Arc<Mutex<u64>>, seq: u64, settings: Settings) {
    // A panicking (or unstartable) write still completes `seq` — as a
    // failed write — so `in_flight` is cleared and the queue never stalls.
    let gate = gate.clone();
    tasks.spawn_op_or(
        "settings-write",
        move |c| OpDone::SettingsWritten {
            seq,
            result: write_through_gate(&gate, seq, || c.update_settings(settings)),
        },
        write_failed(seq),
    );
}

/// What `update_settings` stores for `settings` (it normalized successfully
/// if the write succeeded).
fn settings_normalized(settings: &Settings) -> Settings {
    settings.normalized().unwrap_or_else(|_| settings.clone())
}

/// Run `write` for `seq` under the gate, unless a newer seq was already
/// written (then this older value is moot and reported as done).
fn write_through_gate(
    gate: &Mutex<u64>,
    seq: u64,
    write: impl FnOnce() -> CoreResult<()>,
) -> CoreResult<()> {
    // A panicking write poisons the gate; the seq it guards is still valid.
    let mut newest = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if *newest >= seq {
        return Ok(());
    }
    *newest = seq;
    write()
}

/// The `SettingsWritten` reported when write `seq` panicked or never started.
fn write_failed(seq: u64) -> impl FnOnce(String) -> OpDone + Clone + Send + 'static {
    move |error| OpDone::SettingsWritten {
        seq,
        result: Err(error),
    }
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
    fn a_panicking_write_releases_the_queue() {
        let mut store = SettingsStore::from_settings(settings(true));
        let mut toasts = Toasts::new();
        let (seq, _) = store.enqueue(settings(false)).unwrap();
        assert!(store.enqueue(settings(false)).is_none(), "queued behind");
        // What `start_write`'s worker reports when `update_settings` panics.
        let done = crate::bridge::run_guarded(
            "settings-write",
            &(),
            |_| -> OpDone { panic!("poisoned") },
            write_failed(seq),
        );
        let OpDone::SettingsWritten {
            seq: done_seq,
            result,
        } = done
        else {
            panic!("expected SettingsWritten");
        };
        let (_, next) = store.finish(done_seq, result, &mut toasts);
        assert!(next.is_some(), "the queued write still starts");
        assert!(!toasts.is_empty(), "the failure is toasted");
    }

    #[test]
    fn the_gate_skips_writes_older_than_the_newest() {
        let gate = Mutex::new(0);
        let mut ran = Vec::new();
        let mut write = |seq: u64, result: CoreResult<()>| {
            write_through_gate(&gate, seq, || {
                ran.push(seq);
                result
            })
        };
        assert_eq!(write(3, Ok(())), Ok(()));
        // An older in-flight write that reaches the gate late is skipped.
        assert_eq!(write(2, Err("stale".into())), Ok(()));
        assert_eq!(write(4, Err("disk full".into())), Err("disk full".into()));
        assert_eq!(ran, vec![3, 4]);
    }

    #[test]
    fn confirmed_is_the_normalized_value() {
        let mut store = SettingsStore::from_settings(settings(true));
        let mut toasts = Toasts::new();
        let mut typed = settings(true);
        typed.junk_phrases = vec!["You".into(), " you ".into(), String::new()];
        typed.data_dir = Some(" D:\\Witness ".into());
        let (seq, _) = store.enqueue(typed).unwrap();
        let (outcome, _) = store.finish(seq, Ok(()), &mut toasts);
        assert!(outcome.data_dir_saved);
        assert_eq!(store.confirmed.junk_phrases, vec!["You".to_owned()]);
        assert_eq!(store.confirmed.data_dir.as_deref(), Some("D:\\Witness"));
        assert_eq!(store.current().junk_phrases.len(), 3, "typed value kept");
        // Only whitespace differs: not a data_dir change.
        let mut same = settings(true);
        same.data_dir = Some("D:\\Witness  ".into());
        let (seq, _) = store.enqueue(same).unwrap();
        let (outcome, _) = store.finish(seq, Ok(()), &mut toasts);
        assert!(!outcome.data_dir_saved);
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
