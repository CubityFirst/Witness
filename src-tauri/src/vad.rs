//! Voice-activity chunking: find speech regions in 16 kHz mono audio and cut
//! them into chunks the TDT engine can handle (hard cap 240 s per chunk —
//! Parakeet TDT has a ~4–5 min per-inference limit). Silence between regions
//! is skipped entirely, which is a big throughput win on the loopback track
//! (silent whenever the local user is talking).
//!
//! Uses earshot (pure Rust): the Silero-based `voice_activity_detector`
//! crate pins ort =2.0.0-rc.10 which conflicts with parakeet-rs.

use earshot::Detector;

/// earshot frame: 256 samples @ 16 kHz = 16 ms.
const FRAME: usize = 256;
const FRAME_MS: u64 = 16;
const THRESHOLD: f32 = 0.5;
/// Speech padding on each side of a region (~200 ms).
const PAD_FRAMES: usize = 12;
/// Regions closer than this are merged (~500 ms).
const MERGE_GAP_FRAMES: usize = 31;
/// Hard cap per chunk (TDT limit), in frames: 240 s.
const MAX_CHUNK_FRAMES: usize = (240_000 / FRAME_MS) as usize;

pub struct SpeechChunk {
    /// Offset of the chunk within the track.
    pub start_ms: u64,
    /// Contiguous 16 kHz samples (internal short silences included so ASR
    /// timestamps stay linear).
    pub samples: Vec<f32>,
}

/// Total speech-ish duration in ms across chunks (for progress reporting).
pub fn total_ms(chunks: &[SpeechChunk]) -> u64 {
    chunks.iter().map(|c| c.samples.len() as u64 / 16).sum()
}

pub fn chunk_speech(samples_16k: &[f32]) -> Vec<SpeechChunk> {
    let n_frames = samples_16k.len() / FRAME;
    if n_frames == 0 {
        return Vec::new();
    }

    let mut detector = Detector::const_default();
    let mut speech = vec![false; n_frames];
    for (i, flag) in speech.iter_mut().enumerate() {
        let frame = &samples_16k[i * FRAME..(i + 1) * FRAME];
        if detector.predict_f32(frame) > THRESHOLD {
            *flag = true;
        }
    }

    // Collect raw speech spans [start, end) in frames.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n_frames {
        if speech[i] {
            let start = i;
            while i < n_frames && speech[i] {
                i += 1;
            }
            spans.push((start, i));
        } else {
            i += 1;
        }
    }
    if spans.is_empty() {
        return Vec::new();
    }

    // Pad each span, then merge overlapping/near spans.
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in spans {
        let s = s.saturating_sub(PAD_FRAMES);
        let e = (e + PAD_FRAMES).min(n_frames);
        match merged.last_mut() {
            Some((_, last_e)) if s <= *last_e + MERGE_GAP_FRAMES => {
                *last_e = (*last_e).max(e);
            }
            _ => merged.push((s, e)),
        }
    }

    // Emit chunks, splitting anything over the TDT cap.
    let mut chunks = Vec::new();
    for (s, e) in merged {
        let mut start = s;
        while start < e {
            let end = (start + MAX_CHUNK_FRAMES).min(e);
            chunks.push(SpeechChunk {
                start_ms: start as u64 * FRAME_MS,
                samples: samples_16k[start * FRAME..end * FRAME].to_vec(),
            });
            start = end;
        }
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_yields_no_chunks() {
        assert!(chunk_speech(&vec![0.0f32; 16_000 * 10]).is_empty());
    }

    #[test]
    fn short_and_odd_lengths_do_not_panic() {
        assert!(chunk_speech(&[]).is_empty());
        assert!(chunk_speech(&[0.0; 100]).is_empty()); // below one frame
        let _ = chunk_speech(&vec![0.01; 16_000 + 37]); // non-multiple of frame
    }

    #[test]
    fn chunks_never_exceed_tdt_cap() {
        // Loud broadband noise for 9 minutes; whatever the detector flags,
        // no chunk may exceed 240 s.
        let mut x = 0u32;
        let noise: Vec<f32> = (0..16_000 * 540)
            .map(|_| {
                // xorshift PRNG — deterministic, no rand dependency.
                x ^= x << 13;
                x = x.wrapping_add(0x9E37_79B9);
                x ^= x >> 17;
                (x as f32 / u32::MAX as f32 - 0.5) * 1.6
            })
            .collect();
        for c in chunk_speech(&noise) {
            assert!(c.samples.len() <= 240 * 16_000, "chunk too long: {}", c.samples.len());
        }
    }
}
