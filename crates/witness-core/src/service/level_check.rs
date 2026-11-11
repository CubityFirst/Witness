//! A short, live input-level check for the first-run setup: capture one
//! device (a microphone, or an output's loopback) exactly as a recording
//! would, and expose a decaying peak meter the UI can poll every frame.
//!
//! Nothing is written anywhere — the samples only feed the meter. The check
//! stops when its [`LevelCheck`] is dropped, after [`MAX_DURATION`], or when
//! the device fails; stopping never blocks (the threads notice the flag on
//! their next packet or timeout and exit on their own).

use super::AppCore;
use crate::audio_capture::{spawn_capture, CaptureMsg, TrackKind, CAPTURE_QUEUE_CAPACITY};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A forgotten check releases the device on its own after this long.
pub const MAX_DURATION: Duration = Duration::from_secs(30);
/// The meter's floor: quieter than this shows as empty.
const FLOOR_DB: f32 = -60.0;
/// How fast the meter falls back (meter fractions per second), so a word
/// reads as a swing rather than a flicker.
const DECAY_PER_SEC: f32 = 1.2;
/// How long the reader waits for a packet before decaying anyway (loopback
/// sends nothing while the system is silent).
const POLL: Duration = Duration::from_millis(50);

/// State shared between the reader thread and the UI.
#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    /// The meter fraction (0..1) as `f32` bits.
    level: AtomicU32,
    /// The check ran its course (timeout, device error or stop).
    finished: AtomicBool,
    error: Mutex<Option<String>>,
}

/// A running level check. Drop it to stop.
pub struct LevelCheck {
    shared: Arc<Shared>,
}

impl LevelCheck {
    /// The current meter fraction, 0 (≤ −60 dBFS) to 1 (full scale).
    pub fn level(&self) -> f32 {
        f32::from_bits(self.shared.level.load(Ordering::Relaxed))
    }

    /// Why the device could not be captured, if it could not.
    pub fn error(&self) -> Option<String> {
        self.shared
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The check has ended (timed out or failed); the meter reads 0.
    pub fn finished(&self) -> bool {
        self.shared.finished.load(Ordering::Relaxed)
    }
}

impl Drop for LevelCheck {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

impl AppCore {
    /// Start a level check on `device` (an endpoint ID or legacy friendly
    /// name; `None` = the default device) for `kind`. Spawns the capture
    /// thread and a reader; call it from a worker, like every capture start.
    pub fn start_level_check(&self, kind: TrackKind, device: Option<String>) -> LevelCheck {
        let shared = Arc::new(Shared::default());
        let capture_stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::bounded(CAPTURE_QUEUE_CAPACITY);
        // The capture thread owns `tx`; once the reader exits and drops `rx`
        // its next send fails and it stops too.
        let _capture = spawn_capture(kind, capture_stop.clone(), tx, device);
        let reader = shared.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("level-check-{}", kind.name()))
            .spawn(move || {
                read_levels(&reader, &rx);
                capture_stop.store(true, Ordering::Relaxed);
                reader.level.store(0f32.to_bits(), Ordering::Relaxed);
                reader.finished.store(true, Ordering::Relaxed);
            });
        if let Err(error) = spawned {
            *shared.error.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(format!("could not start the level check: {error}"));
            shared.finished.store(true, Ordering::Relaxed);
        }
        LevelCheck { shared }
    }
}

/// The reader loop: peak per packet into a decaying meter, until stopped,
/// timed out, or the device fails.
fn read_levels(shared: &Shared, rx: &crossbeam_channel::Receiver<CaptureMsg>) {
    let started = Instant::now();
    let mut level = 0.0f32;
    let mut last = Instant::now();
    while !shared.stop.load(Ordering::Relaxed) && started.elapsed() < MAX_DURATION {
        let peak = match rx.recv_timeout(POLL) {
            Ok(CaptureMsg::Packet { samples, .. }) => Some(peak(&samples)),
            Ok(CaptureMsg::Fatal { error, .. }) => {
                *shared.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
                return;
            }
            Ok(_) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        };
        let now = Instant::now();
        level = next_level(
            level,
            peak.map(meter_fraction),
            now.duration_since(last).as_secs_f32(),
        );
        last = now;
        shared.level.store(level.to_bits(), Ordering::Relaxed);
    }
}

/// The largest absolute sample (0 for an empty packet).
fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |peak, s| {
        if s.is_finite() {
            peak.max(s.abs())
        } else {
            peak
        }
    })
}

