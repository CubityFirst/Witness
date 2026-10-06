//! `OpusArchiveReader`: page index, sample-exact seek and raw stereo decode
//! of the archived Ogg Opus files (design §6.2).
//!
//! The archive timeline (what `encoder::encode_opus` writes, and what every
//! existing archive has):
//! - every audio packet is exactly [`FRAME`] = 960 samples (20 ms @ 48 kHz);
//!   pages end every 50 packets or at end of stream, and no packet continues
//!   across a page;
//! - decoded index `d` counts *all* decoded samples from stream start,
//!   including the `pre_skip` priming samples, so playback sample `s` is
//!   decoded index `pre_skip + s`;
//! - non-final page granules are the decoded-sample count *including*
//!   pre-skip (RFC 7845), so a page's first decoded index is the previous
//!   page's granule;
//! - the final granule is `pre_skip + total_samples`, written without a
//!   lookahead flush, so it can overstate the decodable samples by up to
//!   `pre_skip`. EOF therefore always ends the stream, even short of
//!   `total_samples`.
//!
//! The page index is built from packet counts (lacing values), not
//! granules, so it stays exact even where a granule would mislead.
//!
//! "No packet continues across a page" holds at normal bitrates but not at
//! the top of the Settings range: at 510 kbps a 50-packet page needs more
//! than 255 lacing values and the ogg writer splits a packet over two pages.
//! Such pages are indexed too — after `seek_bytes` the ogg reader drops the
//! leading fragment, so seeking to one resumes at the first packet that
//! *starts* at or after it — and seeks stay exact on every archive.
//!
//! A damaged page (CRC mismatch, truncated body) does not end the stream:
//! the reader resyncs at the next indexed page and fills the lost span with
//! silence, so the timeline (and every later position) stays exact. A
//! packet the Opus decoder rejects is concealed (PLC) the same way.

use crate::encoder::{has_mix_tag, FRAME};
use anyhow::{bail, Context, Result};
use audiopus::coder::Decoder;
use audiopus::{Channels, SampleRate};
use ogg::{OggReadError, PacketReader};
use std::fs::File;
use std::io::{BufReader, ErrorKind, Read, Seek, SeekFrom};
use std::path::Path;

/// Decoder pre-roll before a seek target: 200 ms. RFC 7845 §4.6 asks for
/// at least 80 ms, but on our 40 kbps VoIP archives 80 ms leaves the fresh
/// decoder only ~23 dB from a linear decode; 160 ms reaches ~44 dB and
/// 200 ms ~53 dB (measured on the seek test signal). Costs ≤ 1.2 s of
/// decode per seek — a few milliseconds.
const PREROLL: u64 = 9600;
/// Largest Opus frame (120 ms @ 48 kHz) per channel — the decode scratch size.
const MAX_FRAME: usize = 5760;
/// Packets before the first audio page: OpusHead + OpusTags.
const HEADER_PACKETS: u64 = 2;

/// One audio page of the index: a valid seek point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AudioPage {
    /// Byte offset of the page header ("OggS") in the file.
    pub offset: u64,
    /// Decoded index of the first sample the reader produces after seeking
    /// to this page: FRAME × the audio packets before the first packet that
    /// *starts* at or after it (a leading continued fragment is skipped).
    pub first_decoded: u64,
    /// The page's granule position as stored (`u64::MAX` = no packet ends here).
    pub granule: u64,
    /// FRAME × the audio packets completed by the end of this page — what a
    /// non-final granule must equal.
    pub end_decoded: u64,
    /// The page starts with the tail of a packet from the previous page.
    pub continued: bool,
}

/// Result of [`scan_pages`]: every audio page in stream order.
#[derive(Debug, Default)]
pub(crate) struct PageIndex {
    pub pages: Vec<AudioPage>,
}

impl PageIndex {
    /// Granule of the last page that has one (the end-of-stream granule).
    fn final_granule(&self) -> Option<u64> {
        self.pages
            .iter()
            .rev()
            .map(|p| p.granule)
            .find(|&g| g != u64::MAX)
    }
}

