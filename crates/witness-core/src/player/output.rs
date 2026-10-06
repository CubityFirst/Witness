//! The "player" thread: WASAPI shared-mode render with position tracking
//! across speed changes and default-device follow (design §6.3).
//!
//! The thread owns every `!Send` WASAPI object. Its loop: drain commands →
//! `wait_for_event(20 ms)` (a timeout is just a tick) → fill the free buffer
//! space with reader → WSOLA (bypassed at 1×) → ×volume → `write_to_device`.
//!
//! **Position.** Each write pushes `(device frames written so far, source
//! frame at the end of this write)` onto a small FIFO. The audible device
//! frame is `written − padding`; interpolating it between the bracketing
//! FIFO entries yields the source frame being heard. Frames already queued
//! keep the speed they were produced at, so this stays exact across speed
//! changes without any `× speed` guesswork.
//!
//! The device sits behind the small [`Sink`]/[`Backend`] traits so the
//! engine logic is unit-tested with a fake device; the WASAPI implementation
//! is the only platform code.

use super::archive::OpusArchiveReader;
use super::stretch::Wsola;
use super::{Cmd, PlayerLoad, Shared};
use crossbeam_channel::{Receiver, RecvTimeoutError, TryRecvError};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Samples per second per channel; the archive and the device stream rate.
const RATE: u64 = 48_000;
/// Longest wait for a device period event before the loop ticks anyway.
const EVENT_WAIT_MS: u32 = 20;
/// How often a playing stream checks whether the default endpoint changed.
const DEVICE_CHECK_EVERY: Duration = Duration::from_secs(2);
/// Backoff between reopen attempts after the device vanished.
const REOPEN_DELAY: Duration = Duration::from_millis(500);
/// Give up (and show the error) after this many failed reopens (~3 s).
const MAX_REOPEN_FAILURES: u32 = 6;
/// Position FIFO bound: far more than the 50 ms device buffer ever holds
/// (one entry per write, about one write per 10 ms period).
const FIFO_CAP: usize = 64;

/// A device error, classified for the render loop.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SinkError {
    /// `wait_for_event` timed out: a normal loop tick, never device loss.
    Timeout,
    /// The endpoint went away (unplugged, disabled, audio service restart).
    DeviceLost(String),
    /// Anything else: playback stops with a play error (no reopen loop).
    Other(String),
}

impl std::fmt::Display for SinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SinkError::Timeout => f.write_str("timed out waiting for the audio device"),
            SinkError::DeviceLost(e) => write!(f, "audio device lost: {e}"),
            SinkError::Other(e) => f.write_str(e),
        }
    }
}

/// One open render stream: 48 kHz interleaved stereo f32 frames.
pub(crate) trait Sink {
    /// Total buffer size in frames.
    fn buffer_frames(&self) -> u32;
    /// Frames queued but not yet played.
    fn padding(&mut self) -> Result<u32, SinkError>;
    /// Queue interleaved stereo samples (must fit the free space).
    fn write(&mut self, interleaved: &[f32]) -> Result<(), SinkError>;
    fn start(&mut self) -> Result<(), SinkError>;
    fn stop(&mut self) -> Result<(), SinkError>;
    /// Drop everything queued (stream must be stopped).
    fn reset(&mut self) -> Result<(), SinkError>;
    /// Block until the device wants data, or `timeout_ms`.
    fn wait(&mut self, timeout_ms: u32) -> Result<(), SinkError>;
    /// Endpoint ID the stream was opened on.
    fn endpoint_id(&self) -> &str;
}

/// Opens streams on the current default render endpoint.
pub(crate) trait Backend {
    fn open(&mut self) -> Result<Box<dyn Sink>, String>;
    /// ID of the current default render endpoint, if there is one.
    fn default_endpoint_id(&mut self) -> Option<String>;
}

/// Maps device frames to source frames for the audio in flight.
#[derive(Debug)]
pub(crate) struct PositionFifo {
    /// The point every later entry interpolates from.
    base: (u64, f64),
    /// `(device frames written total, source frame at the end of that write)`.
    entries: VecDeque<(u64, f64)>,
}

impl PositionFifo {
    pub(crate) fn new() -> Self {
        PositionFifo {
            base: (0, 0.0),
            entries: VecDeque::with_capacity(FIFO_CAP + 1),
        }
    }

    /// Start over at `source` with `device_written` frames written so far
    /// (seek, reopen: nothing queued maps anywhere else).
    pub(crate) fn reset(&mut self, device_written: u64, source: f64) {
        self.base = (device_written, source);
        self.entries.clear();
    }

    /// Record a write ending at `device_written` that brings the source to
    /// `source_end`.
    pub(crate) fn push(&mut self, device_written: u64, source_end: f64) {
        self.entries.push_back((device_written, source_end));
        while self.entries.len() > FIFO_CAP {
            if let Some(oldest) = self.entries.pop_front() {
                self.base = oldest;
            }
        }
    }

    /// Source frame being heard when the device has played `played` frames
    /// (`written − padding`). Entries fully played are retired.
    pub(crate) fn position(&mut self, played: u64) -> f64 {
        while let Some(&(dev, src)) = self.entries.front() {
            if dev > played {
                break;
            }
            self.base = (dev, src);
            self.entries.pop_front();
        }
        let (d0, s0) = self.base;
        match self.entries.front() {
            Some(&(d1, s1)) if played > d0 => {
                s0 + (s1 - s0) * (played - d0) as f64 / (d1 - d0) as f64
            }
            _ => s0,
        }
    }

    /// Source frame at the end of everything written.
    pub(crate) fn last_source(&self) -> f64 {
        self.entries.back().map_or(self.base.1, |e| e.1)
    }
}

/// Output ready to write, with the source span it covers.
struct Chunk {
    data: Vec<f32>,
    /// Frames already written.
    read: usize,
    src_start: f64,
    src_end: f64,
}

impl Chunk {
    fn frames(&self) -> usize {
        self.data.len() / 2
    }

    fn source_at(&self, frame: usize) -> f64 {
        self.src_start + (self.src_end - self.src_start) * frame as f64 / self.frames() as f64
    }
}

/// One loaded file and, once playback started, its output stream.
struct Session {
    reader: OpusArchiveReader,
    total: u64,
    duration_ms: u64,
    sink: Option<Box<dyn Sink>>,
    /// The sink is started (`IAudioClient::Start` succeeded and no stop
    /// since). Starting a running stream fails with AUDCLNT_E_NOT_STOPPED.
    running: bool,
    /// Device frames written to the current sink.
    written: u64,
    fifo: PositionFifo,
    chunks: VecDeque<Chunk>,
    queued: usize,
    stretcher: Wsola,
    /// Source frame where the stretcher was last flushed.
    anchor: u64,
    /// Speed new output is produced at (prepared output keeps its own).
    speed: f32,
    eof: bool,
    /// Decoding stopped on an error rather than at the end of the file: the
    /// drained stream ends where it is instead of claiming the full length.
    cut_short: bool,
    ended: bool,
    /// Source frame being heard (last computed).
    position: f64,
    /// A reopen is pending: (next attempt, failures so far).
    reopen: Option<(Instant, u32)>,
    scratch: Vec<f32>,
    out: Vec<f32>,
}

