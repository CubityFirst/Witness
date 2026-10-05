//! In-house WSOLA time-stretcher for 0.75×–2× playback (design §6.4).
//!
//! Waveform-similarity overlap-add, the family Chromium's
//! `AudioRendererAlgorithm` uses for `<audio>.playbackRate` with
//! `preservesPitch`: the output is built from 20 ms Hann-windowed segments
//! laid down every 10 ms (50 % overlap, so the windows sum to exactly 1),
//! while the read position in the source advances by `10 ms × speed`. Each
//! segment may slide ±10 ms from its nominal source position to where it
//! best continues the previous one (cross-correlation on a mono downmix),
//! which keeps pitch and avoids phasing; the chosen offset is applied to
//! both channels so the stereo image is untouched.
//!
//! Input and output are interleaved stereo f32 at 48 kHz.

/// Analysis/synthesis window: 20 ms.
const WINDOW: usize = 960;
/// Synthesis hop: 50 % overlap.
const HOP: usize = WINDOW / 2;
/// Overlap between consecutive segments (the correlated span).
const OVERLAP: usize = WINDOW - HOP;
/// Similarity search radius around the nominal position: ±10 ms.
const SEARCH: usize = 480;
/// Coarse search stride (refined to single frames around the best hit).
const COARSE: usize = 4;

/// Pitch-preserving time-stretcher. Holds up to ~40 ms of input.
pub struct Wsola {
    /// Pending input, interleaved stereo.
    input: Vec<f32>,
    /// Mono downmix of `input`, for the similarity search.
    mono: Vec<f32>,
    /// Windowed second half of the previous segment, awaiting overlap-add.
    tail: Vec<f32>,
    /// Periodic Hann window: `w[n] + w[n + HOP] == 1`.
    window: Vec<f32>,
    /// Nominal source position of the next segment, in frames into `input`.
    ana_pos: f64,
    /// Where the previous segment would naturally continue (its start +
    /// HOP, frames into `input`): the template the next segment must match.
    template: Option<usize>,
    /// Input frames received since the last flush.
    received: u64,
    /// Source frames (since the last flush) represented by the output
    /// emitted so far.
    emitted_src: f64,
}

impl Default for Wsola {
    fn default() -> Self {
        Self::new()
    }
}

impl Wsola {
    pub fn new() -> Self {
        let window = (0..WINDOW)
            .map(|n| (0.5 - 0.5 * (std::f64::consts::TAU * n as f64 / WINDOW as f64).cos()) as f32)
            .collect();
        Wsola {
            input: Vec::with_capacity((WINDOW + 2 * SEARCH + 2 * WINDOW) * 2),
            mono: Vec::with_capacity(WINDOW + 2 * SEARCH + 2 * WINDOW),
            tail: vec![0.0; HOP * 2],
            window,
            ana_pos: 0.0,
            template: None,
            received: 0,
            emitted_src: 0.0,
        }
    }

    /// WSOLA holds state (buffered input or an overlap tail in flight).
    fn active(&self) -> bool {
        self.template.is_some() || !self.input.is_empty()
    }

    /// Feed interleaved stereo `input` and append whatever output is ready
    /// to `out`. `speed` may change between calls (the player flushes on a
    /// change anyway). At exactly 1.0× with no WSOLA state in flight the
    /// input passes through bit-exact.
    pub fn process(&mut self, input: &[f32], speed: f32, out: &mut Vec<f32>) {
        let frames_in = (input.len() / 2) as u64;
        self.received += frames_in;
        if speed == 1.0 && !self.active() {
            out.extend_from_slice(input);
            self.emitted_src += frames_in as f64;
            return;
        }
        self.input.extend_from_slice(input);
        self.mono
            .extend(input.chunks_exact(2).map(|f| 0.5 * (f[0] + f[1])));
        let speed = speed.clamp(0.25, 4.0) as f64;

        loop {
            let frames = self.mono.len();
            let nominal = self.ana_pos.round() as usize;
            let lo = nominal.saturating_sub(SEARCH);
            let hi = nominal + SEARCH;
            if hi + WINDOW > frames {
                break; // need more input
            }
            let x = match self.template {
                None => nominal,
                Some(template) => self.best_offset(template, lo, hi),
            };

            // Overlap-add: previous tail + first half of this segment is
            // complete output; the second half becomes the new tail.
            for j in 0..HOP {
                let (w_in, w_out) = (self.window[j], self.window[HOP + j]);
                for c in 0..2 {
                    out.push(self.tail[j * 2 + c] + w_in * self.input[(x + j) * 2 + c]);
                    self.tail[j * 2 + c] = w_out * self.input[(x + HOP + j) * 2 + c];
                }
            }
            self.template = Some(x + HOP);
            self.ana_pos += HOP as f64 * speed;
            self.emitted_src += HOP as f64 * speed;

            // Drop input no future segment or template can touch.
            let keep_from = (x + HOP).min((self.ana_pos.floor() as usize).saturating_sub(SEARCH));
            if keep_from > 0 {
                self.input.drain(..keep_from * 2);
                self.mono.drain(..keep_from);
                self.ana_pos -= keep_from as f64;
                self.template = Some(x + HOP - keep_from);
            }
        }
    }

    /// Emit the input still buffered (padding with silence) so the output
    /// covers everything fed so far, then reset. For end of stream.
    pub fn finish(&mut self, speed: f32, out: &mut Vec<f32>) {
        if self.active() {
            let end = self.received as f64;
            let start_len = out.len();
            let silence = vec![0.0f32; HOP * 2];
            // The buffer holds well under 64 hops of input at any speed.
            for _ in 0..64 {
                if self.emitted_src >= end {
                    break;
                }
                self.process(&silence, speed, out);
            }
            // Trim the output that only covers the padding.
            let excess = ((self.emitted_src - end).max(0.0) / speed as f64) as usize;
            out.truncate((out.len() - excess * 2).max(start_len));
        }
        self.flush();
    }

