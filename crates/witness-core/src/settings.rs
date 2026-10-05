//! App-level preferences, persisted next to the executable in
//! `witness-settings.toml` (vigil's layering: `WITNESS_SETTINGS` env var →
//! settings file → `data_dir` holding db/audio/models/log).

use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub const DEFAULT_OPUS_BITRATE_KBPS: u32 = 40;
pub const MIN_OPUS_BITRATE_KBPS: u32 = 6;
pub const MAX_OPUS_BITRATE_KBPS: u32 = 510;

static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    #[default]
    Parakeet,
    Whisper,
}

impl std::fmt::Display for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Engine::Parakeet => write!(f, "parakeet"),
            Engine::Whisper => write!(f, "whisper"),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_patterns() -> Vec<String> {
    vec!["MSTeams".to_string()]
}

fn default_bitrate() -> u32 {
    DEFAULT_OPUS_BITRATE_KBPS
}

fn default_threshold() -> f32 {
    0.6
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Directory holding the db, audio, models and log. None = next to the exe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,

    /// ASR engine used for new transcriptions.
    #[serde(default)]
    pub engine: Engine,

    /// Start recording automatically when a watched app holds the mic.
    #[serde(default = "default_true")]
    pub auto_record: bool,

    /// Queue a transcription job as soon as a recording finishes.
    #[serde(default = "default_true")]
    pub auto_transcribe: bool,

    /// Show provisional captions while recording (Parakeet on speech
    /// pauses; the final pass still replaces them).
    #[serde(default = "default_true")]
    pub live_transcribe: bool,

    /// Float a small always-on-top captions window while recording.
    #[serde(default = "default_true")]
    pub caption_overlay: bool,

    /// Cosine similarity needed to auto-label a speaker from a voice print.
    #[serde(default = "default_threshold")]
    pub speaker_match_threshold: f32,

    /// Substrings matched (case-insensitively) against mic-consent-store app
    /// names; any match counts as "in a meeting".
    #[serde(default = "default_patterns")]
    pub watch_patterns: Vec<String>,

    /// Exact normalized ASR phrases discarded when a VAD chunk is otherwise
    /// speech-like. Clearing this list disables phrase-based filtering.
    #[serde(default = "crate::asr::default_junk_phrases")]
    pub junk_phrases: Vec<String>,

    /// Target bitrate for the archived stereo Ogg Opus file.
    #[serde(default = "default_bitrate")]
    pub opus_bitrate_kbps: u32,

    /// Capture device (friendly name) for the mic track; None = default.
    /// Useful with per-app routing, e.g. a headset's "Chat Mic".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mic_device: Option<String>,

    /// Render device (friendly name) whose loopback is the "them" track;
    /// None = default output. Pointing this at a dedicated device that only
    /// Teams outputs to (e.g. "Chat") isolates meeting audio from music etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loopback_device: Option<String>,

    /// Look for a newer signed release on GitHub shortly after startup. The
    /// request carries no meeting data; installing always needs a click.
    #[serde(default = "default_true")]
    pub check_for_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            data_dir: None,
            engine: Engine::default(),
            auto_record: true,
            auto_transcribe: true,
            live_transcribe: true,
            caption_overlay: true,
            check_for_updates: true,
            speaker_match_threshold: 0.6,
            watch_patterns: default_patterns(),
            junk_phrases: crate::asr::default_junk_phrases(),
            opus_bitrate_kbps: DEFAULT_OPUS_BITRATE_KBPS,
            mic_device: None,
            loopback_device: None,
        }
    }
}

