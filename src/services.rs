//! Long-lived application services shared by every view.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::{App, AppContext as _, Global, SharedString};

use gym_core::encode::EncoderRegistry;
use gym_core::library::Library;
use gym_core::now_playing::NowPlayingMonitor;
use gym_core::platform::{AppDirs, Platform};
use gym_core::settings::{AppSettings, StorageProviderKind, StorageSettings};
use gym_core::storage::{LocalStorage, NekostorageStorage, StorageProvider};

use crate::settings_store::SettingsStore;

pub struct Services {
    pub platform: Arc<dyn Platform>,
    pub dirs: AppDirs,
    pub encoders: EncoderRegistry,
    pub library: Arc<Library>,
    /// The system now-playing monitor, or why it could not be started.
    pub now_playing: Result<NowPlayingMonitor, SharedString>,
    /// The selected provider and the settings it was built from; kept so that state such as
    /// a server's free space survives between renders.
    storage: Mutex<Option<(StorageSettings, Arc<dyn StorageProvider>)>>,
    /// The last [`StorageProvider::check`] and the settings it was made for.
    storage_status: Mutex<Option<(StorageSettings, StorageStatus)>>,
    /// [`known_storages`](Self::known_storages), and what they were built from.
    known: Mutex<Option<KnownStorages>>,
}

/// A storage the Library shows, and other ids its recordings are filed under.
#[derive(Clone)]
pub struct KnownStorage {
    pub provider: Arc<dyn StorageProvider>,
    /// Library ids that resolve to this storage under a different id, e.g. an older form.
    pub aliases: Vec<String>,
}

struct KnownStorages {
    settings: StorageSettings,
    library_ids: Vec<String>,
    storages: Vec<KnownStorage>,
}

/// Whether the selected storage could be reached when it was last checked.
#[derive(Clone, Debug)]
pub enum StorageStatus {
    Checking,
    Reachable,
    Failed(SharedString),
}

