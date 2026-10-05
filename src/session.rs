//! The recording session: owns the engine and turns its events into UI state.
//!
//! One `RecordingSession` entity exists per application. Views observe it for state and
//! subscribe to [`SessionEvent`]s for one-off notifications.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{
    App, AppContext as _, AsyncApp, Context, EventEmitter, Image, ImageFormat, SharedString,
    Subscription, Task, WeakEntity,
};

use gym_core::capture::CaptureSource;
use gym_core::engine::{
    EngineConfig, EngineEvent, EngineHandle, EngineServices, LevelMeter, OutputConfig,
    PartialReason, RecordingEngine, SegmentId, SkipReason, StopReason,
};
use gym_core::model::{Artwork, NowPlaying};
use gym_core::naming::{NamingFallbacks, NamingTemplate};
use gym_core::now_playing::NowPlayingEvent;
use gym_core::settings::AppSettings;

use crate::services::Services;
use crate::settings_store::SettingsStore;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TrackState {
    Recording,
    Paused,
    /// Ended and waiting for, or in, encoding (progress `0.0..=1.0`).
    Encoding(f32),
    Saved,
    Skipped(SkipReason),
    Failed,
}

/// One track captured in this session, newest first in [`RecordingSession::tracks`].
#[derive(Clone, Debug)]
pub struct SessionTrack {
    pub id: SegmentId,
    pub title: SharedString,
    pub artist: Option<SharedString>,
    pub state: TrackState,
    pub partial: Option<PartialReason>,
    pub duration: Option<Duration>,
    pub path: Option<PathBuf>,
    pub error: Option<SharedString>,
}

#[derive(Clone, Debug)]
pub enum SessionState {
    Idle,
    Starting,
    Recording {
        since: Instant,
        source: CaptureSource,
    },
    /// Capture stopped; queued tracks are still being encoded.
    Stopping,
}

/// One-off notifications for the window.
pub enum SessionEvent {
    StartFailed(SharedString),
    Saved,
    Skipped {
        title: SharedString,
        reason: SkipReason,
    },
    Failed {
        title: SharedString,
        error: SharedString,
    },
    Stopped(StopReason),
}

pub struct RecordingSession {
    state: SessionState,
    engine: Option<EngineHandle>,
    meter: Option<Arc<LevelMeter>>,
    tracks: Vec<SessionTrack>,
    signal_silent: bool,
    now_playing: Option<NowPlaying>,
    artwork: Option<(Artwork, Arc<Image>)>,
    _now_playing_task: Option<Task<()>>,
    _engine_task: Option<Task<()>>,
    _settings_observer: Subscription,
}

impl EventEmitter<SessionEvent> for RecordingSession {}

/// Moves items from a blocking channel to an async one on a helper thread.
fn bridge<T: Send + 'static>(rx: crossbeam_channel::Receiver<T>) -> async_channel::Receiver<T> {
    let (tx, async_rx) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("ui-bridge".into())
        .spawn(move || {
            for item in rx {
                if tx.send_blocking(item).is_err() {
                    break;
                }
            }
        })
        .expect("cannot spawn bridge thread");
    async_rx
}

