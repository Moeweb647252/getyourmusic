//! Application lifecycle: owns the recording session and the main window, and decides what
//! closing the window and quitting mean.
//!
//! The session lives here rather than in a window so recording continues after the window is
//! closed; the tray icon (see `tray`) brings the window back.

use std::time::Duration;

use gpui_kit::component::TitleBar;
use gpui_kit::{
    ActivationPolicy, AnyWindowHandle, App, AppContext as _, BorrowAppContext as _, Bounds, Entity,
    Global, Size, Subscription, SystemNotification, WeakEntity, WindowBounds, WindowId,
    WindowOptions, px, size,
};

use gym_core::engine::StopReason;

use crate::actions::{Quit, ShowLibrary, ShowRecorder, ShowSettings, ToggleRecording};
use crate::app_window::{AppWindow, Page};
use crate::session::{RecordingSession, SessionEvent};
use crate::settings;
use crate::settings_store::SettingsStore;
use crate::tray::POLICY;

const APP_IDENTIFIER: &str = "dev.getyourmusic.GetYourMusic";
/// Upper bound on waiting for queued tracks to finish encoding before quitting anyway.
const QUIT_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Shell {
    session: Entity<RecordingSession>,
    window: Option<(AnyWindowHandle, WeakEntity<AppWindow>)>,
    /// The settings file was unreadable; tell the user in the first window.
    settings_recovered: bool,
    quitting: bool,
    tray_available: bool,
    menu_recording: bool,
    _subscriptions: Vec<Subscription>,
}

impl Global for Shell {}

impl Shell {
    pub fn init(settings_recovered: bool, cx: &mut App) {
        let session = cx.new(RecordingSession::new);
        let subscriptions = vec![
            cx.observe(&session, |session, cx| {
                Self::on_session_changed(&session, cx)
            }),
            cx.subscribe(&session, |_, event: &SessionEvent, cx| {
                Self::on_session_event(event, cx)
            }),
            cx.on_window_closed(|cx, window_id| Self::on_window_closed(window_id, cx)),
        ];
        cx.set_global(Self {
            session,
            window: None,
            settings_recovered,
            quitting: false,
            tray_available: false,
            menu_recording: false,
            _subscriptions: subscriptions,
        });

        // Commands that must work with or without a window.
        cx.on_action(|_: &ToggleRecording, cx| {
            Self::session(cx).update(cx, |session, cx| session.toggle(cx));
        });
        cx.on_action(|_: &ShowRecorder, cx| Self::show_window(Some(Page::Recorder), cx));
        cx.on_action(|_: &ShowLibrary, cx| Self::show_window(Some(Page::Library), cx));
        cx.on_action(|_: &ShowSettings, cx| Self::show_window(Some(Page::Settings), cx));
        cx.on_action(|_: &Quit, cx| Self::request_quit(cx));
    }

    fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    pub fn session(cx: &App) -> Entity<RecordingSession> {
        Self::global(cx).session.clone()
    }

    pub fn is_quitting(cx: &App) -> bool {
        Self::global(cx).quitting
    }

    pub fn has_window(cx: &App) -> bool {
        Self::global(cx).window.is_some()
    }

    /// Records whether a tray icon is shown, which decides what closing the window does.
    pub fn set_tray_available(available: bool, cx: &mut App) {
        if Self::global(cx).tray_available != available {
            cx.update_global::<Self, _>(|shell, _| shell.tray_available = available);
        }
    }

    /// Brings the main window to the front, opening it if needed, optionally on `page`.
    pub fn show_window(page: Option<Page>, cx: &mut App) {
        if POLICY.hide_dock_when_closed {
            cx.set_activation_policy(ActivationPolicy::Regular);
        }
        let shown = Self::global(cx)
            .window
            .clone()
            .is_some_and(|(handle, view)| {
                handle
                    .update(cx, |_, window, cx| {
                        window.activate_window();
                        if let Some(page) = page {
                            let _ = view.update(cx, |view, cx| view.navigate(page, window, cx));
                        }
                    })
                    .is_ok()
            });
        if !shown {
            Self::open_window(page, cx);
        }
        cx.activate(true);
    }