    /// Drop all state (seek, speed change, unload).
    pub fn flush(&mut self) {
        self.input.clear();
        self.mono.clear();
        self.tail.fill(0.0);
        self.ana_pos = 0.0;
        self.template = None;
        self.received = 0;
        self.emitted_src = 0.0;
    }

    /// Source frames received but not yet represented in the output.
    pub fn latency_frames(&self) -> usize {
        (self.received as f64 - self.emitted_src).max(0.0) as usize
    }

    /// Source frames, since the last flush, that the emitted output covers.
    /// Output hop k maps to source `[k·HOP·speed, (k+1)·HOP·speed)`.
    pub fn emitted_source_frames(&self) -> f64 {
        self.emitted_src
    }

    /// Segment start in `[lo, hi]` whose first OVERLAP frames best match the
    /// natural continuation of the previous segment (starting at `template`):
    /// normalized cross-correlation, coarse stride then refined.
    fn best_offset(&self, template: usize, lo: usize, hi: usize) -> usize {
        let tpl = &self.mono[template..template + OVERLAP];
        let score = |x: usize, stride: usize| -> f32 {
            let cand = &self.mono[x..x + OVERLAP];
            let (mut dot, mut energy) = (0.0f32, 1e-9f32);
            for i in (0..OVERLAP).step_by(stride) {
                dot += tpl[i] * cand[i];
                energy += cand[i] * cand[i];
            }
            dot / energy.sqrt()
        };
        let mut best = (template.clamp(lo, hi), f32::MIN);
        for x in (lo..=hi).step_by(COARSE) {
            let s = score(x, 2);
            if s > best.1 {
                best = (x, s);
            }
        }
        let (centre, mut best_score) = (best.0, f32::MIN);
        for x in centre.saturating_sub(COARSE - 1).max(lo)..=(centre + COARSE - 1).min(hi) {
            let s = score(x, 1);
            if s > best_score {
                best = (x, s);
                best_score = s;
            }
        }
        best.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_stereo(hz: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = (i as f32 / 48_000.0 * hz * std::f32::consts::TAU).sin() * 0.5;
                [v, v * 0.8]
            })
            .collect()
    }

    /// Run `input` through in 20 ms packets (as the player feeds it).
    fn stretch(input: &[f32], speed: f32) -> Vec<f32> {
        let mut w = Wsola::new();
        let mut out = Vec::new();
        for chunk in input.chunks(960 * 2) {
            w.process(chunk, speed, &mut out);
        }
        w.finish(speed, &mut out);
        out
    }

    #[test]
    fn output_length_tracks_speed() {
        let frames = 3 * 48_000;
        let input = sine_stereo(300.0, frames);
        for speed in [0.75f32, 1.25, 1.5, 1.75, 2.0] {
            let out = stretch(&input, speed);
            let got = out.len() / 2;
            let want = frames as f64 / speed as f64;
            assert!(
                (got as f64 - want).abs() <= WINDOW as f64,
                "speed {speed}: {got} frames, want ≈{want:.0}"
            );
        }
    }

    /// Positive-going zero crossings per second, skipping the edges.
    fn frequency(signal: &[f32]) -> f64 {
        let left: Vec<f32> = signal.chunks_exact(2).map(|f| f[0]).collect();
        let body = &left[4_800..left.len() - 4_800];
        let crossings = body
            .windows(2)
            .filter(|w| w[0] <= 0.0 && w[1] > 0.0)
            .count();
        crossings as f64 / (body.len() as f64 / 48_000.0)
    }

    #[test]
    fn pitch_is_preserved_at_1_5x() {
        let input = sine_stereo(440.0, 2 * 48_000);
        let out = stretch(&input, 1.5);
        let hz = frequency(&out);
        assert!(
            (hz - 440.0).abs() < 440.0 * 0.02,
            "440 Hz came out at {hz:.1} Hz"
        );
        // And not just the frequency: the signal must stay a clean tone with
        // roughly constant level (no comb-filter dropouts).
        let left: Vec<f32> = out.chunks_exact(2).map(|f| f[0]).collect();
        for block in left[4_800..left.len() - 4_800].chunks(960) {
            let peak = block.iter().fold(0f32, |m, &v| m.max(v.abs()));
            assert!(peak > 0.4, "level dropout: peak {peak}");
        }
    }

    #[test]
    fn unity_speed_is_bit_exact_passthrough() {
        let input: Vec<f32> = (0..48_000 * 2)
            .map(|i| ((i * 7919) % 2001) as f32 / 1000.0 - 1.0)
            .collect();
        let mut w = Wsola::new();
        let mut out = Vec::new();
        for chunk in input.chunks(1234) {
            w.process(chunk, 1.0, &mut out);
        }
        assert_eq!(out, input);
        assert_eq!(w.latency_frames(), 0);
        assert_eq!(w.emitted_source_frames(), 48_000.0);
    }

    #[test]
    fn source_mapping_and_latency_stay_bounded() {
        let mut w = Wsola::new();
        let mut out = Vec::new();
        let input = sine_stereo(200.0, 48_000);
        for chunk in input.chunks(960 * 2) {
            w.process(chunk, 1.25, &mut out);
            // Never more than one window + search span + one packet behind.
            assert!(w.latency_frames() <= WINDOW + 2 * SEARCH + 960);
            let mapped = (out.len() / 2) as f64 * 1.25;
            assert!((mapped - w.emitted_source_frames()).abs() < 1.0);
        }
        w.flush();
        assert_eq!(w.latency_frames(), 0);
        assert_eq!(w.emitted_source_frames(), 0.0);
    }
}
