//! Audio player for archived meetings: a seekable Ogg Opus reader (raw
//! stereo exactly as stored), a WASAPI shared-mode render thread and a WSOLA
//! time-stretcher. Replaces the WebView `<audio>` element (design §6).
//!
//! ```text
//! UI thread                                  "player" thread (owns the !Send WASAPI objects)
//!   Player { tx, shared } ── Cmd ──────────►   drain commands
//!   snapshot(): atomics + a tiny mutex          wait_for_event(20 ms)
//!   (UI repaints itself ~30 Hz while playing)   OpusArchiveReader → Wsola(speed) → ×volume
//!                         ◄── Shared ─────────   → write_to_device; publish position/ended
//! ```
//!
//! The handle mirrors `HTMLAudioElement` semantics the old view relied on:
//! state the UI sets (seek target, speed, volume, play intent) is visible in
//! the very next snapshot, and the thread only ever publishes positions for
//! the latest seek, so a slider never snaps back to a stale value.

pub(crate) mod archive;
mod output;
mod stretch;

pub use archive::OpusArchiveReader;

use crossbeam_channel::Sender;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerLoad {
    Unloaded,
    Loading,
    Ready,
    Error(String),
}

/// Everything the transcript view draws, read every frame while playing.
#[derive(Debug, Clone)]
pub struct PlayerSnapshot {
    pub load: PlayerLoad,
    pub path: Option<PathBuf>,
    pub playing: bool,
    pub ended: bool,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub speed: f32,
    /// Linear gain 0..1.
    pub volume: f32,
    /// Bumped on every load/unload so the UI can drop stale state.
    pub generation: u64,
}

impl Default for PlayerSnapshot {
    fn default() -> Self {
        PlayerSnapshot {
            load: PlayerLoad::Unloaded,
            path: None,
            playing: false,
            ended: false,
            position_ms: 0,
            duration_ms: 0,
            speed: 1.0,
            volume: 1.0,
            generation: 0,
        }
    }
}

/// The playback speeds the UI offers (HTML `playbackRate` menu).
pub const SPEEDS: [f32; 6] = [0.75, 1.0, 1.25, 1.5, 1.75, 2.0];

/// Position word layout: the low 40 bits are milliseconds (~35 years), the
/// high 24 bits the seek sequence the value belongs to. Packing both into one
/// atomic lets the render thread publish "position for seek N" with a single
/// compare-exchange that fails once the UI has issued seek N+1.
const POS_BITS: u32 = 40;
const POS_MASK: u64 = (1 << POS_BITS) - 1;
const SEQ_MASK: u64 = (1 << (64 - POS_BITS)) - 1;

fn pack_position(seq: u64, ms: u64) -> u64 {
    ((seq & SEQ_MASK) << POS_BITS) | ms.min(POS_MASK)
}

fn unpack_position(word: u64) -> (u64, u64) {
    (word >> POS_BITS, word & POS_MASK)
}

/// The non-numeric part of the snapshot. Only ever held to copy a few
/// fields — never across I/O — so a UI read is effectively wait-free.
struct Meta {
    load: PlayerLoad,
    path: Option<PathBuf>,
}

/// State shared between the handle (UI thread) and the player thread.
pub(crate) struct Shared {
    meta: Mutex<Meta>,
    generation: AtomicU64,
    playing: AtomicBool,
    ended: AtomicBool,
    /// Packed `(seek seq, position ms)`, see [`pack_position`].
    position: AtomicU64,
    /// Last seek sequence handed out by the UI (the thread echoes it back).
    seek_seq: AtomicU64,
    duration_ms: AtomicU64,
    speed_bits: AtomicU32,
    volume_bits: AtomicU32,
}

impl Shared {
    fn new() -> Self {
        Shared {
            meta: Mutex::new(Meta {
                load: PlayerLoad::Unloaded,
                path: None,
            }),
            generation: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            position: AtomicU64::new(0),
            seek_seq: AtomicU64::new(0),
            duration_ms: AtomicU64::new(0),
            speed_bits: AtomicU32::new(1.0f32.to_bits()),
            volume_bits: AtomicU32::new(1.0f32.to_bits()),
        }
    }

