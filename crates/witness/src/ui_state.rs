//! Small UI preferences persisted to `data_dir/ui-state.toml`. They replace
//! the three WebView2 localStorage keys (`witness-model-banner-dismissed`,
//! `witness-volume`, `witness-speed`), which reset once at migration.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
}

impl Default for UiPrefs {
    fn default() -> Self {
        UiPrefs {
            model_banner_dismissed: false,
            volume: 1.0,
            speed: 1.0,
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
        };
        prefs.save(&dir).unwrap();
        assert_eq!(UiPrefs::load(&dir), prefs);
        assert!(!dir.join("ui-state.toml.tmp").exists());
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
