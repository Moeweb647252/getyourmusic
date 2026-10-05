//! The recording engine: capture → per-track segments → encode → tag → store.
//!
//! [`RecordingEngine::start`] spawns a recorder thread that owns the capture stream and a
//! finalize worker that turns finished segments into library entries. Progress is reported
//! through [`EngineEvent`]s.

mod finalize;
mod meter;
mod recorder;
mod segmenter;
mod timeline;

use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::capture::{AudioCaptureBackend, CaptureError, CaptureSource, PcmSpec};
use crate::encode::{EncodeSettings, EncoderRegistry};
use crate::library::{Library, RecordingEntry};
use crate::model::NowPlaying;
use crate::naming::{NamingFallbacks, NamingTemplate};
use crate::now_playing::NowPlayingMonitor;
use crate::settings::{IncompletePolicy, SampleRatePolicy};
use crate::storage::{ConflictPolicy, StorageProvider};

pub use meter::{LevelMeter, Levels, to_dbfs};
pub use segmenter::SegmentId;

/// Settings that shape how finished segments are turned into files.
///
/// Captured with each segment when it ends, so changes apply to subsequent tracks.
#[derive(Clone, Debug)]
pub struct OutputConfig {
    pub encode: EncodeSettings,
    pub sample_rate: SampleRatePolicy,
    pub naming: NamingTemplate,
    pub fallbacks: NamingFallbacks,
    pub conflict: ConflictPolicy,
    pub incomplete: IncompletePolicy,
    pub trim_silence: bool,
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Capture source id; `None` uses the default source.
    pub capture_source: Option<String>,
    /// Only record this player; `None` follows whichever player is active.
    pub follow_player: Option<String>,
    /// Stop automatically after this long without anything to record.
    pub auto_stop: Option<Duration>,
    pub output: OutputConfig,
}

/// Collaborators the engine needs; all are shared with the rest of the application.
#[derive(Clone)]
pub struct EngineServices {
    pub capture: Arc<dyn AudioCaptureBackend>,
    pub now_playing: NowPlayingMonitor,
    pub encoders: EncoderRegistry,
    pub storage: Arc<dyn StorageProvider>,
    pub library: Arc<Library>,
    pub spool_dir: PathBuf,
}

/// Why a track was not fully captured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartialReason {
    /// Recording began after the track had started.
    StartedMidTrack,
    /// The track was seeked, or other audio interrupted it.
    Interrupted,
    /// The player moved on before the track finished.
    EndedEarly,
    /// The session was stopped before the track finished.
    StoppedEarly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    Incomplete(PartialReason),
    TooShort,
    /// Nothing audible was captured (usually a routing or permission problem).
    Silent,
    AlreadyExists,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    User,
    /// Nothing was played for the configured auto-stop period.
    Idle,
    DeviceLost(String),
}

#[derive(Clone, Debug)]
pub enum EngineEvent {
    Started {
        source: CaptureSource,
        spec: PcmSpec,
    },
    SegmentStarted {
        id: SegmentId,
        track: NowPlaying,
        partial_start: bool,
    },
    /// Richer metadata (e.g. artwork) arrived for a segment.
    SegmentUpdated {
        id: SegmentId,
        track: NowPlaying,
    },
    SegmentPaused {
        id: SegmentId,
        paused: bool,
    },
    SegmentEnded {
        id: SegmentId,
        partial: Option<PartialReason>,
    },
    Finalizing {
        id: SegmentId,
        progress: f32,
    },
    Saved {
        id: SegmentId,
        entry: RecordingEntry,
    },
    Skipped {
        id: SegmentId,
        reason: SkipReason,
    },
    Failed {
        id: SegmentId,
        error: String,
    },
    /// The followed player is playing but the captured signal is (or no longer is) silent.
    SignalSilent(bool),
    Warning(String),
    /// Capture stopped; queued tracks are still being finalized.
    Stopping(StopReason),
    /// Everything is finished; the engine is gone.
    Stopped(StopReason),
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Capture(#[from] CaptureError),
    #[error("no encoder is available for the selected format")]
    EncoderUnavailable,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub(crate) enum Command {
    Stop,
    UpdateOutput(Box<OutputConfig>),
}

/// Controls a running engine. Dropping the handle stops recording.
pub struct EngineHandle {
    commands: Sender<Command>,
    events: Receiver<EngineEvent>,
    meter: Arc<LevelMeter>,
    recorder: Option<JoinHandle<()>>,
}

impl EngineHandle {
    pub fn events(&self) -> Receiver<EngineEvent> {
        self.events.clone()
    }

    pub fn meter(&self) -> Arc<LevelMeter> {
        Arc::clone(&self.meter)
    }

    /// Requests a stop; [`EngineEvent::Stopped`] follows once queued tracks are finished.
    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }

    /// Applies new output settings to tracks that end from now on.
    pub fn update_output(&self, output: OutputConfig) {
        let _ = self.commands.send(Command::UpdateOutput(Box::new(output)));
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.stop();
        // The recorder exits promptly after a stop; finalization continues on its own thread.
        if let Some(recorder) = self.recorder.take() {
            let _ = recorder.join();
        }
    }
}

pub struct RecordingEngine;

impl RecordingEngine {
    pub fn start(
        config: EngineConfig,
        services: EngineServices,
    ) -> Result<EngineHandle, EngineError> {
        if !services.encoders.is_available(config.output.encode.format) {
            return Err(EngineError::EncoderUnavailable);
        }
        std::fs::create_dir_all(&services.spool_dir)?;

        let capture = services.capture.start(config.capture_source.as_deref())?;
        let (commands_tx, commands_rx) = unbounded();
        let (events_tx, events_rx) = unbounded();
        let meter = Arc::new(LevelMeter::default());

        let _ = events_tx.send(EngineEvent::Started {
            source: capture.source.clone(),
            spec: capture.spec,
        });

        let jobs = finalize::spawn_worker(services.clone(), events_tx.clone())?;
        let recorder = recorder::spawn(
            config,
            services,
            capture,
            commands_rx,
            events_tx,
            jobs,
            Arc::clone(&meter),
        )?;

        Ok(EngineHandle {
            commands: commands_tx,
            events: events_rx,
            meter,
            recorder: Some(recorder),
        })
    }
}

/// Removes leftovers from sessions that did not shut down cleanly.
pub fn clean_spool(spool_dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(spool_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if let Err(err) = std::fs::remove_file(entry.path()) {
            tracing::warn!(%err, path = %entry.path().display(), "cannot remove spool file");
        }
    }
}
