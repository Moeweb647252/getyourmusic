//! The seam between the portable core and an operating system.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::capture::AudioCaptureBackend;
use crate::encode::AudioEncoder;
use crate::now_playing::{NowPlayingError, NowPlayingSource};

/// System privacy panes the application may direct the user to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivacyPane {
    SystemAudioRecording,
    Microphone,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlatformCapabilities {
    /// Output devices can be recorded directly (no virtual loopback driver needed).
    pub system_loopback: bool,
}

/// Everything operating-system specific the application needs.
pub trait Platform: Send + Sync {
    /// Short platform name for diagnostics, e.g. `"macOS 26.6"`.
    fn name(&self) -> String;

    fn capabilities(&self) -> PlatformCapabilities;

    fn now_playing_source(&self) -> Result<Box<dyn NowPlayingSource>, NowPlayingError>;

    fn capture_backend(&self) -> Arc<dyn AudioCaptureBackend>;

    /// Encoders backed by OS facilities, registered in addition to the portable ones.
    fn native_encoders(&self) -> Vec<Arc<dyn AudioEncoder>>;

    /// Shows a file in the system file manager (Finder, Explorer, …).
    fn reveal_in_file_manager(&self, path: &Path) -> io::Result<()>;

    /// Opens a folder in the system file manager.
    fn open_folder(&self, path: &Path) -> io::Result<()>;

    /// Opens a file with its default application.
    fn open_file(&self, path: &Path) -> io::Result<()>;

    /// Moves a file to the system trash, where the user can still recover it.
    fn move_to_trash(&self, path: &Path) -> io::Result<()>;

    fn open_privacy_settings(&self, pane: PrivacyPane) -> io::Result<()>;
}

/// Standard per-user application directories.
#[derive(Clone, Debug)]
pub struct AppDirs {
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub logs: PathBuf,
    /// Default destination for recordings (e.g. `~/Music/GetYourMusic`).
    pub default_music_folder: PathBuf,
}

impl AppDirs {
    pub const APP_NAME: &'static str = "GetYourMusic";

    pub fn discover() -> Option<Self> {
        let project = directories::ProjectDirs::from("dev", "GetYourMusic", Self::APP_NAME)?;
        let user = directories::UserDirs::new()?;
        let music = user
            .audio_dir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| user.home_dir().join("Music"));
        let logs = if cfg!(target_os = "macos") {
            user.home_dir().join("Library/Logs").join(Self::APP_NAME)
        } else {
            project.data_local_dir().join("logs")
        };
        Some(Self {
            config: project.config_dir().to_path_buf(),
            data: project.data_dir().to_path_buf(),
            cache: project.cache_dir().to_path_buf(),
            logs,
            default_music_folder: music.join(Self::APP_NAME),
        })
    }

    pub fn settings_file(&self) -> PathBuf {
        self.config.join("settings.json")
    }

    pub fn library_file(&self) -> PathBuf {
        self.data.join("library.json")
    }

    /// Scratch space for in-progress recordings.
    pub fn spool_dir(&self) -> PathBuf {
        self.cache.join("spool")
    }
}