    fn meta(&self) -> std::sync::MutexGuard<'_, Meta> {
        // A poisoned lock only means a panic elsewhere; the data is plain.
        self.meta.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn speed(&self) -> f32 {
        f32::from_bits(self.speed_bits.load(Ordering::Relaxed))
    }

    pub(crate) fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
    }

    pub(crate) fn duration_ms(&self) -> u64 {
        self.duration_ms.load(Ordering::Relaxed)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Thread side: publish the position for seek `seq` of generation
    /// `generation`; dropped when the UI has moved on to a newer seek/load.
    pub(crate) fn publish_position(&self, generation: u64, seq: u64, ms: u64) {
        if self.generation() != generation {
            return;
        }
        let mut cur = self.position.load(Ordering::Acquire);
        loop {
            if unpack_position(cur).0 != seq & SEQ_MASK {
                return;
            }
            let new = pack_position(seq, ms);
            if new == cur {
                return;
            }
            match self
                .position
                .compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Thread side: the file opened (or failed) for `generation`.
    pub(crate) fn publish_load(&self, generation: u64, load: PlayerLoad, duration_ms: u64) {
        let mut meta = self.meta();
        if self.generation() != generation {
            return;
        }
        if matches!(load, PlayerLoad::Error(_)) {
            self.playing.store(false, Ordering::Release);
        }
        self.duration_ms.store(duration_ms, Ordering::Release);
        meta.load = load;
    }

    /// Thread side: playback stopped on its own (end of file or a device
    /// error). `ended` mirrors `HTMLMediaElement.ended`.
    pub(crate) fn publish_stopped(&self, generation: u64, ended: bool) {
        let _meta = self.meta();
        if self.generation() != generation {
            return;
        }
        self.playing.store(false, Ordering::Release);
        self.ended.store(ended, Ordering::Release);
    }
}

/// Commands to the player thread, processed strictly in order.
#[derive(Debug)]
pub(crate) enum Cmd {
    Load {
        path: PathBuf,
        generation: u64,
        seq: u64,
    },
    Unload {
        generation: u64,
        seq: u64,
    },
    Play,
    Pause,
    /// Seek to `ms` (clamped by the thread once the duration is known).
    Seek {
        ms: u64,
        seq: u64,
    },
    /// Speed or volume changed; the thread re-reads them from `Shared`.
    Wake,
}

/// Handle to the "player" thread. Every method is cheap and non-blocking,
/// so the UI may call them from `ui()`.
pub struct Player {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    on_change: Arc<dyn Fn() + Send + Sync>,
}

impl Player {
    /// Start the player; `on_change` runs whenever the snapshot changes
    /// outside a UI call (the UI passes `ctx.request_repaint`). Position
    /// updates during playback do *not* call it: the view polls at ~30 Hz.
    pub fn spawn(on_change: Arc<dyn Fn() + Send + Sync>) -> Player {
        Self::spawn_with(output::default_backend, on_change)
    }

    /// Start with a specific output backend, built on the player thread
    /// (WASAPI objects are `!Send`). Tests pass a fake device.
    pub(crate) fn spawn_with<B, F>(
        make_backend: F,
        on_change: Arc<dyn Fn() + Send + Sync>,
    ) -> Player
    where
        B: output::Backend + 'static,
        F: FnOnce() -> B + Send + 'static,
    {
        let (tx, rx) = crossbeam_channel::unbounded();
        let shared = Arc::new(Shared::new());
        let thread_shared = shared.clone();
        let thread_change = on_change.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("player".into())
            .spawn(move || output::run(make_backend, rx, thread_shared, thread_change))
        {
            log::error!("could not start the player thread: {e}");
        }
        Player {
            tx,
            shared,
            on_change,
        }
    }

    fn send(&self, cmd: Cmd) {
        if self.tx.send(cmd).is_err() {
            log::warn!("player thread is gone; command dropped");
        }
    }

    fn next_seq(&self) -> u64 {
        (self.shared.seek_seq.fetch_add(1, Ordering::AcqRel) + 1) & SEQ_MASK
    }

    /// Reset everything for a new generation; returns (generation, seq).
    fn reset(&self, meta: &mut Meta, load: PlayerLoad, path: Option<PathBuf>) -> (u64, u64) {
        let generation = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let seq = self.next_seq();
        meta.load = load;
        meta.path = path;
        self.shared.playing.store(false, Ordering::Release);
        self.shared.ended.store(false, Ordering::Release);
        self.shared.duration_ms.store(0, Ordering::Release);
        self.shared
            .position
            .store(pack_position(seq, 0), Ordering::Release);
        (generation, seq)
    }

    /// Open `path` (no-op if the same path is already loaded). Seeks and
    /// `play` issued before it is ready apply once it is.
    pub fn load(&self, path: PathBuf) {
        let (generation, seq) = {
            let mut meta = self.shared.meta();
            if meta.path.as_ref() == Some(&path) {
                return;
            }
            self.reset(&mut meta, PlayerLoad::Loading, Some(path.clone()))
        };
        self.send(Cmd::Load {
            path,
            generation,
            seq,
        });
        (self.on_change)();
    }

    /// Release the file and the audio device.
    pub fn unload(&self) {
        let (generation, seq) = {
            let mut meta = self.shared.meta();
            if meta.path.is_none() && meta.load == PlayerLoad::Unloaded {
                return;
            }
            self.reset(&mut meta, PlayerLoad::Unloaded, None)
        };
        self.send(Cmd::Unload { generation, seq });
        (self.on_change)();
    }

    /// Start (or resume) playback; after `ended` it restarts at 0.
    pub fn play(&self) {
        if self.shared.ended.load(Ordering::Acquire) {
            self.seek_ms(0);
        }
        self.shared.playing.store(true, Ordering::Release);
        self.send(Cmd::Play);
        (self.on_change)();
    }

    pub fn pause(&self) {
        self.shared.playing.store(false, Ordering::Release);
        self.send(Cmd::Pause);
        (self.on_change)();
    }

    /// Play/pause; play after `ended` restarts at 0.
    pub fn toggle(&self) {
        if self.shared.playing.load(Ordering::Acquire) {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Clamped to [0, duration]; works while paused or before ready
    /// (deferred: the thread clamps once the duration is known).
    pub fn seek_ms(&self, ms: u64) {
        let ready = self.shared.meta().load == PlayerLoad::Ready;
        let ms = if ready {
            ms.min(self.shared.duration_ms())
        } else {
            ms
        };
        let seq = self.next_seq();
        self.shared.ended.store(false, Ordering::Release);
        self.shared
            .position
            .store(pack_position(seq, ms), Ordering::Release);
        self.send(Cmd::Seek { ms, seq });
        (self.on_change)();
    }

    /// Relative seek from the current position (±10 s buttons, ←/→ keys).
    pub fn skip_ms(&self, delta_ms: i64) {
        let (_, pos) = unpack_position(self.shared.position.load(Ordering::Acquire));
        self.seek_ms(pos.saturating_add_signed(delta_ms));
    }

    /// One of [`SPEEDS`]; other values are clamped to 0.75–2.
    pub fn set_speed(&self, speed: f32) {
        let speed = if speed.is_finite() {
            speed.clamp(0.75, 2.0)
        } else {
            1.0
        };
        self.shared
            .speed_bits
            .store(speed.to_bits(), Ordering::Relaxed);
        self.send(Cmd::Wake);
        (self.on_change)();
    }

    /// Linear gain 0..1 (software gain, like HTML `volume`; the Windows
    /// mixer slider stays independent).
    pub fn set_volume(&self, volume: f32) {
        let volume = if volume.is_finite() {
            volume.clamp(0.0, 1.0)
        } else {
            1.0
        };
        self.shared
            .volume_bits
            .store(volume.to_bits(), Ordering::Relaxed);
        self.send(Cmd::Wake);
        (self.on_change)();
    }

    /// Current state. [ui-ok]
    pub fn snapshot(&self) -> PlayerSnapshot {
        let (load, path, generation) = {
            let meta = self.shared.meta();
            (
                meta.load.clone(),
                meta.path.clone(),
                self.shared.generation(),
            )
        };
        let (_, position_ms) = unpack_position(self.shared.position.load(Ordering::Acquire));
        let duration_ms = self.shared.duration_ms();
        PlayerSnapshot {
            load,
            path,
            playing: self.shared.playing.load(Ordering::Acquire),
            ended: self.shared.ended.load(Ordering::Acquire),
            position_ms: if duration_ms > 0 {
                position_ms.min(duration_ms)
            } else {
                position_ms
            },
            duration_ms,
            speed: self.shared.speed(),
            volume: self.shared.volume(),
            generation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::archive::tests::{make_archive, scratch};
    use super::output::fake::{FakeBackend, FakeDevice};
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    fn wait_until(player: &Player, what: &str, cond: impl Fn(&PlayerSnapshot) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let snap = player.snapshot();
            if cond(&snap) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {snap:?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn position_word_round_trips_and_wraps() {
        assert_eq!(unpack_position(pack_position(7, 123_456)), (7, 123_456));
        let (seq, ms) = unpack_position(pack_position(SEQ_MASK + 3, 5));
        assert_eq!((seq, ms), (2, 5));
    }

    #[test]
    fn stale_positions_never_overwrite_a_newer_seek() {
        let shared = Shared::new();
        shared.generation.store(1, Ordering::Release);
        shared
            .position
            .store(pack_position(4, 0), Ordering::Release);
        shared.publish_position(1, 4, 500);
        assert_eq!(
            unpack_position(shared.position.load(Ordering::Acquire)).1,
            500
        );
        // UI seeks (seq 5) before the thread has processed it.
        shared
            .position
            .store(pack_position(5, 9_000), Ordering::Release);
        shared.publish_position(1, 4, 520); // stale: dropped
        assert_eq!(
            unpack_position(shared.position.load(Ordering::Acquire)).1,
            9_000
        );
        shared.publish_position(0, 5, 1); // stale generation: dropped
        assert_eq!(
            unpack_position(shared.position.load(Ordering::Acquire)).1,
            9_000
        );
    }

    #[test]
    fn handle_drives_the_thread_end_to_end() {
        let dir = scratch("handle");
        let path = make_archive(&dir, "h", 3 * 48_000);
        let device = FakeDevice::new(960);
        device.state().realtime = true;
        let fake = device.clone();
        let changes = Arc::new(AtomicUsize::new(0));
        let counter = changes.clone();
        let player = Player::spawn_with(
            move || FakeBackend::new(fake),
            Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
        );
        assert_eq!(player.snapshot().load, PlayerLoad::Unloaded);

        // Deep link: seek before the file is ready; it is applied on load
        // and reported immediately.
        player.load(path.clone());
        player.seek_ms(1_500);
        assert_eq!(player.snapshot().position_ms, 1_500);
        wait_until(&player, "ready", |s| s.load == PlayerLoad::Ready);
        let snap = player.snapshot();
        assert_eq!(snap.duration_ms, 3_000);
        assert_eq!(snap.position_ms, 1_500);
        assert_eq!(snap.generation, 1);
        assert_eq!(device.opens(), 0, "no device until play");

        // Same path again is a no-op.
        player.load(path.clone());
        assert_eq!(player.snapshot().generation, 1);

        // Clamped seeks once ready.
        player.seek_ms(99_999);
        assert_eq!(player.snapshot().position_ms, 3_000);
        player.skip_ms(-10_000_000);
        assert_eq!(player.snapshot().position_ms, 0);

        // Play to the end: the fake device consumes in real time-ish.
        player.set_speed(2.0);
        player.set_volume(0.5);
        player.seek_ms(2_000);
        player.play();
        assert!(player.snapshot().playing);
        wait_until(&player, "ended", |s| s.ended);
        let snap = player.snapshot();
        assert!(!snap.playing);
        assert_eq!(snap.position_ms, 3_000);
        assert_eq!(device.opens(), 1);
        let peak = device.peak();
        assert!(
            peak > 0.02 && peak < 0.25,
            "volume not applied: peak {peak}"
        );

        // Play after ended restarts at 0.
        player.toggle();
        let snap = player.snapshot();
        assert!(snap.playing && !snap.ended);
        assert!(
            snap.position_ms < 500,
            "restart at 0, got {}",
            snap.position_ms
        );
        player.pause();
        assert!(!player.snapshot().playing);

        player.unload();
        let snap = player.snapshot();
        assert_eq!(snap.load, PlayerLoad::Unloaded);
        assert_eq!(snap.generation, 2);
        assert!(changes.load(Ordering::Relaxed) >= 4);

        // A broken file reports an error the view can show.
        player.load(dir.join("missing.opus"));
        wait_until(&player, "error", |s| matches!(s.load, PlayerLoad::Error(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