impl RecordingSession {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let now_playing_task = Services::global(cx)
            .now_playing
            .as_ref()
            .ok()
            .map(|monitor| {
                let events = bridge(monitor.subscribe());
                cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                    while let Ok(event) = events.recv().await {
                        let updated = this.update(cx, |session, cx| {
                            session.apply_now_playing(event);
                            cx.notify();
                        });
                        if updated.is_err() {
                            break;
                        }
                    }
                })
            });
        let settings_observer = cx.observe_global::<SettingsStore>(|session, cx| {
            if let Some(engine) = &session.engine {
                engine.update_output(output_config(SettingsStore::get(cx)));
            }
        });
        Self {
            state: SessionState::Idle,
            engine: None,
            meter: None,
            tracks: Vec::new(),
            signal_silent: false,
            now_playing: None,
            artwork: None,
            _now_playing_task: now_playing_task,
            _engine_task: None,
            _settings_observer: settings_observer,
        }
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.state, SessionState::Idle)
    }

    pub fn is_recording(&self) -> bool {
        matches!(self.state, SessionState::Recording { .. })
    }

    pub fn tracks(&self) -> &[SessionTrack] {
        &self.tracks
    }

    pub fn meter(&self) -> Option<Arc<LevelMeter>> {
        self.meter.clone()
    }

    pub fn signal_silent(&self) -> bool {
        self.signal_silent
    }

    pub fn now_playing(&self) -> Option<&NowPlaying> {
        self.now_playing.as_ref()
    }

    pub fn artwork(&self) -> Option<Arc<Image>> {
        self.artwork.as_ref().map(|(_, image)| image.clone())
    }

    pub fn can_record(&self, cx: &App) -> bool {
        Services::global(cx).now_playing.is_ok()
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        match self.state {
            SessionState::Idle => self.start(cx),
            SessionState::Recording { .. } => self.stop(cx),
            SessionState::Starting | SessionState::Stopping => {}
        }
    }

    pub fn start(&mut self, cx: &mut Context<Self>) {
        if self.is_active() || !self.can_record(cx) {
            return;
        }
        let services = Services::global(cx);
        let settings = SettingsStore::get(cx).clone();
        let Ok(monitor) = services.now_playing.clone() else {
            return;
        };
        let engine_services = EngineServices {
            capture: services.platform.capture_backend(),
            now_playing: monitor,
            encoders: services.encoders.clone(),
            storage: services.storage(&settings),
            library: Arc::clone(&services.library),
            spool_dir: services.dirs.spool_dir(),
        };
        let config = EngineConfig {
            capture_source: settings.recording.capture_source.clone(),
            follow_player: settings.recording.follow_player.clone(),
            auto_stop: (settings.recording.auto_stop_minutes > 0)
                .then(|| Duration::from_secs(settings.recording.auto_stop_minutes as u64 * 60)),
            output: output_config(&settings),
        };

        self.state = SessionState::Starting;
        self.tracks.clear();
        self.signal_silent = false;
        cx.notify();

        // Opening an audio device can take a moment; keep the UI responsive.
        let starting =
            cx.background_spawn(async move { RecordingEngine::start(config, engine_services) });
        self._engine_task = Some(cx.spawn(
            async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                let result = starting.await;
                let events = match this.update(cx, |session, cx| session.on_started(result, cx)) {
                    Ok(Some(events)) => events,
                    _ => return,
                };
                while let Ok(event) = events.recv().await {
                    let stopped = matches!(event, EngineEvent::Stopped(_));
                    if this
                        .update(cx, |session, cx| session.apply_engine_event(event, cx))
                        .is_err()
                        || stopped
                    {
                        break;
                    }
                }
            },
        ));
    }

    fn on_started(
        &mut self,
        result: Result<EngineHandle, gym_core::engine::EngineError>,
        cx: &mut Context<Self>,
    ) -> Option<async_channel::Receiver<EngineEvent>> {
        match result {
            Ok(engine) => {
                let events = bridge(engine.events());
                self.meter = Some(engine.meter());
                self.engine = Some(engine);
                cx.notify();
                Some(events)
            }
            Err(err) => {
                tracing::error!(%err, "cannot start recording");
                self.state = SessionState::Idle;
                cx.emit(SessionEvent::StartFailed(err.to_string().into()));
                cx.notify();
                None
            }
        }
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.stop();
            self.state = SessionState::Stopping;
            cx.notify();
        }
    }

    fn track_mut(&mut self, id: SegmentId) -> Option<&mut SessionTrack> {
        self.tracks.iter_mut().find(|t| t.id == id)
    }

    fn track_title(&self, id: SegmentId) -> SharedString {
        self.tracks
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.title.clone())
            .unwrap_or_default()
    }

    fn apply_engine_event(&mut self, event: EngineEvent, cx: &mut Context<Self>) {
        match event {
            EngineEvent::Started { source, .. } => {
                self.state = SessionState::Recording {
                    since: Instant::now(),
                    source,
                };
            }
            EngineEvent::SegmentStarted {
                id,
                track,
                partial_start,
            } => {
                self.tracks.insert(
                    0,
                    SessionTrack {
                        id,
                        title: track.track.title.clone().into(),
                        artist: track.track.artist.clone().map(Into::into),
                        state: TrackState::Recording,
                        partial: partial_start.then_some(PartialReason::StartedMidTrack),
                        duration: track.track.duration,
                        path: None,
                        error: None,
                    },
                );
            }
            EngineEvent::SegmentUpdated { id, track } => {
                if let Some(row) = self.track_mut(id) {
                    row.title = track.track.title.clone().into();
                    row.artist = track.track.artist.clone().map(Into::into);
                    row.duration = track.track.duration;
                }
            }
            EngineEvent::SegmentPaused { id, paused } => {
                if let Some(row) = self.track_mut(id) {
                    row.state = if paused {
                        TrackState::Paused
                    } else {
                        TrackState::Recording
                    };
                }
            }
            EngineEvent::SegmentEnded { id, partial } => {
                if let Some(row) = self.track_mut(id) {
                    row.state = TrackState::Encoding(0.0);
                    row.partial = partial;
                }
            }
            EngineEvent::Finalizing { id, progress } => {
                if let Some(row) = self.track_mut(id) {
                    row.state = TrackState::Encoding(progress);
                }
            }
            EngineEvent::Saved { id, entry } => {
                let path = Services::global(cx)
                    .storage(SettingsStore::get(cx))
                    .local_path(&entry.key);
                if let Some(row) = self.track_mut(id) {
                    row.state = TrackState::Saved;
                    row.duration = Some(Duration::from_millis(entry.duration_ms));
                    row.path = path;
                }
                cx.emit(SessionEvent::Saved);
            }
            EngineEvent::Skipped { id, reason } => {
                let title = self.track_title(id);
                if let Some(row) = self.track_mut(id) {
                    row.state = TrackState::Skipped(reason);
                }
                cx.emit(SessionEvent::Skipped { title, reason });
            }
            EngineEvent::Failed { id, error } => {
                let title = self.track_title(id);
                let error: SharedString = error.into();
                if let Some(row) = self.track_mut(id) {
                    row.state = TrackState::Failed;
                    row.error = Some(error.clone());
                }
                cx.emit(SessionEvent::Failed { title, error });
            }
            EngineEvent::SignalSilent(silent) => self.signal_silent = silent,
            EngineEvent::Warning(message) => tracing::warn!(%message, "engine warning"),
            EngineEvent::Stopping(_) => self.state = SessionState::Stopping,
            EngineEvent::Stopped(reason) => {
                self.state = SessionState::Idle;
                self.engine = None;
                self.meter = None;
                self.signal_silent = false;
                cx.emit(SessionEvent::Stopped(reason));
            }
        }
        cx.notify();
    }

    fn apply_now_playing(&mut self, event: NowPlayingEvent) {
        match event {
            NowPlayingEvent::Updated(np) => {
                self.artwork = match (&np.track.artwork, self.artwork.take()) {
                    (Some(art), Some(cached)) if Arc::ptr_eq(&art.data, &cached.0.data) => {
                        Some(cached)
                    }
                    (Some(art), _) => Some((art.clone(), Arc::new(decode_artwork(art)))),
                    (None, _) => None,
                };
                self.now_playing = Some(np);
            }
            NowPlayingEvent::Cleared => {
                self.now_playing = None;
                self.artwork = None;
            }
            NowPlayingEvent::SourceError(message) => {
                tracing::warn!(%message, "now playing source error");
            }
        }
    }
}

