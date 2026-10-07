//! Small UI preferences persisted to `data_dir/ui-state.toml`. They replace
//! the three WebView2 localStorage keys (`witness-model-banner-dismissed`,
//! `witness-volume`, `witness-speed`), which reset once at migration.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const FILE_NAME: &str = "ui-state.toml";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPrefs {
    /// "Later" on the first-run model banner.
    pub model_banner_dismissed: bool,
    /// Player volume, linear 0..1.
    pub volume: f32,
    /// Player speed (0.75–2).
    pub speed: f32,
    /// Light / dark, or follow Windows' app mode.
    pub appearance: Appearance,
    /// Windows' accent colour, or Witness blue.
    pub accent: AccentSource,
    /// The Windows 11 Mica backdrop behind the window (where supported;
    /// applies on restart).
    pub backdrop: bool,
    /// The first-run setup was finished or skipped.
    pub onboarded: bool,
    /// The split layout's list column width (dragged at its divider).
    pub list_width: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccentSource {
    #[default]
    Windows,
    Witness,
}

impl Default for UiPrefs {
    fn default() -> Self {
        UiPrefs {
            model_banner_dismissed: false,
            volume: 1.0,
            speed: 1.0,
            appearance: Appearance::System,
            accent: AccentSource::Windows,
            backdrop: true,
            onboarded: false,
            list_width: crate::ui::LIST_WIDTH,
        }
    }
}

impl UiPrefs {
    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }

    /// Load from `data_dir`; defaults when the file is missing or invalid.
    pub fn load(data_dir: &Path) -> UiPrefs {
        let path = Self::path(data_dir);
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<UiPrefs>(&text) {
                Ok(prefs) => prefs.sanitized(),
                Err(error) => {
                    log::warn!("ignoring invalid {}: {error}", path.display());
                    UiPrefs::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => UiPrefs::default(),
            Err(error) => {
                log::warn!("could not read {}: {error}", path.display());
                UiPrefs::default()
            }
        }
    }

    /// Clamp hand-edited values into range.
    fn sanitized(mut self) -> UiPrefs {
        let defaults = UiPrefs::default();
        self.volume = if self.volume.is_finite() {
            self.volume.clamp(0.0, 1.0)
        } else {
            defaults.volume
        };
        self.speed = if self.speed.is_finite() {
            self.speed.clamp(0.75, 2.0)
        } else {
            defaults.speed
        };
        self.list_width = if self.list_width.is_finite() {
            self.list_width
                .clamp(crate::ui::LIST_MIN_WIDTH, crate::ui::LIST_MAX_WIDTH)
        } else {
            defaults.list_width
        };
        self
    }

    /// Write atomically (temp file + rename), so a crash never leaves a
    /// truncated file behind.
    pub fn save(&self, data_dir: &Path) -> Result<(), String> {
        let path = Self::path(data_dir);
        let text = toml::to_string(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)
            .and_then(|()| std::fs::rename(&tmp, &path))
            .map_err(|e| format!("could not save {}: {e}", path.display()))
    }
}

/// Saves `UiPrefs` off the UI thread as they change (like the TSX
/// localStorage writes), so a Windows shutdown, crash or kill keeps them.
/// Several writer threads may overlap; each takes the newest queued value
/// under the write lock, so an older value never lands last.
#[derive(Default)]
pub struct PrefsWriter {
    /// The newest prefs not yet taken by a writer (short holds only).
    pending: Mutex<Option<UiPrefs>>,
    /// Held across the file write.
    write: Mutex<()>,
}

impl PrefsWriter {
    /// Queue `prefs` as the newest value (UI thread; never waits on I/O).
    pub fn queue(&self, prefs: UiPrefs) {
        *lock(&self.pending) = Some(prefs);
    }

    /// Write the newest queued value, if a newer writer has not already
    /// (worker thread).
    pub fn write_pending(&self, data_dir: &Path) {
        let _write = lock(&self.write);
        let Some(prefs) = lock(&self.pending).take() else {
            return;
        };
        if let Err(error) = prefs.save(data_dir) {
            log::warn!("{error}");
        }
    }

    /// Exit paths: drop anything queued and write `prefs` now, after any
    /// in-progress write.
    pub fn write_now(&self, prefs: &UiPrefs, data_dir: &Path) {
        let _write = lock(&self.write);
        lock(&self.pending).take();
        if let Err(error) = prefs.save(data_dir) {
            log::warn!("{error}");
        }
    }
}

/// A poisoned lock only means a writer panicked; the data is still usable.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("witness-ui-state-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_gives_defaults_and_round_trips() {
        let dir = scratch("roundtrip");
        assert_eq!(UiPrefs::load(&dir), UiPrefs::default());
        let prefs = UiPrefs {
            model_banner_dismissed: true,
            volume: 0.4,
            speed: 1.5,
            appearance: Appearance::Light,
            accent: AccentSource::Witness,
            backdrop: false,
            onboarded: true,
            list_width: 420.0,
        };
        prefs.save(&dir).unwrap();
        assert_eq!(UiPrefs::load(&dir), prefs);
        assert!(!dir.join("ui-state.toml.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A 0.3.0 file without the appearance keys loads them as defaults.
    #[test]
    fn older_files_get_appearance_defaults() {
        let prefs: UiPrefs = toml::from_str(
            "volume = 0.5
",
        )
        .unwrap();
        assert_eq!(prefs.appearance, Appearance::System);
        assert_eq!(prefs.accent, AccentSource::Windows);
        assert!(prefs.backdrop);
        assert!(!prefs.onboarded);
        let text = toml::to_string(&UiPrefs::default()).unwrap();
        assert!(text.contains("appearance = \"system\""), "{text}");
    }

    #[test]
    fn writer_saves_only_the_newest_queued_value() {
        let dir = scratch("writer");
        let writer = PrefsWriter::default();
        let volume = |v: f32| UiPrefs {
            volume: v,
            ..UiPrefs::default()
        };
        writer.queue(volume(0.2));
        writer.queue(volume(0.3));
        writer.write_pending(&dir);
        assert_eq!(UiPrefs::load(&dir), volume(0.3));
        // A late writer for the 0.2 change finds nothing left to write.
        writer.write_pending(&dir);
        assert_eq!(UiPrefs::load(&dir), volume(0.3));

        writer.queue(volume(0.4));
        writer.write_now(&volume(0.5), &dir);
        writer.write_pending(&dir);
        assert_eq!(UiPrefs::load(&dir), volume(0.5), "exit value wins");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_or_partial_files_are_tolerated() {
        let dir = scratch("invalid");
        std::fs::write(dir.join(FILE_NAME), "volume = 7.0\n").unwrap();
        let prefs = UiPrefs::load(&dir);
        assert_eq!(prefs.volume, 1.0, "clamped");
        assert_eq!(prefs.speed, 1.0, "default");
        std::fs::write(dir.join(FILE_NAME), "not toml [").unwrap();
        assert_eq!(UiPrefs::load(&dir), UiPrefs::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
