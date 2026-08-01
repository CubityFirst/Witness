//! Speaker diarization of the loopback track with Sortformer (up to 4 remote
//! speakers), via parakeet-rs. Fed in ~10 s chunks so long meetings stream
//! through the model's internal state instead of one giant inference.

use anyhow::{Context, Result};
use parakeet_rs::sortformer::{DiarizationConfig, Sortformer};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct DiarSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    /// 0-based speaker index (S1..S4 = 0..3).
    pub speaker: usize,
}

const FEED_CHUNK: usize = 16_000 * 10; // 10 s @ 16 kHz

pub fn diarize(
    models_dir: &Path,
    samples_16k: &[f32],
    mut progress: impl FnMut(f32),
) -> Result<Vec<DiarSegment>> {
    let model_path = crate::models::sortformer_path(models_dir)
        .context("Sortformer model not downloaded — fetch it in Settings → Models")?;
    let mut sortformer = {
        let _permit = crate::ml_scheduler::batch();
        Sortformer::with_config(&model_path, None, DiarizationConfig::callhome())
            .with_context(|| format!("loading Sortformer from {}", model_path.display()))?
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
    let flushed = {
        let _permit = crate::ml_scheduler::batch();
        sortformer.flush().context("Sortformer flush")?
    };
    raw.extend(flushed);

    // SpeakerSegment start/end are in samples @ 16 kHz.
    let mut out: Vec<DiarSegment> = raw
        .into_iter()
        .map(|s| DiarSegment {
            start_ms: s.start / 16,
            end_ms: s.end / 16,
            speaker: s.speaker_id,
        })
        .collect();
    out.sort_by_key(|s| s.start_ms);
    Ok(out)
}

/// Best speaker for an ASR segment = the diarization speaker with the
/// largest time overlap; None if nothing overlaps.
pub fn assign_speaker(diar: &[DiarSegment], start_ms: u64, end_ms: u64) -> Option<usize> {
    let mut overlap_by_speaker = [0u64; 8];
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
