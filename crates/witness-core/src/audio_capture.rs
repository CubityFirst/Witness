//! WASAPI capture threads: one for the default microphone, one for
//! device-level loopback on the default render device (per-process loopback
//! on ms-teams.exe is bugged — records silence — so we don't use it).
//!
//! Each thread initializes COM (MTA) for itself, opens its device in shared
//! event-driven mode with autoconvert to f32, downmixes to mono and ships
//! packets (with QPC timestamps) over a crossbeam channel. No file I/O here.
//! On device invalidation (default device changed, headset unplugged) the
//! thread reopens the current default device; the writer fills the gap with
//! silence using packet timestamps.

use crossbeam_channel::{SendTimeoutError, Sender, TrySendError};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Mic,
    Loopback,
}

impl TrackKind {
    pub fn name(self) -> &'static str {
        match self {
            TrackKind::Mic => "mic",
            TrackKind::Loopback => "loopback",
        }
    }
}

#[derive(Debug)]
pub enum CaptureMsg {
    /// Sent after every (re)open with the device sample rate for this track.
    Format { kind: TrackKind, sample_rate: u32 },
    Packet {
        kind: TrackKind,
        /// Mono f32 samples at the device rate announced in `Format`.
        samples: Vec<f32>,
        /// QPC-derived timestamp of the first frame, 100 ns units (0 = unreliable).
        qpc_100ns: u64,
    },
    /// Audio packets dropped because the bounded writer queue was full.
    /// The writer inserts equivalent silence so both the timeline and the
    /// user-visible health counters remain honest.
    Overflow {
        kind: TrackKind,
        packets: u64,
        samples: u64,
        sample_rate: u32,
    },
    /// Device lost; the thread is retrying with the current default device.
    DeviceLost { kind: TrackKind },
    /// Unrecoverable error (thread exits after sending this).
    Fatal { kind: TrackKind, error: String },
}

/// A little over two seconds at the normal 10 ms WASAPI packet cadence.
/// This bounds memory without making ordinary scheduler stalls lossy.
pub const CAPTURE_QUEUE_CAPACITY: usize = 256;

/// A Windows audio endpoint. `id` is the persistent WASAPI endpoint ID; the
/// friendly `name` is presentation-only and may change after driver updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

