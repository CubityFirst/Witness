//! Model registry: presence checks and hf-hub downloads (with progress) into
//! `data_dir/models/hf-cache`. The app is record-only capable with zero
//! models — these are only needed for transcription.

use crate::settings::Engine;
use anyhow::{Context, Result};
use hf_hub::api::sync::ApiBuilder;
use hf_hub::api::Progress;
use hf_hub::Cache;
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const PARAKEET_REPO: &str = "istupakov/parakeet-tdt-0.6b-v3-onnx";
pub const PARAKEET_FILES: &[&str] = &[
    "encoder-model.onnx",
    "encoder-model.onnx.data",
    "decoder_joint-model.onnx",
    "vocab.txt",
];
pub const SORTFORMER_REPO: &str = "altunenes/parakeet-rs";
pub const SORTFORMER_FILE: &str = "diar_streaming_sortformer_4spk-v2.onnx";
pub const WHISPER_REPO: &str = "ggerganov/whisper.cpp";
pub const WHISPER_FILE: &str = "ggml-large-v3-turbo-q5_0.bin";
// 3D-Speaker ERes2Net (VoxCeleb, English), Apache-2.0, 192-dim embeddings.
// Input "x" [N,T,80] kaldi fbank (samples in [-1,1], CMN applied); output
// "embedding" [N,192], NOT L2-normalized.
pub const SPEAKER_ID_REPO: &str = "csukuangfj/speaker-embedding-models";
pub const SPEAKER_ID_FILE: &str = "3dspeaker_speech_eres2net_sv_en_voxceleb_16k.onnx";

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub engine: Engine,
    pub display_name: String,
    pub present: bool,
    pub size_mb: Option<f64>,
}

fn cache_dir(models_dir: &Path) -> PathBuf {
    models_dir.join("hf-cache")
}

fn cached(models_dir: &Path, repo: &str, file: &str) -> Option<PathBuf> {
    Cache::new(cache_dir(models_dir))
        .model(repo.to_string())
        .get(file)
}

/// Directory containing all Parakeet TDT files (the hf-hub snapshot dir),
/// or None if any file is missing.
pub fn parakeet_dir(models_dir: &Path) -> Option<PathBuf> {
    let mut dir = None;
    for file in PARAKEET_FILES {
        let path = cached(models_dir, PARAKEET_REPO, file)?;
        dir = path.parent().map(|p| p.to_path_buf());
    }
    dir
}

pub fn sortformer_path(models_dir: &Path) -> Option<PathBuf> {
    cached(models_dir, SORTFORMER_REPO, SORTFORMER_FILE)
}

pub fn whisper_path(models_dir: &Path) -> Option<PathBuf> {
    cached(models_dir, WHISPER_REPO, WHISPER_FILE)
}

pub fn speaker_id_path(models_dir: &Path) -> Option<PathBuf> {
    cached(models_dir, SPEAKER_ID_REPO, SPEAKER_ID_FILE)
}

fn size_mb(paths: &[PathBuf]) -> Option<f64> {
    let mut total = 0u64;
    for p in paths {
        total += std::fs::metadata(p).ok()?.len();
    }
    Some(total as f64 / 1e6)
}

