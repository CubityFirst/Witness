//! Speaker diarization of the loopback track with NVIDIA Nemotron 3
//! Diarization (streaming Sortformer v3, up to 8 remote speakers), via
//! parakeet-rs. Fed in ~10 s chunks so long meetings stream through the
//! model's speaker cache instead of one giant inference; the ONNX metadata
//! selects NVIDIA's offline streaming profile (30.4 s windows).

use anyhow::{Context, Result};
use parakeet_rs::sortformer::{DiarizationConfig, Sortformer};
use std::path::Path;

/// Remote speakers the model can separate (labels S1..S8).
pub const MAX_SPEAKERS: usize = parakeet_rs::sortformer::NUM_SPEAKERS;

#[derive(Debug, Clone)]
pub struct DiarSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    /// 0-based speaker index (S1..S8 = 0..7).
    pub speaker: usize,
}

const FEED_CHUNK: usize = 16_000 * 10; // 10 s @ 16 kHz
/// One 80 ms model frame of slack on top of the streaming window.
const FEED_TAIL_MARGIN: usize = 1_280;

pub fn diarize(
    models_dir: &Path,
    samples_16k: &[f32],
    mut progress: impl FnMut(f32),
) -> Result<Vec<DiarSegment>> {
    let model_path = crate::models::sortformer_path(models_dir)
        .context("Diarization model not downloaded — fetch it in Settings → Models")?;
    let mut sortformer = {
        let _permit = crate::ml_scheduler::batch();
        // The default post-processing reproduces NeMo's own diarize() output
        // for this model (plain 0.5 threshold, no padding or smoothing).
        Sortformer::with_config(&model_path, None, DiarizationConfig::default())
            .with_context(|| format!("loading diarization model from {}", model_path.display()))?
    };

    let mut raw = Vec::new();
    let total = samples_16k.len().max(1);
    for (i, chunk) in samples_16k.chunks(FEED_CHUNK).enumerate() {
        let fed = {
            let _permit = crate::ml_scheduler::batch();
            sortformer.feed(chunk).context("Sortformer feed")?
        };
        raw.extend(fed);
        progress(((i + 1) * FEED_CHUNK).min(total) as f32 / total as f32);
    }
    // Drain the tail with one model window of silence instead of `flush()`:
    // parakeet-rs 0.3.8's flush hands ONNX Runtime a column-major mel array
    // whenever the leftover is a whole number of 80 ms frames (~1 in 8
    // recordings, by length alone) and fails with "non-contiguous layout".
    // `feed()` slices its window and so always copies to row-major. The
    // padding reads as the meeting continuing in silence; segments are
    // clamped to the real audio below.
    let window_samples = (sortformer.latency() * 16_000.0).ceil() as usize + FEED_TAIL_MARGIN;
    let fed = {
        let _permit = crate::ml_scheduler::batch();
        sortformer
            .feed(&vec![0.0; window_samples])
            .context("Sortformer feed (tail)")?
    };
    raw.extend(fed);

    // SpeakerSegment start/end are in samples @ 16 kHz.
    let real_ms = samples_16k.len() as u64 / 16;
    let mut out: Vec<DiarSegment> = raw
        .into_iter()
        .map(|s| DiarSegment {
            start_ms: s.start / 16,
            end_ms: (s.end / 16).min(real_ms),
            speaker: s.speaker_id,
        })
        .filter(|s| s.start_ms < s.end_ms)
        .collect();
    out.sort_by_key(|s| s.start_ms);
    Ok(out)
}

