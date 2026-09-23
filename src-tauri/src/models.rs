//! Pinned model registry and verified hf-hub downloads.
//!
//! Model files live in `data_dir/models/hf-cache`. A small, atomically written
//! installation manifest records the exact repository revisions and checksums
//! that were verified. The app remains record-only capable with zero models.

use crate::settings::Engine;
use anyhow::{bail, ensure, Context, Result};
use hf_hub::api::sync::ApiBuilder;
use hf_hub::api::Progress;
use hf_hub::{Cache, Repo, RepoType};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const PARAKEET_REPO: &str = "istupakov/parakeet-tdt-0.6b-v3-onnx";
pub const PARAKEET_REVISION: &str = "8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce";
// NVIDIA Nemotron 3 Diarization (streaming Sortformer v3, 8 speakers),
// OpenMDW-1.1; ONNX export by the parakeet-rs author, validated against NeMo.
pub const SORTFORMER_REPO: &str = "altunenes/parakeet-rs";
pub const SORTFORMER_REVISION: &str = "52f2deee40f20d7dc1f459416e9cd213fb9472df";
pub const SORTFORMER_FILE: &str = "nemotron-3-diarization/nemotron3_diar_v3.onnx";
pub const WHISPER_REPO: &str = "ggerganov/whisper.cpp";
pub const WHISPER_REVISION: &str = "5359861c739e955e79d9a303bcbc70fb988958b1";
pub const WHISPER_FILE: &str = "ggml-large-v3-turbo-q5_0.bin";
// 3D-Speaker ERes2Net (VoxCeleb, English), Apache-2.0, 192-dim embeddings.
// Input "x" [N,T,80] kaldi fbank (samples in [-1,1], CMN applied); output
// "embedding" [N,192], NOT L2-normalized.
pub const SPEAKER_ID_REPO: &str = "csukuangfj/speaker-embedding-models";
pub const SPEAKER_ID_REVISION: &str = "0743f301363dec56491a490f6d6cbc9d67f9a3bf";
pub const SPEAKER_ID_FILE: &str = "3dspeaker_speech_eres2net_sv_en_voxceleb_16k.onnx";

const INSTALL_MANIFEST_VERSION: u32 = 1;
const INSTALL_MANIFEST_FILE: &str = "model-install-manifest-v1.json";
const QUICK_HASH_BYTES: u64 = 64 * 1024;

