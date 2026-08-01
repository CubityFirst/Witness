//! Cross-meeting speaker identification: computes an L2-normalized voice
//! print (speaker embedding) from 16 kHz speech via a small ONNX model, and
//! matches it against enrolled people by cosine similarity.
//!
//! Invariant: auto-matching never mutates stored voice prints — only an
//! explicit user rename enrolls or updates a person (see commands.rs).
//!
//! The model is 3D-Speaker ERes2Net (192-dim, VoxCeleb). Its front-end is a
//! kaldi-compatible log-mel fbank exactly as sherpa-onnx feeds it: 80 bins,
//! 25 ms frames / 10 ms shift, povey window, dither off, 20–7600 Hz mels,
//! natural log of power energies, per-utterance mean subtraction (CMN),
//! samples kept in [-1, 1]. Output is L2-normalized by us.

use anyhow::{Context, Result};
use ndarray::Array3;
use realfft::RealFftPlanner;
use std::path::Path;

pub const SAMPLE_RATE: usize = 16_000;
const FRAME_LEN: usize = 400; // 25 ms
const FRAME_SHIFT: usize = 160; // 10 ms
const FFT_SIZE: usize = 512;
const NUM_MELS: usize = 80;
const LOW_FREQ: f32 = 20.0;
const HIGH_FREQ: f32 = 7_600.0;
const PREEMPH: f32 = 0.97;

// The default same/different-speaker boundary (0.6, sherpa-onnx's default
// for this model family) lives in settings::default_threshold — the value
// is user-tunable in Settings.
/// Don't bother embedding speakers with less speech than this — prints get
/// unreliable (and matching them would produce noise).
pub const MIN_SPEECH_SECONDS: f64 = 4.0;
/// Cap how much audio we feed the model per speaker.
pub const MAX_SPEECH_SECONDS: f64 = 60.0;

fn hz_to_mel(hz: f32) -> f32 {
    1127.0 * (1.0 + hz / 700.0).ln()
}

/// Triangular mel filterbank over FFT bins: (bin_start, weights) per mel.
fn mel_banks() -> Vec<(usize, Vec<f32>)> {
    let nyquist = SAMPLE_RATE as f32 / 2.0;
    let high = if HIGH_FREQ > 0.0 {
        HIGH_FREQ.min(nyquist)
    } else {
        nyquist
    };
    let mel_low = hz_to_mel(LOW_FREQ);
    let mel_high = hz_to_mel(high);
    let n_bins = FFT_SIZE / 2 + 1;
    let fft_bin_width = SAMPLE_RATE as f32 / FFT_SIZE as f32;

    let mut banks = Vec::with_capacity(NUM_MELS);
    for m in 0..NUM_MELS {
        let delta = (mel_high - mel_low) / (NUM_MELS + 1) as f32;
        let left = mel_low + m as f32 * delta;
        let center = mel_low + (m + 1) as f32 * delta;
        let right = mel_low + (m + 2) as f32 * delta;
        let mut first = None;
        let mut weights = Vec::new();
        for bin in 0..n_bins {
            let mel = hz_to_mel(bin as f32 * fft_bin_width);
            if mel > left && mel < right {
                let w = if mel <= center {
                    (mel - left) / (center - left)
                } else {
                    (right - mel) / (right - center)
                };
                if first.is_none() {
                    first = Some(bin);
                }
                weights.push(w);
            }
        }
        banks.push((first.unwrap_or(0), weights));
    }
    banks
}