/// Typing in a storage setting re-checks the destination once the input settles.
const STORAGE_CHECK_DELAY: Duration = Duration::from_millis(800);

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
        // Entries from before local ids named their folder were saved to the music folder.
        let music_folder = dirs.music_folder(&SettingsStore::get(cx).storage);
        let local_id = LocalStorage::new(music_folder).id().to_owned();
        if let Err(err) = library.rename_provider(LocalStorage::ID, &local_id) {
            tracing::warn!(%err, "cannot migrate library entries to folder ids");
        }
        cx.set_global(Self {
            platform,
            dirs,
            encoders,
            library,
            now_playing,
            storage: Mutex::new(None),
            storage_status: Mutex::new(None),
            known: Mutex::new(None),
        });
        Self::check_storage(cx);
        Self::recheck_storage_on_change(cx);
    }

    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    /// The folder recordings are saved to with the current settings.
    pub fn music_folder(&self, settings: &AppSettings) -> PathBuf {
        self.dirs.music_folder(&settings.storage)
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
        let mut cache = self.storage.lock().unwrap();
        if let Some((built_for, provider)) = cache.as_ref()
            && *built_for == settings.storage
        {
            return Arc::clone(provider);
        }
        let provider: Arc<dyn StorageProvider> = match settings.storage.provider {
            StorageProviderKind::Local => Arc::new(LocalStorage::new(self.music_folder(settings))),
            StorageProviderKind::Nekostorage => {
                Arc::new(NekostorageStorage::new(&settings.storage.nekostorage))
            }
        };
        *cache = Some((settings.storage.clone(), Arc::clone(&provider)));
        provider
    }

    /// The provider a library entry was stored with, which need not be the selected one.
    pub fn storage_for(
        &self,
        provider_id: &str,
        settings: &AppSettings,
    ) -> Option<Arc<dyn StorageProvider>> {
        let selected = self.storage(settings);
        if selected.id() == provider_id {
            return Some(selected);
        }
        gym_core::storage::provider_for_id(
            provider_id,
            &settings.storage,
            &self.music_folder(settings),
        )
    }

    /// Every storage the Library shows: this Mac's music folder, the configured server, and
    /// any other storage that holds recordings in the Library. Built again only when the
    /// storage settings or the Library's storages change.
    pub fn known_storages(&self, settings: &AppSettings) -> Vec<KnownStorage> {
        let library_ids = self.library.provider_ids();
        let mut known = self.known.lock().unwrap();
        if let Some(cached) = known.as_ref()
            && cached.settings == settings.storage
            && cached.library_ids == library_ids
        {
            return cached.storages.clone();
        }
        let music_folder = self.music_folder(settings);
        let mut storages = vec![KnownStorage {
            provider: Arc::new(LocalStorage::new(&music_folder)),
            aliases: Vec::new(),
        }];
        if NekostorageStorage::validate(&settings.storage.nekostorage).is_ok() {
            storages.push(KnownStorage {
                provider: Arc::new(NekostorageStorage::new(&settings.storage.nekostorage)),
                aliases: Vec::new(),
            });
        }
        for id in &library_ids {
            let Some(provider) =
                gym_core::storage::provider_for_id(id, &settings.storage, &music_folder)
            else {
                continue;
            };
            // Deduplicated by the id it resolves to, so one folder is never listed twice.
            match storages
                .iter_mut()
                .find(|known| known.provider.id() == provider.id())
            {
                Some(known) if known.provider.id() != id => known.aliases.push(id.clone()),
                Some(_) => {}
                None => {
                    let aliases = if provider.id() == id {
                        Vec::new()
                    } else {
                        vec![id.clone()]
                    };
                    storages.push(KnownStorage { provider, aliases });
                }
            }
        }
        *known = Some(KnownStorages {
            settings: settings.storage.clone(),
            library_ids,
            storages: storages.clone(),
        });
        storages
    }

    /// Space left at the selected destination, when it reports it.
    pub fn free_space(&self, settings: &AppSettings) -> Option<u64> {
        self.storage(settings).available_space()
    }

    /// The last check of the selected storage, unless its settings changed since.
    pub fn storage_status(&self, settings: &AppSettings) -> Option<StorageStatus> {
        self.storage_status
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(checked, _)| *checked == settings.storage)
            .map(|(_, status)| status.clone())
    }

    /// Checks the selected storage in the background, which also refreshes the space it
    /// reports, then redraws so every view shows the result.
    pub fn check_storage(cx: &mut App) {
        let settings = SettingsStore::get(cx);
        let checked = settings.storage.clone();
        let services = Self::global(cx);
        let storage = services.storage(settings);
        *services.storage_status.lock().unwrap() = Some((checked.clone(), StorageStatus::Checking));
        cx.refresh_windows();
        let check = cx.background_spawn(async move { storage.check() });
        cx.spawn(async move |cx| {
            let status = match check.await {
                Ok(()) => StorageStatus::Reachable,
                Err(err) => {
                    tracing::warn!(%err, "storage check failed");
                    StorageStatus::Failed(err.to_string().into())
                }
            };
            cx.update(|cx| {
                let mut current = Self::global(cx).storage_status.lock().unwrap();
                // A newer check may have started for other settings.
                if current
                    .as_ref()
                    .is_some_and(|(settings, _)| *settings == checked)
                {
                    *current = Some((checked, status));
                }
                drop(current);
                cx.refresh_windows();
            });
        })
        .detach();
    }

    /// Re-checks the storage after its settings change, so the space shown is always for the
    /// selected destination.
    fn recheck_storage_on_change(cx: &mut App) {
        let last = Rc::new(RefCell::new(SettingsStore::get(cx).storage.clone()));
        let generation = Rc::new(Cell::new(0u64));
        cx.observe_global::<SettingsStore>(move |cx| {
            let storage = &SettingsStore::get(cx).storage;
            if *last.borrow() == *storage {
                return;
            }
            *last.borrow_mut() = storage.clone();
            let current = generation.get() + 1;
            generation.set(current);
            let generation = Rc::clone(&generation);
            cx.spawn(async move |cx| {
                cx.background_executor().timer(STORAGE_CHECK_DELAY).await;
                if generation.get() == current {
                    cx.update(Self::check_storage);
                }
            })
            .detach();
        })
        .detach();
    }
}
