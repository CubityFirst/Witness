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
/// Per-frame speech score cutoff. 0.5 let steady low-level noise (AC hum)
/// hover across the line; the existing 200 ms padding absorbs the slightly
/// later speech onsets a higher bar causes.
pub const THRESHOLD: f32 = 0.65;
/// A raw span must reach this many consecutive speech frames (~80 ms) to
/// count as speech at all — mic pops and clicks light up 1–2 frames, real
/// words always exceed this. Shared with the live-caption VAD gate.
pub const MIN_SPEECH_FRAMES: usize = 5;
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

    // Emit chunks, splitting anything over the TDT cap.
    let mut chunks = Vec::new();
    for (s, e) in regions(&speech) {
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

/// Per-frame speech flags → padded, merged regions [start, end) in frames.
/// Spans shorter than MIN_SPEECH_FRAMES are discarded before padding — a
/// 1–2 frame blip is a transient, and padding would inflate it into a
/// ~400 ms chunk the ASR then hallucinates words for.
fn regions(speech: &[bool]) -> Vec<(usize, usize)> {
    let n_frames = speech.len();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n_frames {
        if speech[i] {
            let start = i;
            while i < n_frames && speech[i] {
                i += 1;
            }
            if i - start >= MIN_SPEECH_FRAMES {
                spans.push((start, i));
            }
        } else {
            i += 1;
        }
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
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_blips_are_not_speech() {
        // A mic pop: 2 frames flagged in otherwise silent audio → dropped.
        let mut flags = vec![false; 200];
        flags[50] = true;
        flags[51] = true;
        assert!(regions(&flags).is_empty());

        // Two separate pops don't add up to speech either.
        flags[120] = true;
        assert!(regions(&flags).is_empty());
    }

    #[test]
    fn real_spans_survive_with_padding() {
        let mut flags = vec![false; 200];
        for f in &mut flags[50..50 + MIN_SPEECH_FRAMES] {
            *f = true;
        }
        let r = regions(&flags);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0], (50 - PAD_FRAMES, 50 + MIN_SPEECH_FRAMES + PAD_FRAMES));
    }

    #[test]
    fn near_spans_merge() {
        let mut flags = vec![false; 300];
        for f in &mut flags[50..60] {
            *f = true;
        }
        // 20 frames apart (< pad + merge gap) → one region.
        for f in &mut flags[80..90] {
            *f = true;
        }
        assert_eq!(regions(&flags).len(), 1);
    }

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

    /// Real-speech sanity for THRESHOLD/MIN_SPEECH_FRAMES: the detector must
    /// keep the bulk of actual speech. Needs %TEMP%\witness-tts.wav (16 kHz
    /// mono i16, SAPI-generated — see asr_parakeet::tests::tts_transcribe).
    /// `cargo test vad_keeps_tts_speech -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn vad_keeps_tts_speech() {
        let wav = std::env::temp_dir().join("witness-tts.wav");
        let mut reader = hound::WavReader::open(&wav).expect("generate TTS wav first");
        assert_eq!(reader.spec().sample_rate, 16_000);
        let samples: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect();
        let total_s = samples.len() as f64 / 16_000.0;
        let kept_s = total_ms(&chunk_speech(&samples)) as f64 / 1000.0;
        println!("VAD kept {kept_s:.1}s of {total_s:.1}s TTS speech");
        assert!(
            kept_s >= total_s * 0.7,
            "VAD too aggressive: kept {kept_s:.1}s of {total_s:.1}s"
        );
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
            assert!(
                c.samples.len() <= 240 * 16_000,
                "chunk too long: {}",
                c.samples.len()
            );
        }
    }
}