impl Session {
    fn new(reader: OpusArchiveReader, speed: f32) -> Self {
        Session {
            total: reader.total_samples(),
            duration_ms: reader.duration_ms(),
            reader,
            sink: None,
            running: false,
            written: 0,
            fifo: PositionFifo::new(),
            chunks: VecDeque::new(),
            queued: 0,
            stretcher: Wsola::new(),
            anchor: 0,
            speed,
            eof: false,
            cut_short: false,
            ended: false,
            position: 0.0,
            reopen: None,
            scratch: Vec::with_capacity(5760 * 2),
            out: Vec::new(),
        }
    }

    fn position_ms(&self) -> u64 {
        ((self.position / (RATE / 1000) as f64) as u64).min(self.duration_ms)
    }

    /// Use a freshly opened (stopped, empty) stream.
    fn attach(&mut self, sink: Box<dyn Sink>) {
        self.sink = Some(sink);
        self.running = false;
        self.written = 0;
    }

    /// Release the stream (Drop stops and releases the client).
    fn detach(&mut self) {
        self.sink = None;
        self.running = false;
        self.written = 0;
    }

    /// Stop the stream, keeping what is queued.
    fn stop_sink(&mut self) {
        if let Some(sink) = self.sink.as_mut() {
            let _ = sink.stop();
        }
        self.running = false;
    }

    /// Restart decoding at `source` (seek, reopen): drops prepared output
    /// and the stretcher's state.
    fn reanchor(&mut self, source: u64, speed: f32) {
        let source = source.min(self.total);
        let failed = self.reader.seek(source).is_err_and(|e| {
            log::warn!("player: seek failed: {e:#}");
            true
        });
        self.eof = failed;
        self.cut_short = failed;
        self.chunks.clear();
        self.queued = 0;
        self.stretcher.flush();
        self.anchor = source;
        self.speed = speed;
    }

    /// Decode one packet (through the stretcher) onto the chunk queue.
    fn decode_more(&mut self) {
        self.scratch.clear();
        let got = match self.reader.read_stereo(&mut self.scratch) {
            Ok(n) => n,
            Err(e) => {
                // The reader already conceals bad packets and skips damaged
                // pages; what is left (I/O failure) ends playback here.
                log::warn!("player: read error, stopping here: {e:#}");
                self.cut_short = true;
                0
            }
        };
        let before = self.stretcher.emitted_source_frames();
        let mut data = Vec::with_capacity(self.scratch.len() + 1920);
        let after = if got == 0 {
            self.eof = true;
            // `finish` covers everything fed, then resets the counters.
            let end = before + self.stretcher.latency_frames() as f64;
            self.stretcher.finish(self.speed, &mut data);
            end
        } else {
            self.stretcher.process(&self.scratch, self.speed, &mut data);
            self.stretcher.emitted_source_frames()
        };
        if data.is_empty() {
            return;
        }
        let end = self.total as f64;
        self.queued += data.len() / 2;
        self.chunks.push_back(Chunk {
            data,
            read: 0,
            src_start: (self.anchor as f64 + before).min(end),
            src_end: (self.anchor as f64 + after).min(end),
        });
    }

    /// Stage up to `frames` frames into `self.out`; returns the source frame
    /// at the end of the staged audio.
    fn produce(&mut self, frames: usize) -> f64 {
        self.out.clear();
        while self.queued < frames && !self.eof {
            self.decode_more();
        }
        let mut need = frames;
        let mut src_end = self.fifo.last_source();
        while need > 0 {
            let Some(chunk) = self.chunks.front_mut() else {
                break;
            };
            let take = need.min(chunk.frames() - chunk.read);
            self.out
                .extend_from_slice(&chunk.data[chunk.read * 2..(chunk.read + take) * 2]);
            chunk.read += take;
            src_end = chunk.source_at(chunk.read);
            need -= take;
            self.queued -= take;
            if chunk.read == chunk.frames() {
                self.chunks.pop_front();
            }
        }
        src_end
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(sink) = self.sink.as_mut() {
            let _ = sink.stop();
        }
    }
}

/// What the render loop should do after a device step.
enum Step {
    Continue,
    /// The device failed; [`Engine::sink_failed`] decides what to do.
    Failed(SinkError),
}

pub(crate) struct Engine<B: Backend> {
    backend: B,
    shared: Arc<Shared>,
    on_change: Arc<dyn Fn() + Send + Sync>,
    session: Option<Session>,
    /// Generation / seek sequence of the last Load/Unload/Seek processed.
    generation: u64,
    seq: u64,
    /// UI commands processed (all but `Wake`); see `Shared::cmd_seq`.
    handled: u64,
    /// Play intent (the UI's `playing`).
    playing: bool,
    last_device_check: Instant,
    device_check_every: Duration,
}

impl<B: Backend> Engine<B> {
    pub(crate) fn new(
        backend: B,
        shared: Arc<Shared>,
        on_change: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Engine {
            backend,
            shared,
            on_change,
            session: None,
            generation: 0,
            seq: 0,
            handled: 0,
            playing: false,
            last_device_check: Instant::now(),
            device_check_every: DEVICE_CHECK_EVERY,
        }
    }

    /// The stream is running and the loop must service the device.
    pub(crate) fn rendering(&self) -> bool {
        self.playing
            && self
                .session
                .as_ref()
                .is_some_and(|s| s.sink.is_some() && !s.ended)
    }

    /// How long the idle loop may block on the command channel.
    pub(crate) fn idle_timeout(&self) -> Duration {
        match self.session.as_ref().and_then(|s| s.reopen) {
            Some((at, _)) if self.playing => at.saturating_duration_since(Instant::now()),
            _ => Duration::from_secs(1),
        }
    }

    fn publish_position(&self) {
        if let Some(s) = &self.session {
            self.shared
                .publish_position(self.generation, self.seq, s.position_ms());
        }
    }

    /// Publish the engine's `playing`/`ended` (dropped while UI commands are
    /// still queued; see [`Shared::publish_state`]).
    fn publish_state(&self) -> bool {
        let ended = self.session.as_ref().is_some_and(|s| s.ended);
        self.shared
            .publish_state(self.generation, self.handled, self.playing, ended)
    }

