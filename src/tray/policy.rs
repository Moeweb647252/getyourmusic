//! How the tray icon behaves on each operating system.

/// Platform conventions for the tray icon. The implementation is shared; only these differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrayPolicy {
    /// Use a monochrome template image that the system tints (macOS menu bar).
    pub template_icon: bool,
    /// A left click opens the menu (macOS); otherwise it opens the window and the menu is on
    /// right click (Windows notification area, Linux StatusNotifier convention).
    pub menu_on_left_click: bool,
    /// Show the elapsed recording time as text next to the icon; otherwise in the tooltip.
    pub timer_in_title: bool,
    /// Hide the Dock icon while no window is open.
    pub hide_dock_when_closed: bool,
    /// The backend snapshots the menu, so changes need the menu to be set again (ksni).
    pub reset_menu_after_change: bool,
}

#[cfg(target_os = "macos")]
pub const POLICY: TrayPolicy = TrayPolicy {
    template_icon: true,
    menu_on_left_click: true,
    timer_in_title: true,
    hide_dock_when_closed: true,
    reset_menu_after_change: false,
};

#[cfg(target_os = "windows")]
pub const POLICY: TrayPolicy = TrayPolicy {
    template_icon: false,
    menu_on_left_click: false,
    timer_in_title: false,
    hide_dock_when_closed: false,
    reset_menu_after_change: false,
};

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const POLICY: TrayPolicy = TrayPolicy {
    template_icon: false,
    menu_on_left_click: false,
    timer_in_title: false,
    hide_dock_when_closed: false,
    reset_menu_after_change: true,
};