static MODEL_INSTALL_LOCK: Mutex<()> = Mutex::new(());
static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
struct ExpectedFile {
    path: &'static str,
    size: u64,
    sha256: &'static str,
    /// hf-hub's blob key (the LFS SHA-256, or Git blob SHA-1 for small files).
    cache_etag: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct ModelSpec {
    id: &'static str,
    engine: Engine,
    display_name: &'static str,
    repo: &'static str,
    revision: &'static str,
    files: &'static [ExpectedFile],
}

const PARAKEET_EXPECTED_FILES: &[ExpectedFile] = &[
    ExpectedFile {
        path: "encoder-model.onnx",
        size: 41_770_866,
        sha256: "98a74b21b4cc0017c1e7030319a4a96f4a9506e50f0708f3a516d02a77c96bb1",
        cache_etag: "98a74b21b4cc0017c1e7030319a4a96f4a9506e50f0708f3a516d02a77c96bb1",
    },
    ExpectedFile {
        path: "encoder-model.onnx.data",
        size: 2_435_420_160,
        sha256: "9a22d372c51455c34f13405da2520baefb7125bd16981397561423ed32d24f36",
        cache_etag: "9a22d372c51455c34f13405da2520baefb7125bd16981397561423ed32d24f36",
    },
    ExpectedFile {
        path: "decoder_joint-model.onnx",
        size: 72_520_893,
        sha256: "e978ddf6688527182c10fde2eb4b83068421648985ef23f7a86be732be8706c1",
        cache_etag: "e978ddf6688527182c10fde2eb4b83068421648985ef23f7a86be732be8706c1",
    },
    ExpectedFile {
        path: "vocab.txt",
        size: 93_939,
        sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
        cache_etag: "fc43e1c723e262df60b70e1919614417162d1fe2",
    },
];

const SORTFORMER_EXPECTED_FILES: &[ExpectedFile] = &[ExpectedFile {
    path: SORTFORMER_FILE,
    size: 400_506_656,
    sha256: "915e4fa23b0192ed9fadeb1cdd26847df986d50c92012d177be28d0343bbe03a",
    cache_etag: "915e4fa23b0192ed9fadeb1cdd26847df986d50c92012d177be28d0343bbe03a",
}];

const WHISPER_EXPECTED_FILES: &[ExpectedFile] = &[ExpectedFile {
    path: WHISPER_FILE,
    size: 574_041_195,
    sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
    cache_etag: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
}];

const SPEAKER_ID_EXPECTED_FILES: &[ExpectedFile] = &[ExpectedFile {
    path: SPEAKER_ID_FILE,
    size: 26_485_263,
    sha256: "c59158379255ad66e161679cca6af8d52d51e389e3224ab7d7a7baae295c2db5",
    cache_etag: "c59158379255ad66e161679cca6af8d52d51e389e3224ab7d7a7baae295c2db5",
}];

const MODEL_SPECS: &[ModelSpec] = &[
    ModelSpec {
        id: "parakeet",
        engine: Engine::Parakeet,
        display_name: "Parakeet TDT 0.6B v3 (ONNX)",
        repo: PARAKEET_REPO,
        revision: PARAKEET_REVISION,
        files: PARAKEET_EXPECTED_FILES,
    },
    ModelSpec {
        id: "sortformer",
        engine: Engine::Parakeet,
        display_name: "Nemotron 3 Diarization (8 speakers)",
        repo: SORTFORMER_REPO,
        revision: SORTFORMER_REVISION,
        files: SORTFORMER_EXPECTED_FILES,
    },
    ModelSpec {
        id: "whisper",
        engine: Engine::Whisper,
        display_name: "Whisper large-v3-turbo (q5_0)",
        repo: WHISPER_REPO,
        revision: WHISPER_REVISION,
        files: WHISPER_EXPECTED_FILES,
    },
    ModelSpec {
        id: "speaker-id",
        engine: Engine::Parakeet,
        display_name: "Speaker voice prints (ERes2Net)",
        repo: SPEAKER_ID_REPO,
        revision: SPEAKER_ID_REVISION,
        files: SPEAKER_ID_EXPECTED_FILES,
    },
];

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub engine: Engine,
    pub display_name: String,
    pub present: bool,
    pub size_mb: Option<f64>,
    pub expected_revision: String,
    pub installed_revision: Option<String>,
    pub integrity_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct InstalledManifest {
    schema_version: u32,
    models: BTreeMap<String, InstalledModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstalledModel {
    repo: String,
    revision: String,
    expected_files: Vec<InstalledFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstalledFile {
    path: String,
    size: u64,
    sha256: String,
    cache_etag: String,
    quick_sha256: String,
}

fn spec(id: &str) -> &'static ModelSpec {
    MODEL_SPECS
        .iter()
        .find(|model| model.id == id)
        .expect("model registry id must exist")
}

fn cache_dir(models_dir: &Path) -> PathBuf {
    models_dir.join("hf-cache")
}

fn install_manifest_path(models_dir: &Path) -> PathBuf {
    models_dir.join(INSTALL_MANIFEST_FILE)
}

fn pinned_repo(spec: &ModelSpec) -> Repo {
    Repo::with_revision(
        spec.repo.to_string(),
        RepoType::Model,
        spec.revision.to_string(),
    )
}

fn pinned_cached(models_dir: &Path, spec: &ModelSpec, file: &str) -> Option<PathBuf> {
    Cache::new(cache_dir(models_dir))
        .repo(pinned_repo(spec))
        .get(file)
}

fn repo_cache_dir(models_dir: &Path, spec: &ModelSpec) -> PathBuf {
    cache_dir(models_dir).join(format!("models--{}", spec.repo.replace('/', "--")))
}

fn revision_ref_path(models_dir: &Path, spec: &ModelSpec) -> PathBuf {
    repo_cache_dir(models_dir, spec)
        .join("refs")
        .join(spec.revision)
}

fn blob_path(models_dir: &Path, spec: &ModelSpec, file: &ExpectedFile) -> PathBuf {
    repo_cache_dir(models_dir, spec)
        .join("blobs")
        .join(file.cache_etag)
}

fn snapshot_file_path(models_dir: &Path, spec: &ModelSpec, file: &ExpectedFile) -> PathBuf {
    repo_cache_dir(models_dir, spec)
        .join("snapshots")
        .join(spec.revision)
        .join(file.path)
}

fn incomplete_blob_path(models_dir: &Path, spec: &ModelSpec, file: &ExpectedFile) -> PathBuf {
    let mut path = blob_path(models_dir, spec, file);
    path.set_extension("incomplete");
    path
}

fn load_installed_manifest(models_dir: &Path) -> Result<InstalledManifest> {
    let path = install_manifest_path(models_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(InstalledManifest {
                schema_version: INSTALL_MANIFEST_VERSION,
                models: BTreeMap::new(),
            });
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let manifest: InstalledManifest =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    ensure!(
        manifest.schema_version == INSTALL_MANIFEST_VERSION,
        "unsupported model install manifest version {}",
        manifest.schema_version
    );
    Ok(manifest)
}

fn save_installed_manifest(models_dir: &Path, manifest: &InstalledManifest) -> Result<()> {
    std::fs::create_dir_all(models_dir)
        .with_context(|| format!("creating model directory {}", models_dir.display()))?;
    let destination = install_manifest_path(models_dir);
    let bytes =
        serde_json::to_vec_pretty(manifest).context("serializing model install manifest")?;
    let (temporary_path, mut temporary_file) = create_temporary_file(&destination)?;
    let write_result = (|| -> Result<()> {
        temporary_file
            .write_all(&bytes)
            .with_context(|| format!("writing {}", temporary_path.display()))?;
        temporary_file
            .write_all(b"\n")
            .with_context(|| format!("writing {}", temporary_path.display()))?;
        temporary_file
            .flush()
            .with_context(|| format!("flushing {}", temporary_path.display()))?;
        temporary_file
            .sync_all()
            .with_context(|| format!("syncing {}", temporary_path.display()))?;
        drop(temporary_file);
        replace_file(&temporary_path, &destination)
            .with_context(|| format!("replacing {}", destination.display()))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    write_result
}

pub(crate) fn create_temporary_file(destination: &Path) -> Result<(PathBuf, File)> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let name = destination
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("model-manifest"))
        .to_string_lossy();
    for _ in 0..100 {
        let id = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".{name}.{}.{id}.tmp", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", path.display()))
            }
        }
    }
    bail!(
        "failed to allocate a model manifest temporary file next to {}",
        destination.display()
    )
}

#[cfg(not(windows))]
pub(crate) fn replace_file(temporary_path: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(temporary_path, destination)
}

#[cfg(windows)]
pub(crate) fn replace_file(temporary_path: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let temporary_path: Vec<u16> = temporary_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: Both pointers reference NUL-terminated UTF-16 buffers for the
    // duration of the call, and the flags are documented MoveFileExW values.
    let moved = unsafe {
        MoveFileExW(
            temporary_path.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn digest_reader(mut reader: impl Read) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer).context("reading model file")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn full_sha256(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    digest_reader(BufReader::new(file)).with_context(|| format!("hashing {}", path.display()))
}

pub(crate) fn quick_sha256(path: &Path, size: u64) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(size.to_le_bytes());
    if size <= QUICK_HASH_BYTES * 2 {
        let mut reader = BufReader::new(file);
        let mut buffer = Vec::with_capacity(size as usize);
        reader
            .read_to_end(&mut buffer)
            .with_context(|| format!("reading {}", path.display()))?;
        hasher.update(buffer);
    } else {
        let mut buffer = vec![0u8; QUICK_HASH_BYTES as usize];
        file.read_exact(&mut buffer)
            .with_context(|| format!("reading start of {}", path.display()))?;
        hasher.update(&buffer);
        file.seek(SeekFrom::End(-(QUICK_HASH_BYTES as i64)))
            .with_context(|| format!("seeking in {}", path.display()))?;
        file.read_exact(&mut buffer)
            .with_context(|| format!("reading end of {}", path.display()))?;
        hasher.update(&buffer);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn verify_file(path: &Path, expected: &ExpectedFile, verify_full_hash: bool) -> Result<()> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("reading model metadata for {}", expected.path))?;
    ensure!(
        metadata.is_file(),
        "{} is not a regular file",
        expected.path
    );
    ensure!(
        metadata.len() == expected.size,
        "{} has size {}, expected {}",
        expected.path,
        metadata.len(),
        expected.size
    );
    if verify_full_hash {
        let actual = full_sha256(path)?;
        ensure!(
            actual.eq_ignore_ascii_case(expected.sha256),
            "{} failed SHA-256 verification",
            expected.path
        );
    }
    Ok(())
}

fn installed_entry_matches(spec: &ModelSpec, installed: &InstalledModel) -> Result<()> {
    ensure!(
        installed.repo == spec.repo,
        "installed repository does not match"
    );
    ensure!(
        installed.revision == spec.revision,
        "installed revision does not match"
    );
    ensure!(
        installed.expected_files.len() == spec.files.len(),
        "installed file manifest does not match"
    );
    for expected in spec.files {
        let file = installed
            .expected_files
            .iter()
            .find(|file| file.path == expected.path)
            .with_context(|| format!("{} is absent from the install manifest", expected.path))?;
        ensure!(
            file.size == expected.size
                && file.sha256.eq_ignore_ascii_case(expected.sha256)
                && file.cache_etag.eq_ignore_ascii_case(expected.cache_etag),
            "{} install metadata does not match the pinned manifest",
            expected.path
        );
        ensure!(
            !file.quick_sha256.is_empty(),
            "{} has no integrity fingerprint",
            expected.path
        );
    }
    Ok(())
}

fn verified_paths(
    models_dir: &Path,
    spec: &ModelSpec,
    manifest: &InstalledManifest,
) -> Result<Vec<PathBuf>> {
    let installed = manifest
        .models
        .get(spec.id)
        .with_context(|| format!("{} has no verified installation manifest", spec.id))?;
    installed_entry_matches(spec, installed)?;

    let revision = std::fs::read_to_string(revision_ref_path(models_dir, spec))
        .with_context(|| format!("{} pinned cache revision is missing", spec.id))?;
    ensure!(
        revision.trim() == spec.revision,
        "{} cache points at an unexpected revision",
        spec.id
    );

    let mut paths = Vec::with_capacity(spec.files.len());
    for expected in spec.files {
        let path = pinned_cached(models_dir, spec, expected.path)
            .with_context(|| format!("{} is missing", expected.path))?;
        verify_file(&path, expected, false)?;
        let installed_file = installed
            .expected_files
            .iter()
            .find(|file| file.path == expected.path)
            .expect("installed entry was checked above");
        ensure!(
            quick_sha256(&path, expected.size)? == installed_file.quick_sha256,
            "{} failed its quick integrity check",
            expected.path
        );
        paths.push(path);
    }
    Ok(paths)
}

fn installed_model(spec: &ModelSpec, paths: &[PathBuf]) -> Result<InstalledModel> {
    ensure!(paths.len() == spec.files.len(), "model file count mismatch");
    let mut expected_files = Vec::with_capacity(paths.len());
    for (expected, path) in spec.files.iter().zip(paths) {
        // The download path performs a full SHA-256 check before reaching this
        // point. Recheck cheap structural properties while building the receipt.
        verify_file(path, expected, false)?;
        expected_files.push(InstalledFile {
            path: expected.path.to_string(),
            size: expected.size,
            sha256: expected.sha256.to_string(),
            cache_etag: expected.cache_etag.to_string(),
            quick_sha256: quick_sha256(path, expected.size)?,
        });
    }
    Ok(InstalledModel {
        repo: spec.repo.to_string(),
        revision: spec.revision.to_string(),
        expected_files,
    })
}

/// Directory containing all verified Parakeet TDT files, or `None` if the
/// installed set is missing, incomplete, or fails an integrity check.
pub fn parakeet_dir(models_dir: &Path) -> Option<PathBuf> {
    let manifest = load_installed_manifest(models_dir).ok()?;
    let paths = verified_paths(models_dir, spec("parakeet"), &manifest).ok()?;
    let directory = paths.first()?.parent()?.to_path_buf();
    paths
        .iter()
        .all(|path| path.parent() == Some(directory.as_path()))
        .then_some(directory)
}

pub fn sortformer_path(models_dir: &Path) -> Option<PathBuf> {
    let manifest = load_installed_manifest(models_dir).ok()?;
    verified_paths(models_dir, spec("sortformer"), &manifest)
        .ok()?
        .into_iter()
        .next()
}

pub fn whisper_path(models_dir: &Path) -> Option<PathBuf> {
    let manifest = load_installed_manifest(models_dir).ok()?;
    verified_paths(models_dir, spec("whisper"), &manifest)
        .ok()?
        .into_iter()
        .next()
}

pub fn speaker_id_path(models_dir: &Path) -> Option<PathBuf> {
    let manifest = load_installed_manifest(models_dir).ok()?;
    verified_paths(models_dir, spec("speaker-id"), &manifest)
        .ok()?
        .into_iter()
        .next()
}

pub fn status(models_dir: &Path) -> Vec<ModelInfo> {
    let manifest = load_installed_manifest(models_dir);
    MODEL_SPECS
        .iter()
        .map(|spec| {
            let installed_revision = manifest
                .as_ref()
                .ok()
                .and_then(|manifest| manifest.models.get(spec.id))
                .map(|installed| installed.revision.clone());
            let validation = match &manifest {
                Ok(manifest) => verified_paths(models_dir, spec, manifest),
                Err(error) => Err(anyhow::anyhow!(
                    "model install manifest is invalid: {error}"
                )),
            };
            match validation {
                Ok(paths) => ModelInfo {
                    id: spec.id.into(),
                    engine: spec.engine,
                    display_name: spec.display_name.into(),
                    present: true,
                    size_mb: Some(
                        paths
                            .iter()
                            .filter_map(|path| std::fs::metadata(path).ok())
                            .map(|metadata| metadata.len())
                            .sum::<u64>() as f64
                            / 1e6,
                    ),
                    expected_revision: spec.revision.into(),
                    installed_revision,
                    integrity_error: None,
                },
                Err(error) => ModelInfo {
                    id: spec.id.into(),
                    engine: spec.engine,
                    display_name: spec.display_name.into(),
                    present: false,
                    size_mb: None,
                    expected_revision: spec.revision.into(),
                    installed_revision,
                    integrity_error: Some(error.to_string()),
                },
            }
        })
        .collect()
}

/// Bridges hf-hub's Progress trait onto a plain callback
/// (model_id, file, downloaded, total).
type DownloadProgress<'a> = dyn Fn(&str, &str, u64, Option<u64>) + 'a;

/// Job-wide progress callback: adds the file's 1-based position and the
/// total file count across every repo the download job covers.
type JobProgress<'a> = dyn Fn(&str, &str, u64, Option<u64>, u32, u32) + 'a;

struct ProgressBridge<'a> {
    model_id: &'a str,
    file: String,
    downloaded: u64,
    total: u64,
    cb: &'a DownloadProgress<'a>,
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
        // Rate-limit to approximately every 4 MB to avoid event spam.
        if self.downloaded % (4 << 20) < size as u64 {
            (self.cb)(self.model_id, &self.file, self.downloaded, Some(self.total));
        }
    }

