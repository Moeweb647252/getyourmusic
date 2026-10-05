//! Application commands, their key bindings and the native menu bar.
//!
//! Each command is one Action; the menu bar, shortcuts and in-window buttons all dispatch it.

use gpui_kit::component::input;
use gpui_kit::{App, KeyBinding, Menu, MenuItem, actions};

actions!(
    gym,
    [
        ToggleRecording,
        ShowRecorder,
        ShowLibrary,
        ShowSettings,
        About,
        Hide,
        Minimize,
        Quit
    ]
);

#[cfg(target_os = "macos")]
const PRIMARY: &str = "cmd";
#[cfg(not(target_os = "macos"))]
const PRIMARY: &str = "ctrl";

pub fn init(cx: &mut App) {
    let key = |k: &str| format!("{PRIMARY}-{k}");
    // Bind before building menus: the menu bar captures shortcuts when it is set.
    cx.bind_keys([
        KeyBinding::new(&key("r"), ToggleRecording, None),
        KeyBinding::new(&key("1"), ShowRecorder, None),
        KeyBinding::new(&key("2"), ShowLibrary, None),
        KeyBinding::new(&key(","), ShowSettings, None),
        KeyBinding::new(&key("m"), Minimize, None),
        KeyBinding::new(&key("q"), Quit, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-h", Hide, None),
    ]);
    // Quit and the recording/navigation commands are handled by `Shell`.
    cx.on_action(|_: &Hide, cx| cx.hide());
    set_menus(false, cx);
}

/// Rebuilds the menu bar; the recording command's title follows the session state.
pub fn set_menus(recording: bool, cx: &mut App) {
    let toggle_title = if recording {
        tr!("menu.stop_recording")
    } else {
        tr!("menu.start_recording")
    };
    cx.set_menus(vec![
        Menu {
            name: tr!("app.name"),
            items: vec![
                MenuItem::action(tr!("menu.about"), About),
                MenuItem::separator(),
                MenuItem::action(tr!("menu.settings"), ShowSettings),
                MenuItem::separator(),
                MenuItem::action(tr!("menu.hide"), Hide),
                MenuItem::action(tr!("menu.quit"), Quit),
            ],
            disabled: false,
        },
        Menu {
            name: tr!("menu.edit"),
            items: vec![
                MenuItem::action(tr!("menu.undo"), input::Undo),
                MenuItem::action(tr!("menu.redo"), input::Redo),
                MenuItem::separator(),
                MenuItem::action(tr!("menu.cut"), input::Cut),
                MenuItem::action(tr!("menu.copy"), input::Copy),
                MenuItem::action(tr!("menu.paste"), input::Paste),
                MenuItem::separator(),
                MenuItem::action(tr!("menu.select_all"), input::SelectAll),
            ],
            disabled: false,
        },
        Menu {
            name: tr!("menu.recording"),
            items: vec![MenuItem::action(toggle_title, ToggleRecording)],
            disabled: false,
        },
        Menu {
            name: tr!("menu.view"),
            items: vec![
                MenuItem::action(tr!("nav.recorder"), ShowRecorder),
                MenuItem::action(tr!("nav.library"), ShowLibrary),
            ],
            disabled: false,
        },
        Menu {
            name: tr!("menu.window"),
            items: vec![MenuItem::action(tr!("menu.minimize"), Minimize)],
            disabled: false,
        },
    ]);
}