pub fn status(models_dir: &Path) -> Vec<ModelInfo> {
    let parakeet = parakeet_dir(models_dir);
    let parakeet_paths: Vec<PathBuf> = PARAKEET_FILES
        .iter()
        .filter_map(|f| cached(models_dir, PARAKEET_REPO, f))
        .collect();
    let sortformer = sortformer_path(models_dir);
    let whisper = whisper_path(models_dir);
    vec![
        ModelInfo {
            id: "parakeet".into(),
            engine: Engine::Parakeet,
            display_name: "Parakeet TDT 0.6B v3 (ONNX)".into(),
            present: parakeet.is_some(),
            size_mb: if parakeet.is_some() { size_mb(&parakeet_paths) } else { None },
        },
        ModelInfo {
            id: "sortformer".into(),
            engine: Engine::Parakeet,
            display_name: "Sortformer diarization (4 speakers)".into(),
            present: sortformer.is_some(),
            size_mb: sortformer.as_ref().and_then(|p| size_mb(std::slice::from_ref(p))),
        },
        ModelInfo {
            id: "whisper".into(),
            engine: Engine::Whisper,
            display_name: "Whisper large-v3-turbo (q5_0)".into(),
            present: whisper.is_some(),
            size_mb: whisper.as_ref().and_then(|p| size_mb(std::slice::from_ref(p))),
        },
        {
            let speaker = speaker_id_path(models_dir);
            ModelInfo {
                id: "speaker-id".into(),
                engine: Engine::Parakeet,
                display_name: "Speaker voice prints (ERes2Net)".into(),
                present: speaker.is_some(),
                size_mb: speaker.as_ref().and_then(|p| size_mb(std::slice::from_ref(p))),
            }
        },
    ]
}

/// Bridges hf-hub's Progress trait onto a plain callback
/// (model_id, file, downloaded, total).
struct ProgressBridge<'a> {
    model_id: &'a str,
    file: String,
    downloaded: u64,
    total: u64,
    cb: &'a dyn Fn(&str, &str, u64, Option<u64>),
}

impl Progress for ProgressBridge<'_> {
    fn init(&mut self, size: usize, filename: &str) {
        self.total = size as u64;
        self.file = filename.to_string();
        self.downloaded = 0;
        (self.cb)(self.model_id, &self.file, 0, Some(self.total));
    }

    fn update(&mut self, size: usize) {
        self.downloaded += size as u64;
        // Rate-limit to ~every 4 MB to avoid event spam.
        if self.downloaded % (4 << 20) < size as u64 {
            (self.cb)(self.model_id, &self.file, self.downloaded, Some(self.total));
        }
    }

    fn finish(&mut self) {
        (self.cb)(self.model_id, &self.file, self.total, Some(self.total));
    }
}

fn download_repo(
    models_dir: &Path,
    model_id: &str,
    repo: &str,
    files: &[&str],
    cb: &dyn Fn(&str, &str, u64, Option<u64>),
) -> Result<()> {
    let api = ApiBuilder::new()
        .with_cache_dir(cache_dir(models_dir))
        .with_progress(false)
        .build()
        .context("creating hf-hub client")?;
    let repo_api = api.model(repo.to_string());
    for file in files {
        if cached(models_dir, repo, file).is_some() {
            continue; // already present
        }
        let bridge = ProgressBridge {
            model_id,
            file: file.to_string(),
            downloaded: 0,
            total: 0,
            cb,
        };
        repo_api
            .download_with_progress(file, bridge)
            .with_context(|| format!("downloading {repo}/{file}"))?;
    }
    Ok(())
}

/// Blocking download of just the speaker-embedding model (used by tests).
#[cfg(test)]
pub fn download_speaker_id(models_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(cache_dir(models_dir)).ok();
    download_repo(models_dir, "speaker-id", SPEAKER_ID_REPO, &[SPEAKER_ID_FILE], &|_, _, _, _| {})
}

/// Blocking download of everything the given engine needs.
pub fn download(
    engine: Engine,
    models_dir: &Path,
    cb: impl Fn(&str, &str, u64, Option<u64>),
) -> Result<()> {
    std::fs::create_dir_all(cache_dir(models_dir)).ok();
    match engine {
        Engine::Parakeet => {
            download_repo(models_dir, "parakeet", PARAKEET_REPO, PARAKEET_FILES, &cb)?;
            download_repo(
                models_dir,
                "sortformer",
                SORTFORMER_REPO,
                &[SORTFORMER_FILE],
                &cb,
            )?;
            download_repo(
                models_dir,
                "speaker-id",
                SPEAKER_ID_REPO,
                &[SPEAKER_ID_FILE],
                &cb,
            )?;
        }
        Engine::Whisper => {
            download_repo(models_dir, "whisper", WHISPER_REPO, &[WHISPER_FILE], &cb)?;
        }
    }
    Ok(())
}
