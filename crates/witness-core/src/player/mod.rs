//! Audio player for archived meetings: a seekable Ogg Opus reader (raw
//! stereo exactly as stored), a WASAPI shared-mode render thread and a WSOLA
//! time-stretcher. Replaces the WebView `<audio>` element.
//!
//! STUB (F0.1): the public API is final (design §6.1); the player unit
//! implements it. Until then `load` reports "Player not implemented yet".

mod archive;
mod output;
mod stretch;

use std::path::PathBuf;
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

/// Handle to the "player" thread. Cheap to call from the UI thread.
pub struct Player {
    shared: Arc<Mutex<PlayerSnapshot>>,
    on_change: Arc<dyn Fn() + Send + Sync>,
}

impl Player {
    /// Start the player; `on_change` runs whenever the snapshot changes
    /// outside a UI call (the UI passes `ctx.request_repaint`).
    pub fn spawn(on_change: Arc<dyn Fn() + Send + Sync>) -> Player {
        Player {
            shared: Arc::new(Mutex::new(PlayerSnapshot::default())),
            on_change,
        }
    }

    fn update(&self, change: impl FnOnce(&mut PlayerSnapshot)) {
        change(&mut self.shared.lock().unwrap());
        (self.on_change)();
    }

    /// Open `path` (no-op if the same path is already loaded).
    pub fn load(&self, path: PathBuf) {
        if self.shared.lock().unwrap().path.as_ref() == Some(&path) {
            return;
        }
        self.update(|s| {
            s.generation += 1;
            s.path = Some(path);
            s.playing = false;
            s.ended = false;
            s.position_ms = 0;
            s.duration_ms = 0;
            s.load = PlayerLoad::Error("Player not implemented yet".into());
        });
    }

    /// Release the file and the audio device.
    pub fn unload(&self) {
        self.update(|s| {
            s.generation += 1;
            s.path = None;
            s.playing = false;
            s.ended = false;
            s.position_ms = 0;
            s.duration_ms = 0;
            s.load = PlayerLoad::Unloaded;
        });
    }

    pub fn play(&self) {}

    pub fn pause(&self) {}

    /// Play after `ended` restarts at 0.
    pub fn toggle(&self) {}

    /// Clamped to [0, duration]; works while paused or before ready (deferred).
    pub fn seek_ms(&self, ms: u64) {
        self.update(|s| s.position_ms = ms.min(s.duration_ms));
    }

    pub fn skip_ms(&self, delta_ms: i64) {
        self.update(|s| {
            s.position_ms = s
                .position_ms
                .saturating_add_signed(delta_ms)
                .min(s.duration_ms)
        });
    }

    /// One of 0.75, 1, 1.25, 1.5, 1.75, 2.
    pub fn set_speed(&self, speed: f32) {
        self.update(|s| s.speed = speed);
    }

    /// Linear gain 0..1.
    pub fn set_volume(&self, volume: f32) {
        self.update(|s| s.volume = volume.clamp(0.0, 1.0));
    }

    /// Current state. [ui-ok]
    pub fn snapshot(&self) -> PlayerSnapshot {
        self.shared.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn stub_reports_not_implemented_after_load() {
        let changes = Arc::new(AtomicUsize::new(0));
        let counter = changes.clone();
        let player = Player::spawn(Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }));
        assert_eq!(player.snapshot().load, PlayerLoad::Unloaded);
        player.load(PathBuf::from("a.opus"));
        let snapshot = player.snapshot();
        assert_eq!(
            snapshot.load,
            PlayerLoad::Error("Player not implemented yet".into())
        );
        assert_eq!(snapshot.generation, 1);
        player.load(PathBuf::from("a.opus"));
        assert_eq!(player.snapshot().generation, 1, "same path is a no-op");
        player.unload();
        assert_eq!(player.snapshot().load, PlayerLoad::Unloaded);
        assert!(changes.load(Ordering::Relaxed) >= 2);
    }
}