    fn finish(&mut self) {
        (self.cb)(self.model_id, &self.file, self.total, Some(self.total));
    }
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

fn invalidate_cached_file(models_dir: &Path, spec: &ModelSpec, file: &ExpectedFile) -> Result<()> {
    if let Some(pointer) = pinned_cached(models_dir, spec, file.path) {
        remove_file_if_present(&pointer)?;
    }
    remove_file_if_present(&blob_path(models_dir, spec, file))?;
    Ok(())
}

fn repair_pinned_ref(models_dir: &Path, spec: &ModelSpec) -> Result<()> {
    Cache::new(cache_dir(models_dir))
        .repo(pinned_repo(spec))
        .create_ref(spec.revision)
        .with_context(|| format!("creating pinned cache reference for {}", spec.id))
}

fn recover_orphaned_blob(
    models_dir: &Path,
    spec: &ModelSpec,
    expected: &ExpectedFile,
) -> Result<Option<PathBuf>> {
    let blob = blob_path(models_dir, spec, expected);
    if !blob.exists() {
        return Ok(None);
    }
    if verify_file(&blob, expected, true).is_err() {
        remove_file_if_present(&blob)?;
        return Ok(None);
    }

    let pointer = snapshot_file_path(models_dir, spec, expected);
    if let Some(parent) = pointer.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating model snapshot directory {}", parent.display()))?;
    }
    if std::fs::symlink_metadata(&pointer).is_ok() {
        remove_file_if_present(&pointer)?;
    }
    match std::fs::hard_link(&blob, &pointer) {
        Ok(()) => Ok(Some(pointer)),
        Err(_) => {
            // hf-hub cannot atomically rename a completed temporary download
            // over an orphaned blob on every platform. Remove it before retrying.
            remove_file_if_present(&blob)?;
            Ok(None)
        }
    }
}