    fn open_window(page: Option<Page>, cx: &mut App) {
        let window_size = size(px(1120.), px(760.));
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                window_size,
                cx,
            ))),
            window_min_size: Some(Size {
                width: px(900.),
                height: px(600.),
            }),
            app_id: Some(APP_IDENTIFIER.into()),
            ..TitleBar::window_options()
        };
        let session = Self::session(cx);
        let settings_recovered = std::mem::take(&mut cx.global_mut::<Self>().settings_recovered);
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            settings::apply_theme(window, cx);
            cx.new(|cx| AppWindow::new(session, page, settings_recovered, window, cx))
        });
        match opened {
            Ok((handle, view)) => {
                cx.update_global::<Self, _>(|shell, _| {
                    shell.window = Some((handle, view.downgrade()));
                });
            }
            Err(err) => tracing::error!(%err, "cannot open the main window"),
        }
    }

    fn on_window_closed(window_id: WindowId, cx: &mut App) {
        let is_main = Self::global(cx)
            .window
            .as_ref()
            .is_some_and(|(handle, _)| handle.window_id() == window_id);
        if !is_main {
            return;
        }
        cx.update_global::<Self, _>(|shell, _| shell.window = None);
        if Self::is_quitting(cx) {
            return;
        }
        let general = &SettingsStore::get(cx).general;
        let keep_running = Self::global(cx).tray_available
            && general.show_tray_icon
            && general.keep_running_when_closed;
        if keep_running {
            if POLICY.hide_dock_when_closed {
                cx.set_activation_policy(ActivationPolicy::Accessory);
            }
        } else {
            Self::request_quit(cx);
        }
    }

    /// Quits once recording has stopped and queued tracks are saved.
    pub fn request_quit(cx: &mut App) {
        let session = Self::session(cx);
        if !session.read(cx).is_active() {
            cx.quit();
            return;
        }
        if Self::is_quitting(cx) {
            return;
        }
        tracing::info!("quitting after the recording session finishes");
        cx.update_global::<Self, _>(|shell, _| shell.quitting = true);
        session.update(cx, |session, cx| session.stop(cx));
        cx.spawn(async move |cx| {
            cx.background_executor().timer(QUIT_TIMEOUT).await;
            tracing::warn!("timed out waiting for the recording session; quitting");
            cx.update(|cx| cx.quit());
        })
        .detach();
    }

    fn on_session_changed(session: &Entity<RecordingSession>, cx: &mut App) {
        let (active, recording) = {
            let session = session.read(cx);
            (session.is_active(), session.is_recording())
        };
        // A quit requested while the device was still opening stops as soon as it is open.
        if Self::is_quitting(cx) && recording {
            session.update(cx, |session, cx| session.stop(cx));
        }
        if Self::global(cx).menu_recording != active {
            cx.global_mut::<Self>().menu_recording = active;
            crate::actions::set_menus(active, cx);
        }
    }

    fn on_session_event(event: &SessionEvent, cx: &mut App) {
        if Self::is_quitting(cx)
            && matches!(
                event,
                SessionEvent::Stopped(_) | SessionEvent::StartFailed(_)
            )
        {
            cx.quit();
            return;
        }
        // With a window open, it shows these as in-app notifications instead.
        if Self::has_window(cx) {
            return;
        }
        let (tag, title, body) = match event {
            SessionEvent::StartFailed(error) => {
                ("start-failed", tr!("toast.start_failed"), error.clone())
            }
            SessionEvent::Failed { title, error } => (
                "save-failed",
                tr!("toast.save_failed", title = title),
                error.clone(),
            ),
            SessionEvent::Stopped(StopReason::Idle) => (
                "stopped",
                tr!("app.name"),
                tr!(
                    "toast.stopped_idle",
                    minutes = SettingsStore::get(cx).recording.auto_stop_minutes
                ),
            ),
            SessionEvent::Stopped(StopReason::DeviceLost(_)) => {
                ("stopped", tr!("app.name"), tr!("toast.stopped_device_lost"))
            }
            _ => return,
        };
        cx.show_system_notification(SystemNotification {
            tag: tag.into(),
            title,
            body,
            actions: Vec::new(),
        });
    }
}
