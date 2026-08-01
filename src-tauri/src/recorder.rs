//! Recording session: two capture threads (mic + loopback) feed one writer
//! thread that resamples to 48 kHz mono i16 and streams two WAVs into
//! `data_dir/rec-tmp/`. Tracks are aligned by QPC timestamps: the later
//! starter gets silence pre-padding, and any gap/drift > 250 ms (device
//! switch, silent loopback stretches) is filled with silence.

use crate::audio_capture::{spawn_capture, CaptureMsg, TrackKind, CAPTURE_QUEUE_CAPACITY};
use crate::live_transcribe::LiveMsg;
use crate::resample::StreamResampler;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const TARGET_RATE: u32 = 48_000;
/// Gap/drift beyond this is corrected with inserted silence.
const GAP_THRESHOLD: u64 = (TARGET_RATE as u64) / 4; // 250 ms
const LEVEL_INTERVAL: Duration = Duration::from_millis(500);
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Crash-recovery sidecar written next to the WAVs.
#[derive(Debug, Serialize, Deserialize)]
pub struct RecMeta {
    pub meeting_id: i64,
    pub started_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TrackHealth {
    pub connected: bool,
    pub device_loss_count: u64,
    pub capture_overflow_count: u64,
    pub capture_dropped_ms: u64,
    pub live_dropped_ms: u64,
    pub live_disconnected: bool,
    pub fatal_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RecordingHealth {
    pub mic: TrackHealth,
    pub loopback: TrackHealth,
    pub writer_error: Option<String>,
}

impl RecordingHealth {
    fn track_mut(&mut self, kind: TrackKind) -> &mut TrackHealth {
        match kind {
            TrackKind::Mic => &mut self.mic,
            TrackKind::Loopback => &mut self.loopback,
        }
    }
}

pub struct RecordingStats {
    pub duration_ms: u64,
    pub health: RecordingHealth,
}

pub struct RecorderHandle {
    pub meeting_id: i64,
    pub started_at: chrono::DateTime<chrono::Local>,
    stop: Arc<AtomicBool>,
    writer: Option<JoinHandle<Result<RecordingStats>>>,
    captures: Vec<JoinHandle<()>>,
}

impl RecorderHandle {
    /// Signals all threads, waits for the WAVs to be finalized.
    pub fn stop(mut self) -> Result<RecordingStats> {
        self.stop.store(true, Ordering::Relaxed);
        for h in self.captures.drain(..) {
            let _ = h.join();
        }
        let stats = self
            .writer
            .take()
            .expect("writer joined twice")
            .join()
            .map_err(|_| anyhow::anyhow!("recording writer thread panicked"))??;
        log::debug!("recording stopped with health: {:?}", stats.health);
        Ok(stats)
    }
}

pub fn wav_paths(rec_dir: &Path, meeting_id: i64) -> (PathBuf, PathBuf, PathBuf) {
    (
        rec_dir.join(format!("{meeting_id}-mic.wav")),
        rec_dir.join(format!("{meeting_id}-loopback.wav")),
        rec_dir.join(format!("{meeting_id}.meta.toml")),
    )
}

struct Track {
    kind: TrackKind,
    writer: hound::WavWriter<BufWriter<File>>,
    resampler: Option<StreamResampler>,
    written: u64, // 48 kHz samples written so far
    sq_sum: f64,  // running sum of squares for the level meter
    sq_n: u64,
    /// Tee to the live-transcription worker (dropped when it's disabled).
    live: Option<crossbeam_channel::Sender<LiveMsg>>,
    /// A full live queue is represented by a compact timeline gap instead of
    /// letting live inference block the lossless WAV writer.
    live_gap_48k: u64,
    live_dropped_48k: u64,
    live_disconnected: bool,
}

impl Track {
    fn open(
        kind: TrackKind,
        path: &Path,
        live: Option<crossbeam_channel::Sender<LiveMsg>>,
    ) -> Result<Track> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: TARGET_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(path, spec)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(Track {
            kind,
            writer,
            resampler: None,
            written: 0,
            sq_sum: 0.0,
            sq_n: 0,
            live,
            live_gap_48k: 0,
            live_dropped_48k: 0,
            live_disconnected: false,
        })
    }

    fn send_live(&mut self, msg: LiveMsg, timeline_samples: u64) {
        let Some(live) = self.live.as_ref() else {
            return;
        };

        if self.live_gap_48k > 0 {
            match live.try_send(LiveMsg::Silence {
                kind: self.kind,
                count_48k: self.live_gap_48k,
            }) {
                Ok(()) => self.live_gap_48k = 0,
                Err(crossbeam_channel::TrySendError::Full(_)) => {
                    self.live_gap_48k += timeline_samples;
                    self.live_dropped_48k += timeline_samples;
                    return;
                }
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                    log::warn!(
                        "{} live-transcription worker disconnected",
                        self.kind.name()
                    );
                    self.live = None;
                    self.live_gap_48k = 0;
                    self.live_disconnected = true;
                    return;
                }
            }
        }

        match live.try_send(msg) {
            Ok(()) => {}
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                self.live_gap_48k += timeline_samples;
                self.live_dropped_48k += timeline_samples;
                if self.live_dropped_48k == timeline_samples
                    || self
                        .live_dropped_48k
                        .is_multiple_of(TARGET_RATE as u64 * 10)
                {
                    log::warn!(
                        "{} live-transcription queue full; {:.1}s omitted",
                        self.kind.name(),
                        self.live_dropped_48k as f64 / TARGET_RATE as f64
                    );
                }
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                log::warn!(
                    "{} live-transcription worker disconnected",
                    self.kind.name()
                );
                self.live = None;
                self.live_disconnected = true;
            }
        }
    }

    fn write_samples(&mut self, samples: &[f32]) -> Result<()> {
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            self.writer.write_sample(v)?;
            self.sq_sum += (s as f64) * (s as f64);
        }
        self.sq_n += samples.len() as u64;
        self.written += samples.len() as u64;
        if !samples.is_empty() {
            self.send_live(
                LiveMsg::Audio {
                    kind: self.kind,
                    samples_48k: samples.to_vec(),
                },
                samples.len() as u64,
            );
        }
        Ok(())
    }

    fn write_silence(&mut self, count: u64) -> Result<()> {
        for _ in 0..count {
            self.writer.write_sample(0i16)?;
        }
        self.written += count;
        if count > 0 {
            self.send_live(
                LiveMsg::Silence {
                    kind: self.kind,
                    count_48k: count,
                },
                count,
            );
        }
        Ok(())
    }

    fn take_rms(&mut self) -> f32 {
        let rms = if self.sq_n == 0 {
            0.0
        } else {
            (self.sq_sum / self.sq_n as f64).sqrt() as f32
        };
        self.sq_sum = 0.0;
        self.sq_n = 0;
        rms
    }
}

