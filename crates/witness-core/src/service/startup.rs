//! Startup chores that run inside `AppCore::start`: logging, crash recovery
//! of interrupted recordings/jobs, and the 30-day recycle-bin purge.

use crate::db::Db;
use crate::pipeline::{Job, Pipeline};
use crate::recorder;
use crate::settings::Settings;
use crate::state::AppState;
use std::path::Path;

const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// Days a deleted meeting stays in the recycle bin.
const BIN_RETENTION_DAYS: i64 = 30;

/// Terminal + `data_dir/witness.log` logging at Info. The log rotates once
/// it passes 5 MB, keeping one previous generation (`witness.log.old`).
pub(crate) fn init_logging(data_dir: &Path) {
    use simplelog::{
        ColorChoice, CombinedLogger, Config, LevelFilter, SharedLogger, TermLogger, TerminalMode,
        WriteLogger,
    };
    let log_path = data_dir.join("witness.log");
    // Simple size-capped rotation: keep one previous generation.
    if std::fs::metadata(&log_path)
        .map(|m| m.len() > LOG_ROTATE_BYTES)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&log_path, data_dir.join("witness.log.old"));
    }
    let mut loggers: Vec<Box<dyn SharedLogger>> = vec![TermLogger::new(
        LevelFilter::Info,
        Config::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )];
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        loggers.push(WriteLogger::new(LevelFilter::Info, Config::default(), file));
    }
    let _ = CombinedLogger::init(loggers);
}

/// A crash can leave a WAV whose header undercounts what's on disk (the
/// header is only refreshed on the periodic flush). Patch the RIFF/data
/// sizes from the real file size so nothing recorded is lost.
pub(crate) fn repair_wav_header(path: &Path) {
    use std::io::{Seek, SeekFrom, Write};
    let Ok(mut file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    else {
        return;
    };
    let Ok(meta) = file.metadata() else { return };
    let size = meta.len();
    // hound writes the canonical 44-byte PCM header.
    if size <= 44 {
        return;
    }
    let riff = ((size - 8) as u32).to_le_bytes();
    let data = ((size - 44) as u32).to_le_bytes();
    let _ = file
        .seek(SeekFrom::Start(4))
        .and_then(|_| file.write_all(&riff));
    let _ = file
        .seek(SeekFrom::Start(40))
        .and_then(|_| file.write_all(&data));
}

/// Re-enqueue work interrupted by a crash or quit:
/// - meetings stuck in 'recording' with leftover rec-tmp WAVs → close them
///   out (duration from the WAV) and queue processing;
/// - meetings stuck in 'processing' → queue transcription again;
/// - 'recorded' meetings never encoded → queue (encode-only unless
///   auto-transcribe is on).
pub(crate) fn recover_orphans(db: &Db, settings: &Settings, pipeline: &Pipeline) {
    let rec_dir = settings.rec_tmp_dir();
    if let Ok(entries) = std::fs::read_dir(&rec_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(meta) = toml::from_str::<recorder::RecMeta>(&text) else {
                continue;
            };
            let Ok(Some(meeting)) = db.get_meeting(meta.meeting_id) else {
                // Meeting row is gone; clean up strays.
                let (mic, lop, meta_path) = recorder::wav_paths(&rec_dir, meta.meeting_id);
                for p in [mic, lop, meta_path] {
                    let _ = std::fs::remove_file(p);
                }
                continue;
            };
            if meeting.status == "recording" {
                let (mic, lop, _) = recorder::wav_paths(&rec_dir, meta.meeting_id);
                repair_wav_header(&mic);
                repair_wav_header(&lop);
                let duration_ms = hound::WavReader::open(&mic)
                    .map(|r| r.duration() as i64 * 1000 / r.spec().sample_rate as i64)
                    .unwrap_or(0);
                let ended = chrono::Local::now().to_rfc3339();
                let _ = db.finish_recording(meta.meeting_id, &ended, duration_ms);
                log::info!(
                    "recovered interrupted recording: meeting {} ({duration_ms} ms)",
                    meta.meeting_id
                );
                if let Err(error) = pipeline.enqueue(Job {
                    meeting_id: meta.meeting_id,
                    transcribe: settings.auto_transcribe,
                    engine: None,
                }) {
                    log::error!(
                        "could not queue recovered meeting {}: {error:#}",
                        meta.meeting_id
                    );
                }
            }
        }
    }
    if let Ok(processing) = db.meetings_with_status(&["processing"]) {
        for m in processing {
            log::info!("re-queueing interrupted transcription: meeting {}", m.id);
            if let Err(error) = pipeline.enqueue(Job {
                meeting_id: m.id,
                transcribe: true,
                engine: None,
            }) {
                log::error!("could not re-queue meeting {}: {error:#}", m.id);
            }
        }
    }
    if let Ok(recorded) = db.meetings_with_status(&["recorded"]) {
        for m in recorded.into_iter().filter(|m| m.audio_path.is_none()) {
            // WAVs must still exist for these to be processable.
            let (mic, lop, _) = recorder::wav_paths(&rec_dir, m.id);
            if mic.exists() && lop.exists() {
                log::info!("re-queueing unencoded recording: meeting {}", m.id);
                if let Err(error) = pipeline.enqueue(Job {
                    meeting_id: m.id,
                    transcribe: settings.auto_transcribe,
                    engine: None,
                }) {
                    log::error!("could not re-queue meeting {}: {error:#}", m.id);
                }
            }
        }
    }
}

/// Recycle bin: purge anything deleted more than 30 days ago.
pub(crate) fn purge_expired_bin(state: &AppState) {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(BIN_RETENTION_DAYS);
    let Ok(deleted) = state.db.list_deleted_meetings() else {
        return;
    };
    for (meeting, deleted_at) in deleted {
        let expired = chrono::DateTime::parse_from_rfc3339(&deleted_at)
            .map(|t| t.with_timezone(&chrono::Utc) < cutoff)
            .unwrap_or(false);
        if expired {
            log::info!(
                "recycle bin: purging meeting {} ({})",
                meeting.id,
                meeting.title
            );
            if let Err(e) = super::library::purge_meeting_data(state, meeting.id) {
                log::warn!("purge of meeting {} failed: {e}", meeting.id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_wav_header_patches_sizes_from_file_length() {
        let dir = std::env::temp_dir().join(format!(
            "witness-repair-wav-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mic.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..480 {
            writer.write_sample(0i16).unwrap();
        }
        writer.finalize().unwrap();
        // Simulate a crash: samples on disk past what the header claims.
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(&[0u8; 960]).unwrap();
        }
        assert_eq!(hound::WavReader::open(&path).unwrap().duration(), 480);
        repair_wav_header(&path);
        assert_eq!(hound::WavReader::open(&path).unwrap().duration(), 960);
        let _ = std::fs::remove_dir_all(dir);
    }
}