fn prepare_resumable_download(
    models_dir: &Path,
    spec: &ModelSpec,
    expected: &ExpectedFile,
) -> Result<()> {
    let incomplete = incomplete_blob_path(models_dir, spec, expected);
    if std::fs::metadata(&incomplete).is_ok_and(|metadata| metadata.len() > expected.size) {
        remove_file_if_present(&incomplete)?;
    }
    Ok(())
}

fn fetch_and_verify_file(
    models_dir: &Path,
    spec: &ModelSpec,
    expected: &ExpectedFile,
    repo_api: &hf_hub::api::sync::ApiRepo,
    cb: &DownloadProgress<'_>,
) -> Result<PathBuf> {
    prepare_resumable_download(models_dir, spec, expected)?;
    let bridge = ProgressBridge {
        model_id: spec.id,
        file: expected.path.to_string(),
        downloaded: 0,
        total: 0,
        cb,
    };
    let path = repo_api
        .download_with_progress(expected.path, bridge)
        .with_context(|| {
            format!(
                "downloading {}/{} at {}",
                spec.repo, expected.path, spec.revision
            )
        })?;
    if let Err(error) = verify_file(&path, expected, true) {
        invalidate_cached_file(models_dir, spec, expected)?;
        return Err(error).with_context(|| {
            format!(
                "verifying {}/{} at {}",
                spec.repo, expected.path, spec.revision
            )
        });
    }
    Ok(path)
}

