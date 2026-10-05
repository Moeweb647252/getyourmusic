//! Placeholder for operating systems without a dedicated integration yet.
//!
//! Capture still goes through cpal (input devices only); now playing and native encoders are
//! reported as unavailable. Implement `Platform` in a new module to add a system.

use std::io;
use std::path::Path;
use std::sync::Arc;

use gym_core::capture::AudioCaptureBackend;
use gym_core::encode::AudioEncoder;
use gym_core::now_playing::{NowPlayingError, NowPlayingSource};
use gym_core::platform::{Platform, PlatformCapabilities, PrivacyPane};

use crate::cpal_capture::CpalCapture;

pub struct UnsupportedPlatform {
    capture: Arc<CpalCapture>,
}

impl UnsupportedPlatform {
    pub fn new() -> Self {
        Self {
            capture: Arc::new(CpalCapture::new(false)),
        }
    }
}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "not supported on this platform yet",
    )
}

impl Platform for UnsupportedPlatform {
    fn name(&self) -> String {
        std::env::consts::OS.to_owned()
    }

    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities::default()
    }

    fn now_playing_source(&self) -> Result<Box<dyn NowPlayingSource>, NowPlayingError> {
        Err(NowPlayingError::Unavailable(format!(
            "now playing is not implemented for {}",
            std::env::consts::OS
        )))
    }

    fn capture_backend(&self) -> Arc<dyn AudioCaptureBackend> {
        self.capture.clone()
    }

    fn native_encoders(&self) -> Vec<Arc<dyn AudioEncoder>> {
        Vec::new()
    }

    fn reveal_in_file_manager(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported())
    }

    fn open_folder(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported())
    }

    fn open_file(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported())
    }

    fn move_to_trash(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported())
    }

    fn open_privacy_settings(&self, _pane: PrivacyPane) -> io::Result<()> {
        Err(unsupported())
    }
}
