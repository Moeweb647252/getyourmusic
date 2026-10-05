//! macOS integration: MediaRemote now playing, Core Audio capture, AudioToolbox encoders.

mod m4a;
mod now_playing;

use std::io;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use objc2_foundation::{NSFileManager, NSOperatingSystemVersion, NSProcessInfo, NSString, NSURL};

use gym_core::capture::AudioCaptureBackend;
use gym_core::encode::AudioEncoder;
use gym_core::now_playing::{NowPlayingError, NowPlayingSource};
use gym_core::platform::{Platform, PlatformCapabilities, PrivacyPane};

use crate::cpal_capture::CpalCapture;

pub use m4a::M4aEncoder;
pub use now_playing::AdapterNowPlaying;

pub struct MacPlatform {
    version: NSOperatingSystemVersion,
    capture: Arc<CpalCapture>,
}

/// Core Audio process taps, used for output loopback, exist since macOS 14.2.
fn supports_loopback(version: &NSOperatingSystemVersion) -> bool {
    (version.majorVersion, version.minorVersion) >= (14, 2)
}

impl MacPlatform {
    pub fn new() -> Self {
        let version = NSProcessInfo::processInfo().operatingSystemVersion();
        Self {
            capture: Arc::new(CpalCapture::new(supports_loopback(&version))),
            version,
        }
    }
}

impl Default for MacPlatform {
    fn default() -> Self {
        Self::new()
    }
}

fn open(args: &[&std::ffi::OsStr]) -> io::Result<()> {
    let status = Command::new("/usr/bin/open").args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("open exited with {status}")))
    }
}

impl Platform for MacPlatform {
    fn name(&self) -> String {
        format!(
            "macOS {}.{}.{}",
            self.version.majorVersion, self.version.minorVersion, self.version.patchVersion
        )
    }

    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            system_loopback: supports_loopback(&self.version),
        }
    }

    fn now_playing_source(&self) -> Result<Box<dyn NowPlayingSource>, NowPlayingError> {
        Ok(Box::new(AdapterNowPlaying::locate()?))
    }

    fn capture_backend(&self) -> Arc<dyn AudioCaptureBackend> {
        self.capture.clone()
    }

    fn native_encoders(&self) -> Vec<Arc<dyn AudioEncoder>> {
        vec![Arc::new(M4aEncoder)]
    }

    fn reveal_in_file_manager(&self, path: &Path) -> io::Result<()> {
        open(&["-R".as_ref(), path.as_os_str()])
    }

    fn open_folder(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)?;
        open(&[path.as_os_str()])
    }

    fn open_file(&self, path: &Path) -> io::Result<()> {
        open(&[path.as_os_str()])
    }

    fn move_to_trash(&self, path: &Path) -> io::Result<()> {
        let path = path
            .to_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path is not UTF-8"))?;
        let url = NSURL::fileURLWithPath(&NSString::from_str(path));
        NSFileManager::defaultManager()
            .trashItemAtURL_resultingItemURL_error(&url, None)
            .map_err(|error| io::Error::other(error.localizedDescription().to_string()))
    }

    fn open_privacy_settings(&self, pane: PrivacyPane) -> io::Result<()> {
        let anchor = match pane {
            PrivacyPane::SystemAudioRecording => "Privacy_AudioCapture",
            PrivacyPane::Microphone => "Privacy_Microphone",
        };
        let url = format!("x-apple.systempreferences:com.apple.preference.security?{anchor}");
        open(&[url.as_ref()])
    }
}