    pub(crate) fn handle(&mut self, cmd: Cmd) {
        if !matches!(cmd, Cmd::Wake) {
            self.handled += 1;
        }
        match cmd {
            Cmd::Load {
                path,
                generation,
                seq,
            } => {
                self.session = None;
                self.generation = generation;
                self.seq = seq;
                self.playing = false;
                match OpusArchiveReader::open(&path) {
                    Ok(reader) => {
                        let session = Session::new(reader, self.shared.speed());
                        log::info!(
                            "player: loaded {} ({} ms)",
                            path.display(),
                            session.duration_ms
                        );
                        self.shared.publish_load(
                            generation,
                            PlayerLoad::Ready,
                            session.duration_ms,
                        );
                        self.session = Some(session);
                    }
                    Err(e) => {
                        log::warn!("player: cannot open {}: {e:#}", path.display());
                        self.shared.publish_load(
                            generation,
                            PlayerLoad::Error(format!("{e:#}")),
                            0,
                        );
                    }
                }
            }
            Cmd::Unload { generation, seq } => {
                self.session = None; // Drop stops and releases the client
                self.generation = generation;
                self.seq = seq;
                self.playing = false;
            }
            Cmd::Seek { ms, seq } => {
                self.seq = seq;
                if let Some(s) = self.session.as_mut() {
                    let source = (ms.saturating_mul(RATE / 1000)).min(s.total);
                    self.seek_to(source);
                    self.publish_position();
                    self.publish_state();
                }
            }
            Cmd::Play => {
                self.play();
                self.publish_state();
            }
            Cmd::Pause => {
                self.playing = false;
                if let Some(s) = self.session.as_mut() {
                    s.stop_sink();
                }
                self.update_position();
                self.publish_state();
            }
            // Speed and volume are read from Shared on every fill.
            Cmd::Wake => {}
        }
        // The one repaint request per UI command, made from this thread:
        // `Player` methods never call `on_change` themselves, so the UI may
        // call them inside egui's input/memory closures (a synchronous
        // `request_repaint` there would deadlock on the Context lock).
        (self.on_change)();
    }

    /// HTML `play()`: starts a stopped stream, restarts at 0 from the end
    /// of the media (ended, or a paused seek to the end), and leaves a
    /// stream that is already running alone (a segment click sends seek +
    /// play while playing; the seek already restarted it).
    fn play(&mut self) {
        self.playing = true;
        let Some(s) = self.session.as_mut() else {
            // Nothing (valid) loaded: the intent cannot be honoured.
            self.playing = false;
            return;
        };
        // HTML "ended playback" is the position at the end, whether or not
        // the stream drained there.
        let restart = s.ended || s.position_ms() >= s.duration_ms;
        if restart {
            s.position = 0.0;
        }
        if s.reopen.is_some() {
            // The pending reopen resumes playback once the device is back.
            return;
        }
        if s.sink.is_some() && !restart {
            if !s.running {
                if let Step::Failed(e) = self.resume() {
                    self.sink_failed(e);
                }
            }
            return;
        }
        if s.sink.is_none() {
            match self.backend.open() {
                Ok(sink) => s.attach(sink),
                Err(e) => {
                    self.output_failed(format!("could not open the audio output: {e}"));
                    return;
                }
            }
        }
        let at = s.position.round() as u64;
        self.seek_to(at);
        self.publish_position();
    }

    /// Prefill and start an already-positioned, stopped stream.
    fn resume(&mut self) -> Step {
        if let Step::Failed(e) = self.fill() {
            return Step::Failed(e);
        }
        let Some(s) = self.session.as_mut() else {
            return Step::Continue;
        };
        let Some(sink) = s.sink.as_mut() else {
            return Step::Continue;
        };
        if s.running || s.ended {
            return Step::Continue;
        }
        match sink.start() {
            Ok(()) => {
                s.running = true;
                Step::Continue
            }
            Err(e) => Step::Failed(e),
        }
    }

    /// Stop, flush the device and decoder, and restart at `source`.
    fn seek_to(&mut self, source: u64) {
        let speed = self.shared.speed();
        let Some(s) = self.session.as_mut() else {
            return;
        };
        s.stop_sink();
        if let Some(sink) = s.sink.as_mut() {
            let _ = sink.reset();
        }
        s.reanchor(source, speed);
        s.ended = false;
        s.position = s.anchor as f64;
        s.fifo.reset(s.written, s.position);
        if s.sink.is_some() && self.playing && s.reopen.is_none() {
            if let Step::Failed(e) = self.resume() {
                self.sink_failed(e);
            }
        }
    }

    /// Recompute and publish the audible position (paused streams too).
    fn update_position(&mut self) {
        if let Some(s) = self.session.as_mut() {
            if let Some(sink) = s.sink.as_mut() {
                if let Ok(padding) = sink.padding() {
                    s.position = s.fifo.position(s.written.saturating_sub(padding as u64));
                }
            }
        }
        self.publish_position();
    }

    /// Top up the device buffer; detects the end of the stream.
    fn fill(&mut self) -> Step {
        let speed = self.shared.speed();
        let volume = self.shared.volume();
        let Some(s) = self.session.as_mut() else {
            return Step::Continue;
        };
        let Some(mut sink) = s.sink.take() else {
            return Step::Continue;
        };
        let step = (|| {
            // A failed padding query means the device is gone, whatever the
            // code (design §6.3).
            let padding = sink.padding().map_err(|e| match e {
                SinkError::Other(m) => SinkError::DeviceLost(m),
                e => e,
            })? as u64;
            s.position = s.fifo.position(s.written.saturating_sub(padding));

            if s.eof && s.queued == 0 {
                if padding == 0 && !s.ended {
                    // Drained: the HTML `ended` event.
                    let _ = sink.stop();
                    s.running = false;
                    s.ended = true;
                    if !s.cut_short {
                        // The final granule can overstate what decodes by
                        // a few ms: a clean end is the full duration.
                        s.position = s.total as f64;
                    }
                }
                return Ok(());
            }
            if speed != s.speed {
                // No re-decode and no flush: output already prepared keeps
                // the speed it was made at (the FIFO knows), and the
                // stretcher carries its state into the new speed, so the
                // change is seamless.
                log::debug!(
                    "player: speed {}× → {speed}× ({} source frames buffered in WSOLA)",
                    s.speed,
                    s.stretcher.latency_frames()
                );
                s.speed = speed;
            }
            let space = (sink.buffer_frames() as u64).saturating_sub(padding) as usize;
            if space == 0 {
                return Ok(());
            }
            let src_end = s.produce(space);
            if s.out.is_empty() {
                return Ok(());
            }
            if volume != 1.0 {
                for v in s.out.iter_mut() {
                    *v *= volume;
                }
            }
            sink.write(&s.out)?;
            s.written += (s.out.len() / 2) as u64;
            s.fifo.push(s.written, src_end);
            Ok::<(), SinkError>(())
        })();
        s.sink = Some(sink);
        let just_ended = s.ended && self.playing;
        match step {
            Ok(()) => {
                self.publish_position();
                // While UI commands are queued the end is not announced:
                // they decide the state (a seek clears `ended` and, with the
                // play intent still set, resumes).
                if just_ended
                    && self
                        .shared
                        .publish_state(self.generation, self.handled, false, true)
                {
                    self.playing = false;
                    (self.on_change)();
                }
                Step::Continue
            }
            Err(e) => Step::Failed(e),
        }
    }

    /// Route a device error: a vanished endpoint is reopened, anything else
    /// stops playback with a play error the next `play` can recover from.
    fn sink_failed(&mut self, e: SinkError) {
        match e {
            SinkError::Timeout => {}
            SinkError::DeviceLost(_) => self.device_lost(e),
            SinkError::Other(_) => self.output_failed(format!("audio output error: {e}")),
        }
    }

