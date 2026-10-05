//! The menu bar (macOS) / system tray (Windows, Linux) icon.
//!
//! `TrayController` keeps the native icon in step with the recording session and turns menu
//! clicks into application commands. The native objects live on the main thread.

mod icons;
mod native;
mod policy;
mod view;

use std::time::Duration;

use gpui_kit::{App, AsyncApp, BorrowAppContext as _, Global, Subscription, Task};
use tray_icon::menu::MenuEvent;
use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

use crate::app_window::Page;
use crate::services::Services;
use crate::session::{SessionState, TrackState};
use crate::settings_store::SettingsStore;
use crate::shell::Shell;

use native::NativeTray;
use view::{Phase, SessionSnapshot, TrayView};

pub use policy::POLICY;

/// Commands from the tray menu or icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    ToggleRecording,
    Open,
    ShowLibrary,
    ShowSettings,
    RevealRecordings,
    Quit,
}

impl TrayCommand {
    const ALL: [TrayCommand; 6] = [
        TrayCommand::ToggleRecording,
        TrayCommand::Open,
        TrayCommand::ShowLibrary,
        TrayCommand::ShowSettings,
        TrayCommand::RevealRecordings,
        TrayCommand::Quit,
    ];

    /// Menu item id; stable so events can be mapped back to commands.
    fn id(self) -> &'static str {
        match self {
            TrayCommand::ToggleRecording => "gym.toggle-recording",
            TrayCommand::Open => "gym.open",
            TrayCommand::ShowLibrary => "gym.show-library",
            TrayCommand::ShowSettings => "gym.show-settings",
            TrayCommand::RevealRecordings => "gym.reveal-recordings",
            TrayCommand::Quit => "gym.quit",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|command| command.id() == id)
    }
}

pub struct TrayController {
    native: Option<NativeTray>,
    ticker: Option<Task<()>>,
    _commands: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl Global for TrayController {}

pub fn init(cx: &mut App) {
    let (tx, rx) = async_channel::unbounded::<TrayCommand>();

    // Native callbacks run on the platform's event thread; forward them into GPUI.
    let menu_tx = tx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Some(command) = TrayCommand::from_id(&event.id.0) {
            let _ = menu_tx.send_blocking(command);
        }
    }));
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        // Where the menu is not on left click, a left click opens the window.
        if !POLICY.menu_on_left_click
            && let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
        {
            let _ = tx.send_blocking(TrayCommand::Open);
        }
    }));

    let commands = cx.spawn(async move |cx: &mut AsyncApp| {
        while let Ok(command) = rx.recv().await {
            cx.update(|cx| handle(command, cx));
        }
    });

    let session = Shell::session(cx);
    let subscriptions = vec![
        cx.observe(&session, |_, cx| refresh(cx)),
        cx.observe_global::<SettingsStore>(refresh),
        cx.observe_global::<Shell>(refresh),
    ];
    cx.set_global(TrayController {
        native: None,
        ticker: None,
        _commands: commands,
        _subscriptions: subscriptions,
    });
    refresh(cx);
}

fn handle(command: TrayCommand, cx: &mut App) {
    match command {
        TrayCommand::ToggleRecording => {
            Shell::session(cx).update(cx, |session, cx| session.toggle(cx));
        }
        TrayCommand::Open => Shell::show_window(None, cx),
        TrayCommand::ShowLibrary => Shell::show_window(Some(Page::Library), cx),
        TrayCommand::ShowSettings => Shell::show_window(Some(Page::Settings), cx),
        TrayCommand::RevealRecordings => {
            let services = Services::global(cx);
            let folder = services.music_folder(SettingsStore::get(cx));
            if let Err(err) = services.platform.open_folder(&folder) {
                tracing::warn!(%err, "cannot open the recordings folder");
            }
        }
        TrayCommand::Quit => Shell::request_quit(cx),
    }
}

fn snapshot(cx: &App) -> SessionSnapshot {
    let session = Shell::session(cx);
    let session = session.read(cx);
    let (phase, elapsed) = match session.state() {
        SessionState::Idle => (Phase::Idle, None),
        SessionState::Starting => (Phase::Starting, None),
        SessionState::Recording { since, .. } => (Phase::Recording, Some(since.elapsed())),
        SessionState::Stopping => (Phase::Stopping, None),
    };
    let tracks = session.tracks();
    SessionSnapshot {
        phase: Some(phase),
        elapsed,
        title: session.now_playing().map(|np| np.track.title.clone()),
        artist: session.now_playing().and_then(|np| np.track.artist.clone()),
        saved: tracks
            .iter()
            .filter(|t| t.state == TrackState::Saved)
            .count(),
        skipped: tracks
            .iter()
            .filter(|t| matches!(t.state, TrackState::Skipped(_)))
            .count(),
        can_record: session.can_record(cx),
        quitting: Shell::is_quitting(cx),
    }
}

/// Recomputes the tray from the current state, creating or removing the icon as needed.
fn refresh(cx: &mut App) {
    let wanted = SettingsStore::get(cx).general.show_tray_icon;
    let snapshot = snapshot(cx);
    let view = TrayView::build(&snapshot, &POLICY);
    let recording = snapshot.phase == Some(Phase::Recording);

    let available = cx.update_global::<TrayController, _>(|tray, cx| {
        match (&mut tray.native, wanted) {
            (Some(native), true) => native.apply(&view),
            (None, true) => match NativeTray::new(&view) {
                Ok(native) => tray.native = Some(native),
                Err(err) => tracing::error!(%err, "cannot create the tray icon"),
            },
            (Some(_), false) => tray.native = None,
            (None, false) => {}
        }
        // Tick once a second while recording so the elapsed time stays current.
        let ticking = recording && tray.native.is_some();
        if ticking && tray.ticker.is_none() {
            tray.ticker = Some(cx.spawn(async move |cx: &mut AsyncApp| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    cx.update(refresh);
                }
            }));
        } else if !ticking {
            tray.ticker = None;
        }
        tray.native.is_some()
    });
    Shell::set_tray_available(available, cx);
}