/// Walk the page headers of an Ogg stream (27-byte header + segment table),
/// skipping the bodies, and index every page a seek can resume audio from
/// (the first packet starting at or after it is an audio packet). ~3,600
/// pages per hour of audio; only the headers are read.
///
/// A truncated trailing page simply ends the scan.
pub(crate) fn scan_pages<R: Read + Seek>(r: &mut R) -> Result<PageIndex> {
    let mut index = PageIndex::default();
    let mut offset = r.stream_position()?;
    let mut completed: u64 = 0; // packets completed so far, headers included
    let mut header = [0u8; 27];
    let mut lacing = vec![0u8; 255];
    loop {
        match r.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e).context("reading Ogg page header"),
        }
        if &header[..4] != b"OggS" {
            if completed == 0 {
                bail!("not an Ogg file");
            }
            log::warn!("archive: lost Ogg sync at byte {offset}; ignoring the rest");
            break;
        }
        let header_type = header[5];
        let granule = u64::from_le_bytes(header[6..14].try_into().unwrap());
        let segments = header[26] as usize;
        if r.read_exact(&mut lacing[..segments]).is_err() {
            break; // truncated page
        }
        let body: u64 = lacing[..segments].iter().map(|&l| l as u64).sum();
        // A packet ends at every lacing value < 255.
        let ends = lacing[..segments].iter().filter(|&&l| l < 255).count() as u64;

        let continued = header_type & 0x01 != 0;
        // Packet `completed` is the first to end on this page; when it began
        // on an earlier page, a reader seeked here drops that fragment and
        // yields the next one.
        let first_packet = completed + continued as u64;
        let after = completed + ends;
        if first_packet >= HEADER_PACKETS {
            index.pages.push(AudioPage {
                offset,
                first_decoded: FRAME as u64 * (first_packet - HEADER_PACKETS),
                granule,
                end_decoded: FRAME as u64 * after.saturating_sub(HEADER_PACKETS),
                continued,
            });
        }
        completed = after;

        let next = offset + 27 + segments as u64 + body;
        if r.seek(SeekFrom::Start(next)).is_err() {
            break;
        }
        offset = next;
    }

    // Debug aid: non-final granules must equal the decoded count at the end
    // of the page (packets × FRAME, pre-skip included). The final page is
    // exempt — it carries `pre_skip + total_samples` instead.
    let pages = &index.pages;
    let mismatch = pages.iter().enumerate().find(|(i, page)| {
        i + 1 < pages.len() && page.granule != u64::MAX && page.granule != page.end_decoded
    });
    if let Some((i, page)) = mismatch {
        log::warn!(
            "archive: page {i} granule {} != packet-count index {}",
            page.granule,
            page.end_decoded
        );
    }
    Ok(index)
}

/// Seekable reader over one archived meeting: raw interleaved stereo at
/// 48 kHz, exactly as stored (comfort mix or legacy hard-pan).
pub struct OpusArchiveReader {
    reader: PacketReader<BufReader<File>>,
    decoder: Decoder,
    pre_skip: u64,
    mixed: bool,
    total_samples: u64,
    /// Seek points in stream order (`first_decoded` non-decreasing).
    pages: Vec<AudioPage>,
    /// Decoded index of the next sample the decoder will produce.
    decoded: u64,
    /// Decoded index of the next sample to hand out (`pre_skip + position`).
    /// Below `decoded` only after a resync over a damaged page: that span is
    /// handed out as silence.
    target: u64,
    /// The physical stream (or `total_samples`) has ended.
    eof: bool,
    /// Decode scratch, interleaved stereo (heap, not stack — see CLAUDE.md).
    pcm: Vec<f32>,
}

impl OpusArchiveReader {
    /// Parse OpusHead/OpusTags and index the pages. Cheap: only page headers
    /// are read, no audio is decoded.
    pub fn open(path: &Path) -> Result<Self> {
        let open = || File::open(path).with_context(|| format!("opening {}", path.display()));
        let mut reader = PacketReader::new(BufReader::new(open()?));
        let (pre_skip, mixed) = read_headers(&mut reader)?;

        let index = scan_pages(&mut BufReader::new(open()?))?;
        let total_samples = index
            .final_granule()
            .map(|g| g.saturating_sub(pre_skip))
            .unwrap_or(0);

        Ok(OpusArchiveReader {
            reader,
            // Always decode stereo: a stereo decoder upmixes mono packets.
            decoder: new_decoder()?,
            pre_skip,
            mixed,
            total_samples,
            pages: index.pages,
            decoded: 0,
            target: pre_skip,
            eof: false,
            pcm: vec![0.0; MAX_FRAME * 2],
        })
    }