/// Best speaker for an ASR segment = the diarization speaker with the
/// largest time overlap; None if nothing overlaps.
pub fn assign_speaker(diar: &[DiarSegment], start_ms: u64, end_ms: u64) -> Option<usize> {
    let mut overlap_by_speaker = [0u64; MAX_SPEAKERS];
    for d in diar {
        if d.end_ms <= start_ms {
            continue;
        }
        if d.start_ms >= end_ms {
            break; // diar is sorted by start
        }
        let overlap = d
            .end_ms
            .min(end_ms)
            .saturating_sub(d.start_ms.max(start_ms));
        if d.speaker < overlap_by_speaker.len() {
            overlap_by_speaker[d.speaker] += overlap;
        }
    }
    let (best, best_overlap) = overlap_by_speaker
        .iter()
        .enumerate()
        .max_by_key(|(_, &v)| v)?;
    if *best_overlap == 0 {
        None
    } else {
        Some(best)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assign_speaker_picks_largest_overlap_across_all_slots() {
        let diar = [
            DiarSegment {
                start_ms: 0,
                end_ms: 1_000,
                speaker: 0,
            },
            DiarSegment {
                start_ms: 900,
                end_ms: 3_000,
                speaker: MAX_SPEAKERS - 1,
            },
        ];
        assert_eq!(assign_speaker(&diar, 800, 2_000), Some(MAX_SPEAKERS - 1));
        assert_eq!(assign_speaker(&diar, 0, 500), Some(0));
        assert_eq!(assign_speaker(&diar, 5_000, 6_000), None);
    }

    /// Regression: 35 680 samples leave 224 mel frames (a whole number of
    /// 80 ms frames), which made parakeet-rs's `flush()` fail with a
    /// non-contiguous ONNX input. Needs the model (see `cuda_smoke`).
    /// `cargo test diarize_tail_frame_multiple -- --ignored`
    #[test]
    #[ignore]
    fn diarize_tail_frame_multiple() {
        let models_dir = std::env::var("WITNESS_MODELS_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/debug/models"));
        let tone: Vec<f32> = (0..35_680).map(|i| (i as f32 * 0.05).sin() * 0.1).collect();
        diarize(&models_dir, &tone, |_| {}).unwrap();
    }

    /// Two voices alternating over six turns must come out as two
    /// diarization speakers, each turn owned by its voice's speaker.
    /// Generate %TEMP%\witness-turn-{0..5}.wav first (16 kHz mono i16; even
    /// turns Hazel, odd turns Zira). Zira is pitched down 20% here: the two
    /// stock SAPI voices are close enough that Nemotron 3 merges them (v2
    /// split them; on real meetings v2's extra splits were one person).
    /// `cargo test tts_dialogue_diarization -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn tts_dialogue_diarization() {
        let models_dir = std::env::var("WITNESS_MODELS_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/debug/models"));
        const GAP: usize = 16_000 / 2;
        let mut audio = Vec::new();
        let mut turns = Vec::new();
        for i in 0..6 {
            let path = std::env::temp_dir().join(format!("witness-turn-{i}.wav"));
            let mut r = hound::WavReader::open(&path)
                .unwrap_or_else(|_| panic!("generate {} first", path.display()));
            assert_eq!(r.spec().sample_rate, 16_000);
            let start = audio.len();
            let turn: Vec<f32> = r
                .samples::<i16>()
                .map(|s| s.unwrap() as f32 / 32768.0)
                .collect();
            if i % 2 == 0 {
                audio.extend(turn);
            } else {
                // Linear-interpolation resample to 1.25x length = 0.8x pitch.
                let n = turn.len() * 5 / 4;
                audio.extend((0..n).map(|k| {
                    let x = k as f32 * 0.8;
                    let j = x as usize;
                    let f = x - j as f32;
                    turn[j] * (1.0 - f) + turn.get(j + 1).copied().unwrap_or(0.0) * f
                }));
            }
            turns.push(((start / 16) as u64, (audio.len() / 16) as u64));
            audio.extend(std::iter::repeat_n(0.0, GAP));
        }

        let started = std::time::Instant::now();
        let diar = diarize(&models_dir, &audio, |_| {}).unwrap();
        println!(
            "diarized {:.1}s in {:.2}s: {} segments",
            audio.len() as f64 / 16_000.0,
            started.elapsed().as_secs_f64(),
            diar.len()
        );
        let owners: Vec<usize> = turns
            .iter()
            .map(|&(s, e)| assign_speaker(&diar, s, e).expect("turn without a speaker"))
            .collect();
        println!("turn owners: {owners:?}");
        assert_ne!(owners[0], owners[1], "the two voices were merged");
        for (i, owner) in owners.iter().enumerate() {
            assert_eq!(*owner, owners[i % 2], "turn {i} went to the wrong speaker");
        }
    }
}