fn download_repo(
    models_dir: &Path,
    spec: &ModelSpec,
    manifest: &mut InstalledManifest,
    file_offset: u32,
    file_count: u32,
    cb: &JobProgress<'_>,
) -> Result<()> {
    repair_pinned_ref(models_dir, spec)?;
    let api = ApiBuilder::new()
        .with_cache_dir(cache_dir(models_dir))
        .with_progress(false)
        .build()
        .context("creating hf-hub client")?;
    let repo_api = api.repo(pinned_repo(spec));
    let mut paths = Vec::with_capacity(spec.files.len());

    for (index, expected) in spec.files.iter().enumerate() {
        let file_index = file_offset + index as u32 + 1;
        let file_cb = |model_id: &str, file: &str, downloaded: u64, total: Option<u64>| {
            cb(model_id, file, downloaded, total, file_index, file_count)
        };
        let cached_path = pinned_cached(models_dir, spec, expected.path);
        let path = match cached_path {
            Some(path) => match verify_file(&path, expected, true) {
                Ok(()) => path,
                Err(_) => {
                    invalidate_cached_file(models_dir, spec, expected)?;
                    fetch_and_verify_file(models_dir, spec, expected, &repo_api, &file_cb)?
                }
            },
            None => match recover_orphaned_blob(models_dir, spec, expected)? {
                Some(path) => path,
                None => fetch_and_verify_file(models_dir, spec, expected, &repo_api, &file_cb)?,
            },
        };
        paths.push(path);
    }

    manifest.schema_version = INSTALL_MANIFEST_VERSION;
    manifest
        .models
        .insert(spec.id.to_string(), installed_model(spec, &paths)?);
    save_installed_manifest(models_dir, manifest)
}

