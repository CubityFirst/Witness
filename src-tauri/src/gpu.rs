//! CUDA runtime preflight for the ONNX Runtime CUDA execution provider.
//!
//! ort registers the CUDA EP with fallback semantics: when
//! `onnxruntime_providers_cuda.dll` or one of its load-time imports cannot be
//! resolved, session creation silently proceeds on CPU and the only symptom
//! is a single-digit realtime factor. This module makes that failure mode
//! loud and, where possible, repairs it before an engine loads: known cuDNN
//! install locations are prepended to the process PATH, and anything still
//! unresolvable is logged as an actionable warning.
//!
//! Call [`preflight`] before creating a Parakeet engine (pipeline job, live
//! captions) and at startup so the warning lands next to the work it affects.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Where gpu_libs.rs keeps the app-managed CUDA/cuDNN DLLs
/// (`data_dir/cuda`). Set once at startup, before any preflight caller can
/// race it; preflight only uses it while the install verifies.
static MANAGED_LIBS_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn set_managed_libs_dir(dir: PathBuf) {
    let _ = MANAGED_LIBS_DIR.set(dir);
}

/// Runtime libraries of `onnxruntime_providers_cuda.dll` as shipped with the
/// pinned ort build (=2.0.0-rc.13, CUDA 13 binaries), minus DLLs Windows
/// always provides. cuBLAS is a PE import; since rc.13 cuFFT (and cuDNN,
/// below) are loaded by name at runtime instead, so `dumpbin /dependents`
/// alone no longer lists them — check the embedded DLL names too when
/// bumping ort.
const CUDA_EP_DLLS: &[&str] = &[
    "cublas64_13.dll",
    "cublasLt64_13.dll",
    "cufft64_12.dll",
    // Loaded dynamically by cuFFT for LTO kernels, so it is absent from the
    // provider's PE import table even though the pinned cuFFT package needs it.
    "nvJitLink_130_0.dll",
];

/// cuDNN 9 is a loader shim that pulls these sibling sub-libraries at runtime.
/// This exact list is also the managed install contract in `gpu_libs.rs`.
pub(crate) const CUDNN_DLLS: &[&str] = &[
    "cudnn64_9.dll",
    "cudnn_adv64_9.dll",
    "cudnn_cnn64_9.dll",
    "cudnn_engines_precompiled64_9.dll",
    "cudnn_engines_runtime_compiled64_9.dll",
    "cudnn_engines_tensor_ir64_9.dll",
    "cudnn_ext64_9.dll",
    "cudnn_graph64_9.dll",
    "cudnn_heuristic64_9.dll",
    "cudnn_ops64_9.dll",
];

#[derive(Debug, Clone, PartialEq)]
pub struct CudaStatus {
    /// NVIDIA driver present (`nvcuda.dll`) — nothing works without it.
    pub driver_present: bool,
    /// CUDA EP runtime libraries the DLL loader cannot resolve.
    pub missing_dlls: Vec<String>,
}

impl CudaStatus {
    pub fn ready(&self) -> bool {
        self.driver_present && self.missing_dlls.is_empty()
    }
}

/// Check that every runtime library the CUDA EP needs is resolvable,
/// prepending known cuDNN install directories to the process PATH when that
/// fixes a gap. Logs a warning naming exactly what is missing (every call —
/// it is only invoked per pipeline job, per live session, at startup and
/// from the Settings GPU check, so repetition is signal, not spam).
pub fn preflight() -> CudaStatus {
    // env mutation + duplicated log suppression under one lock; concurrent
    // callers (pipeline worker vs. live thread) otherwise race on PATH.
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap();

    let driver_present = system32_dir().join("nvcuda.dll").exists();

    let required: Vec<&str> = CUDA_EP_DLLS
        .iter()
        .chain(CUDNN_DLLS.iter())
        .copied()
        .collect();
    let mut missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|name| find_dll(name).is_none())
        .collect();

    // Only prepend a directory while it still repairs a gap, and re-check
    // after each one: candidates are ordered newest-first, and prepending a
    // later (older) dir once everything resolves would put it in FRONT of
    // the newer one on PATH — an older cuDNN/CUDA build winning resolution
    // is exactly the silent breakage this module exists to prevent.
    if !missing.is_empty() {
        for dir in cudnn_candidate_dirs() {
            if missing.is_empty() {
                break;
            }
            if missing.iter().any(|name| dir.join(name).is_file()) {
                log::info!(
                    "CUDA preflight: adding {} to PATH for the ONNX Runtime CUDA provider",
                    dir.display()
                );
                prepend_to_path(&dir);
                missing.retain(|name| find_dll(name).is_none());
            }
        }
    }

    let status = CudaStatus {
        driver_present,
        missing_dlls: missing.iter().map(|s| s.to_string()).collect(),
    };
    if !driver_present {
        log::info!("no NVIDIA driver (nvcuda.dll) — transcription runs on CPU");
    } else if !status.missing_dlls.is_empty() {
        log::warn!(
            "CUDA unavailable for transcription: {} not found next to the app, in System32 or on PATH — \
             Parakeet will silently run on CPU (single-digit realtime factor). \
             Install cuDNN 9 for CUDA 13 and/or the CUDA 13 runtime, or set CUDNN_PATH to the install root.",
            status.missing_dlls.join(", ")
        );
    }
    status
}

