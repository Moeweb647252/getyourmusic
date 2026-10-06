//! Long-lived application services shared by every view.

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::{App, Global, SharedString};

use gym_core::encode::EncoderRegistry;
use gym_core::library::Library;
use gym_core::now_playing::NowPlayingMonitor;
use gym_core::platform::{AppDirs, Platform};
use gym_core::settings::{AppSettings, StorageProviderKind};
use gym_core::storage::{LocalStorage, StorageProvider};

use crate::settings_store::SettingsStore;

pub struct Services {
    pub platform: Arc<dyn Platform>,
    pub dirs: AppDirs,
    pub encoders: EncoderRegistry,
    pub library: Arc<Library>,
    /// The system now-playing monitor, or why it could not be started.
    pub now_playing: Result<NowPlayingMonitor, SharedString>,
}

impl Global for Services {}

impl Services {
    pub fn init(dirs: AppDirs, cx: &mut App) {
        let platform = gym_platform::current();
        tracing::info!(platform = %platform.name(), "platform detected");

        let mut encoders = EncoderRegistry::new();
        gym_media::register_encoders(&mut encoders);
        for encoder in platform.native_encoders() {
            encoders.register(encoder);
        }

        // Leftovers from a session that did not shut down cleanly cannot be resumed.
        gym_core::engine::clean_spool(&dirs.spool_dir());
        if let Some(folder) = &SettingsStore::get(cx).cache.folder {
            gym_core::engine::clean_spool(folder);
        }

        let now_playing = platform
            .now_playing_source()
            .and_then(|source| NowPlayingMonitor::start(source.as_ref()))
            .map_err(|err| {
                tracing::error!(%err, "now playing is unavailable");
                SharedString::from(err.to_string())
            });

        let library = Arc::new(Library::open(dirs.library_file()));
        cx.set_global(Self {
            platform,
            dirs,
            encoders,
            library,
            now_playing,
        });
    }

    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    /// The folder recordings are saved to with the current settings.
    pub fn music_folder(&self, settings: &AppSettings) -> PathBuf {
        settings
            .storage
            .local_folder
            .clone()
            .unwrap_or_else(|| self.dirs.default_music_folder.clone())
    }

    /// The folder for audio of tracks being recorded with the current settings.
    pub fn cache_folder(&self, settings: &AppSettings) -> PathBuf {
        settings
            .cache
            .folder
            .clone()
            .unwrap_or_else(|| self.dirs.spool_dir())
    }

    /// The storage provider selected in settings.
    pub fn storage(&self, settings: &AppSettings) -> Arc<dyn StorageProvider> {
        match settings.storage.provider {
            StorageProviderKind::Local => Arc::new(LocalStorage::new(self.music_folder(settings))),
        }
    }
}
