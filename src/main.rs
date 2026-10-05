//! GetYourMusic: records what a music player plays into tagged audio files.
//!
//! The binary composes the portable core (`gym-core`), encoders (`gym-media`) and the
//! operating system integration (`gym-platform`) behind a GPUI Kit interface.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
mod i18n;
mod actions;
mod app_window;
mod assets;
mod display;
mod library;
mod logging;
mod recorder;
mod services;
mod session;
mod settings;
mod settings_store;
mod shell;
mod tray;

use gpui_kit::App;
use rust_i18n::t;

use crate::services::Services;
use crate::settings_store::SettingsStore;
use crate::shell::Shell;

rust_i18n::i18n!("locales", fallback = "en");

const APP_IDENTIFIER: &str = "dev.getyourmusic.GetYourMusic";

fn main() {
    let dirs = gym_core::platform::AppDirs::discover().expect("cannot resolve user directories");
    let _log_guard = logging::init(&dirs.logs);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");

    let app = gpui_kit::application().with_assets(assets::AppAssets);
    // Clicking the Dock icon with no window open.
    app.on_reopen(|cx| Shell::show_window(None, cx));
    app.run(move |cx: &mut App| {
        {
            use gpui_kit::component as gpui_component;
            rust_i18n::extend!(gpui_component);
        }
        gpui_kit::init(cx);
        cx.set_app_identity(APP_IDENTIFIER, &t!("app.name"));

        let recovered_settings = SettingsStore::init(dirs.settings_file(), cx);
        Services::init(dirs.clone(), cx);
        rust_i18n::set_locale(&SettingsStore::get(cx).appearance.locale);
        actions::init(cx);
        Shell::init(recovered_settings.is_some(), cx);
        tray::init(cx);
        Shell::show_window(None, cx);

        if cfg!(debug_assertions) && std::env::var_os("GYM_AUTOSTART").is_some() {
            Shell::session(cx).update(cx, |session, cx| session.start(cx));
        }
    });
}