impl Settings {
    /// Location of the settings file. Overridable with the `WITNESS_SETTINGS`
    /// environment variable (a file path, or a directory to hold the default
    /// filename); otherwise it sits next to the executable.
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("WITNESS_SETTINGS") {
            let p = p.trim();
            if !p.is_empty() {
                let pb = PathBuf::from(p);
                return if pb.is_dir() {
                    pb.join("witness-settings.toml")
                } else {
                    pb
                };
            }
        }
        exe_dir().join("witness-settings.toml")
    }

    /// Load and validate settings. A missing file uses defaults, while every
    /// error from an existing file is preserved for the startup UI/log.
    pub fn load_result() -> Result<Settings, String> {
        Self::load_from_path(&Self::path())
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to_path(&Self::path())
    }

    /// Load settings from an explicit path. This is also useful for setup and
    /// recovery flows that need to display the exact configuration error.
    pub fn load_from_path(path: &Path) -> Result<Settings, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Settings::default().normalized();
            }
            Err(error) => {
                return Err(format!(
                    "failed to read settings from {}: {error}",
                    path.display()
                ));
            }
        };

        let settings: Settings = toml::from_str(&text).map_err(|error| {
            format!("failed to parse settings from {}: {error}", path.display())
        })?;
        settings
            .normalized()
            .map_err(|error| format!("invalid settings in {}: {error}", path.display()))
    }

    /// Save through a same-directory temporary file so a crash cannot leave a
    /// partially written TOML file. The temporary file is flushed and synced
    /// before it atomically replaces the previous configuration.
    pub fn save_to_path(&self, path: &Path) -> Result<(), String> {
        let settings = self.normalized()?;
        let text = toml::to_string_pretty(&settings)
            .map_err(|error| format!("failed to serialize settings: {error}"))?;
        let parent = usable_parent(path);
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "failed to create settings directory {}: {error}",
                parent.display()
            )
        })?;

        let (temporary_path, mut temporary_file) = create_temporary_file(path)?;
        let write_result = (|| -> Result<(), String> {
            temporary_file.write_all(text.as_bytes()).map_err(|error| {
                format!(
                    "failed to write temporary settings file {}: {error}",
                    temporary_path.display()
                )
            })?;
            temporary_file.flush().map_err(|error| {
                format!(
                    "failed to flush temporary settings file {}: {error}",
                    temporary_path.display()
                )
            })?;
            temporary_file.sync_all().map_err(|error| {
                format!(
                    "failed to sync temporary settings file {}: {error}",
                    temporary_path.display()
                )
            })?;
            drop(temporary_file);

            replace_file(&temporary_path, path).map_err(|error| {
                format!(
                    "failed to replace settings file {}: {error}",
                    path.display()
                )
            })
        })();

        if write_result.is_err() {
            let _ = std::fs::remove_file(&temporary_path);
        }
        write_result
    }

    /// Validate configuration values accepted by the backend. The UI's
    /// recommended speaker threshold range is 0.4..=0.8, but the full cosine
    /// similarity range remains valid for advanced configurations.
    pub fn validate(&self) -> Result<(), String> {
        if !self.speaker_match_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.speaker_match_threshold)
        {
            return Err("speaker_match_threshold must be between 0 and 1".into());
        }
        if !(MIN_OPUS_BITRATE_KBPS..=MAX_OPUS_BITRATE_KBPS).contains(&self.opus_bitrate_kbps) {
            return Err(format!(
                "opus_bitrate_kbps must be between {MIN_OPUS_BITRATE_KBPS} and {MAX_OPUS_BITRATE_KBPS}"
            ));
        }
        if let Some(data_dir) = &self.data_dir {
            validate_nonempty_text("data_dir", data_dir)?;
        }
        if self.watch_patterns.is_empty() {
            return Err("watch_patterns must contain at least one pattern".into());
        }
        for (index, pattern) in self.watch_patterns.iter().enumerate() {
            validate_nonempty_text(&format!("watch_patterns[{index}]"), pattern)?;
        }
        if self.junk_phrases.len() > 100 {
            return Err("junk_phrases cannot contain more than 100 phrases".into());
        }
        for (index, phrase) in self.junk_phrases.iter().enumerate() {
            validate_nonempty_text(&format!("junk_phrases[{index}]"), phrase)?;
        }
        if let Some(device) = &self.mic_device {
            validate_nonempty_text("mic_device", device)?;
        }
        if let Some(device) = &self.loopback_device {
            validate_nonempty_text("loopback_device", device)?;
        }
        Ok(())
    }

    /// Return a validated copy with surrounding whitespace removed from user
    /// entered paths, app patterns, and device names. Blank optional device
    /// names mean "use the default device" and are canonicalized to `None`.
    pub fn normalized(&self) -> Result<Settings, String> {
        let mut settings = self.clone();
        settings.data_dir = settings.data_dir.map(|value| value.trim().to_owned());
        settings.watch_patterns = settings
            .watch_patterns
            .into_iter()
            .map(|value| value.trim().to_owned())
            .collect();
        let mut seen_phrases = std::collections::HashSet::new();
        settings.junk_phrases = settings
            .junk_phrases
            .into_iter()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .filter(|value| seen_phrases.insert(value.to_lowercase()))
            .collect();
        settings.mic_device = normalize_optional_text(settings.mic_device);
        settings.loopback_device = normalize_optional_text(settings.loopback_device);
        settings.validate()?;
        Ok(settings)
    }

    /// The effective data directory (configured one, or the exe's folder).
    pub fn data_dir(&self) -> PathBuf {
        self.data_dir
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(exe_dir)
    }

    pub fn audio_dir(&self) -> PathBuf {
        self.data_dir().join("audio")
    }

    pub fn rec_tmp_dir(&self) -> PathBuf {
        self.data_dir().join("rec-tmp")
    }

    pub fn models_dir(&self) -> PathBuf {
        self.data_dir().join("models")
    }

    /// App-managed CUDA/cuDNN runtime DLLs (gpu_libs.rs).
    pub fn cuda_dir(&self) -> PathBuf {
        self.data_dir().join("cuda")
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir().join("witness.db")
    }
}

