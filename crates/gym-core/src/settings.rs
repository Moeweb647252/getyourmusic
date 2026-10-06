//! Persistent user settings.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::encode::EncodeSettings;
use crate::naming::DEFAULT_TEMPLATE;
use crate::storage::ConflictPolicy;

pub const SETTINGS_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub version: u32,
    pub general: GeneralSettings,
    pub recording: RecordingSettings,
    pub output: OutputSettings,
    pub storage: StorageSettings,
    pub cache: CacheSettings,
    pub appearance: AppearanceSettings,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            general: GeneralSettings::default(),
            recording: RecordingSettings::default(),
            output: OutputSettings::default(),
            storage: StorageSettings::default(),
            cache: CacheSettings::default(),
            appearance: AppearanceSettings::default(),
        }
    }
}

/// Application-wide behavior.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralSettings {
    /// Show an icon in the menu bar (macOS) or system tray (Windows, Linux).
    pub show_tray_icon: bool,
    /// Keep running, and recording, from the tray icon after the window is closed.
    pub keep_running_when_closed: bool,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            show_tray_icon: true,
            keep_running_when_closed: true,
        }
    }
}

/// What to do with tracks that were not recorded from start to finish.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncompletePolicy {
    #[default]
    Discard,
    Keep,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingSettings {
    /// Capture source id; `None` uses the system default output.
    pub capture_source: Option<String>,
    /// Only record this player (by id); `None` records whichever player is active.
    pub follow_player: Option<String>,
    pub incomplete_tracks: IncompletePolicy,
    /// Trim digital silence at the start and end of each track.
    pub trim_silence: bool,
    /// Stop recording after this many idle minutes; `0` disables auto-stop.
    pub auto_stop_minutes: u32,
}

impl Default for RecordingSettings {
    fn default() -> Self {
        Self {
            capture_source: None,
            follow_player: None,
            incomplete_tracks: IncompletePolicy::default(),
            trim_silence: true,
            auto_stop_minutes: 10,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleRatePolicy {
    /// Keep the capture device's rate when the format allows it.
    Source,
    /// The rate of nearly all released music; output devices often run faster.
    #[default]
    Hz44100,
    Hz48000,
}

impl SampleRatePolicy {
    pub fn resolve(self, source_rate: u32) -> u32 {
        match self {
            SampleRatePolicy::Source => source_rate,
            SampleRatePolicy::Hz44100 => 44_100,
            SampleRatePolicy::Hz48000 => 48_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputSettings {
    pub encode: EncodeSettings,
    pub sample_rate: SampleRatePolicy,
    pub naming_template: String,
    pub conflict_policy: ConflictPolicy,
}

impl Default for OutputSettings {
    fn default() -> Self {
        Self {
            encode: EncodeSettings::default(),
            sample_rate: SampleRatePolicy::default(),
            naming_template: DEFAULT_TEMPLATE.into(),
            conflict_policy: ConflictPolicy::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageProviderKind {
    #[default]
    Local,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageSettings {
    pub provider: StorageProviderKind,
    /// Destination folder for [`StorageProviderKind::Local`]; `None` uses the default.
    pub local_folder: Option<PathBuf>,
}

/// Where audio is buffered while a track records, before it is encoded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheMode {
    #[default]
    Disk,
    Memory,
}

/// What to do when in-memory audio reaches [`CacheSettings::memory_limit_mb`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryOverflow {
    /// Keep what is in memory and write the rest of the track to the cache folder.
    #[default]
    SpillToDisk,
    /// Don't save the track.
    Fail,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheSettings {
    pub mode: CacheMode,
    /// Memory for audio of all tracks not yet saved, in MiB; `0` means no limit.
    pub memory_limit_mb: u32,
    pub on_overflow: MemoryOverflow,
    /// Folder for cache files; `None` uses the default.
    pub folder: Option<PathBuf>,
}

impl Default for CacheSettings {
    fn default() -> Self {
        Self {
            mode: CacheMode::default(),
            memory_limit_mb: 1024,
            on_overflow: MemoryOverflow::default(),
            folder: None,
        }
    }
}

impl CacheSettings {
    /// The memory limit in bytes, or `None` when unlimited.
    pub fn memory_limit_bytes(&self) -> Option<u64> {
        (self.memory_limit_mb > 0).then(|| u64::from(self.memory_limit_mb) * 1024 * 1024)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    pub theme: ThemePreference,
    pub locale: String,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: ThemePreference::default(),
            locale: "en".into(),
        }
    }
}

/// Result of loading settings from disk.
#[derive(Debug)]
pub struct LoadedSettings {
    pub settings: AppSettings,
    /// Set when an unreadable file was moved aside and defaults were used.
    pub recovered_from: Option<PathBuf>,
}

impl AppSettings {
    /// Loads settings, falling back to defaults when the file is missing or corrupt.
    ///
    /// A corrupt file is renamed to `*.corrupt` so the user's data is not silently lost.
    pub fn load(path: &Path) -> io::Result<LoadedSettings> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(LoadedSettings {
                    settings: Self::default(),
                    recovered_from: None,
                });
            }
            Err(err) => return Err(err),
        };
        match serde_json::from_slice::<AppSettings>(&bytes) {
            Ok(mut settings) => {
                settings.version = SETTINGS_VERSION;
                Ok(LoadedSettings {
                    settings,
                    recovered_from: None,
                })
            }
            Err(err) => {
                tracing::warn!(%err, path = %path.display(), "settings file is corrupt");
                let backup = path.with_extension("json.corrupt");
                fs::rename(path, &backup)?;
                Ok(LoadedSettings {
                    settings: Self::default(),
                    recovered_from: Some(backup),
                })
            }
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        write_atomically(path, &json)
    }
}

/// Writes `bytes` to a temporary sibling and renames it over `path`.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_fills_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut settings = AppSettings::default();
        settings.recording.follow_player = Some("com.tencent.QQMusicMac".into());
        settings.cache.mode = CacheMode::Memory;
        settings.cache.on_overflow = MemoryOverflow::Fail;
        settings.cache.folder = Some("/tmp/gym-cache".into());
        settings.save(&path).unwrap();
        let loaded = AppSettings::load(&path).unwrap();
        assert_eq!(loaded.settings, settings);

        fs::write(&path, r#"{"recording":{"trim_silence":false}}"#).unwrap();
        let partial = AppSettings::load(&path).unwrap().settings;
        assert!(!partial.recording.trim_silence);
        assert_eq!(partial.output, OutputSettings::default());
        assert_eq!(partial.cache, CacheSettings::default());
    }

    #[test]
    fn corrupt_files_are_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, "{not json").unwrap();
        let loaded = AppSettings::load(&path).unwrap();
        assert_eq!(loaded.settings, AppSettings::default());
        assert!(loaded.recovered_from.unwrap().exists());
        assert!(!path.exists());
    }
}
