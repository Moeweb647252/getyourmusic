//! Lists every known storage in the background and records what each one holds.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{AppContext as _, Context, SharedString, Task};

use gym_core::settings::StorageSettings;
use gym_core::storage::StorageError;

use crate::services::Services;
use crate::settings_store::SettingsStore;

/// A listing older than this is repeated when the Library is shown. Listing a server costs
/// one request per folder, each of which may reach a rate-limited cloud backend.
const STALE_AFTER: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug)]
pub enum ScanStatus {
    Scanning,
    Scanned,
    /// The storage couldn't be listed; the Library keeps showing what it held before.
    Offline(SharedString),
}

pub struct LibraryScanner {
    statuses: HashMap<String, ScanStatus>,
    last_scan: Option<Instant>,
    /// The storage settings the last scan used.
    scanned_with: Option<StorageSettings>,
    /// One listing per storage, so a storage that hangs doesn't hold up the others.
    tasks: HashMap<String, Task<()>>,
}

impl LibraryScanner {
    pub fn new() -> Self {
        Self {
            statuses: HashMap::new(),
            last_scan: None,
            scanned_with: None,
            tasks: HashMap::new(),
        }
    }

    pub fn status(&self, provider_id: &str) -> Option<&ScanStatus> {
        self.statuses.get(provider_id)
    }

    pub fn is_scanning(&self) -> bool {
        self.statuses
            .values()
            .any(|s| matches!(s, ScanStatus::Scanning))
    }

    /// Scans unless a recent scan used the current storage settings.
    pub fn scan_if_stale(&mut self, cx: &mut Context<Self>) {
        let fresh = self.last_scan.is_some_and(|at| at.elapsed() < STALE_AFTER)
            && self.scanned_with.as_ref() == Some(&SettingsStore::get(cx).storage);
        if !fresh {
            self.scan(cx);
        }
    }

    /// Lists every known storage that isn't being listed already, each on its own.
    pub fn scan(&mut self, cx: &mut Context<Self>) {
        let settings = SettingsStore::get(cx);
        self.scanned_with = Some(settings.storage.clone());
        self.last_scan = Some(Instant::now());
        let services = Services::global(cx);
        let storages = services.known_storages(settings);
        let library = Arc::clone(&services.library);
        for known in storages {
            let id = known.provider.id().to_owned();
            if matches!(self.statuses.get(&id), Some(ScanStatus::Scanning)) {
                continue;
            }
            self.statuses.insert(id.clone(), ScanStatus::Scanning);
            let storage = known.provider;
            let library = Arc::clone(&library);
            let listing = cx.background_spawn({
                let id = id.clone();
                async move {
                    let started = library.begin_listing();
                    let result = match storage.list() {
                        Ok(files) => library
                            .apply_listing(&id, files, started)
                            .map_err(|err| err.to_string()),
                        Err(err) => {
                            library.abandon_listing(started);
                            match err {
                                // Nothing was ever saved there, e.g. before the first recording.
                                StorageError::Missing(_) if !library.has_items(&id) => Ok(()),
                                err => Err(err.to_string()),
                            }
                        }
                    };
                    if let Err(err) = &result {
                        tracing::warn!(storage = %id, %err, "cannot list storage");
                    }
                    result
                }
            });
            let task = cx.spawn({
                let id = id.clone();
                async move |this, cx| {
                    let status = match listing.await {
                        Ok(()) => ScanStatus::Scanned,
                        Err(err) => ScanStatus::Offline(err.into()),
                    };
                    let _ = this.update(cx, |this, cx| {
                        this.statuses.insert(id, status);
                        cx.notify();
                    });
                }
            });
            self.tasks.insert(id, task);
        }
        cx.notify();
    }
}
