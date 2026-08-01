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

use crossbeam_channel::Sender;
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
    /// Device lost; the thread is retrying with the current default device.
    DeviceLost { kind: TrackKind },
    /// Unrecoverable error (thread exits after sending this).
    Fatal { kind: TrackKind, error: String },
}

/// ~200 ms shared-mode buffer: roomy enough to survive scheduling hiccups.
#[cfg(windows)]
const BUFFER_DURATION_HNS: i64 = 2_000_000;

/// Enumerate active device friendly names: (render/outputs, capture/mics).
/// Runs on its own thread so COM state never leaks into the caller.
pub fn list_devices() -> Result<(Vec<String>, Vec<String>), String> {
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
                let mut names = Vec::new();
                for i in 0..collection.get_nbr_devices().map_err(|e| e.to_string())? {
                    if let Ok(device) = collection.get_device_at_index(i) {
                        if let Ok(name) = device.get_friendlyname() {
                            names.push(name);
                        }
                    }
                }
                lists.push(names);
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
        let wanted_lower = wanted.to_lowercase();
        let collection = enumerator.get_device_collection(direction)?;
        let mut substring_hit = None;
        for i in 0..collection.get_nbr_devices()? {
            let device = collection.get_device_at_index(i)?;
            let device_name = device.get_friendlyname().unwrap_or_default();
            let lower = device_name.to_lowercase();
            if lower == wanted_lower {
                return Ok(device);
            }
            if substring_hit.is_none() && lower.contains(&wanted_lower) {
                substring_hit = Some(device);
            }
        }
        return substring_hit.ok_or_else(|| {
            wasapi::WasapiError::DeviceNotFound(format!("configured device '{wanted}' not present"))
        });
    }
    enumerator.get_default_device(direction)
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

            let _ = tx.send(CaptureMsg::Format {
                kind,
                sample_rate: rate,
            });
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
                    let _ = tx.send(CaptureMsg::Packet {
                        kind,
                        samples: mono,
                        qpc_100ns: qpc,
                    });
                }
            }
            let _ = audio_client.stop_stream();
            Ok(())
        })();

        match result {
            Ok(()) => return Ok(()), // stop requested
            Err(e) => {
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                // Any stream error (device invalidated 0x88890004, unplugged,
                // default changed) → reopen the current default device.
                log::warn!("{} capture error, reopening: {}", kind.name(), e);
                let _ = tx.send(CaptureMsg::DeviceLost { kind });
                std::thread::sleep(Duration::from_millis(500));
                continue 'reopen;
            }
        }
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
            if let Err(e) = run_capture(kind, &stop, &tx, device_name.as_deref()) {
                let _ = tx.send(CaptureMsg::Fatal { kind, error: e });
            }
        })
        .expect("spawn capture thread")
}

#[cfg(test)]
mod tests {
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