fn sync_live_health(health: &mut RecordingHealth, mic: &Track, lop: &Track) {
    for (track, track_health) in [(mic, &mut health.mic), (lop, &mut health.loopback)] {
        track_health.live_dropped_ms = track.live_dropped_48k * 1000 / TARGET_RATE as u64;
        track_health.live_disconnected = track.live_disconnected;
    }
}

fn emit_health_if_changed(
    health: &RecordingHealth,
    last_health: &mut RecordingHealth,
    on_health: &impl Fn(RecordingHealth),
) {
    if health != last_health {
        on_health(health.clone());
        *last_health = health.clone();
    }
}

/// Starts a recording and reports capture/runtime health changes. This keeps
/// the application event layer informed about device loss, queue overflow,
/// live-caption loss, and writer failure. Device identifiers or legacy
/// friendly names select endpoints (None = system default).
#[allow(clippy::too_many_arguments)]
pub fn start_with_health(
    rec_dir: &Path,
    meeting_id: i64,
    mic_device: Option<String>,
    loopback_device: Option<String>,
    live: Option<crossbeam_channel::Sender<LiveMsg>>,
    on_level: impl Fn(f32, f32, u64) + Send + 'static,
    on_health: impl Fn(RecordingHealth) + Send + 'static,
) -> Result<RecorderHandle> {
    std::fs::create_dir_all(rec_dir).with_context(|| format!("creating {}", rec_dir.display()))?;
    let (mic_path, loop_path, meta_path) = wav_paths(rec_dir, meeting_id);
    let started_at = chrono::Local::now();

    let meta = RecMeta {
        meeting_id,
        started_at: started_at.to_rfc3339(),
    };
    std::fs::write(&meta_path, toml::to_string(&meta)?)
        .with_context(|| format!("writing {}", meta_path.display()))?;

    let mut mic = Track::open(TrackKind::Mic, &mic_path, live.clone())?;
    let mut lop = Track::open(TrackKind::Loopback, &loop_path, live)?;

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = crossbeam_channel::bounded::<CaptureMsg>(CAPTURE_QUEUE_CAPACITY);
    let captures = vec![
        spawn_capture(TrackKind::Mic, stop.clone(), tx.clone(), mic_device),
        spawn_capture(
            TrackKind::Loopback,
            stop.clone(),
            tx.clone(),
            loopback_device,
        ),
    ];
    drop(tx); // writer sees Disconnected once both capture threads exit

    let writer_stop = stop.clone();
    let writer = std::thread::Builder::new()
        .name("rec-writer".into())
        .spawn(move || -> Result<RecordingStats> {
            let mut health = RecordingHealth::default();
            let mut last_health = health.clone();
            let writer_run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || -> Result<RecordingStats> {
                    // QPC of the session origin = first packet seen on either track.
                    let mut session_start: Option<u64> = None;
                    let mut last_level = Instant::now();
                    let mut last_flush = Instant::now();

                    loop {
                        match rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(CaptureMsg::Format { kind, sample_rate }) => {
                                let idx = (kind == TrackKind::Loopback) as usize;
                                let track = if idx == 0 { &mut mic } else { &mut lop };
                                // Flush the previous device's resampler tail before
                                // switching rates (device change mid-recording).
                                if let Some(mut r) = track.resampler.take() {
                                    let tail = r.finish()?;
                                    track.write_samples(&tail)?;
                                }
                                track.resampler =
                                    Some(StreamResampler::new(sample_rate, TARGET_RATE)?);
                                let track_health = health.track_mut(kind);
                                track_health.connected = true;
                                track_health.fatal_error = None;
                            }
                            Ok(CaptureMsg::Packet {
                                kind,
                                samples,
                                qpc_100ns,
                            }) => {
                                let track = if kind == TrackKind::Mic {
                                    &mut mic
                                } else {
                                    &mut lop
                                };
                                if track.resampler.is_none() {
                                    // Packet before Format shouldn't happen; be safe.
                                    track.resampler =
                                        Some(StreamResampler::new(TARGET_RATE, TARGET_RATE)?);
                                }
                                let mut skip = 0usize;
                                if qpc_100ns != 0 {
                                    let origin = *session_start.get_or_insert(qpc_100ns);
                                    let expected = qpc_100ns
                                        .saturating_sub(origin)
                                        .saturating_mul(TARGET_RATE as u64)
                                        / 10_000_000;
                                    // Pre-pad late starters / fill gaps (silent
                                    // loopback stretches deliver no packets at all).
                                    if expected > track.written + GAP_THRESHOLD {
                                        track.write_silence(expected - track.written)?;
                                    } else if track.written > expected + GAP_THRESHOLD {
                                        // Device clock running fast: drop the excess
                                        // so tracks can't drift apart unboundedly.
                                        // (Skip counted in 48 kHz samples but applied
                                        // at device rate — coarse on purpose; the
                                        // threshold keeps it from thrashing.)
                                        skip = ((track.written - expected - GAP_THRESHOLD / 2)
                                            as usize)
                                            .min(samples.len());
                                    }
                                }
                                let out =
                                    track.resampler.as_mut().unwrap().push(&samples[skip..])?;
                                track.write_samples(&out)?;
                            }
                            Ok(CaptureMsg::Overflow {
                                kind,
                                packets,
                                samples,
                                sample_rate,
                            }) => {
                                let silence = samples.saturating_mul(TARGET_RATE as u64)
                                    / sample_rate.max(1) as u64;
                                let track = if kind == TrackKind::Mic {
                                    &mut mic
                                } else {
                                    &mut lop
                                };
                                track.write_silence(silence)?;
                                let track_health = health.track_mut(kind);
                                track_health.capture_overflow_count += packets;
                                track_health.capture_dropped_ms +=
                                    samples.saturating_mul(1000) / sample_rate.max(1) as u64;
                                log::warn!(
                                    "{} capture overflow: {} packets / {} ms replaced with silence",
                                    kind.name(),
                                    packets,
                                    samples.saturating_mul(1000) / sample_rate.max(1) as u64
                                );
                            }
                            Ok(CaptureMsg::DeviceLost { kind }) => {
                                log::warn!("{} device lost; waiting for reopen", kind.name());
                                let track_health = health.track_mut(kind);
                                track_health.connected = false;
                                track_health.device_loss_count += 1;
                            }
                            Ok(CaptureMsg::Fatal { kind, error }) => {
                                // Keep recording the surviving track rather than
                                // aborting the whole meeting.
                                log::error!(
                                    "{} capture failed permanently: {}",
                                    kind.name(),
                                    error
                                );
                                let track_health = health.track_mut(kind);
                                track_health.connected = false;
                                track_health.fatal_error = Some(error);
                            }
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        }

                        if last_level.elapsed() >= LEVEL_INTERVAL {
                            last_level = Instant::now();
                            let elapsed_ms =
                                mic.written.max(lop.written) * 1000 / TARGET_RATE as u64;
                            on_level(mic.take_rms(), lop.take_rms(), elapsed_ms);
                            sync_live_health(&mut health, &mic, &lop);
                            emit_health_if_changed(&health, &mut last_health, &on_health);
                        }
                        if last_flush.elapsed() >= FLUSH_INTERVAL {
                            last_flush = Instant::now();
                            mic.writer.flush().context("flushing microphone WAV")?;
                            lop.writer.flush().context("flushing loopback WAV")?;
                        }
                    }

                    // Finalize: flush resampler tails, pad to a common length.
                    for track in [&mut mic, &mut lop] {
                        if let Some(mut r) = track.resampler.take() {
                            let tail = r.finish()?;
                            track.write_samples(&tail)?;
                        }
                    }
                    let target = mic.written.max(lop.written);
                    for track in [&mut mic, &mut lop] {
                        if track.written < target {
                            track.write_silence(target - track.written)?;
                        }
                    }
                    let duration_ms = target * 1000 / TARGET_RATE as u64;
                    sync_live_health(&mut health, &mic, &lop);
                    emit_health_if_changed(&health, &mut last_health, &on_health);
                    mic.writer.finalize()?;
                    lop.writer.finalize()?;
                    Ok(RecordingStats {
                        duration_ms,
                        health: health.clone(),
                    })
                },
            ));
            let writer_result = match writer_run {
                Ok(result) => result,
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<&str>()
                        .map(|message| (*message).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string());
                    Err(anyhow::anyhow!("recording writer panicked: {message}"))
                }
            };

            if let Err(error) = &writer_result {
                health.writer_error = Some(format!("{error:#}"));
                on_health(health.clone());
                writer_stop.store(true, Ordering::Relaxed);
                log::error!("recording writer failed: {error:#}");
            }
            writer_result
        })
        .expect("spawn rec-writer thread");

    Ok(RecorderHandle {
        meeting_id,
        started_at,
        stop,
        writer: Some(writer),
        captures,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_live_queue_becomes_an_explicit_timeline_gap() {
        let path = std::env::temp_dir().join(format!(
            "witness-live-backpressure-{}-{:?}.wav",
            std::process::id(),
            std::thread::current().id()
        ));
        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.send(LiveMsg::Silence {
            kind: TrackKind::Mic,
            count_48k: 1,
        })
        .unwrap();
        let mut track = Track::open(TrackKind::Mic, &path, Some(tx)).unwrap();

        track.send_live(
            LiveMsg::Audio {
                kind: TrackKind::Mic,
                samples_48k: vec![0.0; 480],
            },
            480,
        );
        assert_eq!(track.live_gap_48k, 480);
        assert_eq!(track.live_dropped_48k, 480);

        let _ = rx.recv().unwrap();
        track.send_live(
            LiveMsg::Audio {
                kind: TrackKind::Mic,
                samples_48k: vec![0.0; 240],
            },
            240,
        );
        match rx.recv().unwrap() {
            LiveMsg::Silence { kind, count_48k } => {
                assert_eq!(kind, TrackKind::Mic);
                assert_eq!(count_48k, 480);
            }
            LiveMsg::Audio { .. } => panic!("expected compacted timeline gap"),
        }
        assert_eq!(track.live_gap_48k, 240);
        assert_eq!(track.live_dropped_48k, 720);

        drop(track);
        let _ = std::fs::remove_file(path);
    }

    /// Real-device smoke test (needs a default mic + render device):
    /// `cargo test capture_smoke -- --ignored --nocapture`
    /// Set WITNESS_TEST_MIC / WITNESS_TEST_LOOPBACK to exercise named devices.
    #[test]
    #[ignore]
    fn capture_smoke() {
        let dir = std::env::temp_dir().join(format!("witness-rec-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mic_dev = std::env::var("WITNESS_TEST_MIC").ok();
        let loop_dev = std::env::var("WITNESS_TEST_LOOPBACK").ok();
        let handle = start_with_health(
            &dir,
            1,
            mic_dev,
            loop_dev,
            None,
            |mic, lop, ms| {
                println!("level mic={mic:.4} loop={lop:.4} at {ms} ms");
            },
            |_| {},
        )
        .unwrap();
        std::thread::sleep(Duration::from_secs(4));
        let stats = handle.stop().unwrap();
        println!("recorded {} ms", stats.duration_ms);
        assert!(
            stats.duration_ms >= 2_500,
            "too short: {} ms",
            stats.duration_ms
        );

        let (mic, lop, meta) = wav_paths(&dir, 1);
        assert!(meta.exists());
        for path in [mic, lop] {
            let reader = hound::WavReader::open(&path).unwrap();
            let spec = reader.spec();
            assert_eq!(spec.sample_rate, TARGET_RATE);
            assert_eq!(spec.channels, 1);
            assert!(
                reader.duration() as u64 * 1000 / TARGET_RATE as u64 >= 2_500,
                "{} too short",
                path.display()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
