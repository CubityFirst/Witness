//! App-managed CUDA runtime + cuDNN installation ("Download GPU libraries").
//!
//! ort's CUDA execution provider needs cuDNN 9 and the CUDA 13 runtime DLLs
//! at load time (see gpu.rs). Developers have them via the CUDA toolkit;
//! ordinary users with only a display driver do not, and NVIDIA offers no
//! end-user installer for just these libraries. This module downloads the
//! DLLs from NVIDIA's official PyPI wheels — immutable, versioned,
//! SHA-256-pinned archives — and extracts exactly the needed DLLs into
//! `data_dir/cuda/`, which gpu::preflight() then puts on the loader path.
//!
//! The design mirrors models.rs: a pinned registry, verified downloads, an
//! atomically written install manifest with quick integrity fingerprints,
//! and a Repair path when verification fails. Downloads resume via HTTP
//! Range requests; a file becomes usable only after its pinned archive
//! hash passed and the manifest was saved.

use crate::models::{create_temporary_file, quick_sha256, replace_file};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MANIFEST_VERSION: u32 = 1;
const MANIFEST_FILE: &str = "gpu-libs-manifest-v1.json";
const PROGRESS_STEP_BYTES: u64 = 4 << 20;

static INSTALL_LOCK: Mutex<()> = Mutex::new(());

/// One NVIDIA-published wheel (a zip archive) and the DLLs Witness needs
/// from it. URLs point at files.pythonhosted.org, which is immutable per
/// filename; sizes and SHA-256 digests come from the PyPI release metadata
/// and were re-verified against the downloaded archives when pinned.
struct GpuLibArchive {
    id: &'static str,
    /// Wheel filename; also the on-disk download name inside `cuda/`.
    file: &'static str,
    url: &'static str,
    size: u64,
    sha256: &'static str,
    dlls: &'static [&'static str],
}