    /// Drop the stream and schedule a reopen of the default endpoint at the
    /// current position (HTML audio follows the default device too).
    fn device_lost(&mut self, reason: SinkError) {
        let Some(s) = self.session.as_mut() else {
            return;
        };
        log::info!("player: {reason}; reopening the default output");
        s.detach();
        s.reopen = Some((Instant::now(), 0));
    }

    fn try_reopen(&mut self) {
        let Some(s) = self.session.as_mut() else {
            return;
        };
        let Some((_, failures)) = s.reopen else {
            return;
        };
        match self.backend.open() {
            Ok(sink) => {
                s.attach(sink);
                s.reopen = None;
                let at = s.position.round() as u64;
                self.seek_to(at);
            }
            Err(e) if failures + 1 >= MAX_REOPEN_FAILURES => {
                self.output_failed(format!("could not open the audio output: {e}"));
            }
            Err(e) => {
                log::debug!("player: reopen attempt {} failed: {e}", failures + 1);
                s.reopen = Some((Instant::now() + REOPEN_DELAY, failures + 1));
            }
        }
    }

    /// The output cannot play right now: pause, keep the file and position,
    /// and report `message` as a play error (the next `play` reopens).
    fn output_failed(&mut self, message: String) {
        log::warn!("player: {message}");
        if let Some(s) = self.session.as_mut() {
            s.detach();
            s.reopen = None;
        }
        self.playing = false;
        self.shared.publish_play_error(self.generation, message);
        self.publish_position();
        self.publish_state();
        (self.on_change)();
    }

    /// One loop iteration with no command pending.
    pub(crate) fn tick(&mut self) {
        let Some(reopen) = self.session.as_ref().map(|s| s.reopen) else {
            return;
        };
        if let Some((at, _)) = reopen {
            if self.playing && Instant::now() >= at {
                self.try_reopen();
            }
            return;
        }
        if !self.rendering() {
            return;
        }
        let Some(sink) = self.session.as_mut().and_then(|s| s.sink.as_mut()) else {
            return;
        };
        // A timeout is a normal tick (e.g. the engine is starved): only real
        // errors mean the device is gone.
        match sink.wait(EVENT_WAIT_MS) {
            Ok(()) | Err(SinkError::Timeout) => {}
            Err(e) => return self.sink_failed(e),
        }
        if let Step::Failed(e) = self.fill() {
            return self.sink_failed(e);
        }
        if self.last_device_check.elapsed() >= self.device_check_every {
            self.last_device_check = Instant::now();
            let current = self
                .session
                .as_ref()
                .and_then(|s| s.sink.as_ref())
                .map(|sink| sink.endpoint_id().to_owned());
            if let (Some(current), Some(default)) = (current, self.backend.default_endpoint_id()) {
                if current != default {
                    self.device_lost(SinkError::DeviceLost("default output changed".into()));
                }
            }
        }
    }
}