fn decode_artwork(artwork: &Artwork) -> Image {
    let format = match artwork.mime_type.as_str() {
        "image/png" => ImageFormat::Png,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::Webp,
        "image/bmp" => ImageFormat::Bmp,
        "image/tiff" => ImageFormat::Tiff,
        _ => ImageFormat::Jpeg,
    };
    Image::from_bytes(format, artwork.data.to_vec())
}

/// Output configuration derived from the current settings.
pub fn output_config(settings: &AppSettings) -> OutputConfig {
    let naming = NamingTemplate::parse(&settings.output.naming_template).unwrap_or_else(|err| {
        tracing::warn!(%err, "invalid naming template; using the default");
        NamingTemplate::parse(gym_core::naming::DEFAULT_TEMPLATE)
            .expect("default template is valid")
    });
    OutputConfig {
        encode: settings.output.encode,
        sample_rate: settings.output.sample_rate,
        naming,
        fallbacks: NamingFallbacks {
            unknown_artist: tr!("naming.unknown_artist").to_string(),
            unknown_album: tr!("naming.unknown_album").to_string(),
            untitled: tr!("naming.untitled").to_string(),
        },
        conflict: settings.output.conflict_policy,
        incomplete: settings.recording.incomplete_tracks,
        trim_silence: settings.recording.trim_silence,
    }
}
