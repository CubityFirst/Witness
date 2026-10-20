//! Streaming resampler wrappers around rubato (mono only). Used for
//! device-rate → 48 kHz while recording, and 48 kHz → 16 kHz for ASR.

use anyhow::{ensure, Result};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

const CHUNK: usize = 1024;
#[cfg(test)]
const ONE_SHOT_BATCH: usize = CHUNK * 16;

/// Mono streaming resampler; passthrough when rates match.
pub struct StreamResampler {
    inner: Option<SincFixedIn<f32>>,
    pending: Vec<f32>,
}

impl StreamResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Result<StreamResampler> {
        ensure!(in_rate > 0, "input sample rate must be greater than zero");
        ensure!(out_rate > 0, "output sample rate must be greater than zero");
        if in_rate == out_rate {
            return Ok(StreamResampler {
                inner: None,
                pending: Vec::with_capacity(CHUNK),
            });
        }
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::Blackman2,
        };
        let inner =
            SincFixedIn::<f32>::new(out_rate as f64 / in_rate as f64, 1.1, params, CHUNK, 1)?;
        Ok(StreamResampler {
            inner: Some(inner),
            pending: Vec::with_capacity(CHUNK),
        })
    }

    /// Feed input samples; returns whatever output is ready.
    pub fn push(&mut self, samples: &[f32]) -> Result<Vec<f32>> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(samples.to_vec());
        };
        let mut out = Vec::new();
        let mut remaining = samples;

        // Complete the partial block retained from the previous call first.
        // Never append the whole caller buffer: that used to make a large push
        // allocate proportional to its input and repeatedly shift that buffer.
        if !self.pending.is_empty() {
            let needed = CHUNK - self.pending.len();
            let take = needed.min(remaining.len());
            self.pending.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];

            if self.pending.len() == CHUNK {
                let mut result = inner.process(&[self.pending.as_slice()], None)?;
                out.append(&mut result[0]);
                self.pending.clear();
            }
        }

        // Rubato accepts borrowed channel slices, so full blocks can be
        // processed directly from the caller's buffer without a copy.
        while remaining.len() >= CHUNK {
            let (chunk, rest) = remaining.split_at(CHUNK);
            let mut result = inner.process(&[chunk], None)?;
            out.append(&mut result[0]);
            remaining = rest;
        }

        self.pending.extend_from_slice(remaining);
        debug_assert!(self.pending.len() < CHUNK);
        Ok(out)
    }

    /// Flush any buffered tail (call once at end of stream).
    pub fn finish(&mut self) -> Result<Vec<f32>> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        if !self.pending.is_empty() {
            let mut result = inner.process_partial(Some(&[self.pending.as_slice()]), None)?;
            out.append(&mut result[0]);
            self.pending.clear();
        }
        // Drain the resampler's internal delay line.
        let mut result = inner.process_partial::<Vec<f32>>(None, None)?;
        out.append(&mut result[0]);
        Ok(out)
    }
}

/// One-shot mono resample of a whole buffer.
#[cfg(test)]
pub fn resample_all(samples: &[f32], in_rate: u32, out_rate: u32) -> Result<Vec<f32>> {
    let mut r = StreamResampler::new(in_rate, out_rate)?;
    if in_rate == out_rate {
        return Ok(samples.to_vec());
    }

    // Keep each temporary output returned by `push` bounded even when this is
    // called for a multi-hour recording. The returned Vec itself necessarily
    // remains proportional to the requested one-shot output.
    let mut out = Vec::new();
    for batch in samples.chunks(ONE_SHOT_BATCH) {
        out.append(&mut r.push(batch)?);
    }
    out.append(&mut r.finish()?);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, seconds: f32, hz: f32) -> Vec<f32> {
        (0..(rate as f32 * seconds) as usize)
            .map(|i| (i as f32 / rate as f32 * hz * 2.0 * std::f32::consts::PI).sin() * 0.5)
            .collect()
    }

    #[test]
    fn ratio_and_energy_preserved_44k_to_48k() {
        let input = sine(44_100, 1.0, 440.0);
        let out = resample_all(&input, 44_100, 48_000).unwrap();
        // finish() flushes the sinc delay line, so the output runs a couple
        // of thousand samples long — never short.
        let expected = 48_000f64;
        let len = out.len() as f64;
        assert!(
            len >= expected * 0.99 && len <= expected * 1.06,
            "length {} vs ~{expected}",
            out.len()
        );
        let rms = |s: &[f32]| (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt();
        let (a, b) = (rms(&input), rms(&out));
        assert!((a - b).abs() / a < 0.1, "rms drifted: {a} -> {b}");
    }

    #[test]
    fn downsample_48k_to_16k() {
        let input = sine(48_000, 2.0, 300.0);
        let out = resample_all(&input, 48_000, 16_000).unwrap();
        assert!(
            (out.len() as i64 - 32_000).unsigned_abs() < 700,
            "length {}",
            out.len()
        );
    }

    #[test]
    fn passthrough_when_rates_match() {
        let input = sine(48_000, 0.1, 440.0);
        assert_eq!(resample_all(&input, 48_000, 48_000).unwrap(), input);
    }

    fn stream_with_pattern(
        input: &[f32],
        in_rate: u32,
        out_rate: u32,
        pattern: &[usize],
    ) -> Vec<f32> {
        let mut resampler = StreamResampler::new(in_rate, out_rate).unwrap();
        let mut out = Vec::new();
        let mut offset = 0;
        let mut step = 0;
        while offset < input.len() {
            let end = (offset + pattern[step % pattern.len()]).min(input.len());
            out.append(&mut resampler.push(&input[offset..end]).unwrap());
            offset = end;
            step += 1;
        }
        out.append(&mut resampler.finish().unwrap());
        out
    }

    #[test]
    fn arbitrary_push_boundaries_do_not_lose_or_duplicate_samples() {
        let input = sine(44_100, 2.37, 997.0);
        let aligned = stream_with_pattern(&input, 44_100, 48_000, &[CHUNK]);
        let irregular = stream_with_pattern(
            &input,
            44_100,
            48_000,
            &[1, 7, 1023, 2, 4097, 31, 511, 8192],
        );

        assert_eq!(irregular.len(), aligned.len());
        let max_delta = irregular
            .iter()
            .zip(&aligned)
            .map(|(left, right)| (left - right).abs())
            .fold(0.0_f32, f32::max);
        assert!(
            max_delta < 1e-6,
            "chunk boundaries changed output by {max_delta}"
        );
    }

    #[test]
    fn large_push_keeps_internal_buffer_bounded() {
        let input = sine(48_000, 20.0, 440.0);
        let mut resampler = StreamResampler::new(48_000, 16_000).unwrap();
        let out = resampler.push(&input).unwrap();

        assert!(resampler.pending.len() < CHUNK);
        assert!(
            resampler.pending.capacity() <= CHUNK,
            "pending buffer retained capacity for {} samples",
            resampler.pending.capacity()
        );
        assert!(out.len() > 300_000, "large push lost output samples");
    }

    #[test]
    fn one_shot_matches_irregular_streaming() {
        let input = sine(48_000, 3.125, 523.25);
        let one_shot = resample_all(&input, 48_000, 16_000).unwrap();
        let streamed = stream_with_pattern(&input, 48_000, 16_000, &[13, 2048, 5, 777]);

        assert_eq!(one_shot, streamed);
    }

    #[test]
    fn zero_sample_rates_are_rejected() {
        assert!(StreamResampler::new(0, 48_000).is_err());
        assert!(StreamResampler::new(48_000, 0).is_err());
    }
}
