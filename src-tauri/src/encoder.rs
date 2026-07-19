//! Archive encoding: the two 48 kHz mono WAVs become one stereo Ogg Opus
//! file (~40 kbps VBR ≈ 18 MB/h), per RFC 7845. Rather than hard-panning
//! (mic left / loopback right — fatiguing on headphones), tracks are
//! blended with an invertible comfort mix: L = 0.75·mic + 0.25·loopback,
//! R = 0.25·mic + 0.75·loopback. Both ears always hear everything, with a
//! gentle spatial cue, and decode_opus() can still recover the two tracks
//! exactly (modulo codec noise) by inverting the matrix. A WITNESS_MIX
//! comment in OpusTags records the mix so legacy hard-panned files decode
//! correctly too.

/// Comfort-mix coefficients (main/cross). Determinant 0.5, so the unmix is
/// well-conditioned; a convex combination, so it can never clip.
const MIX_MAIN: f32 = 0.75;
const MIX_CROSS: f32 = 0.25;
const MIX_TAG: &str = "WITNESS_MIX=75";

use anyhow::{bail, Context, Result};
use audiopus::coder::{Decoder, Encoder};
use audiopus::{Application, Channels, SampleRate};
use ogg::writing::PacketWriteEndInfo;
use ogg::{PacketReader, PacketWriter};
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

/// 20 ms @ 48 kHz.
const FRAME: usize = 960;
/// Recommended max packet size per RFC.
const MAX_PACKET: usize = 4000;

/// Encode `mic` + `loopback` (48 kHz mono i16 WAVs of equal length) into a
/// stereo Ogg Opus file. `progress` gets 0..1.
pub fn encode_opus(
    mic_wav: &Path,
    loopback_wav: &Path,
    out_path: &Path,
    bitrate_kbps: u32,
    mut progress: impl FnMut(f32),
) -> Result<()> {
    let mut mic = hound::WavReader::open(mic_wav)
        .with_context(|| format!("opening {}", mic_wav.display()))?;
    let mut lop = hound::WavReader::open(loopback_wav)
        .with_context(|| format!("opening {}", loopback_wav.display()))?;
    for (name, r) in [("mic", &mic), ("loopback", &lop)] {
        let spec = r.spec();
        if spec.channels != 1 || spec.sample_rate != 48_000 || spec.bits_per_sample != 16 {
            bail!("{name} WAV is not 48 kHz mono i16");
        }
    }
    let total_samples = mic.len().max(lop.len()) as u64;

    let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Voip)
        .context("creating Opus encoder")?;
    encoder
        .set_bitrate(audiopus::Bitrate::BitsPerSecond(bitrate_kbps as i32 * 1000))
        .context("setting Opus bitrate")?;
    let pre_skip: u16 = encoder.lookahead().map(|l| l as u16).unwrap_or(312);

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp_path = out_path.with_extension("opus.part");
    let file = BufWriter::new(
        File::create(&tmp_path).with_context(|| format!("creating {}", tmp_path.display()))?,
    );
    let mut writer = PacketWriter::new(file);
    // Serial just needs to be stable-ish and non-colliding within the file.
    let serial: u32 = 0x57495453; // "WITS"

    // --- OpusHead (own page) ---
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(2); // channels
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes()); // original rate (informational)
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // mapping family 0
    writer.write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)?;

    // --- OpusTags (own page) ---
    let vendor = b"witness";
    let mut tags = Vec::new();
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&1u32.to_le_bytes()); // one comment: the mix marker
    tags.extend_from_slice(&(MIX_TAG.len() as u32).to_le_bytes());
    tags.extend_from_slice(MIX_TAG.as_bytes());
    writer.write_packet(tags, serial, PacketWriteEndInfo::EndPage, 0)?;

    // --- audio ---
    let mut mic_iter = mic.samples::<i16>();
    let mut lop_iter = lop.samples::<i16>();
    let mut interleaved = vec![0i16; FRAME * 2];
    let mut packet = vec![0u8; MAX_PACKET];
    let mut encoded_samples: u64 = 0;
    let mut packets_on_page = 0u32;

    while encoded_samples < total_samples {
        for i in 0..FRAME {
            let m = mic_iter.next().transpose()?.unwrap_or(0) as f32;
            let l = lop_iter.next().transpose()?.unwrap_or(0) as f32;
            interleaved[i * 2] = (MIX_MAIN * m + MIX_CROSS * l) as i16;
            interleaved[i * 2 + 1] = (MIX_CROSS * m + MIX_MAIN * l) as i16;
        }
        let n = encoder
            .encode(&interleaved, &mut packet)
            .context("Opus encode")?;
        encoded_samples += FRAME as u64;
        packets_on_page += 1;

        let last = encoded_samples >= total_samples;
        let granule = if last {
            pre_skip as u64 + total_samples
        } else {
            encoded_samples
        };
        let end_info = if last {
            PacketWriteEndInfo::EndStream
        } else if packets_on_page >= 50 {
            packets_on_page = 0;
            PacketWriteEndInfo::EndPage // ~1 s pages keep seeking snappy
        } else {
            PacketWriteEndInfo::NormalPacket
        };
        writer.write_packet(packet[..n].to_vec(), serial, end_info, granule)?;

        if encoded_samples % (FRAME as u64 * 250) == 0 {
            progress((encoded_samples as f32 / total_samples as f32).min(1.0));
        }
    }
    drop(writer);
    std::fs::rename(&tmp_path, out_path)
        .with_context(|| format!("renaming {} into place", tmp_path.display()))?;
    progress(1.0);
    Ok(())
}

