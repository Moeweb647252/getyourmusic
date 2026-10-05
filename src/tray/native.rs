//! The tray icon and its menu, built on `tray-icon` (macOS, Windows, Linux via ksni).
//!
//! Must be created and used on the main thread.

use tray_icon::menu::accelerator::{Accelerator, Code, Modifiers};
use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use super::TrayCommand;
use super::icons::TrayIcons;
use super::policy::POLICY;
use super::view::TrayView;

#[cfg(target_os = "macos")]
const PRIMARY: Modifiers = Modifiers::META;
#[cfg(not(target_os = "macos"))]
const PRIMARY: Modifiers = Modifiers::CONTROL;

pub struct NativeTray {
    tray: TrayIcon,
    icons: TrayIcons,
    menu: Menu,
    status: MenuItem,
    now_playing: MenuItem,
    counts: MenuItem,
    toggle: MenuItem,
    current: TrayView,
}

/// Position of the counts row: after the status and now-playing rows.
const COUNTS_POSITION: usize = 2;

impl NativeTray {
    pub fn new(view: &TrayView) -> Result<Self, String> {
        let icons = TrayIcons::load()?;
        let info = |text: &str| MenuItem::new(text, false, None);
        let status = info(&view.status);
        let now_playing = info(&view.now_playing);
        let counts = info(view.counts.as_deref().unwrap_or_default());
        let toggle = MenuItem::with_id(
            TrayCommand::ToggleRecording.id(),
            &view.toggle_label,
            view.toggle_enabled,
            Some(Accelerator::new(PRIMARY, Code::KeyR)),
        );
        let command = |command: TrayCommand, label: &str, key: Option<Code>| {
            MenuItem::with_id(
                command.id(),
                label,
                true,
                key.map(|key| Accelerator::new(PRIMARY, key)),
            )
        };
        let reveal_label = if cfg!(target_os = "macos") {
            tr!("tray.show_recordings_macos")
        } else {
            tr!("tray.show_recordings_other")
        };

        let menu = Menu::new();
        let separator = PredefinedMenuItem::separator;
        menu.append_items(&[
            &status,
            &now_playing,
            &separator(),
            &toggle,
            &separator(),
            &command(TrayCommand::Open, &tr!("tray.open"), None),
            &command(TrayCommand::ShowLibrary, &tr!("tray.show_library"), None),
            &command(TrayCommand::RevealRecordings, &reveal_label, None),
            &command(
                TrayCommand::ShowSettings,
                &tr!("menu.settings"),
                Some(Code::Comma),
            ),
            &separator(),
            &command(TrayCommand::Quit, &tr!("menu.quit"), Some(Code::KeyQ)),
        ])
        .map_err(|e| e.to_string())?;
        if view.counts.is_some() {
            menu.insert(&counts, COUNTS_POSITION)
                .map_err(|e| e.to_string())?;
        }

        let icon = if view.recording {
            icons.recording.clone()
        } else {
            icons.idle.clone()
        };
        let builder = TrayIconBuilder::new().with_id("gym-tray");
        let builder = if POLICY.template_icon {
            builder.with_icon_templated(icon)
        } else {
            builder.with_icon(icon)
        };
        let mut builder = builder
            .with_menu(Box::new(menu.clone()))
            .with_menu_on_left_click(POLICY.menu_on_left_click)
            .with_tooltip(&view.tooltip);
        if let Some(title) = &view.title {
            builder = builder.with_title(title);
        }
        let tray = builder.build().map_err(|e| e.to_string())?;

        Ok(Self {
            tray,
            icons,
            menu,
            status,
            now_playing,
            counts,
            toggle,
            current: view.clone(),
        })
    }

    /// Updates only what changed since the last call.
    pub fn apply(&mut self, view: &TrayView) {
        if *view == self.current {
            return;
        }
        let old = std::mem::replace(&mut self.current, view.clone());
        let mut menu_changed = false;

        if view.recording != old.recording {
            let icon = if view.recording {
                &self.icons.recording
            } else {
                &self.icons.idle
            };
            let result = if POLICY.template_icon {
                self.tray.set_icon_templated(Some(icon.clone()))
            } else {
                self.tray.set_icon(Some(icon.clone()))
            };
            if let Err(err) = result {
                tracing::warn!(%err, "cannot update the tray icon");
            }
        }
        if view.title != old.title {
            self.tray.set_title(view.title.as_deref());
        }
        if view.tooltip != old.tooltip
            && let Err(err) = self.tray.set_tooltip(Some(&view.tooltip))
        {
            tracing::warn!(%err, "cannot update the tray tooltip");
        }
        if view.status != old.status {
            self.status.set_text(&view.status);
            menu_changed = true;
        }
        if view.now_playing != old.now_playing {
            self.now_playing.set_text(&view.now_playing);
            menu_changed = true;
        }
        if view.counts != old.counts {
            match (&view.counts, &old.counts) {
                (Some(text), None) => {
                    self.counts.set_text(text);
                    let _ = self.menu.insert(&self.counts, COUNTS_POSITION);
                }
                (None, Some(_)) => {
                    let _ = self.menu.remove(&self.counts);
                }
                (Some(text), Some(_)) => self.counts.set_text(text),
                (None, None) => {}
            }
            menu_changed = true;
        }
        if view.toggle_label != old.toggle_label {
            self.toggle.set_text(&view.toggle_label);
            menu_changed = true;
        }
        if view.toggle_enabled != old.toggle_enabled {
            self.toggle.set_enabled(view.toggle_enabled);
            menu_changed = true;
        }
        if menu_changed && POLICY.reset_menu_after_change {
            self.tray.set_menu(Some(Box::new(self.menu.clone())));
        }
    }
}
