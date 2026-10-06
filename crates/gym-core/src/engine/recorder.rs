//! The recorder thread: drains the capture queue, feeds the segmenter and spools segments.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Utc};
use crossbeam_channel::{Receiver, Sender, TryRecvError};

use crate::capture::{CaptureEvent, CaptureHandle, PcmSpec};
use crate::model::{NowPlaying, TrackId};
use crate::now_playing::NowPlayingEvent;

use super::finalize::{FinalizeJob, Job};
use super::segmenter::{SegmentEndCause, SegmentId, SegmentSink, Segmenter, SegmenterConfig};
use super::spool::{self, MemoryBudget, SpoolWriter};
use super::timeline::CaptureTimeline;
use super::{
    CacheConfig, Command, EngineConfig, EngineEvent, EngineServices, LevelMeter, OutputConfig,
    PartialReason, StopReason,
};

const TICK: Duration = Duration::from_millis(15);
/// Peak below which captured audio counts as digital silence (-80 dBFS).
const SILENCE_PEAK: f32 = 1e-4;
const SILENCE_WARNING_AFTER: Duration = Duration::from_secs(10);

pub(super) fn spawn(
    config: EngineConfig,
    services: EngineServices,
    capture: CaptureHandle,
    commands: Receiver<Command>,
    events: Sender<EngineEvent>,
    jobs: Sender<Job>,
    meter: Arc<LevelMeter>,
) -> io::Result<JoinHandle<()>> {
    let spec = capture.spec;
    let segmenter = Segmenter::new(
        spec,
        SegmenterConfig {
            follow_player: config.follow_player.clone(),
            ..Default::default()
        },
    );
    let timeline = CaptureTimeline::new(Arc::clone(&capture.clock), spec.sample_rate);
    let now_playing = services.now_playing.subscribe();
    let sink = SpoolSink {
        spool_dir: services.spool_dir.clone(),
        spec,
        session: spool::new_session(),
        cache: config.cache,
        budget: MemoryBudget::new(config.cache.memory_limit),
        output: config.output.clone(),
        segments: HashMap::new(),
        latest: HashMap::new(),
        events: events.clone(),
        jobs: jobs.clone(),
    };
    let recorder = Recorder {
        auto_stop: config.auto_stop,
        segmenter,
        timeline,
        sink,
        capture,
        now_playing,
        commands,
        events,
        jobs,
        meter,
        scratch: Vec::new(),
        pending: Vec::new(),
        silent_for: Duration::ZERO,
        silence_reported: false,
        idle_since: None,
    };
    thread::Builder::new()
        .name("recorder".into())
        .spawn(move || recorder.run())
}

struct Recorder {
    auto_stop: Option<Duration>,
    segmenter: Segmenter,
    timeline: CaptureTimeline,
    sink: SpoolSink,
    capture: CaptureHandle,
    now_playing: Receiver<NowPlayingEvent>,
    commands: Receiver<Command>,
    events: Sender<EngineEvent>,
    jobs: Sender<Job>,
    meter: Arc<LevelMeter>,
    scratch: Vec<f32>,
    /// Now-playing events received before the first audio anchored the timeline.
    pending: Vec<(NowPlayingEvent, SystemTime)>,
    silent_for: Duration,
    silence_reported: bool,
    idle_since: Option<Instant>,
}

impl Recorder {
    fn run(mut self) {
        let reason = loop {
            if let Some(reason) = self.tick() {
                break reason;
            }
            thread::sleep(TICK);
        };
        self.shutdown(reason);
    }

    /// One iteration of the loop; returns a reason when recording must stop.
    fn tick(&mut self) -> Option<StopReason> {
        loop {
            match self.commands.try_recv() {
                Ok(Command::Stop) | Err(TryRecvError::Disconnected) => {
                    return Some(StopReason::User);
                }
                Ok(Command::UpdateOutput(output)) => self.sink.output = *output,
                Err(TryRecvError::Empty) => break,
            }
        }
        while let Ok(event) = self.capture.events.try_recv() {
            match event {
                CaptureEvent::Disconnected(message) => {
                    return Some(StopReason::DeviceLost(message));
                }
                CaptureEvent::Warning(message) => {
                    let _ = self.events.send(EngineEvent::Warning(message));
                }
            }
        }
        while let Ok(event) = self.now_playing.try_recv() {
            self.pending.push((event, SystemTime::now()));
        }
        self.drain_audio();
        if self.timeline.is_ready() {
            for (event, received_at) in std::mem::take(&mut self.pending) {
                self.handle_now_playing(event, received_at);
            }
        }
        self.segmenter.advance(&mut self.sink);
        self.watch_signal();
        self.watch_idle()
    }