fn sort_devices(devices: &mut [AudioDevice]) {
    devices.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// ~200 ms shared-mode buffer: roomy enough to survive scheduling hiccups.
#[cfg(windows)]
const BUFFER_DURATION_HNS: i64 = 2_000_000;

/// Enumerate active device endpoint IDs and friendly names:
/// (render/outputs, capture/mics).
/// Runs on its own thread so COM state never leaks into the caller.
pub fn list_devices() -> Result<(Vec<AudioDevice>, Vec<AudioDevice>), String> {
    #[cfg(windows)]
    {
        let handle = std::thread::spawn(|| -> Result<_, String> {
            let _ = wasapi::initialize_mta();
            let enumerator = wasapi::DeviceEnumerator::new().map_err(|e| e.to_string())?;
            let mut lists = Vec::new();
            for direction in [wasapi::Direction::Render, wasapi::Direction::Capture] {
                let collection = enumerator
                    .get_device_collection(&direction)
                    .map_err(|e| e.to_string())?;
                let mut devices = Vec::new();
                for i in 0..collection.get_nbr_devices().map_err(|e| e.to_string())? {
                    if let Ok(device) = collection.get_device_at_index(i) {
                        if let (Ok(id), Ok(name)) = (device.get_id(), device.get_friendlyname()) {
                            devices.push(AudioDevice { id, name });
                        }
                    }
                }
                sort_devices(&mut devices);
                lists.push(devices);
            }
            let capture = lists.pop().unwrap_or_default();
            let render = lists.pop().unwrap_or_default();
            Ok((render, capture))
        });
        handle
            .join()
            .map_err(|_| "device enumeration panicked".to_string())?
    }
    #[cfg(not(windows))]
    {
        Ok((Vec::new(), Vec::new()))
    }
}

/// Find a device by friendly name (exact first, then substring, both
/// case-insensitive). A configured name that can't be found is an error —
/// the capture loop keeps retrying and the writer pads silence — because
/// silently recording a *different* device (e.g. the Game channel instead
/// of Chat) would be worse than a documented gap.
#[cfg(windows)]
fn resolve_device(
    enumerator: &wasapi::DeviceEnumerator,
    direction: &wasapi::Direction,
    name: Option<&str>,
) -> Result<wasapi::Device, wasapi::WasapiError> {
    if let Some(wanted) = name.filter(|n| !n.trim().is_empty()) {
        // Endpoint IDs are stable across friendly-name changes and can be
        // persisted by newer callers. Keep friendly-name matching below for
        // compatibility with existing settings and the current UI.
        if let Ok(device) = enumerator.get_device(wanted) {
            return Ok(device);
        }
        let wanted_lower = wanted.to_lowercase();
        let collection = enumerator.get_device_collection(direction)?;
        let mut substring_hit = None;
        let mut substring_ambiguous = false;
        for i in 0..collection.get_nbr_devices()? {
            let device = collection.get_device_at_index(i)?;
            let device_name = device.get_friendlyname().unwrap_or_default();
            let lower = device_name.to_lowercase();
            if lower == wanted_lower {
                return Ok(device);
            }
            if lower.contains(&wanted_lower) {
                if substring_hit.is_some() {
                    substring_ambiguous = true;
                } else {
                    substring_hit = Some(device);
                }
            }
        }
        if substring_ambiguous {
            return Err(wasapi::WasapiError::DeviceNotFound(format!(
                "configured legacy device name '{wanted}' matches multiple endpoints"
            )));
        }
        return substring_hit.ok_or_else(|| {
            wasapi::WasapiError::DeviceNotFound(format!("configured device '{wanted}' not present"))
        });
    }
    enumerator.get_default_device(direction)
}

/// Control messages must not be silently discarded. A full audio queue is
/// expected to drain; a disconnected queue means the writer has exited and
/// the capture thread should stop promptly.
fn send_control(stop: &AtomicBool, tx: &Sender<CaptureMsg>, mut msg: CaptureMsg) -> bool {
    loop {
        match tx.send_timeout(msg, Duration::from_millis(100)) {
            Ok(()) => return true,
            Err(SendTimeoutError::Timeout(returned)) => {
                if stop.load(Ordering::Relaxed) {
                    return false;
                }
                msg = returned;
            }
            Err(SendTimeoutError::Disconnected(_)) => return false,
        }
    }
}

#[derive(Default)]
struct PendingOverflow {
    packets: u64,
    samples: u64,
    sample_rate: u32,
}

impl PendingOverflow {
    fn add(&mut self, samples: usize, sample_rate: u32) {
        self.packets += 1;
        self.samples += samples as u64;
        self.sample_rate = sample_rate;
    }

    fn take_msg(&mut self, kind: TrackKind) -> Option<CaptureMsg> {
        if self.packets == 0 {
            return None;
        }
        let msg = CaptureMsg::Overflow {
            kind,
            packets: self.packets,
            samples: self.samples,
            sample_rate: self.sample_rate,
        };
        self.packets = 0;
        self.samples = 0;
        Some(msg)
    }

    fn restore(&mut self, msg: CaptureMsg) {
        if let CaptureMsg::Overflow {
            packets,
            samples,
            sample_rate,
            ..
        } = msg
        {
            self.packets += packets;
            self.samples += samples;
            self.sample_rate = sample_rate;
        }
    }
}

#[cfg(windows)]
fn run_capture(
    kind: TrackKind,
    stop: &AtomicBool,
    tx: &Sender<CaptureMsg>,
    device_name: Option<&str>,
) -> Result<(), String> {
    use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

    // Loopback = a *render* device opened for *capture* in shared mode.
    let device_direction = match kind {
        TrackKind::Mic => Direction::Capture,
        TrackKind::Loopback => Direction::Render,
    };

    let mut overflow = PendingOverflow::default();
    'reopen: while !stop.load(Ordering::Relaxed) {
        let result: Result<(), wasapi::WasapiError> = (|| {
            let enumerator = DeviceEnumerator::new()?;
            let device = resolve_device(&enumerator, &device_direction, device_name)?;
            let mut audio_client = device.get_iaudioclient()?;

            // Keep the engine mix rate/channels but ask for f32 samples;
            // autoconvert handles any engine-format mismatch.
            let mix = audio_client.get_mixformat()?;
            let rate = mix.get_samplespersec();
            let channels = mix.get_nchannels();
            let format = WaveFormat::new(
                32,
                32,
                &SampleType::Float,
                rate as usize,
                channels as usize,
                None,
            );
            let blockalign = format.get_blockalign() as usize;

            let mode = StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: BUFFER_DURATION_HNS,
            };
            audio_client.initialize_client(&format, &Direction::Capture, &mode)?;
            let h_event = audio_client.set_get_eventhandle()?;
            let capture_client = audio_client.get_audiocaptureclient()?;
            audio_client.start_stream()?;

            if !send_control(
                stop,
                tx,
                CaptureMsg::Format {
                    kind,
                    sample_rate: rate,
                },
            ) {
                return Ok(());
            }
            log::info!(
                "{} capture open on '{}': {} Hz, {} ch",
                kind.name(),
                device.get_friendlyname().unwrap_or_else(|_| "?".into()),
                rate,
                channels
            );

            let mut byte_buf: Vec<u8> = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                // Loopback delivers no events while the system is silent, so
                // a timeout here is normal — just check the stop flag again.
                let _ = h_event.wait_for_event(250);

                loop {
                    let frames = match capture_client.get_next_packet_size()? {
                        Some(0) | None => break,
                        Some(n) => n as usize,
                    };
                    byte_buf.resize(frames * blockalign, 0);
                    let (read, info) = capture_client.read_from_device(&mut byte_buf)?;
                    let read = read as usize;
                    if read == 0 {
                        break;
                    }
                    let mut mono = Vec::with_capacity(read);
                    if info.flags.silent {
                        mono.resize(read, 0.0f32);
                    } else {
                        let ch = channels as usize;
                        for frame in byte_buf[..read * blockalign].chunks_exact(blockalign) {
                            let mut acc = 0.0f32;
                            for c in 0..ch {
                                let off = c * 4;
                                acc += f32::from_le_bytes([
                                    frame[off],
                                    frame[off + 1],
                                    frame[off + 2],
                                    frame[off + 3],
                                ]);
                            }
                            mono.push(acc / ch as f32);
                        }
                    }
                    let qpc = if info.flags.timestamp_error {
                        0
                    } else {
                        info.timestamp
                    };
                    if let Some(msg) = overflow.take_msg(kind) {
                        match tx.try_send(msg) {
                            Ok(()) => {}
                            Err(TrySendError::Full(msg)) => {
                                overflow.restore(msg);
                                overflow.add(mono.len(), rate);
                                continue;
                            }
                            Err(TrySendError::Disconnected(_)) => return Ok(()),
                        }
                    }

                    let sample_count = mono.len();
                    match tx.try_send(CaptureMsg::Packet {
                        kind,
                        samples: mono,
                        qpc_100ns: qpc,
                    }) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            overflow.add(sample_count, rate);
                            if overflow.packets == 1 || overflow.packets.is_multiple_of(100) {
                                log::warn!(
                                    "{} capture writer queue full; {} packets dropped",
                                    kind.name(),
                                    overflow.packets
                                );
                            }
                        }
                        Err(TrySendError::Disconnected(_)) => return Ok(()),
                    }
                }
            }
            let _ = audio_client.stop_stream();
            Ok(())
        })();

        match result {
            Ok(()) => break 'reopen, // stop requested
            Err(e) => {
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                // Any stream error (device invalidated 0x88890004, unplugged,
                // default changed) → reopen the current default device.
                log::warn!("{} capture error, reopening: {}", kind.name(), e);
                if !send_control(stop, tx, CaptureMsg::DeviceLost { kind }) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(500));
                continue 'reopen;
            }
        }
    }
    if let Some(msg) = overflow.take_msg(kind) {
        // Preserve a dropped tail in the recording timeline when possible.
        // This is deliberately bounded so Stop cannot hang behind a failed
        // writer.
        let _ = tx.send_timeout(msg, Duration::from_millis(100));
    }
    Ok(())
}