/// The player thread body.
pub(crate) fn run<B, F>(
    make_backend: F,
    rx: Receiver<Cmd>,
    shared: Arc<Shared>,
    on_change: Arc<dyn Fn() + Send + Sync>,
) where
    B: Backend,
    F: FnOnce() -> B,
{
    #[cfg(windows)]
    let _ = wasapi::initialize_mta();
    // Transcription can saturate every core; keep the render thread ahead.
    #[cfg(windows)]
    let _mmcss = wasapi_out::Mmcss::register();
    let mut engine = Engine::new(make_backend(), shared, on_change);
    loop {
        let cmd = if engine.rendering() {
            match rx.try_recv() {
                Ok(cmd) => Some(cmd),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => break,
            }
        } else {
            match rx.recv_timeout(engine.idle_timeout()) {
                Ok(cmd) => Some(cmd),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        };
        match cmd {
            Some(cmd) => engine.handle(cmd),
            None => engine.tick(),
        }
    }
    // Dropping the engine stops and releases the stream.
}

#[cfg(windows)]
pub(crate) type DefaultBackend = wasapi_out::WasapiBackend;
#[cfg(not(windows))]
pub(crate) type DefaultBackend = NoBackend;

/// The production output: the default WASAPI render endpoint.
pub(crate) fn default_backend() -> DefaultBackend {
    DefaultBackend::default()
}

/// Non-Windows builds have no audio output.
#[cfg(not(windows))]
#[derive(Default)]
pub(crate) struct NoBackend;

#[cfg(not(windows))]
impl Backend for NoBackend {
    fn open(&mut self) -> Result<Box<dyn Sink>, String> {
        Err("audio playback is Windows-only".into())
    }
    fn default_endpoint_id(&mut self) -> Option<String> {
        None
    }
}

/// HRESULTs meaning the endpoint is gone rather than the call being wrong.
#[cfg(windows)]
const DEVICE_GONE: [u32; 3] = [
    0x8889_0004, // AUDCLNT_E_DEVICE_INVALIDATED
    0x8889_0010, // AUDCLNT_E_SERVICE_NOT_RUNNING
    0x8889_0026, // AUDCLNT_E_RESOURCES_INVALIDATED
];

/// The render loop's error classifier. In wasapi 0.23 a `wait_for_event`
/// timeout is an `Err(EventTimeout)`: that is a tick, never device loss.
#[cfg(windows)]
pub(crate) fn classify(e: &wasapi::WasapiError) -> SinkError {
    match e {
        wasapi::WasapiError::EventTimeout => SinkError::Timeout,
        wasapi::WasapiError::Windows(err) if DEVICE_GONE.contains(&(err.code().0 as u32)) => {
            SinkError::DeviceLost(e.to_string())
        }
        _ => SinkError::Other(e.to_string()),
    }
}

#[cfg(windows)]
mod wasapi_out {
    use super::{classify, Backend, Sink, SinkError};
    use wasapi::{
        AudioClient, AudioRenderClient, DeviceEnumerator, Direction, Handle, SampleType,
        StreamMode, WasapiError, WaveFormat,
    };

    /// 50 ms device buffer (the engine period is ~10 ms): headroom against
    /// CPU contention from transcription. Latency does not affect position
    /// accuracy (the FIFO accounts for it) nor seeks (they reset the
    /// buffer); only speed/volume changes take up to 50 ms longer to land.
    const BUFFER_DURATION_HNS: i64 = 500_000;
    /// AUDCLNT_E_NOT_STOPPED: `Start` on a running stream (harmless).
    const NOT_STOPPED: u32 = 0x8889_0005;

    /// MMCSS "Playback" registration for the player thread, reverted on drop.
    pub(crate) struct Mmcss(windows::Win32::Foundation::HANDLE);

    impl Mmcss {
        pub(crate) fn register() -> Option<Mmcss> {
            use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
            let mut index = 0u32;
            // SAFETY: a static task name and a valid out-pointer.
            match unsafe {
                AvSetMmThreadCharacteristicsW(windows::core::w!("Playback"), &mut index)
            } {
                Ok(handle) => Some(Mmcss(handle)),
                Err(e) => {
                    log::debug!("player: MMCSS registration failed: {e}");
                    None
                }
            }
        }
    }

    impl Drop for Mmcss {
        fn drop(&mut self) {
            use windows::Win32::System::Threading::AvRevertMmThreadCharacteristics;
            // SAFETY: the handle came from AvSetMmThreadCharacteristicsW on
            // this thread and is reverted once.
            let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
        }
    }

    #[derive(Default)]
    pub(crate) struct WasapiBackend {
        enumerator: Option<DeviceEnumerator>,
    }

    impl WasapiBackend {
        fn enumerator(&mut self) -> Result<&DeviceEnumerator, WasapiError> {
            if self.enumerator.is_none() {
                self.enumerator = Some(DeviceEnumerator::new()?);
            }
            Ok(self.enumerator.as_ref().expect("just set"))
        }

        fn open_default(&mut self) -> Result<WasapiSink, WasapiError> {
            let device = self.enumerator()?.get_default_device(&Direction::Render)?;
            let id = device.get_id()?;
            let mut client = device.get_iaudioclient()?;
            // Ask for the archive's own format; autoconvert lets the engine
            // resample to the device mix rate/channels.
            let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
            client.initialize_client(
                &format,
                &Direction::Render,
                &StreamMode::EventsShared {
                    autoconvert: true,
                    buffer_duration_hns: BUFFER_DURATION_HNS,
                },
            )?;
            let event = client.set_get_eventhandle()?;
            let buffer = client.get_buffer_size()?;
            let render = client.get_audiorenderclient()?;
            log::info!(
                "player output open on '{}': {buffer} frame buffer",
                device.get_friendlyname().unwrap_or_else(|_| "?".into())
            );
            Ok(WasapiSink {
                render,
                event,
                client,
                id,
                buffer,
                bytes: Vec::new(),
            })
        }
    }

    impl Backend for WasapiBackend {
        fn open(&mut self) -> Result<Box<dyn Sink>, String> {
            match self.open_default() {
                Ok(sink) => Ok(Box::new(sink)),
                Err(e) => {
                    // A stale enumerator can outlive an audio service restart.
                    self.enumerator = None;
                    Err(e.to_string())
                }
            }
        }

        fn default_endpoint_id(&mut self) -> Option<String> {
            self.enumerator()
                .ok()?
                .get_default_device(&Direction::Render)
                .ok()?
                .get_id()
                .ok()
        }
    }

    struct WasapiSink {
        render: AudioRenderClient,
        event: Handle,
        client: AudioClient,
        id: String,
        buffer: u32,
        bytes: Vec<u8>,
    }

    impl Sink for WasapiSink {
        fn buffer_frames(&self) -> u32 {
            self.buffer
        }

        fn padding(&mut self) -> Result<u32, SinkError> {
            self.client.get_current_padding().map_err(|e| classify(&e))
        }

        fn write(&mut self, interleaved: &[f32]) -> Result<(), SinkError> {
            self.bytes.clear();
            self.bytes
                .extend(interleaved.iter().flat_map(|v| v.to_le_bytes()));
            self.render
                .write_to_device(interleaved.len() / 2, &self.bytes, None)
                .map_err(|e| classify(&e))
        }

        fn start(&mut self) -> Result<(), SinkError> {
            match self.client.start_stream() {
                Err(WasapiError::Windows(e)) if e.code().0 as u32 == NOT_STOPPED => Ok(()),
                r => r.map_err(|e| classify(&e)),
            }
        }

        fn stop(&mut self) -> Result<(), SinkError> {
            self.client.stop_stream().map_err(|e| classify(&e))
        }

        fn reset(&mut self) -> Result<(), SinkError> {
            self.client.reset_stream().map_err(|e| classify(&e))
        }

        fn wait(&mut self, timeout_ms: u32) -> Result<(), SinkError> {
            self.event
                .wait_for_event(timeout_ms)
                .map_err(|e| classify(&e))
        }

        fn endpoint_id(&self) -> &str {
            &self.id
        }
    }

    impl Drop for WasapiSink {
        fn drop(&mut self) {
            let _ = self.client.stop_stream();
        }
    }
}

/// A scriptable in-memory device for the engine tests.
#[cfg(test)]
pub(crate) mod fake {
    use super::{Backend, Sink, SinkError};
    use std::sync::{Arc, Mutex};

    /// Frames the fake "plays" per wait — one 10 ms device period.
    const PERIOD: u32 = 480;

    #[derive(Default)]
    pub(crate) struct State {
        pub buffer: u32,
        pub queued: u32,
        pub running: bool,
        pub opens: usize,
        pub open_failures: u32,
        pub endpoint: String,
        pub default_endpoint: String,
        pub timeouts: u32,
        pub padding_error: Option<SinkError>,
        pub wait_error: Option<SinkError>,
        pub played: u64,
        pub starts: usize,
        pub peak: f32,
        /// Sleep a little per period (end-to-end tests through the thread,
        /// so state can be observed between periods).
        pub realtime: bool,
    }

    #[derive(Clone)]
    pub(crate) struct FakeDevice(pub Arc<Mutex<State>>);

    impl FakeDevice {
        pub(crate) fn new(buffer: u32) -> Self {
            FakeDevice(Arc::new(Mutex::new(State {
                buffer,
                endpoint: "fake-0".into(),
                default_endpoint: "fake-0".into(),
                ..State::default()
            })))
        }

        pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, State> {
            self.0.lock().unwrap()
        }

        pub(crate) fn opens(&self) -> usize {
            self.state().opens
        }

        pub(crate) fn peak(&self) -> f32 {
            self.state().peak
        }
    }

    pub(crate) struct FakeBackend(FakeDevice);

    impl FakeBackend {
        pub(crate) fn new(device: FakeDevice) -> Self {
            FakeBackend(device)
        }
    }

    impl Backend for FakeBackend {
        fn open(&mut self) -> Result<Box<dyn Sink>, String> {
            let mut st = self.0.state();
            if st.open_failures > 0 {
                st.open_failures -= 1;
                return Err("no device".into());
            }
            st.opens += 1;
            st.endpoint = st.default_endpoint.clone();
            st.queued = 0;
            st.running = false;
            let id = st.endpoint.clone();
            drop(st);
            Ok(Box::new(FakeSink {
                device: self.0.clone(),
                id,
            }))
        }

        fn default_endpoint_id(&mut self) -> Option<String> {
            Some(self.0.state().default_endpoint.clone())
        }
    }

    struct FakeSink {
        device: FakeDevice,
        id: String,
    }

    impl Sink for FakeSink {
        fn buffer_frames(&self) -> u32 {
            self.device.state().buffer
        }

        fn padding(&mut self) -> Result<u32, SinkError> {
            let mut st = self.device.state();
            match st.padding_error.take() {
                Some(e) => Err(e),
                None => Ok(st.queued),
            }
        }

        fn write(&mut self, interleaved: &[f32]) -> Result<(), SinkError> {
            let mut st = self.device.state();
            st.queued += (interleaved.len() / 2) as u32;
            assert!(st.queued <= st.buffer, "overfilled the device buffer");
            st.peak = interleaved.iter().fold(st.peak, |m, v| m.max(v.abs()));
            Ok(())
        }

        /// Like raw WASAPI: starting a running stream is an error.
        fn start(&mut self) -> Result<(), SinkError> {
            let mut st = self.device.state();
            if st.running {
                return Err(SinkError::Other("AUDCLNT_E_NOT_STOPPED".into()));
            }
            st.running = true;
            st.starts += 1;
            Ok(())
        }

        fn stop(&mut self) -> Result<(), SinkError> {
            self.device.state().running = false;
            Ok(())
        }

        fn reset(&mut self) -> Result<(), SinkError> {
            let mut st = self.device.state();
            assert!(!st.running, "reset on a running stream");
            st.queued = 0;
            Ok(())
        }

        /// "Plays" one period per call while running.
        fn wait(&mut self, _timeout_ms: u32) -> Result<(), SinkError> {
            let mut st = self.device.state();
            if let Some(e) = st.wait_error.take() {
                return Err(e);
            }
            if st.timeouts > 0 {
                st.timeouts -= 1;
                return Err(SinkError::Timeout);
            }
            if !st.running {
                return Err(SinkError::Timeout);
            }
            let n = PERIOD.min(st.queued);
            st.queued -= n;
            st.played += n as u64;
            let realtime = st.realtime;
            drop(st);
            if realtime {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok(())
        }

        fn endpoint_id(&self) -> &str {
            &self.id
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeBackend, FakeDevice};
    use super::*;
    use crate::player::archive::tests::{make_archive, scratch};
    use crate::player::{pack_position, unpack_position};
    use std::path::Path;
    use std::sync::atomic::Ordering;

    #[test]
    fn fifo_maps_device_frames_across_a_speed_change() {
        let mut fifo = PositionFifo::new();
        fifo.reset(0, 1_000.0);
        // 480 device frames at 1× (source 1000→1480), then 480 at 2×
        // (source 1480→2440), then 240 at 0.75× (2440→2620).
        fifo.push(480, 1_480.0);
        fifo.push(960, 2_440.0);
        fifo.push(1_200, 2_620.0);
        assert_eq!(fifo.last_source(), 2_620.0);
        // Padding 1200 → nothing played yet.
        assert_eq!(fifo.position(0), 1_000.0);
        // Mid 1× write.
        assert_eq!(fifo.position(240), 1_240.0);
        // Boundary.
        assert_eq!(fifo.position(480), 1_480.0);
        // Mid 2× write: 1480 + 240·2.
        assert_eq!(fifo.position(720), 1_960.0);
        // Mid 0.75× write: 2440 + 120·0.75.
        assert_eq!(fifo.position(1_080), 2_530.0);
        // Underrun past everything written: holds the last source frame.
        assert_eq!(fifo.position(5_000), 2_620.0);
        // Seek: everything in flight is void.
        fifo.reset(5_000, 42.0);
        assert_eq!(fifo.position(5_000), 42.0);
        assert_eq!(fifo.last_source(), 42.0);
    }

    #[test]
    fn fifo_stays_bounded() {
        let mut fifo = PositionFifo::new();
        for i in 1..=1_000u64 {
            fifo.push(i * 10, i as f64 * 10.0);
        }
        assert!(fifo.entries.len() <= FIFO_CAP);
        // Interpolation still works from the retired base.
        assert_eq!(fifo.position(9_995), 9_995.0);
    }

    #[cfg(windows)]
    #[test]
    fn wait_timeout_is_a_tick_not_device_loss() {
        use windows::core::{Error, HRESULT};
        assert_eq!(
            classify(&wasapi::WasapiError::EventTimeout),
            SinkError::Timeout
        );
        let invalidated =
            wasapi::WasapiError::Windows(Error::from_hresult(HRESULT(0x8889_0004u32 as i32)));
        assert!(matches!(classify(&invalidated), SinkError::DeviceLost(_)));
        let other = wasapi::WasapiError::Windows(Error::from_hresult(HRESULT(
            0x8007_0057u32 as i32, // E_INVALIDARG
        )));
        assert!(matches!(classify(&other), SinkError::Other(_)));
    }

    struct Rig {
        engine: Engine<FakeBackend>,
        shared: Arc<Shared>,
        device: FakeDevice,
    }

    impl Rig {
        fn new(path: &Path) -> Rig {
            let device = FakeDevice::new(960);
            let shared = Arc::new(Shared::new());
            let mut engine = Engine::new(
                FakeBackend::new(device.clone()),
                shared.clone(),
                Arc::new(|| {}),
            );
            shared.generation.store(1, Ordering::Release);
            shared.seek_seq.store(1, Ordering::Release);
            shared.cmd_seq.store(1, Ordering::Release);
            shared
                .position
                .store(pack_position(1, 0), Ordering::Release);
            engine.handle(Cmd::Load {
                path: path.to_path_buf(),
                generation: 1,
                seq: 1,
            });
            Rig {
                engine,
                shared,
                device,
            }
        }

        fn position_ms(&self) -> u64 {
            unpack_position(self.shared.position.load(Ordering::Acquire)).1
        }

        /// Source frame being heard, straight from the engine.
        fn source(&self) -> f64 {
            self.engine.session.as_ref().unwrap().position
        }

        /// What `Player::seek_ms` does on the UI thread; returns the command
        /// it would send.
        fn ui_seek(&self, ms: u64) -> Cmd {
            let _meta = self.shared.meta();
            self.shared.cmd_seq.fetch_add(1, Ordering::AcqRel);
            let seq = self.shared.seek_seq.fetch_add(1, Ordering::AcqRel) + 1;
            self.shared.ended.store(false, Ordering::Release);
            self.shared
                .position
                .store(pack_position(seq, ms), Ordering::Release);
            Cmd::Seek { ms, seq }
        }

        /// What `Player::play` does on the UI thread.
        fn ui_play(&self) -> Cmd {
            let _meta = self.shared.meta();
            self.shared.cmd_seq.fetch_add(1, Ordering::AcqRel);
            self.shared.playing.store(true, Ordering::Release);
            Cmd::Play
        }

        fn ui_pause(&self) -> Cmd {
            let _meta = self.shared.meta();
            self.shared.cmd_seq.fetch_add(1, Ordering::AcqRel);
            self.shared.playing.store(false, Ordering::Release);
            Cmd::Pause
        }

        fn seek(&mut self, ms: u64) {
            let cmd = self.ui_seek(ms);
            self.engine.handle(cmd);
        }

        fn play(&mut self) {
            let cmd = self.ui_play();
            self.engine.handle(cmd);
        }

        fn pause(&mut self) {
            let cmd = self.ui_pause();
            self.engine.handle(cmd);
        }

        fn playing(&self) -> bool {
            self.shared.playing.load(Ordering::Acquire)
        }

        fn ended(&self) -> bool {
            self.shared.ended.load(Ordering::Acquire)
        }

        fn play_error(&self) -> Option<String> {
            self.shared.meta().play_error.take()
        }

        /// Make a scheduled reopen attempt due now.
        fn reopen_now(&mut self) {
            if let Some(s) = self.engine.session.as_mut() {
                if let Some((_, f)) = s.reopen {
                    s.reopen = Some((Instant::now(), f));
                }
            }
        }

        fn ticks(&mut self, n: usize) {
            for _ in 0..n {
                self.engine.tick();
            }
        }
    }

    #[test]
    fn engine_plays_seeks_and_ends() {
        let dir = scratch("engine-basic");
        let path = make_archive(&dir, "b", 2 * 48_000);
        let mut rig = Rig::new(&path);
        assert_eq!(rig.shared.meta().load, PlayerLoad::Ready);

        // Seek while paused: exact, and no device yet.
        rig.seek(1_250);
        assert_eq!(rig.position_ms(), 1_250);
        assert_eq!(rig.device.opens(), 0);

        rig.play();
        assert_eq!(rig.device.opens(), 1);
        assert!(rig.engine.rendering());
        assert_eq!(
            rig.position_ms(),
            1_250,
            "prefill must not move the position"
        );
        // Each tick plays one 10 ms period at 1×.
        rig.ticks(10);
        let heard = rig.source() - 1_250.0 * 48.0;
        assert!((heard - 4_800.0).abs() <= 1.0, "heard {heard} frames");
        assert_eq!(rig.position_ms(), 1_350);

        // Pause holds the position; resume continues from it.
        rig.pause();
        rig.ticks(5);
        assert_eq!(rig.position_ms(), 1_350);
        rig.play();
        rig.ticks(200);
        // Drained to the end: ended, stopped, position == duration.
        assert!(!rig.engine.rendering());
        assert!(rig.shared.ended.load(Ordering::Acquire));
        assert!(!rig.shared.playing.load(Ordering::Acquire));
        assert_eq!(rig.position_ms(), 2_000);
        assert!(!rig.device.state().running);

        // Play after the end restarts at 0.
        rig.play();
        assert_eq!(rig.position_ms(), 0);
        rig.ticks(3);
        assert!(rig.position_ms() > 0 && rig.position_ms() <= 30);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A paused seek to the very end (no drain, so not `ended`) followed by
    /// play restarts at 0 instead of ending again immediately.
    #[test]
    fn play_from_the_end_restarts_at_zero() {
        let dir = scratch("engine-end");
        let path = make_archive(&dir, "pe", 2 * 48_000);
        let mut rig = Rig::new(&path);
        rig.seek(2_000);
        assert!(!rig.ended());
        rig.play();
        assert_eq!(rig.position_ms(), 0);
        assert!(rig.engine.rendering());
        rig.ticks(10);
        assert_eq!(rig.position_ms(), 100);
        assert!(rig.playing() && !rig.ended());
        // After an ended stream, a seek back to the end clears `ended`;
        // play there still restarts.
        rig.seek(1_990);
        rig.ticks(200);
        assert!(rig.ended());
        rig.seek(2_000);
        rig.play();
        rig.ticks(3);
        assert!(rig.playing() && rig.position_ms() <= 30);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A damaged page mid-file plays through (silence over the lost span)
    /// and the stream still ends at the full duration.
    #[test]
    fn damaged_page_plays_through_to_the_end() {
        let dir = scratch("engine-damaged");
        let path = make_archive(&dir, "dm", 3 * 48_000);
        let offset = OpusArchiveReader::open(&path).unwrap().pages()[1].offset as usize;
        let mut bytes = std::fs::read(&path).unwrap();
        let segments = bytes[offset + 26] as usize;
        bytes[offset + 27 + segments + 5] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        let mut rig = Rig::new(&path);
        rig.play();
        rig.ticks(400);
        assert!(rig.ended() && !rig.playing());
        assert_eq!(rig.position_ms(), 3_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn engine_position_follows_speed_changes() {
        let dir = scratch("engine-speed");
        let path = make_archive(&dir, "s", 4 * 48_000);
        let mut rig = Rig::new(&path);
        rig.play();
        rig.ticks(20);
        let at_1x = rig.source();
        assert!((at_1x - 9_600.0).abs() <= 1.0);

        rig.shared
            .speed_bits
            .store(2.0f32.to_bits(), Ordering::Relaxed);
        rig.engine.handle(Cmd::Wake);
        // The 20 ms already queued still plays at 1×, then 2× takes over.
        rig.ticks(2);
        let switched = rig.source();
        assert!(
            (switched - (at_1x + 960.0)).abs() <= 1.0,
            "queued audio must keep its speed: {switched}"
        );
        // Now each 480-frame period covers ≈960 source frames. The WSOLA
        // mapping is per hop, so allow a hop of slack.
        let before = rig.source();
        rig.ticks(50);
        let advanced = rig.source() - before;
        assert!(
            (advanced - 50.0 * 960.0).abs() <= 960.0,
            "2× advanced {advanced} source frames over 24000 device frames"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn timeouts_never_reopen_the_device() {
        let dir = scratch("engine-timeout");
        let path = make_archive(&dir, "t", 2 * 48_000);
        let mut rig = Rig::new(&path);
        rig.play();
        rig.device.state().timeouts = 50;
        rig.ticks(50);
        assert_eq!(rig.device.opens(), 1, "a wait timeout reopened the device");
        assert!(rig.engine.rendering());
        assert!(rig.shared.playing.load(Ordering::Acquire));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn device_loss_reopens_and_resumes_in_place() {
        let dir = scratch("engine-lost");
        let path = make_archive(&dir, "l", 3 * 48_000);
        let mut rig = Rig::new(&path);
        rig.play();
        rig.ticks(30);
        let before = rig.source();

        // Invalidated: the failed padding query triggers the reopen path.
        rig.device.state().padding_error =
            Some(SinkError::DeviceLost("AUDCLNT_E_DEVICE_INVALIDATED".into()));
        rig.device.state().open_failures = 2; // first two reopen attempts fail
        rig.ticks(1);
        assert_eq!(rig.device.opens(), 1);
        // Retries are scheduled (500 ms backoff); force them due.
        for _ in 0..3 {
            rig.reopen_now();
            rig.ticks(1);
        }
        assert_eq!(rig.device.opens(), 2, "reopened once the device came back");
        assert!(rig.engine.rendering());
        let resumed = rig.source();
        assert!(
            (resumed - before).abs() <= 480.0,
            "resumed at {resumed}, was at {before}"
        );
        rig.ticks(10);
        assert!(rig.source() > resumed);

        // Default endpoint changes are followed too.
        rig.engine.device_check_every = Duration::ZERO;
        rig.device.state().default_endpoint = "fake-1".into();
        rig.ticks(2);
        assert_eq!(rig.device.opens(), 3);
        assert_eq!(rig.device.state().endpoint, "fake-1");
        assert!(rig.engine.rendering());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A segment click while playing sends seek + play. The seek restarts
    /// the stream; the play must then leave it alone (real WASAPI rejects
    /// `Start` on a running stream, and the fake does too).
    #[test]
    fn seek_and_play_while_playing_keeps_the_stream() {
        let dir = scratch("engine-seekplay");
        let path = make_archive(&dir, "sp", 3 * 48_000);
        let mut rig = Rig::new(&path);
        rig.play();
        rig.ticks(10);
        assert_eq!(rig.device.state().starts, 1);

        rig.seek(500);
        rig.play();
        rig.play(); // and a stray extra play
        assert_eq!(rig.device.opens(), 1, "seek + play reopened the device");
        assert_eq!(rig.device.state().starts, 2, "only the seek restarts");
        assert!(rig.engine.session.as_ref().unwrap().reopen.is_none());
        assert!(rig.engine.rendering());
        assert!(rig.playing() && rig.play_error().is_none());
        assert_eq!(rig.position_ms(), 500);
        rig.ticks(10);
        assert_eq!(rig.position_ms(), 600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No usable output: the file stays loaded at its position, `playing`
    /// drops back, the view gets a play error, and the next play retries.
    #[test]
    fn no_device_is_a_recoverable_play_error() {
        let dir = scratch("engine-nodev");
        let path = make_archive(&dir, "n", 2 * 48_000);
        let mut rig = Rig::new(&path);
        rig.seek(500);
        rig.device.state().open_failures = 1;
        rig.play();
        assert_eq!(rig.shared.meta().load, PlayerLoad::Ready);
        assert!(!rig.playing());
        let err = rig.play_error();
        assert!(
            err.as_deref().is_some_and(|e| e.contains("audio output")),
            "{err:?}"
        );
        assert_eq!(rig.position_ms(), 500);

        // The headset is back: play just works, from where it was.
        rig.play();
        assert_eq!(rig.device.opens(), 1);
        assert!(rig.engine.rendering() && rig.playing());
        assert_eq!(rig.position_ms(), 500);
        rig.ticks(5);
        assert_eq!(rig.position_ms(), 550);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exhausted_reopens_and_other_errors_pause_recoverably() {
        let dir = scratch("engine-exhaust");
        let path = make_archive(&dir, "x", 3 * 48_000);
        let mut rig = Rig::new(&path);
        rig.play();
        rig.ticks(20);
        let before = rig.position_ms();

        // The device vanishes and does not come back within the retries.
        rig.device.state().padding_error = Some(SinkError::DeviceLost("gone".into()));
        rig.device.state().open_failures = 100;
        rig.ticks(1);
        for _ in 0..MAX_REOPEN_FAILURES {
            rig.reopen_now();
            rig.ticks(1);
        }
        assert!(!rig.playing() && !rig.engine.rendering());
        assert!(rig.play_error().is_some());
        assert_eq!(rig.shared.meta().load, PlayerLoad::Ready);
        assert!(rig.engine.session.as_ref().unwrap().reopen.is_none());
        let held = rig.position_ms();
        assert!(held.abs_diff(before) <= 10, "{held} vs {before}");

        rig.device.state().open_failures = 0;
        rig.play();
        assert!(rig.engine.rendering());
        assert_eq!(rig.position_ms(), held);

        // An unexpected (non device-loss) error pauses instead of looping
        // through reopens.
        let opens = rig.device.opens();
        rig.device.state().wait_error = Some(SinkError::Other("E_UNEXPECTED".into()));
        rig.ticks(3);
        assert_eq!(rig.device.opens(), opens);
        assert!(!rig.playing() && rig.play_error().is_some());
        rig.play();
        assert!(rig.engine.rendering());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The end of the file is detected in the same instant the user clicks
    /// a segment (seek + play queued): the click wins, and the snapshot
    /// says playing at the click target, not paused and ended.
    #[test]
    fn end_racing_a_segment_click_leaves_a_consistent_snapshot() {
        let dir = scratch("engine-endrace");
        let path = make_archive(&dir, "e", 2 * 48_000);
        let mut rig = Rig::new(&path);
        rig.seek(1_900);
        rig.play();
        let seek = rig.ui_seek(200);
        let play = rig.ui_play();
        for _ in 0..100 {
            if rig.engine.session.as_ref().unwrap().ended {
                break;
            }
            rig.engine.tick();
        }
        assert!(rig.engine.session.as_ref().unwrap().ended);
        assert!(
            rig.playing() && !rig.ended(),
            "end published over the click"
        );
        rig.engine.handle(seek);
        rig.engine.handle(play);
        assert!(rig.engine.rendering());
        assert!(rig.playing() && !rig.ended());
        assert_eq!(rig.position_ms(), 200);

        // A seek alone (UI still playing) resumes playback at the target.
        rig.seek(1_950);
        let seek = rig.ui_seek(300);
        rig.ticks(100);
        assert!(rig.playing() && !rig.ended());
        rig.engine.handle(seek);
        assert!(rig.engine.rendering() && rig.playing() && !rig.ended());
        assert_eq!(rig.position_ms(), 300);

        // With nothing queued the end is published as before.
        rig.seek(1_950);
        rig.ticks(100);
        assert!(!rig.playing() && rig.ended());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Plays 3 s of a generated archive on the real default device, seeking
    /// and changing speed on the way. Listen for: a gliding two-tone hum,
    /// a jump, then faster (same pitch) playback.
    #[test]
    #[ignore = "plays audio on the default output device"]
    fn playback_smoke() {
        let dir = scratch("smoke");
        let path = make_archive(&dir, "smoke", 8 * 48_000);
        let player = crate::player::Player::spawn(Arc::new(|| {}));
        player.load(path);
        let deadline = Instant::now() + Duration::from_secs(5);
        while player.snapshot().load != PlayerLoad::Ready {
            assert!(Instant::now() < deadline, "{:?}", player.snapshot());
            std::thread::sleep(Duration::from_millis(10));
        }
        player.set_volume(0.3);
        player.play();
        std::thread::sleep(Duration::from_millis(1_000));
        let p1 = player.snapshot().position_ms;
        assert!(
            (800..=1_200).contains(&p1),
            "after 1 s of playback position is {p1} ms"
        );
        // A segment click: seek + play while already playing.
        player.seek_ms(4_000);
        player.play();
        std::thread::sleep(Duration::from_millis(1_000));
        assert!(player.take_play_error().is_none());
        let p2 = player.snapshot().position_ms;
        assert!((4_800..=5_200).contains(&p2), "after seek + 1 s: {p2} ms");
        player.set_speed(1.5);
        std::thread::sleep(Duration::from_millis(1_000));
        let p3 = player.snapshot().position_ms;
        assert!(
            (p3 - p2) >= 1_300 && (p3 - p2) <= 1_700,
            "1 s at 1.5× advanced {} ms",
            p3 - p2
        );
        eprintln!("playback_smoke positions: {p1} ms, {p2} ms, {p3} ms");
        let snap = player.snapshot();
        assert!(snap.playing, "{snap:?}");
        player.unload();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