    fn handle_now_playing(&mut self, event: NowPlayingEvent, received_at: SystemTime) {
        match event {
            NowPlayingEvent::Updated(np) => {
                self.sink.remember(&np);
                self.segmenter.on_update(&np, &self.timeline);
            }
            NowPlayingEvent::Cleared => {
                self.segmenter.on_cleared(received_at, &self.timeline);
            }
            NowPlayingEvent::SourceError(message) => {
                let _ = self.events.send(EngineEvent::Warning(message));
            }
        }
    }

    /// Moves captured samples into the segmenter and tracks how long the input was silent.
    fn drain_audio(&mut self) {
        let channels = self.capture.spec.channels as usize;
        let available = self.capture.samples.slots();
        let available = available - available % channels;
        if available == 0 {
            return;
        }
        let Ok(chunk) = self.capture.samples.read_chunk(available) else {
            return;
        };
        let (first, second) = chunk.as_slices();
        self.scratch.clear();
        self.scratch.extend_from_slice(first);
        self.scratch.extend_from_slice(second);
        chunk.commit_all();

        self.meter.publish(&self.scratch, channels);
        self.segmenter.push(&self.scratch);
        let peak = self.scratch.iter().fold(0f32, |m, s| m.max(s.abs()));
        let duration = self
            .capture
            .spec
            .frames_to_duration((self.scratch.len() / channels) as u64);
        if peak < SILENCE_PEAK {
            self.silent_for += duration;
        } else {
            self.silent_for = Duration::ZERO;
        }
    }

    /// Warns when the followed player plays but nothing reaches the capture device.
    fn watch_signal(&mut self) {
        let silent = self.segmenter.is_live() && self.silent_for >= SILENCE_WARNING_AFTER;
        if silent != self.silence_reported {
            self.silence_reported = silent;
            let _ = self.events.send(EngineEvent::SignalSilent(silent));
        }
    }

    fn watch_idle(&mut self) -> Option<StopReason> {
        let auto_stop = self.auto_stop?;
        if self.segmenter.is_live() || self.segmenter.active_segment().is_some() {
            self.idle_since = None;
            return None;
        }
        let since = *self.idle_since.get_or_insert_with(Instant::now);
        (since.elapsed() >= auto_stop).then_some(StopReason::Idle)
    }

    fn shutdown(mut self, reason: StopReason) {
        let _ = self.events.send(EngineEvent::Stopping(reason.clone()));
        self.drain_audio();
        self.segmenter.flush(&mut self.sink);
        self.meter.reset();
        // Dropping the capture handle stops the platform stream.
        drop(self.capture);
        let _ = self.jobs.send(Job::Shutdown(reason));
    }
}

struct SpoolSegment {
    track_id: TrackId,
    /// Snapshot the segment started with; superseded by newer metadata for the same track.
    initial: NowPlaying,
    /// `None` once the segment failed; the failure has been reported.
    writer: Option<SpoolWriter>,
    frames: u64,
    peak: f32,
    partial_start: bool,
    interrupted: bool,
    recorded_at: DateTime<Utc>,
}

/// Buffers segments in the cache and hands finished ones to the finalize worker.
struct SpoolSink {
    spool_dir: PathBuf,
    spec: PcmSpec,
    session: String,
    cache: CacheConfig,
    budget: Arc<MemoryBudget>,
    output: OutputConfig,
    segments: HashMap<SegmentId, SpoolSegment>,
    /// Most recent metadata per track; players often publish artwork after the title.
    latest: HashMap<TrackId, NowPlaying>,
    events: Sender<EngineEvent>,
    jobs: Sender<Job>,
}

impl SpoolSink {
    fn remember(&mut self, np: &NowPlaying) {
        let changed = self
            .latest
            .get(&np.track_id)
            .is_some_and(|previous| previous.track != np.track);
        self.latest.insert(np.track_id.clone(), np.clone());
        if changed {
            for (id, segment) in &self.segments {
                if segment.track_id == np.track_id {
                    let _ = self.events.send(EngineEvent::SegmentUpdated {
                        id: *id,
                        track: np.clone(),
                    });
                }
            }
        }
        if self.latest.len() > 32 {
            let keep: Vec<TrackId> = self.segments.values().map(|s| s.track_id.clone()).collect();
            self.latest
                .retain(|id, _| keep.contains(id) || *id == np.track_id);
        }
    }