/// Standard loader search order for a dependent DLL: application directory,
/// System32, then every PATH entry. (KnownDLLs and the app's own directory
/// features don't apply to the CUDA runtime set.)
fn loader_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
        }
    }
    dirs.push(system32_dir());
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs
}

fn find_dll(name: &str) -> Option<PathBuf> {
    find_in_dirs(name, &loader_search_dirs())
}

fn find_in_dirs(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

fn system32_dir() -> PathBuf {
    let windir = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    windir.join("System32")
}

/// Directories where the CUDA runtime + cuDNN may live when they are not
/// already on PATH: the app-managed install (Settings → "Download GPU
/// libraries", checked first because it is integrity-verified), `CUDNN_PATH`
/// (official env var, points at the install root), the official installer
/// layout under Program Files, and the same layout under LOCALAPPDATA for
/// unprivileged installs.
fn cudnn_candidate_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = MANAGED_LIBS_DIR
        .get()
        .and_then(|dir| crate::gpu_libs::dir_if_ready(dir))
        .into_iter()
        .collect();

    let mut roots = Vec::new();
    for var in ["CUDNN_PATH", "CUDNN_HOME"] {
        if let Some(root) = std::env::var_os(var) {
            let root = PathBuf::from(root);
            if root.is_dir() {
                roots.push(root);
            }
        }
    }
    for base in [
        std::env::var_os("ProgramFiles").map(PathBuf::from),
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
    ]
    .into_iter()
    .flatten()
    {
        roots.extend(version_dirs(&base.join("NVIDIA").join("CUDNN")));
    }

    for root in roots {
        // Official 9.x installers nest per-CUDA-version dirs under bin
        // (bin\13.0, bin\12.9, …); zip/wheel layouts put DLLs in bin itself.
        let bin = root.join("bin");
        for dir in version_dirs(&bin)
            .into_iter()
            .chain(bin.is_dir().then_some(bin))
        {
            // Vec::dedup only drops adjacent repeats; CUDNN_PATH and
            // CUDNN_HOME pointing at the same root interleave, so dedup
            // across the whole list.
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// Subdirectories of `base`, newest first by the numeric components of the
/// directory name (`v9.24` before `v9.8`, `13.0` before `12.9`).
fn version_dirs(base: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort_by_key(|dir| std::cmp::Reverse(numeric_key(dir)));
    dirs
}

/// The numbers embedded in a directory name, for natural version ordering
/// (lexicographic comparison would put `v9.8` above `v9.24`).
fn numeric_key(path: &Path) -> Vec<u64> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut key = Vec::new();
    let mut current: Option<u64> = None;
    for ch in name.chars() {
        if let Some(digit) = ch.to_digit(10) {
            current = Some(
                current
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(u64::from(digit)),
            );
        } else if let Some(number) = current.take() {
            key.push(number);
        }
    }
    key.extend(current);
    key
}

fn prepend_to_path(dir: &Path) {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let entries: Vec<PathBuf> = std::iter::once(dir.to_path_buf())
        .chain(std::env::split_paths(&current))
        .collect();
    if let Ok(joined) = std::env::join_paths(entries) {
        std::env::set_var("PATH", joined);
    }
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
            let path = std::env::temp_dir()
                .join(format!("witness-gpu-{label}-{}-{id}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn find_in_dirs_locates_only_existing_files() {
        let dir = TestDir::new("find");
        std::fs::write(dir.0.join("cudnn64_9.dll"), b"x").unwrap();
        let dirs = vec![dir.0.join("nope"), dir.0.clone()];
        assert_eq!(
            find_in_dirs("cudnn64_9.dll", &dirs),
            Some(dir.0.join("cudnn64_9.dll"))
        );
        assert_eq!(find_in_dirs("cublas64_13.dll", &dirs), None);
    }

    #[test]
    fn version_dirs_numeric_newest_first() {
        let dir = TestDir::new("versions");
        for name in ["v9.8", "v9.24", "v9.12"] {
            std::fs::create_dir(dir.0.join(name)).unwrap();
        }
        // Order matters: the repair loop prepends the first matching dir and
        // stops once everything resolves, so newest must come first — and
        // numerically, not lexicographically (v9.8 vs v9.24).
        let versions: Vec<String> = version_dirs(&dir.0)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(versions, ["v9.24", "v9.12", "v9.8"]);
    }

    #[test]
    fn numeric_key_orders_cuda_style_versions() {
        assert!(numeric_key(Path::new("v9.24")) > numeric_key(Path::new("v9.8")));
        assert!(numeric_key(Path::new("13.0")) > numeric_key(Path::new("12.9")));
        assert!(numeric_key(Path::new("bin")).is_empty());
    }

    #[test]
    fn version_dirs_missing_base_is_empty() {
        let dir = TestDir::new("nobase");
        assert!(version_dirs(&dir.0.join("does-not-exist")).is_empty());
    }
}