/// Kaldi-style log-mel fbank with per-utterance mean subtraction (CMN).
/// Returns (num_frames, NUM_MELS) row-major.
pub fn fbank(samples: &[f32]) -> Vec<Vec<f32>> {
    if samples.len() < FRAME_LEN {
        return Vec::new();
    }
    let num_frames = (samples.len() - FRAME_LEN) / FRAME_SHIFT + 1;
    let banks = mel_banks();

    // Povey window: (0.5 - 0.5*cos(2*pi*n/(N-1)))^0.85
    let window: Vec<f32> = (0..FRAME_LEN)
        .map(|n| {
            let hann =
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / (FRAME_LEN - 1) as f32).cos();
            hann.powf(0.85)
        })
        .collect();

    let mut planner = RealFftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let mut fft_in = fft.make_input_vec();
    let mut fft_out = fft.make_output_vec();

    let mut frames = Vec::with_capacity(num_frames);
    for f in 0..num_frames {
        let start = f * FRAME_SHIFT;
        let mut frame: Vec<f32> = samples[start..start + FRAME_LEN].to_vec();

        // Remove DC offset.
        let mean = frame.iter().sum::<f32>() / FRAME_LEN as f32;
        for v in frame.iter_mut() {
            *v -= mean;
        }
        // Pre-emphasis (kaldi: in-place, first sample uses itself).
        for i in (1..FRAME_LEN).rev() {
            frame[i] -= PREEMPH * frame[i - 1];
        }
        frame[0] -= PREEMPH * frame[0];
        // Window + zero-pad to FFT size.
        for i in 0..FRAME_LEN {
            fft_in[i] = frame[i] * window[i];
        }
        for v in fft_in[FRAME_LEN..].iter_mut() {
            *v = 0.0;
        }
        fft.process(&mut fft_in, &mut fft_out).expect("fft");

        // Power spectrum → mel energies → natural log.
        let power: Vec<f32> = fft_out.iter().map(|c| c.norm_sqr()).collect();
        let mut mels = Vec::with_capacity(NUM_MELS);
        for (first_bin, weights) in &banks {
            let mut e = 0.0f32;
            for (i, w) in weights.iter().enumerate() {
                e += w * power[first_bin + i];
            }
            mels.push(e.max(f32::EPSILON).ln());
        }
        frames.push(mels);
    }

    // Per-utterance CMN (wespeaker applies mean-norm over time, per bin).
    let mut mean = vec![0.0f32; NUM_MELS];
    for frame in &frames {
        for (m, v) in mean.iter_mut().zip(frame) {
            *m += v;
        }
    }
    for m in mean.iter_mut() {
        *m /= frames.len() as f32;
    }
    for frame in frames.iter_mut() {
        for (v, m) in frame.iter_mut().zip(&mean) {
            *v -= m;
        }
    }
    frames
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

pub fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Merge an existing print with new evidence, weighted by speech seconds.
#[cfg(test)]
pub fn merge_embeddings(old: &[f32], old_secs: f64, new: &[f32], new_secs: f64) -> Vec<f32> {
    let mut out: Vec<f32> = old
        .iter()
        .zip(new)
        .map(|(o, n)| {
            (o * old_secs as f32 + n * new_secs as f32) / (old_secs + new_secs).max(1e-6) as f32
        })
        .collect();
    l2_normalize(&mut out);
    out
}

pub struct Embedder {
    session: ort::session::Session,
    input_name: String,
}

impl Embedder {
    /// Loads the embedding model (CPU — it's tiny and runs in milliseconds).
    pub fn new(models_dir: &Path) -> Result<Embedder> {
        let path = crate::models::speaker_id_path(models_dir)
            .context("speaker-ID model not downloaded — fetch it in Settings → Models")?;
        let session = ort::session::Session::builder()
            .context("ort session builder")?
            .commit_from_file(&path)
            .with_context(|| format!("loading speaker model {}", path.display()))?;
        let input_name = session.inputs()[0].name().to_string();
        Ok(Embedder {
            session,
            input_name,
        })
    }

    /// Voice print of one speaker's 16 kHz speech (L2-normalized).
    pub fn embed(&mut self, samples_16k: &[f32]) -> Result<Vec<f32>> {
        let max = (MAX_SPEECH_SECONDS * SAMPLE_RATE as f64) as usize;
        let samples = &samples_16k[..samples_16k.len().min(max)];
        let feats = fbank(samples);
        anyhow::ensure!(feats.len() > 10, "not enough audio for a voice print");

        let t = feats.len();
        let flat: Vec<f32> = feats.into_iter().flatten().collect();
        let array = Array3::from_shape_vec((1, t, NUM_MELS), flat).context("feature shape")?;
        let value = ort::value::Value::from_array(array).context("input tensor")?;
        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => value])
            .context("speaker model inference")?;
        let (shape, data) = outputs[0]
            .try_extract_tensor::<f32>()
            .context("extract embedding")?;
        let dim = *shape.as_ref().last().unwrap_or(&0) as usize;
        anyhow::ensure!(
            dim > 0 && data.len() >= dim,
            "unexpected embedding shape {shape:?}"
        );
        let mut emb = data[data.len() - dim..].to_vec();
        l2_normalize(&mut emb);
        Ok(emb)
    }
}

/// Best enrolled match for a voice print: (person_id, name, similarity).
pub fn best_match(
    people: &[crate::db::Person],
    embedding: &[f32],
    threshold: f32,
) -> Option<(i64, String, f32)> {
    people
        .iter()
        .map(|p| (p.id, p.name.clone(), cosine(&p.embedding, embedding)))
        .max_by(|a, b| a.2.total_cmp(&b.2))
        .filter(|(_, _, sim)| *sim >= threshold)
}

fn default_name_for(label: &str) -> String {
    label
        .strip_prefix('S')
        .map(|n| format!("Speaker {n}"))
        .unwrap_or_else(|| label.to_string())
}

