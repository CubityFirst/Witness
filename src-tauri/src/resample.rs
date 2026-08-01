//! Streaming resampler wrappers around rubato (mono only). Used for
//! device-rate → 48 kHz while recording, and 48 kHz → 16 kHz for ASR.

use anyhow::Result;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

const CHUNK: usize = 1024;

/// Mono streaming resampler; passthrough when rates match.
pub struct StreamResampler {
    inner: Option<SincFixedIn<f32>>,
    pending: Vec<f32>,
}

impl StreamResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Result<StreamResampler> {
        if in_rate == out_rate {
            return Ok(StreamResampler {
                inner: None,
                pending: Vec::new(),
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
            pending: Vec::new(),
        })
    }

    /// Feed input samples; returns whatever output is ready.
    pub fn push(&mut self, samples: &[f32]) -> Result<Vec<f32>> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(samples.to_vec());
        };
        self.pending.extend_from_slice(samples);
        let mut out = Vec::new();
        while self.pending.len() >= CHUNK {
            let chunk: Vec<f32> = self.pending.drain(..CHUNK).collect();
            let mut result = inner.process(&[chunk], None)?;
            out.append(&mut result[0]);
        }
        Ok(out)
    }

    /// Flush any buffered tail (call once at end of stream).
    pub fn finish(&mut self) -> Result<Vec<f32>> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        if !self.pending.is_empty() {
            let tail: Vec<f32> = self.pending.drain(..).collect();
            let mut result = inner.process_partial(Some(&[tail]), None)?;
            out.append(&mut result[0]);
        }
        // Drain the resampler's internal delay line.
        let mut result = inner.process_partial::<Vec<f32>>(None, None)?;
        out.append(&mut result[0]);
        Ok(out)
    }
}

/// One-shot mono resample of a whole buffer.
pub fn resample_all(samples: &[f32], in_rate: u32, out_rate: u32) -> Result<Vec<f32>> {
    if in_rate == out_rate {
        return Ok(samples.to_vec());
    }
    let mut r = StreamResampler::new(in_rate, out_rate)?;
    let mut out = r.push(samples)?;
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
        assert_eq!(
            resample_all(&input, 48_000, 48_000).unwrap().len(),
            input.len()
        );
    }
}