fn manifest_for_download(models_dir: &Path) -> InstalledManifest {
    match load_installed_manifest(models_dir) {
        Ok(manifest) => manifest,
        Err(error) => {
            log::warn!("discarding invalid model install manifest: {error:#}");
            InstalledManifest {
                schema_version: INSTALL_MANIFEST_VERSION,
                models: BTreeMap::new(),
            }
        }
    }
}

/// Blocking download of just the speaker-embedding model (used by tests).
#[cfg(test)]
pub fn download_speaker_id(models_dir: &Path) -> Result<()> {
    let _guard = MODEL_INSTALL_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("model installation lock is poisoned"))?;
    std::fs::create_dir_all(cache_dir(models_dir)).with_context(|| {
        format!(
            "creating model cache at {}",
            cache_dir(models_dir).display()
        )
    })?;
    let mut manifest = manifest_for_download(models_dir);
    download_repo(
        models_dir,
        spec("speaker-id"),
        &mut manifest,
        0,
        1,
        &|_, _, _, _, _, _| {},
    )
}

/// Blocking download of everything the given engine needs. Downloads use
/// hf-hub's resumable temporary blobs; files become usable only after their
/// pinned SHA-256 checksums pass and the install manifest is atomically saved.
pub fn download(
    engine: Engine,
    models_dir: &Path,
    cb: impl Fn(&str, &str, u64, Option<u64>, u32, u32),
) -> Result<()> {
    let _guard = MODEL_INSTALL_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("model installation lock is poisoned"))?;
    std::fs::create_dir_all(cache_dir(models_dir)).with_context(|| {
        format!(
            "creating model cache at {}",
            cache_dir(models_dir).display()
        )
    })?;
    let mut manifest = manifest_for_download(models_dir);
    match engine {
        Engine::Parakeet => {
            let ids = ["parakeet", "sortformer", "speaker-id"];
            let file_count: u32 = ids.iter().map(|id| spec(id).files.len() as u32).sum();
            let mut file_offset = 0;
            for id in ids {
                download_repo(
                    models_dir,
                    spec(id),
                    &mut manifest,
                    file_offset,
                    file_count,
                    &cb,
                )?;
                file_offset += spec(id).files.len() as u32;
            }
        }
        Engine::Whisper => {
            let file_count = spec("whisper").files.len() as u32;
            download_repo(
                models_dir,
                spec("whisper"),
                &mut manifest,
                0,
                file_count,
                &cb,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_FILES: &[ExpectedFile] = &[ExpectedFile {
        path: "model.bin",
        size: 3,
        sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        cache_etag: "test-etag",
    }];
    const TEST_SPEC: ModelSpec = ModelSpec {
        id: "test",
        engine: Engine::Parakeet,
        display_name: "Test model",
        repo: "witness/test-model",
        revision: "0123456789abcdef0123456789abcdef01234567",
        files: TEST_FILES,
    };

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let id = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "witness-model-manifest-test-{}-{id}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("create isolated model test directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn install_test_model(directory: &Path) -> (InstalledManifest, PathBuf) {
        let snapshot = repo_cache_dir(directory, &TEST_SPEC)
            .join("snapshots")
            .join(TEST_SPEC.revision);
        std::fs::create_dir_all(&snapshot).unwrap();
        let path = snapshot.join(TEST_FILES[0].path);
        std::fs::write(&path, b"abc").unwrap();
        verify_file(&path, &TEST_FILES[0], true).unwrap();
        repair_pinned_ref(directory, &TEST_SPEC).unwrap();
        let mut manifest = InstalledManifest {
            schema_version: INSTALL_MANIFEST_VERSION,
            models: BTreeMap::new(),
        };
        manifest.models.insert(
            TEST_SPEC.id.into(),
            installed_model(&TEST_SPEC, std::slice::from_ref(&path)).unwrap(),
        );
        (manifest, path)
    }

    #[test]
    fn verified_model_requires_exact_revision_and_manifest() {
        let directory = TestDir::new();
        let (manifest, path) = install_test_model(&directory.0);

        assert_eq!(
            verified_paths(&directory.0, &TEST_SPEC, &manifest).unwrap(),
            vec![path]
        );
        assert_eq!(manifest.models[TEST_SPEC.id].revision, TEST_SPEC.revision);
        assert_eq!(
            manifest.models[TEST_SPEC.id].expected_files[0].sha256,
            TEST_FILES[0].sha256
        );
    }

    #[test]
    fn incomplete_model_is_not_present() {
        let directory = TestDir::new();
        let (manifest, path) = install_test_model(&directory.0);
        std::fs::remove_file(path).unwrap();

        let error = verified_paths(&directory.0, &TEST_SPEC, &manifest).unwrap_err();
        assert!(error.to_string().contains("missing"));
    }

    #[test]
    fn same_size_corruption_fails_quick_integrity_check() {
        let directory = TestDir::new();
        let (manifest, path) = install_test_model(&directory.0);
        std::fs::write(path, b"abd").unwrap();

        let error = verified_paths(&directory.0, &TEST_SPEC, &manifest).unwrap_err();
        assert!(error.to_string().contains("quick integrity"));
    }

    #[test]
    fn wrong_revision_is_rejected() {
        let directory = TestDir::new();
        let (mut manifest, _) = install_test_model(&directory.0);
        manifest.models.get_mut(TEST_SPEC.id).unwrap().revision = "main".into();

        let error = verified_paths(&directory.0, &TEST_SPEC, &manifest).unwrap_err();
        assert!(error.to_string().contains("revision"));
    }

    #[test]
    fn installation_manifest_round_trips_through_atomic_save() {
        let directory = TestDir::new();
        let (manifest, _) = install_test_model(&directory.0);

        save_installed_manifest(&directory.0, &manifest).unwrap();
        let loaded = load_installed_manifest(&directory.0).unwrap();

        assert_eq!(loaded.schema_version, INSTALL_MANIFEST_VERSION);
        assert_eq!(loaded.models[TEST_SPEC.id].repo, TEST_SPEC.repo);
        assert_eq!(
            loaded.models[TEST_SPEC.id].expected_files[0].quick_sha256,
            manifest.models[TEST_SPEC.id].expected_files[0].quick_sha256
        );
        let temporary_files = std::fs::read_dir(&directory.0)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(temporary_files, 0);
    }

    #[test]
    fn orphaned_verified_blob_is_recovered_without_network() {
        let directory = TestDir::new();
        repair_pinned_ref(&directory.0, &TEST_SPEC).unwrap();
        let blob = blob_path(&directory.0, &TEST_SPEC, &TEST_FILES[0]);
        std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
        std::fs::write(&blob, b"abc").unwrap();

        let pointer = recover_orphaned_blob(&directory.0, &TEST_SPEC, &TEST_FILES[0]).unwrap();

        assert_eq!(
            pointer,
            pinned_cached(&directory.0, &TEST_SPEC, TEST_FILES[0].path)
        );
        assert_eq!(std::fs::read(pointer.unwrap()).unwrap(), b"abc");
    }

    #[test]
    fn invalid_oversized_resume_is_removed() {
        let directory = TestDir::new();
        let incomplete = incomplete_blob_path(&directory.0, &TEST_SPEC, &TEST_FILES[0]);
        std::fs::create_dir_all(incomplete.parent().unwrap()).unwrap();
        std::fs::write(&incomplete, b"oversized").unwrap();

        prepare_resumable_download(&directory.0, &TEST_SPEC, &TEST_FILES[0]).unwrap();

        assert!(!incomplete.exists());
    }
}