fn validate_nonempty_text(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.contains('\0') {
        return Err(format!("{field} must not contain a NUL character"));
    }
    Ok(())
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

fn usable_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn create_temporary_file(path: &Path) -> Result<(PathBuf, File), String> {
    let parent = usable_parent(path);
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("settings path {} has no file name", path.display()))?;

    for _ in 0..100 {
        let id = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = OsString::from(".");
        temporary_name.push(file_name);
        temporary_name.push(format!(".{}.{}.tmp", std::process::id(), id));
        let temporary_path = parent.join(temporary_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((temporary_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "failed to create temporary settings file {}: {error}",
                    temporary_path.display()
                ));
            }
        }
    }

    Err(format!(
        "failed to allocate a temporary settings file next to {}",
        path.display()
    ))
}

#[cfg(not(windows))]
fn replace_file(temporary_path: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(temporary_path, destination)
}

#[cfg(windows)]
fn replace_file(temporary_path: &Path, destination: &Path) -> io::Result<()> {
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

/// Directory containing the running executable (falls back to the cwd).
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let id = NEXT_TEST_DIR_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("witness-settings-test-{}-{id}", std::process::id()));
            std::fs::create_dir(&path).expect("create isolated settings test directory");
            Self(path)
        }

        fn settings_path(&self) -> PathBuf {
            self.0.join("witness-settings.toml")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_settings_file_loads_defaults() {
        let directory = TestDir::new();
        let settings = Settings::load_from_path(&directory.settings_path()).unwrap();

        assert_eq!(settings.opus_bitrate_kbps, DEFAULT_OPUS_BITRATE_KBPS);
        assert_eq!(settings.watch_patterns, vec!["MSTeams"]);
    }

    #[test]
    fn malformed_settings_file_returns_error_without_modifying_it() {
        let directory = TestDir::new();
        let path = directory.settings_path();
        let malformed = "engine = [this is not TOML";
        std::fs::write(&path, malformed).unwrap();

        let error = Settings::load_from_path(&path).unwrap_err();

        assert!(error.contains("failed to parse settings"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), malformed);
    }

    #[test]
    fn invalid_existing_settings_are_reported() {
        let directory = TestDir::new();
        let path = directory.settings_path();
        std::fs::write(
            &path,
            "speaker_match_threshold = 1.5\nopus_bitrate_kbps = 40\nwatch_patterns = [\"Teams\"]\n",
        )
        .unwrap();

        let error = Settings::load_from_path(&path).unwrap_err();

        assert!(error.contains("invalid settings"));
        assert!(error.contains("speaker_match_threshold"));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn validation_enforces_numeric_and_text_bounds() {
        let mut settings = Settings::default();
        settings.speaker_match_threshold = f32::NAN;
        assert!(settings.validate().is_err());

        settings.speaker_match_threshold = 0.6;
        settings.opus_bitrate_kbps = MIN_OPUS_BITRATE_KBPS - 1;
        assert!(settings.validate().is_err());

        settings.opus_bitrate_kbps = MAX_OPUS_BITRATE_KBPS + 1;
        assert!(settings.validate().is_err());

        settings.opus_bitrate_kbps = DEFAULT_OPUS_BITRATE_KBPS;
        settings.data_dir = Some("  ".into());
        assert!(settings.validate().is_err());

        settings.data_dir = None;
        settings.watch_patterns = vec!["\t".into()];
        assert!(settings.validate().is_err());

        settings.watch_patterns = vec!["Teams".into()];
        settings.junk_phrases = vec!["phrase".into(); 101];
        assert!(settings.validate().is_err());

        settings.junk_phrases = vec![];
        settings.mic_device = Some("bad\0device".into());
        assert!(settings.validate().is_err());
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn save_is_atomic_and_normalizes_user_entered_text() {
        let directory = TestDir::new();
        let path = directory.settings_path();
        let mut settings = Settings::default();
        settings.data_dir = Some("  C:\\Witness Data  ".into());
        settings.watch_patterns = vec!["  Teams  ".into(), " Zoom ".into()];
        settings.junk_phrases = vec!["  Thank you.  ".into(), "thank YOU.".into(), " ".into()];
        settings.mic_device = Some("   ".into());
        settings.loopback_device = Some("  Speakers  ".into());

        settings.save_to_path(&path).unwrap();
        settings.engine = Engine::Whisper;
        settings.save_to_path(&path).unwrap();
        let loaded = Settings::load_from_path(&path).unwrap();

        assert_eq!(loaded.data_dir.as_deref(), Some("C:\\Witness Data"));
        assert_eq!(loaded.watch_patterns, vec!["Teams", "Zoom"]);
        assert_eq!(loaded.junk_phrases, vec!["Thank you."]);
        assert_eq!(loaded.mic_device, None);
        assert_eq!(loaded.loopback_device.as_deref(), Some("Speakers"));
        assert!(matches!(loaded.engine, Engine::Whisper));

        let leftover_temporary_files = std::fs::read_dir(&directory.0)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftover_temporary_files, 0);
    }

    #[test]
    fn invalid_save_does_not_replace_existing_settings() {
        let directory = TestDir::new();
        let path = directory.settings_path();
        let original = Settings::default();
        original.save_to_path(&path).unwrap();
        let original_text = std::fs::read_to_string(&path).unwrap();

        let mut invalid = original;
        invalid.opus_bitrate_kbps = 0;
        assert!(invalid.save_to_path(&path).is_err());

        assert_eq!(std::fs::read_to_string(path).unwrap(), original_text);
    }

    #[test]
    fn parent_directory_creation_errors_are_returned() {
        let directory = TestDir::new();
        let blocking_file = directory.0.join("not-a-directory");
        std::fs::write(&blocking_file, "occupied").unwrap();
        let path = blocking_file.join("witness-settings.toml");

        let error = Settings::default().save_to_path(&path).unwrap_err();

        assert!(error.contains("failed to create settings directory"));
    }
}