/// Retroactively re-match stored voice prints against the current people
/// list — no audio needed, just cosine math over the DB. Manual labels are
/// never touched; auto labels can be applied, corrected, or reverted.
/// Returns how many speaker rows changed.
pub fn rematch_all(db: &crate::db::Db, threshold: f32) -> Result<usize> {
    let people = db.list_people()?;
    let mut changed = 0usize;
    for sp in db.speakers_with_embeddings()? {
        // Eligible: previously auto-labeled, or still wearing the default
        // "Speaker N" name with no manual person link.
        let is_default = sp.display_name == default_name_for(&sp.label);
        if !(sp.auto_labeled || (sp.person_id.is_none() && is_default)) {
            continue;
        }
        let Some(embedding) = db.get_embedding_only(sp.id)? else {
            continue;
        };
        match best_match(&people, &embedding, threshold) {
            Some((pid, name, _sim)) => {
                if sp.person_id != Some(pid) || sp.display_name != name {
                    changed += usize::from(db.set_auto_match(sp.id, &name, Some(pid))?);
                }
            }
            None if sp.auto_labeled => {
                // Its person was deleted or the print moved away: revert.
                changed +=
                    usize::from(db.set_auto_match(sp.id, &default_name_for(&sp.label), None)?);
            }
            None => {}
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fbank_shape_and_finiteness() {
        let samples: Vec<f32> = (0..SAMPLE_RATE) // 1 s of 440 Hz tone
            .map(|i| (i as f32 / SAMPLE_RATE as f32 * 440.0 * 2.0 * std::f32::consts::PI).sin())
            .collect();
        let feats = fbank(&samples);
        assert_eq!(feats.len(), (SAMPLE_RATE - FRAME_LEN) / FRAME_SHIFT + 1);
        assert!(feats.iter().all(|f| f.len() == NUM_MELS));
        assert!(feats.iter().flatten().all(|v| v.is_finite()));
        // CMN: per-bin mean ≈ 0.
        let mut mean = vec![0.0f32; NUM_MELS];
        for f in &feats {
            for (m, v) in mean.iter_mut().zip(f) {
                *m += v;
            }
        }
        assert!(mean.iter().all(|m| (m / feats.len() as f32).abs() < 1e-4));
    }

    /// Real-model discrimination test on two Windows TTS voices (Hazel and
    /// Zira), two utterances each — same-voice similarity must clearly beat
    /// cross-voice. Downloads the 25 MB model on first run. Inputs are
    /// generated beforehand into %TEMP%\witness-voice-{a1,a2,b1,b2}.wav.
    /// `cargo test voice_print_discrimination -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn voice_print_discrimination() {
        let models_dir = std::env::var("WITNESS_MODELS_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/debug/models"));
        std::fs::create_dir_all(&models_dir).unwrap();
        crate::models::download_speaker_id(&models_dir).unwrap();

        let load = |tag: &str| -> Vec<f32> {
            let path = std::env::temp_dir().join(format!("witness-voice-{tag}.wav"));
            let mut r = hound::WavReader::open(&path)
                .unwrap_or_else(|_| panic!("generate {} first", path.display()));
            assert_eq!(r.spec().sample_rate, 16_000);
            r.samples::<i16>()
                .map(|s| s.unwrap() as f32 / 32768.0)
                .collect()
        };
        let mut embedder = Embedder::new(&models_dir).unwrap();
        let a1 = embedder.embed(&load("a1")).unwrap();
        let a2 = embedder.embed(&load("a2")).unwrap();
        let b1 = embedder.embed(&load("b1")).unwrap();
        let b2 = embedder.embed(&load("b2")).unwrap();
        assert_eq!(a1.len(), 192);

        let same_a = cosine(&a1, &a2);
        let same_b = cosine(&b1, &b2);
        let cross = [
            cosine(&a1, &b1),
            cosine(&a1, &b2),
            cosine(&a2, &b1),
            cosine(&a2, &b2),
        ];
        let worst_cross = cross.iter().cloned().fold(f32::MIN, f32::max);
        println!("same-voice: hazel {same_a:.3}, zira {same_b:.3}");
        println!("cross-voice: {cross:?} (max {worst_cross:.3})");
        assert!(same_a > worst_cross + 0.15, "hazel print not distinctive");
        assert!(same_b > worst_cross + 0.15, "zira print not distinctive");
    }

    #[test]
    fn cosine_and_merge_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        let merged = merge_embeddings(&[1.0, 0.0], 10.0, &[0.0, 1.0], 10.0);
        assert!((cosine(&merged, &[1.0, 0.0]) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3);
        let norm: f32 = merged.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }
}