const ARCHIVES: &[GpuLibArchive] = &[
    GpuLibArchive {
        id: "cudnn",
        file: "nvidia_cudnn_cu13-9.24.0.43-py3-none-win_amd64.whl",
        url: "https://files.pythonhosted.org/packages/31/23/1dd3aa15cc4ab62c8fc88f8049ef137bc44c17892f5577bc80d994941f77/nvidia_cudnn_cu13-9.24.0.43-py3-none-win_amd64.whl",
        size: 412_314_771,
        sha256: "67a7273b5cf062f9446fd76cf464351a1c0f66501e6cd78f6675c0d604d8ac87",
        // cudnn64_9 is a shim that loads the sub-libraries at run time — the
        // whole set ships or convolutions fail mid-inference.
        dlls: crate::gpu::CUDNN_DLLS,
    },
    GpuLibArchive {
        id: "cublas",
        file: "nvidia_cublas-13.6.0.2-py3-none-win_amd64.whl",
        url: "https://files.pythonhosted.org/packages/08/8f/890a96ea1ff615100296977cce23296052dcb8c114d4e451201ec39df9bf/nvidia_cublas-13.6.0.2-py3-none-win_amd64.whl",
        size: 394_568_225,
        sha256: "3b5bcd6bfb6f65010ebf195851bcb9b2aa34b9fe08479432002991c1fe84b67d",
        dlls: &["cublas64_13.dll", "cublasLt64_13.dll"],
    },
    GpuLibArchive {
        id: "cufft",
        file: "nvidia_cufft-12.3.0.29-py3-none-win_amd64.whl",
        url: "https://files.pythonhosted.org/packages/94/64/8e9d808720559d3cbfcd1d1bc8a2e6f55deb29d692513d5a93c8d417b7e5/nvidia_cufft-12.3.0.29-py3-none-win_amd64.whl",
        size: 183_939_745,
        sha256: "510036a2bbab5c83ae93dc5c907c3a49d3518e3066ac3a2052ff0f7f9b27dfc4",
        dlls: &["cufft64_12.dll"],
    },
    GpuLibArchive {
        id: "nvrtc",
        // cuDNN's runtime-compiled fusion engines want NVRTC; without it they
        // silently fall back to precompiled engines, which is usually fine
        // but measurably slower on some graphs. 45 MB of insurance.
        file: "nvidia_cuda_nvrtc-13.3.33-py3-none-win_amd64.whl",
        url: "https://files.pythonhosted.org/packages/a1/42/edce72f2c5a0f587168109c867f25f4a9a6cd7289ecf0d68ed2b1070f273/nvidia_cuda_nvrtc-13.3.33-py3-none-win_amd64.whl",
        size: 45_319_163,
        sha256: "7d2af818851c0c224d5f92221e9226e51ee23c236df4b51f9194563979c888be",
        dlls: &["nvrtc64_130_0.dll", "nvrtc-builtins64_133.dll"],
    },
    GpuLibArchive {
        id: "nvjitlink",
        // cuFFT declares this package as a dependency and dynamically loads
        // it for LTO kernels; direct provider import inspection cannot see it.
        file: "nvidia_nvjitlink-13.3.33-py3-none-win_amd64.whl",
        url: "https://files.pythonhosted.org/packages/67/f2/ec9c05a108095828dfc58840978c627b3c313fdf2a567c6de9ffbbb46901/nvidia_nvjitlink-13.3.33-py3-none-win_amd64.whl",
        size: 37_766_359,
        sha256: "4297ee49639b4f2e07255a1d69b3acc7ab2d011bb892b403e91ac98368962e3b",
        dlls: &["nvJitLink_130_0.dll"],
    },
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GpuLibsManifest {
    schema_version: u32,
    archives: BTreeMap<String, InstalledArchive>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstalledArchive {
    url: String,
    sha256: String,
    dlls: Vec<InstalledDll>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstalledDll {
    name: String,
    size: u64,
    sha256: String,
    quick_sha256: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GpuLibsStatus {
    pub present: bool,
    pub size_mb: Option<f64>,
    pub integrity_error: Option<String>,
}

/// Total download size of every pinned archive, for the Settings button.
pub fn download_size_bytes() -> u64 {
    ARCHIVES.iter().map(|archive| archive.size).sum()
}

fn manifest_path(cuda_dir: &Path) -> PathBuf {
    cuda_dir.join(MANIFEST_FILE)
}

fn load_manifest(cuda_dir: &Path) -> Result<GpuLibsManifest> {
    let path = manifest_path(cuda_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(GpuLibsManifest {
                schema_version: MANIFEST_VERSION,
                archives: BTreeMap::new(),
            });
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let manifest: GpuLibsManifest =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    ensure!(
        manifest.schema_version == MANIFEST_VERSION,
        "unsupported GPU library manifest version {}",
        manifest.schema_version
    );
    Ok(manifest)
}

fn save_manifest(cuda_dir: &Path, manifest: &GpuLibsManifest) -> Result<()> {
    std::fs::create_dir_all(cuda_dir)
        .with_context(|| format!("creating {}", cuda_dir.display()))?;
    let destination = manifest_path(cuda_dir);
    let bytes = serde_json::to_vec_pretty(manifest).context("serializing GPU library manifest")?;
    let (temporary_path, mut temporary_file) = create_temporary_file(&destination)?;
    let write_result = (|| -> Result<()> {
        temporary_file
            .write_all(&bytes)
            .and_then(|()| temporary_file.write_all(b"\n"))
            .and_then(|()| temporary_file.flush())
            .and_then(|()| temporary_file.sync_all())
            .with_context(|| format!("writing {}", temporary_path.display()))?;
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

/// Every pinned DLL passes its quick integrity check against a manifest that
/// matches the current registry pins.
fn verify_installed(cuda_dir: &Path, manifest: &GpuLibsManifest) -> Result<Vec<PathBuf>> {
    verify_archives(cuda_dir, manifest, ARCHIVES)
}

fn verify_archives(
    cuda_dir: &Path,
    manifest: &GpuLibsManifest,
    archives: &[GpuLibArchive],
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for archive in archives {
        let installed = manifest
            .archives
            .get(archive.id)
            .with_context(|| format!("{} has not been installed", archive.id))?;
        ensure!(
            installed.url == archive.url && installed.sha256.eq_ignore_ascii_case(archive.sha256),
            "{} was installed from a superseded release — download again",
            archive.id
        );
        ensure!(
            installed.dlls.len() == archive.dlls.len(),
            "{} install record does not match the pinned file list",
            archive.id
        );
        for name in archive.dlls {
            let dll = installed
                .dlls
                .iter()
                .find(|dll| dll.name == *name)
                .with_context(|| format!("{name} is absent from the install record"))?;
            let path = cuda_dir.join(name);
            let metadata =
                std::fs::metadata(&path).with_context(|| format!("{name} is missing"))?;
            ensure!(metadata.is_file(), "{name} is not a regular file");
            ensure!(
                metadata.len() == dll.size,
                "{name} has size {}, expected {}",
                metadata.len(),
                dll.size
            );
            ensure!(
                quick_sha256(&path, dll.size)? == dll.quick_sha256,
                "{name} failed its quick integrity check"
            );
            paths.push(path);
        }
    }
    Ok(paths)
}

pub fn status(cuda_dir: &Path) -> GpuLibsStatus {
    let verified =
        load_manifest(cuda_dir).and_then(|manifest| verify_installed(cuda_dir, &manifest));
    match verified {
        Ok(paths) => GpuLibsStatus {
            present: true,
            size_mb: Some(
                paths
                    .iter()
                    .filter_map(|path| std::fs::metadata(path).ok())
                    .map(|metadata| metadata.len())
                    .sum::<u64>() as f64
                    / 1e6,
            ),
            integrity_error: None,
        },
        Err(error) => GpuLibsStatus {
            present: false,
            size_mb: None,
            // "not installed" is the normal state, not an integrity problem.
            integrity_error: (manifest_path(cuda_dir).exists()
                || cuda_dir.join("cudnn64_9.dll").exists())
            .then(|| error.to_string()),
        },
    }
}

/// The managed directory when its contents verify, for gpu::preflight().
pub fn dir_if_ready(cuda_dir: &Path) -> Option<PathBuf> {
    let manifest = load_manifest(cuda_dir).ok()?;
    verify_installed(cuda_dir, &manifest).ok()?;
    Some(cuda_dir.to_path_buf())
}

/// Same callback shape as models.rs: (archive_id, file, downloaded, total).
type DownloadProgress<'a> = dyn Fn(&str, &str, u64, Option<u64>) + 'a;

fn stream_sha256(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Download the archive with Range-based resume; only a verified archive is
/// left at the returned path.
fn download_archive(
    cuda_dir: &Path,
    archive: &GpuLibArchive,
    cb: &DownloadProgress<'_>,
) -> Result<PathBuf> {
    let final_path = cuda_dir.join(archive.file);
    let partial_path = cuda_dir.join(format!("{}.partial", archive.file));

    // A fully verified archive from an interrupted earlier run.
    if final_path.is_file() {
        if std::fs::metadata(&final_path)?.len() == archive.size
            && stream_sha256(&final_path)?.eq_ignore_ascii_case(archive.sha256)
        {
            return Ok(final_path);
        }
        std::fs::remove_file(&final_path)
            .with_context(|| format!("removing corrupt {}", final_path.display()))?;
    }

    // Two passes: resume once, then restart from scratch if the resumed file
    // fails verification (a changed server file cannot happen — the URL is
    // immutable — so a hash failure means local corruption).
    for attempt in 0..2 {
        let mut existing = std::fs::metadata(&partial_path)
            .map(|m| m.len())
            .unwrap_or(0);
        if existing > archive.size || attempt > 0 {
            let _ = std::fs::remove_file(&partial_path);
            existing = 0;
        }

        if existing < archive.size {
            let request = ureq::get(archive.url);
            let request = if existing > 0 {
                request.set("Range", &format!("bytes={existing}-"))
            } else {
                request
            };
            let response = request
                .call()
                .with_context(|| format!("requesting {}", archive.url))?;
            let mut file = match response.status() {
                206 => OpenOptions::new()
                    .append(true)
                    .open(&partial_path)
                    .with_context(|| format!("opening {}", partial_path.display()))?,
                200 => {
                    existing = 0;
                    File::create(&partial_path)
                        .with_context(|| format!("creating {}", partial_path.display()))?
                }
                status => bail!("unexpected HTTP status {status} for {}", archive.url),
            };

            let mut reader = response.into_reader();
            let mut written = existing;
            let mut last_report = 0u64;
            let mut buffer = [0u8; 256 * 1024];
            cb(archive.id, archive.file, written, Some(archive.size));
            loop {
                let read = reader
                    .read(&mut buffer)
                    .with_context(|| format!("downloading {}", archive.file))?;
                if read == 0 {
                    break;
                }
                written += read as u64;
                ensure!(
                    written <= archive.size,
                    "{} is larger than the pinned size",
                    archive.file
                );
                file.write_all(&buffer[..read])
                    .with_context(|| format!("writing {}", partial_path.display()))?;
                if written - last_report >= PROGRESS_STEP_BYTES {
                    last_report = written;
                    cb(archive.id, archive.file, written, Some(archive.size));
                }
            }
            file.flush()
                .and_then(|()| file.sync_all())
                .with_context(|| format!("syncing {}", partial_path.display()))?;
            cb(archive.id, archive.file, written, Some(archive.size));
        }

        let complete = std::fs::metadata(&partial_path)
            .map(|m| m.len())
            .unwrap_or(0)
            == archive.size
            && stream_sha256(&partial_path)?.eq_ignore_ascii_case(archive.sha256);
        if complete {
            replace_file(&partial_path, &final_path)
                .with_context(|| format!("publishing {}", final_path.display()))?;
            return Ok(final_path);
        }
        log::warn!(
            "{} failed size/hash verification after download (attempt {})",
            archive.file,
            attempt + 1
        );
    }
    bail!(
        "{} did not verify against its pinned SHA-256 after retrying",
        archive.file
    )
}

/// Extract the archive's pinned DLLs into `cuda_dir`, hashing while copying.
/// Entries are matched by basename, so wheel layout changes don't matter, and
/// the destination name is always our own pinned basename (no zip-slip).
fn extract_dlls(
    cuda_dir: &Path,
    archive: &GpuLibArchive,
    zip_path: &Path,
) -> Result<Vec<InstalledDll>> {
    let file = File::open(zip_path).with_context(|| format!("opening {}", zip_path.display()))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file))
        .with_context(|| format!("reading archive {}", zip_path.display()))?;

    let mut index_by_name: BTreeMap<&'static str, usize> = BTreeMap::new();
    for index in 0..zip.len() {
        let entry = zip.by_index_raw(index).context("listing archive")?;
        let basename = entry
            .name()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_string();
        if let Some(wanted) = archive.dlls.iter().find(|dll| **dll == basename) {
            ensure!(
                index_by_name.insert(wanted, index).is_none(),
                "{basename} appears more than once in {}",
                archive.file
            );
        }
    }

    let mut installed = Vec::with_capacity(archive.dlls.len());
    for name in archive.dlls {
        let index = *index_by_name
            .get(name)
            .with_context(|| format!("{name} is missing from {}", archive.file))?;
        let mut entry = zip
            .by_index(index)
            .with_context(|| format!("opening {name}"))?;
        let destination = cuda_dir.join(name);
        let (temporary_path, mut temporary_file) = create_temporary_file(&destination)?;
        let extract_result = (|| -> Result<(u64, String)> {
            let mut hasher = Sha256::new();
            let mut size = 0u64;
            let mut buffer = [0u8; 1024 * 1024];
            loop {
                let read = entry
                    .read(&mut buffer)
                    .with_context(|| format!("decompressing {name}"))?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
                temporary_file
                    .write_all(&buffer[..read])
                    .with_context(|| format!("writing {}", temporary_path.display()))?;
                size += read as u64;
            }
            temporary_file
                .flush()
                .and_then(|()| temporary_file.sync_all())
                .with_context(|| format!("syncing {}", temporary_path.display()))?;
            Ok((size, hex::encode(hasher.finalize())))
        })();
        let (size, sha256) = match extract_result {
            Ok(result) => result,
            Err(error) => {
                let _ = std::fs::remove_file(&temporary_path);
                return Err(error);
            }
        };
        drop(entry);
        if let Err(error) = replace_file(&temporary_path, &destination)
            .with_context(|| format!("installing {name}"))
        {
            let _ = std::fs::remove_file(&temporary_path);
            return Err(error);
        }
        installed.push(InstalledDll {
            name: (*name).to_string(),
            size,
            sha256,
            quick_sha256: quick_sha256(&destination, size)?,
        });
    }
    Ok(installed)
}

fn archive_is_installed(
    cuda_dir: &Path,
    archive: &GpuLibArchive,
    manifest: &GpuLibsManifest,
) -> bool {
    let Some(installed) = manifest.archives.get(archive.id) else {
        return false;
    };
    if installed.url != archive.url || !installed.sha256.eq_ignore_ascii_case(archive.sha256) {
        return false;
    }
    if installed.dlls.len() != archive.dlls.len() {
        return false;
    }
    archive.dlls.iter().all(|name| {
        installed.dlls.iter().any(|dll| {
            dll.name == *name
                && std::fs::metadata(cuda_dir.join(name))
                    .is_ok_and(|metadata| metadata.len() == dll.size)
                && quick_sha256(&cuda_dir.join(name), dll.size)
                    .is_ok_and(|quick| quick == dll.quick_sha256)
        })
    })
}

/// Blocking download + install of every pinned archive. Idempotent: archives
/// whose DLLs already verify are skipped, so Repair only refetches what is
/// broken. The manifest is saved after each archive, making the whole
/// operation resumable at archive granularity. The callback matches
/// models.rs job-wide downloads: (id, file, downloaded, total, file_index,
/// file_count), where each wheel archive counts as one file.
pub fn download(
    cuda_dir: &Path,
    cb: impl Fn(&str, &str, u64, Option<u64>, u32, u32),
) -> Result<()> {
    let _guard = INSTALL_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("GPU library install lock is poisoned"))?;
    std::fs::create_dir_all(cuda_dir)
        .with_context(|| format!("creating {}", cuda_dir.display()))?;
    let mut manifest = match load_manifest(cuda_dir) {
        Ok(manifest) => manifest,
        Err(error) => {
            log::warn!("discarding invalid GPU library manifest: {error:#}");
            GpuLibsManifest {
                schema_version: MANIFEST_VERSION,
                archives: BTreeMap::new(),
            }
        }
    };

    let file_count = ARCHIVES.len() as u32;
    for (index, archive) in ARCHIVES.iter().enumerate() {
        if archive_is_installed(cuda_dir, archive, &manifest) {
            continue;
        }
        let file_index = index as u32 + 1;
        let archive_cb = |id: &str, file: &str, downloaded: u64, total: Option<u64>| {
            cb(id, file, downloaded, total, file_index, file_count)
        };
        let zip_path = download_archive(cuda_dir, archive, &archive_cb)?;
        let dlls = extract_dlls(cuda_dir, archive, &zip_path)?;
        let _ = std::fs::remove_file(&zip_path);
        manifest.schema_version = MANIFEST_VERSION;
        manifest.archives.insert(
            archive.id.to_string(),
            InstalledArchive {
                url: archive.url.to_string(),
                sha256: archive.sha256.to_string(),
                dlls,
            },
        );
        save_manifest(cuda_dir, &manifest)?;
        log::info!("installed GPU library archive {}", archive.id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT_TEST_ID: AtomicU32 = AtomicU32::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "witness-gpulibs-{label}-{}-{id}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const TEST_ARCHIVE: GpuLibArchive = GpuLibArchive {
        id: "test",
        file: "test.whl",
        url: "https://example.invalid/test.whl",
        size: 0,
        sha256: "0",
        dlls: &["fake64_9.dll", "other64_9.dll"],
    };

    fn write_test_zip(path: &Path) {
        let file = File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        writer
            .start_file("nvidia/fake/bin/fake64_9.dll", options)
            .unwrap();
        writer.write_all(b"fake dll bytes").unwrap();
        writer
            .start_file("nvidia/fake/bin/other64_9.dll", options)
            .unwrap();
        writer.write_all(b"other dll bytes").unwrap();
        writer
            .start_file("nvidia/fake/notes/readme.txt", options)
            .unwrap();
        writer.write_all(b"not a dll").unwrap();
        writer.finish().unwrap();
    }

    fn install_test_archive(directory: &Path) -> GpuLibsManifest {
        let zip_path = directory.join(TEST_ARCHIVE.file);
        write_test_zip(&zip_path);
        let dlls = extract_dlls(directory, &TEST_ARCHIVE, &zip_path).unwrap();
        let mut manifest = GpuLibsManifest {
            schema_version: MANIFEST_VERSION,
            archives: BTreeMap::new(),
        };
        manifest.archives.insert(
            TEST_ARCHIVE.id.into(),
            InstalledArchive {
                url: TEST_ARCHIVE.url.into(),
                sha256: TEST_ARCHIVE.sha256.into(),
                dlls,
            },
        );
        manifest
    }

    #[test]
    fn extraction_matches_by_basename_and_hashes_content() {
        let directory = TestDir::new("extract");
        let manifest = install_test_archive(&directory.0);

        let installed = &manifest.archives["test"].dlls;
        assert_eq!(installed.len(), 2);
        assert_eq!(
            std::fs::read(directory.0.join("fake64_9.dll")).unwrap(),
            b"fake dll bytes"
        );
        let expected = hex::encode(Sha256::digest(b"fake dll bytes"));
        assert_eq!(installed[0].sha256, expected);
        assert!(archive_is_installed(&directory.0, &TEST_ARCHIVE, &manifest));
    }

    #[test]
    fn missing_pinned_dll_fails_extraction() {
        let directory = TestDir::new("missing");
        let zip_path = directory.0.join("bad.whl");
        let file = File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file(
                "nvidia/fake/bin/fake64_9.dll",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        writer.write_all(b"x").unwrap();
        writer.finish().unwrap();

        let error = extract_dlls(&directory.0, &TEST_ARCHIVE, &zip_path).unwrap_err();
        assert!(error.to_string().contains("other64_9.dll"));
    }

    #[test]
    fn corrupted_dll_fails_verification_and_repair_detects_it() {
        let directory = TestDir::new("corrupt");
        let manifest = install_test_archive(&directory.0);
        save_manifest(&directory.0, &manifest).unwrap();
        assert!(verify_archives(&directory.0, &manifest, &[TEST_ARCHIVE]).is_ok());

        // Same-size corruption must be caught by the quick fingerprint.
        std::fs::write(directory.0.join("fake64_9.dll"), b"evil dll bytes").unwrap();
        let error = verify_archives(&directory.0, &manifest, &[TEST_ARCHIVE]).unwrap_err();
        assert!(error.to_string().contains("quick integrity"));
        assert!(!archive_is_installed(
            &directory.0,
            &TEST_ARCHIVE,
            &manifest
        ));
        assert!(dir_if_ready(&directory.0).is_none());
    }

    #[test]
    fn inconsistent_manifest_is_not_skipped_during_repair() {
        let directory = TestDir::new("inconsistent");
        let mut manifest = install_test_archive(&directory.0);
        manifest
            .archives
            .get_mut("test")
            .unwrap()
            .dlls
            .push(InstalledDll {
                name: "unexpected.dll".into(),
                size: 0,
                sha256: String::new(),
                quick_sha256: String::new(),
            });

        let error = verify_archives(&directory.0, &manifest, &[TEST_ARCHIVE]).unwrap_err();
        assert!(error.to_string().contains("pinned file list"));
        assert!(!archive_is_installed(
            &directory.0,
            &TEST_ARCHIVE,
            &manifest
        ));
    }

    #[test]
    fn absent_install_is_not_an_integrity_error() {
        let directory = TestDir::new("absent");
        let state = status(&directory.0);
        assert!(!state.present);
        assert!(state.integrity_error.is_none());
    }

    #[test]
    fn manifest_round_trips_atomically() {
        let directory = TestDir::new("manifest");
        let manifest = install_test_archive(&directory.0);
        save_manifest(&directory.0, &manifest).unwrap();
        let loaded = load_manifest(&directory.0).unwrap();
        assert_eq!(loaded.archives["test"].dlls.len(), 2);
        let leftovers = std::fs::read_dir(&directory.0)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    /// Real network install into a temp dir: downloads every pinned wheel
    /// (~1 GB), verifies hashes, extracts, and checks the loader can resolve
    /// the full CUDA EP dependency set from the managed directory alone.
    /// `cargo test gpu_libs_real_download -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn gpu_libs_real_download() {
        let directory = TestDir::new("real");
        download(&directory.0, |id, file, done, total, index, count| {
            if let Some(total) = total {
                println!(
                    "[{index}/{count}] {id}/{file}: {} / {} MB",
                    done / 1_000_000,
                    total / 1_000_000
                );
            }
        })
        .unwrap();
        let state = status(&directory.0);
        println!("status: {state:?}");
        assert!(state.present, "install did not verify: {state:?}");
        assert_eq!(dir_if_ready(&directory.0), Some(directory.0.clone()));
        for archive in ARCHIVES {
            assert!(
                !directory.0.join(archive.file).exists(),
                "downloaded archive {} was not cleaned up",
                archive.file
            );
        }
    }
}