    /// Samples per channel (48 kHz) as declared by the final granule.
    pub fn total_samples(&self) -> u64 {
        self.total_samples
    }

    /// Duration in milliseconds (what Chromium showed: last granule − pre-skip).
    pub fn duration_ms(&self) -> u64 {
        self.total_samples / 48
    }

    /// The OpusTags carry our comfort-mix marker (`WITNESS_MIX=75`).
    pub fn mixed(&self) -> bool {
        self.mixed
    }

    /// Playback sample index (per channel) of the next sample `read_stereo`
    /// returns.
    pub fn position(&self) -> u64 {
        self.target - self.pre_skip
    }

    /// Decode the next packet and append its samples as interleaved stereo
    /// f32, exactly as stored. Returns the number of frames appended; 0 means
    /// end of stream. Stops at `total_samples`, and also at physical EOF even
    /// when that comes first (the final granule can overstate the length).
    pub fn read_stereo(&mut self, out: &mut Vec<f32>) -> Result<usize> {
        let end = self.pre_skip + self.total_samples;
        loop {
            if self.eof || self.target >= end {
                self.eof = true;
                return Ok(0);
            }
            if self.target < self.decoded {
                // Audio lost to a damaged page: silence keeps the timeline.
                let to = self.decoded.min(end).min(self.target + MAX_FRAME as u64);
                let n = (to - self.target) as usize;
                out.resize(out.len() + n * 2, 0.0);
                self.target = to;
                return Ok(n);
            }
            let packet = match self.reader.read_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => {
                    self.eof = true;
                    return Ok(0);
                }
                Err(e) => {
                    self.resync(&e)?;
                    continue;
                }
            };
            let n = self.decode_packet(&packet.data);
            let start = self.decoded;
            self.decoded += n as u64;
            // Hand out decoded indices in [target, min(decoded, end)): drops
            // pre-skip priming at stream start and the pre-roll after a seek.
            let from = self.target.max(start);
            let to = self.decoded.min(end);
            if from >= to {
                continue;
            }
            let (a, b) = ((from - start) as usize, (to - start) as usize);
            out.extend_from_slice(&self.pcm[a * 2..b * 2]);
            self.target = to;
            return Ok(b - a);
        }
    }

    /// Decode one packet into `self.pcm`; returns frames decoded. An empty
    /// packet is a dropped frame and a packet the decoder rejects is damaged:
    /// both are concealed (Opus PLC, else silence) so the timeline keeps its
    /// length.
    fn decode_packet(&mut self, data: &[u8]) -> usize {
        if !data.is_empty() {
            match self
                .decoder
                .decode_float(Some(data), &mut self.pcm[..], false)
            {
                Ok(n) => return n,
                Err(e) => log::warn!(
                    "archive: undecodable Opus packet near sample {}: {e}; concealed",
                    self.decoded.saturating_sub(self.pre_skip)
                ),
            }
        }
        let frame = &mut self.pcm[..FRAME * 2];
        match self.decoder.decode_float(None::<&[u8]>, &mut *frame, false) {
            Ok(n) => n,
            Err(_) => {
                frame.fill(0.0);
                FRAME
            }
        }
    }

    /// The Ogg layer failed (CRC mismatch, truncated or garbled page): skip
    /// to the next seek point past what has been decoded. `read_stereo`
    /// fills the skipped span with silence. Each resync moves strictly
    /// forward, so a run of bad pages cannot loop. No page left: EOF.
    fn resync(&mut self, err: &OggReadError) -> Result<()> {
        let decoded = self.decoded;
        let next = self.pages.partition_point(|p| p.first_decoded <= decoded);
        let Some(&page) = self.pages.get(next) else {
            log::warn!(
                "archive: damaged Ogg data near sample {} ({err}); ending there",
                decoded.saturating_sub(self.pre_skip)
            );
            self.eof = true;
            return Ok(());
        };
        log::warn!(
            "archive: damaged Ogg data near sample {} ({err}); resuming at sample {}",
            decoded.saturating_sub(self.pre_skip),
            page.first_decoded.saturating_sub(self.pre_skip)
        );
        self.reader
            .seek_bytes(SeekFrom::Start(page.offset))
            .context("seeking archive")?;
        self.decoded = page.first_decoded;
        self.decoder = new_decoder()?;
        Ok(())
    }

    /// Position the stream so the next `read_stereo` starts exactly at
    /// playback sample `sample` (clamped to `total_samples`). The decoder
    /// restarts ≥ 200 ms early on a page boundary so its state has converged:
    /// the position is exact, the samples approximately equal a linear decode.
    pub fn seek(&mut self, sample: u64) -> Result<()> {
        let sample = sample.min(self.total_samples);
        let d = self.pre_skip + sample;
        self.eof = false;
        self.target = d;
        if self.pages.is_empty() {
            self.eof = true;
            return Ok(());
        }
        let goal = d.saturating_sub(PREROLL);
        match self.pages.partition_point(|p| p.first_decoded <= goal) {
            0 => {
                // No seek point early enough (only in a malformed file whose
                // first audio packet starts on a header page): decode
                // linearly from the start.
                log::debug!(
                    "archive: no seek point before sample {sample}; decoding from the start"
                );
                self.reader
                    .seek_bytes(SeekFrom::Start(0))
                    .context("seeking archive")?;
                read_headers(&mut self.reader)?;
                self.decoded = 0;
            }
            k => {
                let page = self.pages[k - 1];
                self.reader
                    .seek_bytes(SeekFrom::Start(page.offset))
                    .context("seeking archive")?;
                self.decoded = page.first_decoded;
            }
        }
        // audiopus 0.2 has no reset_state: a fresh decoder is the reset.
        self.decoder = new_decoder()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn pre_skip(&self) -> u64 {
        self.pre_skip
    }

    #[cfg(test)]
    pub(crate) fn pages(&self) -> &[AudioPage] {
        &self.pages
    }
}