#[cfg(not(windows))]
fn run_capture(
    _kind: TrackKind,
    _stop: &AtomicBool,
    _tx: &Sender<CaptureMsg>,
    _device_name: Option<&str>,
) -> Result<(), String> {
    Err("audio capture is Windows-only".into())
}

/// Spawns a capture thread; it runs until `stop` is set or a fatal error.
/// `device_name` picks a specific device by friendly name (None = default).
pub fn spawn_capture(
    kind: TrackKind,
    stop: Arc<AtomicBool>,
    tx: Sender<CaptureMsg>,
    device_name: Option<String>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("capture-{}", kind.name()))
        .spawn(move || {
            #[cfg(windows)]
            {
                // Per-thread COM init; already-initialized is fine.
                let _ = wasapi::initialize_mta();
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_capture(kind, &stop, &tx, device_name.as_deref())
            }));
            let error = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error),
                Err(payload) => Some(
                    payload
                        .downcast_ref::<&str>()
                        .map(|message| (*message).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "capture thread panicked".to_string()),
                ),
            };
            if let Some(error) = error {
                log::error!("{} capture stopped: {}", kind.name(), error);
                let _ = send_control(&stop, &tx, CaptureMsg::Fatal { kind, error });
            }
        })
        .expect("spawn capture thread")
}

#[cfg(test)]
mod tests {
    use super::{sort_devices, AudioDevice, PendingOverflow, TrackKind};

    #[test]
    fn devices_sort_by_name_then_stable_id() {
        let mut devices = vec![
            AudioDevice {
                id: "z".into(),
                name: "Speakers".into(),
            },
            AudioDevice {
                id: "b".into(),
                name: "headset".into(),
            },
            AudioDevice {
                id: "a".into(),
                name: "Headset".into(),
            },
        ];
        sort_devices(&mut devices);
        assert_eq!(
            devices
                .into_iter()
                .map(|device| device.id)
                .collect::<Vec<_>>(),
            ["a", "b", "z"]
        );
    }

    #[test]
    fn pending_overflow_is_losslessly_restored_when_queue_stays_full() {
        let mut overflow = PendingOverflow::default();
        overflow.add(480, 48_000);
        overflow.add(960, 48_000);

        let msg = overflow.take_msg(TrackKind::Mic).unwrap();
        assert_eq!(overflow.packets, 0);
        assert_eq!(overflow.samples, 0);
        overflow.restore(msg);

        assert_eq!(overflow.packets, 2);
        assert_eq!(overflow.samples, 1_440);
        assert_eq!(overflow.sample_rate, 48_000);
    }

    /// Real-device enumeration: `cargo test list_devices_smoke -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn list_devices_smoke() {
        let (render, capture) = super::list_devices().unwrap();
        println!("render devices: {render:#?}");
        println!("capture devices: {capture:#?}");
        assert!(!render.is_empty() && !capture.is_empty());
    }
}