/// True when an OpusTags packet carries our comfort-mix marker.
fn has_mix_tag(data: &[u8]) -> bool {
    // OpusTags: magic(8) + vendor_len(u32) + vendor + count(u32) + comments.
    if data.len() < 16 || &data[..8] != b"OpusTags" {
        return false;
    }
    let read_u32 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
    let mut pos = 8;
    let vendor_len = read_u32(&data[pos..]);
    pos += 4 + vendor_len;
    if pos + 4 > data.len() {
        return false;
    }
    let count = read_u32(&data[pos..]);
    pos += 4;
    for _ in 0..count {
        if pos + 4 > data.len() {
            return false;
        }
        let len = read_u32(&data[pos..]);
        pos += 4;
        if pos + len > data.len() {
            return false;
        }
        if &data[pos..pos + len] == MIX_TAG.as_bytes() {
            return true;
        }
        pos += len;
    }
    false
}

/// Decode an archived stereo Ogg Opus file back into (mic, loopback) mono
/// f32 tracks at 48 kHz, undoing the comfort mix when the file has one
/// (legacy files were hard-panned L=mic / R=loopback).
pub fn decode_opus(path: &Path) -> Result<(Vec<f32>, Vec<f32>)> {
    let file = BufReader::new(
        File::open(path).with_context(|| format!("opening {}", path.display()))?,
    );
    let mut reader = PacketReader::new(file);
    let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Stereo)
        .context("creating Opus decoder")?;

    let mut mic = Vec::new();
    let mut lop = Vec::new();
    let mut pre_skip: usize = 0;
    let mut header_packets = 0;
    let mut mixed = false;
    // Max Opus frame is 120 ms = 5760 samples/channel.
    let mut pcm = vec![0i16; 5760 * 2];

    // Inverse of [main cross; cross main] (determinant 0.5).
    let det = MIX_MAIN * MIX_MAIN - MIX_CROSS * MIX_CROSS;

    while let Some(pkt) = reader.read_packet()? {
        if header_packets < 2 {
            if header_packets == 0 && pkt.data.len() >= 19 && &pkt.data[..8] == b"OpusHead" {
                pre_skip = u16::from_le_bytes([pkt.data[10], pkt.data[11]]) as usize;
            }
            if header_packets == 1 {
                mixed = has_mix_tag(&pkt.data);
            }
            header_packets += 1;
            continue;
        }
        let decoded = decoder
            .decode(Some(&pkt.data), &mut pcm, false)
            .context("Opus decode")?;
        for frame in pcm[..decoded * 2].chunks_exact(2) {
            let left = frame[0] as f32 / 32768.0;
            let right = frame[1] as f32 / 32768.0;
            if mixed {
                mic.push(((MIX_MAIN * left - MIX_CROSS * right) / det).clamp(-1.0, 1.0));
                lop.push(((MIX_MAIN * right - MIX_CROSS * left) / det).clamp(-1.0, 1.0));
            } else {
                mic.push(left);
                lop.push(right);
            }
        }
    }
    // Drop the encoder lookahead the granule accounting says to skip.
    if pre_skip > 0 && pre_skip < mic.len() {
        mic.drain(..pre_skip);
        lop.drain(..pre_skip);
    }
    Ok((mic, lop))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_test_wav(path: &Path, seconds: u32, tone_hz: Option<f32>) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..(48_000 * seconds) {
            let v = match tone_hz {
                Some(hz) => {
                    let t = i as f32 / 48_000.0;
                    ((t * hz * 2.0 * std::f32::consts::PI).sin() * 12_000.0) as i16
                }
                None => 0,
            };
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
    }

    /// Raw (no unmix) per-channel RMS of an archived file.
    fn raw_channel_rms(path: &Path) -> (f32, f32) {
        let file = BufReader::new(File::open(path).unwrap());
        let mut reader = PacketReader::new(file);
        let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Stereo).unwrap();
        let mut pcm = vec![0i16; 5760 * 2];
        let (mut l2, mut r2, mut n) = (0f64, 0f64, 0u64);
        let mut headers = 0;
        while let Some(pkt) = reader.read_packet().unwrap() {
            if headers < 2 {
                headers += 1;
                continue;
            }
            let decoded = decoder.decode(Some(&pkt.data), &mut pcm, false).unwrap();
            for frame in pcm[..decoded * 2].chunks_exact(2) {
                let l = frame[0] as f64 / 32768.0;
                let r = frame[1] as f64 / 32768.0;
                l2 += l * l;
                r2 += r * r;
                n += 1;
            }
        }
        (((l2 / n as f64) as f32).sqrt(), ((r2 / n as f64) as f32).sqrt())
    }

    #[test]
    fn opus_roundtrip_comfort_mix_and_unmix() {
        let dir = std::env::temp_dir().join(format!("witness-opus-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mic = dir.join("mic.wav");
        let lop = dir.join("loop.wav");
        let out = dir.join("out.opus");
        write_test_wav(&mic, 3, Some(440.0)); // tone on the mic track
        write_test_wav(&lop, 3, None); // silence on the loopback track

        encode_opus(&mic, &lop, &out, 40, |_| {}).unwrap();
        assert!(out.exists());
        let size = std::fs::metadata(&out).unwrap().len();
        // 3 s @ ~40 kbps ≈ 15 kB; sanity-check the ballpark (tone compresses well).
        assert!(size > 1_000 && size < 80_000, "unexpected opus size {size}");

        // Playback comfort: the mic tone must be audible in BOTH ears,
        // stronger on the left (0.75 vs 0.25).
        let (raw_l, raw_r) = raw_channel_rms(&out);
        assert!(raw_l > 0.1, "left ear silent (rms {raw_l})");
        assert!(raw_r > 0.03, "right ear silent — comfort mix missing (rms {raw_r})");
        assert!(raw_l > raw_r * 2.0, "expected mic biased left ({raw_l} vs {raw_r})");

        // Retranscription: unmix must recover the separated tracks.
        let (mic_dec, lop_dec) = decode_opus(&out).unwrap();
        let expected = 3 * 48_000;
        assert!(
            (mic_dec.len() as i64 - expected).unsigned_abs() < 960,
            "decoded length {} vs expected {expected}",
            mic_dec.len()
        );
        let rms = |s: &[f32]| (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt();
        let mic_rms = rms(&mic_dec);
        let lop_rms = rms(&lop_dec);
        assert!(mic_rms > 0.1, "mic track lost its tone (rms {mic_rms})");
        assert!(
            lop_rms < 0.05 && lop_rms < mic_rms / 6.0,
            "loopback track leaked audio after unmix (rms {lop_rms} vs mic {mic_rms})"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