    fn fail(&self, id: SegmentId, error: impl ToString) {
        let _ = self.events.send(EngineEvent::Failed {
            id,
            error: error.to_string(),
        });
    }

    fn partial_reason(
        &self,
        segment: &SpoolSegment,
        track: &NowPlaying,
        cause: SegmentEndCause,
    ) -> Option<PartialReason> {
        if segment.partial_start {
            return Some(PartialReason::StartedMidTrack);
        }
        if segment.interrupted {
            return Some(PartialReason::Interrupted);
        }
        let early = if cause == SegmentEndCause::SessionStopped {
            PartialReason::StoppedEarly
        } else {
            PartialReason::EndedEarly
        };
        match track.track.duration.filter(|d| !d.is_zero()) {
            Some(duration) => {
                let captured = self.spec.frames_to_duration(segment.frames);
                let slack = (duration / 10).max(Duration::from_secs(3));
                (captured + slack < duration).then_some(early)
            }
            None => (cause == SegmentEndCause::SessionStopped).then_some(early),
        }
    }
}

impl SegmentSink for SpoolSink {
    fn begin(&mut self, id: SegmentId, track: &NowPlaying, partial_start: bool) {
        let path = self
            .spool_dir
            .join(spool::spool_file_name(&self.session, id));
        let writer = match SpoolWriter::create(path, self.spec, &self.cache, &self.budget) {
            Ok(writer) => Some(writer),
            Err(err) => {
                self.fail(id, err);
                None
            }
        };
        let latest = self.latest.get(&track.track_id).unwrap_or(track).clone();
        self.segments.insert(
            id,
            SpoolSegment {
                track_id: track.track_id.clone(),
                initial: latest.clone(),
                writer,
                frames: 0,
                peak: 0.0,
                partial_start,
                interrupted: false,
                recorded_at: Utc::now(),
            },
        );
        tracing::info!(segment = id.0, track = %latest.track.display_name(), partial_start, "segment started");
        let _ = self.events.send(EngineEvent::SegmentStarted {
            id,
            track: latest,
            partial_start,
        });
    }

    fn write(&mut self, id: SegmentId, samples: &[f32]) {
        let channels = self.spec.channels as u64;
        let Some(segment) = self.segments.get_mut(&id) else {
            return;
        };
        let Some(writer) = segment.writer.as_mut() else {
            return;
        };
        match writer.write(samples) {
            Ok(()) => {
                segment.frames += samples.len() as u64 / channels;
                segment.peak = samples.iter().fold(segment.peak, |m, s| m.max(s.abs()));
            }
            Err(err) => {
                if let Some(writer) = segment.writer.take() {
                    writer.discard();
                }
                tracing::warn!(segment = id.0, %err, "segment failed");
                self.fail(id, err);
            }
        }
    }

    fn set_paused(&mut self, id: SegmentId, paused: bool) {
        let _ = self.events.send(EngineEvent::SegmentPaused { id, paused });
    }

    fn interrupt(&mut self, id: SegmentId) {
        if let Some(segment) = self.segments.get_mut(&id) {
            segment.interrupted = true;
        }
    }

    fn end(&mut self, id: SegmentId, cause: SegmentEndCause) {
        let Some(mut segment) = self.segments.remove(&id) else {
            return;
        };
        let Some(writer) = segment.writer.take() else {
            // Already reported as failed.
            return;
        };
        let audio = match writer.finish() {
            Ok(audio) => audio,
            Err(err) => {
                self.fail(id, err);
                return;
            }
        };
        let track = self
            .latest
            .get(&segment.track_id)
            .cloned()
            .unwrap_or_else(|| segment.initial.clone());
        let partial = self.partial_reason(&segment, &track, cause);
        tracing::info!(
            segment = id.0,
            track = %track.track.display_name(),
            captured = ?self.spec.frames_to_duration(segment.frames),
            reported = ?track.track.duration,
            ?cause,
            ?partial,
            "segment ended"
        );
        let _ = self.events.send(EngineEvent::SegmentEnded { id, partial });
        let _ = self.jobs.send(Job::Finalize(Box::new(FinalizeJob {
            id,
            audio,
            frames: segment.frames,
            peak: segment.peak,
            track,
            partial,
            recorded_at: segment.recorded_at,
            output: self.output.clone(),
        })));
    }
}