/// A linear peak (0..1) on the meter's dB scale: −60 dBFS → 0, 0 dBFS → 1.
pub fn meter_fraction(peak: f32) -> f32 {
    if peak.is_nan() || peak <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * peak.min(1.0).log10();
    ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

/// The meter after `dt` seconds: jumps up to a louder `new` reading, falls
/// back at [`DECAY_PER_SEC`] otherwise.
fn next_level(current: f32, new: Option<f32>, dt: f32) -> f32 {
    let decayed = (current - DECAY_PER_SEC * dt.max(0.0)).max(0.0);
    new.map_or(decayed, |new| new.max(decayed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_maps_dbfs_onto_zero_to_one() {
        assert_eq!(meter_fraction(0.0), 0.0);
        assert_eq!(meter_fraction(f32::NAN), 0.0);
        assert_eq!(meter_fraction(-0.5), 0.0);
        assert_eq!(meter_fraction(1.0), 1.0);
        assert_eq!(meter_fraction(4.0), 1.0, "clipping reads full");
        // −60 dBFS is the floor, −30 dBFS half way.
        assert!(meter_fraction(0.001) < 1e-4);
        assert!((meter_fraction(0.031_622_8) - 0.5).abs() < 1e-3);
    }

    #[test]
    fn peaks_ignore_sign_and_garbage() {
        assert_eq!(peak(&[]), 0.0);
        assert_eq!(peak(&[0.1, -0.7, 0.3]), 0.7);
        assert_eq!(peak(&[f32::NAN, 0.2, f32::INFINITY]), 0.2);
    }

    #[test]
    fn meter_jumps_up_and_falls_back_slowly() {
        assert_eq!(next_level(0.2, Some(0.8), 0.01), 0.8);
        // A quieter packet does not pull a loud reading straight down.
        let after = next_level(0.8, Some(0.1), 0.1);
        assert!((after - 0.68).abs() < 1e-5, "{after}");
        // Silence (no packets) decays to zero and stays there.
        assert_eq!(next_level(0.5, None, 1.0), 0.0);
        assert_eq!(next_level(0.0, None, -1.0), 0.0);
    }

    #[test]
    fn a_dropped_check_stops_its_reader() {
        let shared = Arc::new(Shared::default());
        let (tx, rx) = crossbeam_channel::bounded(4);
        let reader = shared.clone();
        let thread = std::thread::spawn(move || read_levels(&reader, &rx));
        tx.send(CaptureMsg::Packet {
            kind: TrackKind::Mic,
            samples: vec![0.5; 480],
            qpc_100ns: 0,
        })
        .unwrap();
        let dropped = Instant::now();
        drop(LevelCheck {
            shared: shared.clone(),
        });
        thread.join().unwrap();
        // Stopped by the flag, not by running out MAX_DURATION.
        assert!(
            dropped.elapsed() < MAX_DURATION / 6,
            "{:?}",
            dropped.elapsed()
        );
    }

    #[test]
    fn a_device_failure_is_reported() {
        let shared = Shared::default();
        let (tx, rx) = crossbeam_channel::bounded(4);
        tx.send(CaptureMsg::Fatal {
            kind: TrackKind::Loopback,
            error: "configured device 'Chat' not present".into(),
        })
        .unwrap();
        read_levels(&shared, &rx);
        let check = LevelCheck {
            shared: Arc::new(shared),
        };
        assert_eq!(
            check.error().as_deref(),
            Some("configured device 'Chat' not present")
        );
    }
}
