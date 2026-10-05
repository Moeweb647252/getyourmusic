//! The user's settings as a GPUI global, persisted on every change.

use std::path::PathBuf;

use gpui_kit::{App, BorrowAppContext as _, Global};

use gym_core::settings::AppSettings;

pub struct SettingsStore {
    settings: AppSettings,
    path: PathBuf,
}

impl Global for SettingsStore {}

impl SettingsStore {
    /// Loads settings from `path`. Returns the backup path if a corrupt file was replaced.
    pub fn init(path: PathBuf, cx: &mut App) -> Option<PathBuf> {
        let (settings, recovered) = match AppSettings::load(&path) {
            Ok(loaded) => (loaded.settings, loaded.recovered_from),
            Err(err) => {
                tracing::error!(%err, "cannot read settings; using defaults");
                (AppSettings::default(), None)
            }
        };
        cx.set_global(Self { settings, path });
        recovered
    }

    pub fn get(cx: &App) -> &AppSettings {
        &cx.global::<Self>().settings
    }

    /// Applies a change, saves it and notifies observers of the global.
    pub fn update(cx: &mut App, change: impl FnOnce(&mut AppSettings)) {
        cx.update_global::<Self, _>(|store, _| {
            let before = store.settings.clone();
            change(&mut store.settings);
            if store.settings != before
                && let Err(err) = store.settings.save(&store.path)
            {
                tracing::error!(%err, "cannot save settings");
            }
        });
    }
}