fn new_decoder() -> Result<Decoder> {
    Decoder::new(SampleRate::Hz48000, Channels::Stereo).context("creating Opus decoder")
}

/// Read OpusHead and OpusTags from the start of the stream; returns
/// (`pre_skip`, comfort-mix tag present).
fn read_headers(reader: &mut PacketReader<BufReader<File>>) -> Result<(u64, bool)> {
    let head = reader
        .read_packet()
        .context("reading OpusHead")?
        .context("empty Ogg stream")?;
    if head.data.len() < 19 || &head.data[..8] != b"OpusHead" {
        bail!("not an Ogg Opus file");
    }
    let channels = head.data[9];
    if !(1..=2).contains(&channels) {
        bail!("unsupported Opus channel count {channels}");
    }
    let pre_skip = u16::from_le_bytes([head.data[10], head.data[11]]) as u64;
    let tags = reader
        .read_packet()
        .context("reading OpusTags")?
        .context("Ogg Opus file has no OpusTags")?;
    Ok((pre_skip, has_mix_tag(&tags.data)))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::encoder::encode_opus;
    use std::path::PathBuf;

    /// A scratch dir unique to this test process + name.
    pub(crate) fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("witness-player-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_wav(path: &Path, samples: &[i16]) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for &s in samples {
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
    }

    /// Speech-ish test signal: two gliding tones under a slow AM envelope, so
    /// it is never periodic over a seek window (a pure tone would make a
    /// misaligned seek look right).
    pub(crate) fn test_signal(samples: usize, seed: f32) -> Vec<i16> {
        (0..samples)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                let f1 = 220.0 + 80.0 * (t * 0.7 + seed).sin();
                let f2 = 660.0 + 150.0 * (t * 0.3 + seed * 2.0).cos();
                let env = 0.6 + 0.4 * (t * 3.1 + seed).sin();
                let v = (t * f1 * std::f32::consts::TAU).sin() * 0.6
                    + (t * f2 * std::f32::consts::TAU).sin() * 0.3;
                (v * env * 12_000.0) as i16
            })
            .collect()
    }

    /// Encode a generated archive of `samples` per track with the production
    /// encoder at the default 40 kbps; returns its path.
    pub(crate) fn make_archive(dir: &Path, name: &str, samples: usize) -> PathBuf {
        make_archive_at(dir, name, samples, 40)
    }

    /// [`make_archive`] at a given bitrate.
    pub(crate) fn make_archive_at(dir: &Path, name: &str, samples: usize, kbps: u32) -> PathBuf {
        let mic = dir.join(format!("{name}-mic.wav"));
        let lop = dir.join(format!("{name}-loop.wav"));
        write_wav(&mic, &test_signal(samples, 0.0));
        write_wav(&lop, &test_signal(samples, 1.7));
        let out = dir.join(format!("{name}.opus"));
        encode_opus(&mic, &lop, &out, kbps, |_| {}).unwrap();
        out
    }

    fn decode_all(r: &mut OpusArchiveReader) -> Vec<f32> {
        let mut out = Vec::new();
        while r.read_stereo(&mut out).unwrap() > 0 {}
        out
    }

    #[test]
    fn page_index_matches_non_final_granules() {
        let dir = scratch("index");
        let samples = 10 * 48_000 + 123; // not a whole number of frames
        let path = make_archive(&dir, "ten", samples);
        let r = OpusArchiveReader::open(&path).unwrap();
        let pages = r.pages();
        let packets = samples.div_ceil(FRAME) as u64;
        // 50 packets per page, ~1 s pages.
        assert_eq!(pages.len() as u64, packets.div_ceil(50));
        assert_eq!(pages[0].first_decoded, 0);
        for (i, page) in pages.iter().enumerate() {
            assert!(!page.continued, "page {i} continues a packet");
            if i + 1 < pages.len() {
                assert_eq!(
                    page.end_decoded, page.granule,
                    "page {i}: packet-count index disagrees with its granule"
                );
                assert_eq!(pages[i + 1].first_decoded, page.granule);
            }
        }
        // Final granule = pre_skip + total_samples.
        assert_eq!(r.total_samples(), samples as u64);
        assert_eq!(pages.last().unwrap().granule, r.pre_skip() + samples as u64);
        assert!(r.mixed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    pub(crate) fn snr_db(reference: &[f32], test: &[f32]) -> f64 {
        let signal: f64 = reference.iter().map(|&x| (x as f64).powi(2)).sum();
        let noise: f64 = reference
            .iter()
            .zip(test)
            .map(|(&a, &b)| (a as f64 - b as f64).powi(2))
            .sum();
        10.0 * (signal / noise.max(1e-20)).log10()
    }

    /// Seek to each target and compare the next 4,800 samples with a linear
    /// decode: the position must be exact and the audio within 40 dB SNR.
    fn assert_seeks_match_linear(path: &Path, targets: &[u64]) {
        let mut linear = OpusArchiveReader::open(path).unwrap();
        let reference = decode_all(&mut linear);
        let mut r = OpusArchiveReader::open(path).unwrap();
        for &s in targets {
            r.seek(s).unwrap();
            assert_eq!(r.position(), s, "position after seek to {s}");
            let mut got = Vec::new();
            while got.len() < 4_800 * 2 {
                if r.read_stereo(&mut got).unwrap() == 0 {
                    break;
                }
            }
            let want_end = ((s as usize + 4_800) * 2).min(reference.len());
            let want = &reference[s as usize * 2..want_end];
            got.truncate(want.len());
            assert_eq!(got.len(), want.len(), "seek {s}: short read");
            let snr = snr_db(want, &got);
            assert!(snr > 40.0, "seek {s}: SNR {snr:.1} dB");
        }
        // Clamped past the end: immediate EOF.
        r.seek(u64::MAX).unwrap();
        assert_eq!(r.position(), r.total_samples());
        assert_eq!(r.read_stereo(&mut Vec::new()).unwrap(), 0);
    }

    #[test]
    fn seek_is_sample_exact_and_matches_linear_decode() {
        let dir = scratch("seek");
        let samples = 6 * 48_000;
        let path = make_archive(&dir, "seek", samples);
        let pre_skip = OpusArchiveReader::open(&path).unwrap().pre_skip();
        // Page boundaries, mid-page, inside page 0's pre-roll, the shortest
        // possible pre-roll (target − pre_skip − PREROLL on a page start),
        // odd offsets, backwards jumps and near the end.
        let shortest = |page: u64| page * 48_000 + PREROLL - pre_skip;
        let targets = [
            0u64,
            1,
            959,
            960,
            3_839,
            3_840,
            3_841,
            47_999,
            48_000,
            48_001,
            shortest(1),
            shortest(2) + 1,
            shortest(4) + 7,
            100_003,
            150_000,
            12_345,
            200_017,
            250_000,
            280_000,
            5,
        ];
        assert_seeks_match_linear(&path, &targets);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// At the top of the Settings bitrate range a 50-packet page needs more
    /// than 255 lacing values, so the ogg writer splits packets across
    /// pages. Those pages must still be seek points (no linear fallback).
    #[test]
    fn seeks_stay_indexed_when_packets_span_pages() {
        let dir = scratch("seek510");
        let samples = 6 * 48_000;
        let path = make_archive_at(&dir, "seek510", samples, 510);
        let r = OpusArchiveReader::open(&path).unwrap();
        let pre_skip = r.pre_skip();
        let continued: Vec<AudioPage> = r.pages().iter().copied().filter(|p| p.continued).collect();
        assert!(
            !continued.is_empty(),
            "fixture must split packets across pages at 510 kbps"
        );
        // Every page resumes on a packet boundary, in order.
        for w in r.pages().windows(2) {
            assert!(w[0].first_decoded <= w[1].first_decoded);
            assert_eq!(w[1].first_decoded % FRAME as u64, 0);
        }
        // Targets whose seek lands exactly on (and just after) each split
        // page, plus a spread over the file.
        let mut targets: Vec<u64> = continued
            .iter()
            .filter_map(|p| (p.first_decoded + PREROLL).checked_sub(pre_skip))
            .flat_map(|s| [s, s + 1, s + 959])
            .filter(|&s| s < samples as u64)
            .collect();
        targets.extend([0, 1, 47_999, 100_003, 200_017, 280_000, 12_345]);
        assert_seeks_match_linear(&path, &targets);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A page with a bad CRC mid-file: decoding resyncs at the next page,
    /// the lost span is silence, and the timeline keeps its full length.
    #[test]
    fn damaged_page_is_skipped_without_shifting_the_timeline() {
        let dir = scratch("damaged");
        let samples = 6 * 48_000;
        let path = make_archive(&dir, "damaged", samples);
        let clean = decode_all(&mut OpusArchiveReader::open(&path).unwrap());
        let (pages, pre_skip) = {
            let r = OpusArchiveReader::open(&path).unwrap();
            (r.pages().to_vec(), r.pre_skip())
        };
        // Flip a body byte of page 2 (CRC mismatch).
        let mut bytes = std::fs::read(&path).unwrap();
        let at = pages[2].offset as usize;
        let segments = bytes[at + 26] as usize;
        bytes[at + 27 + segments + 10] ^= 0x5a;
        let bad = dir.join("bad.opus");
        std::fs::write(&bad, &bytes).unwrap();

        let damaged = decode_all(&mut OpusArchiveReader::open(&bad).unwrap());
        assert_eq!(damaged.len(), clean.len(), "timeline length changed");
        let lost = (pages[2].first_decoded - pre_skip) as usize
            ..(pages[3].first_decoded - pre_skip) as usize;
        assert!(damaged[lost.start * 2..lost.end * 2]
            .iter()
            .all(|&v| v == 0.0));
        assert!(clean[lost.start * 2..lost.end * 2]
            .iter()
            .any(|&v| v != 0.0));
        // Well after the resync the audio is the clean decode again.
        let from = (pages[4].first_decoded - pre_skip) as usize * 2;
        let snr = snr_db(&clean[from..], &damaged[from..]);
        assert!(snr > 40.0, "after resync: SNR {snr:.1} dB");
        // And the same holds through a seek past the damage.
        let mut r = OpusArchiveReader::open(&bad).unwrap();
        r.seek(lost.start as u64 - 100).unwrap();
        let mut got = Vec::new();
        while r.read_stereo(&mut got).unwrap() > 0 {}
        assert_eq!(got.len() / 2, clean.len() / 2 - (lost.start - 100));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A legacy (pre-comfort-mix) archive: hard-panned L = mic, R = loopback
    /// and no WITNESS_MIX tag. The mic carries the test signal, the loopback
    /// is silent.
    pub(crate) fn write_legacy_hard_panned(dir: &Path, samples: usize) -> PathBuf {
        use ogg::writing::PacketWriteEndInfo;
        use ogg::PacketWriter;
        let mic = test_signal(samples, 0.0);
        let encoder = audiopus::coder::Encoder::new(
            SampleRate::Hz48000,
            Channels::Stereo,
            audiopus::Application::Voip,
        )
        .unwrap();
        let pre_skip = encoder.lookahead().unwrap() as u16;
        let path = dir.join("legacy.opus");
        let mut w = PacketWriter::new(std::io::BufWriter::new(File::create(&path).unwrap()));
        let mut head = b"OpusHead".to_vec();
        head.push(1);
        head.push(2);
        head.extend_from_slice(&pre_skip.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes());
        head.push(0);
        w.write_packet(head, 1, PacketWriteEndInfo::EndPage, 0)
            .unwrap();
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&7u32.to_le_bytes());
        tags.extend_from_slice(b"witness");
        tags.extend_from_slice(&0u32.to_le_bytes());
        w.write_packet(tags, 1, PacketWriteEndInfo::EndPage, 0)
            .unwrap();
        let mut frame = vec![0i16; FRAME * 2];
        let mut packet = vec![0u8; 4000];
        let mut encoded = 0usize;
        let mut on_page = 0;
        while encoded < samples {
            for i in 0..FRAME {
                frame[i * 2] = mic.get(encoded + i).copied().unwrap_or(0); // mic → L
                frame[i * 2 + 1] = 0; // silent loopback → R
            }
            let n = encoder.encode(&frame, &mut packet).unwrap();
            encoded += FRAME;
            on_page += 1;
            let (granule, end) = if encoded >= samples {
                (
                    pre_skip as u64 + samples as u64,
                    PacketWriteEndInfo::EndStream,
                )
            } else if on_page == 50 {
                on_page = 0;
                (encoded as u64, PacketWriteEndInfo::EndPage)
            } else {
                (encoded as u64, PacketWriteEndInfo::NormalPacket)
            };
            w.write_packet(packet[..n].to_vec(), 1, end, granule)
                .unwrap();
        }
        drop(w);
        path
    }

    #[test]
    fn legacy_untagged_file_plays_as_stored() {
        let dir = scratch("legacy");
        let path = write_legacy_hard_panned(&dir, 2 * 48_000);
        let mut r = OpusArchiveReader::open(&path).unwrap();
        assert!(!r.mixed());
        let pcm = decode_all(&mut r);
        let (mut l2, mut r2) = (0f64, 0f64);
        for f in pcm.chunks_exact(2) {
            l2 += (f[0] as f64).powi(2);
            r2 += (f[1] as f64).powi(2);
        }
        // Hard-panned exactly as stored: no playback matrix bleeds the mic
        // into the right ear (the comfort matrix would put a third there).
        assert!(l2 > 100.0, "left channel lost its signal");
        assert!(
            r2 < l2 * 1e-3,
            "right channel picked up the mic: {r2} vs {l2}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eof_before_total_samples_ends_the_stream() {
        let dir = scratch("eof");
        // 50 packets; the decodable 50 × 960 − pre_skip falls short of the
        // declared total because the encoder never flushes its lookahead.
        let samples = 50 * FRAME - 100;
        let path = make_archive(&dir, "eof", samples);
        let mut r = OpusArchiveReader::open(&path).unwrap();
        let total = r.total_samples();
        assert_eq!(total, samples as u64);
        let decodable = 50 * FRAME as u64 - r.pre_skip();
        assert!(decodable < total, "fixture must exercise the overstatement");
        let pcm = decode_all(&mut r);
        assert_eq!(pcm.len() as u64 / 2, decodable);
        assert!(r.position() < total);
        // Repeated reads at EOF stay at EOF.
        assert_eq!(r.read_stereo(&mut Vec::new()).unwrap(), 0);
        // Seeking into the undecodable tail reports EOF promptly too.
        r.seek(total - 10).unwrap();
        assert_eq!(r.position(), total - 10);
        assert_eq!(r.read_stereo(&mut Vec::new()).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_non_ogg_files() {
        let dir = scratch("bad");
        let path = dir.join("bad.opus");
        std::fs::write(&path, b"definitely not an ogg file at all, just text").unwrap();
        assert!(OpusArchiveReader::open(&path).is_err());
        assert!(OpusArchiveReader::open(&dir.join("missing.opus")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
