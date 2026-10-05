//! Turns a continuous capture stream plus now-playing updates into per-track segments.
//!
//! Audio is held in a delay line before it is committed to a segment. Now-playing updates
//! become frame-accurate markers (start, pause, resume, end) computed from the player's own
//! timing information, so a track change that is reported late still splits at the right
//! sample. Track starts are additionally snapped to the quietest point nearby.

use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

use crate::capture::PcmSpec;
use crate::model::{NowPlaying, TrackId};

use super::timeline::FrameClock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SegmentId(pub u64);

#[derive(Clone, Debug)]
pub(crate) struct SegmenterConfig {
    /// Only follow this player; `None` follows whichever player is active.
    pub follow_player: Option<String>,
    /// How long audio stays in the delay line before it is committed.
    pub delay: Duration,
    /// Search radius when snapping a track start to silence.
    pub refine_window: Duration,
    /// A track whose first `tolerance` is missing still counts as complete.
    pub partial_start_tolerance: Duration,
    /// Position jumps larger than this are treated as seeks.
    pub seek_tolerance: Duration,
    /// RMS below which a window counts as silence when snapping starts.
    pub silence_threshold: f32,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        Self {
            follow_player: None,
            delay: Duration::from_secs(3),
            refine_window: Duration::from_millis(300),
            partial_start_tolerance: Duration::from_millis(1500),
            seek_tolerance: Duration::from_secs(2),
            silence_threshold: 0.003_16, // -50 dBFS
        }
    }
}

/// Why a segment stopped receiving audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SegmentEndCause {
    /// Another track started.
    NextTrack,
    /// Playback stopped, switched to an unrecordable item, or nothing is playing.
    PlaybackEnded,
    /// The recording session was stopped.
    SessionStopped,
}

/// Receives the segmenter's decisions.
pub(crate) trait SegmentSink {
    fn begin(&mut self, id: SegmentId, track: &NowPlaying, partial_start: bool);
    fn write(&mut self, id: SegmentId, samples: &[f32]);
    fn set_paused(&mut self, id: SegmentId, paused: bool);
    /// Audio of the segment is not contiguous (seek or interruption).
    fn interrupt(&mut self, id: SegmentId);
    fn end(&mut self, id: SegmentId, cause: SegmentEndCause);
}

#[derive(Debug)]
enum MarkerKind {
    Start {
        track: Box<NowPlaying>,
        partial_start: bool,
        refined: bool,
    },
    Pause {
        interrupted: bool,
    },
    Resume,
    Interrupt,
    End,
}

#[derive(Debug)]
struct Marker {
    frame: u64,
    kind: MarkerKind,
}

struct ActiveSegment {
    id: SegmentId,
    routing: bool,
}

/// Interpretation of the followed player's state, ahead of the committed audio.
#[derive(Default)]
struct Tracker {
    /// Last snapshot from the followed player.
    last: Option<NowPlaying>,
    /// Track that owns the most recently scheduled segment.
    scheduled: Option<TrackId>,
    /// Another player currently owns "now playing".
    foreign: bool,
}

pub(crate) struct Segmenter {
    config: SegmenterConfig,
    channels: usize,
    sample_rate: f64,
    delay_frames: u64,
    buffer: VecDeque<f32>,
    /// Frame index of the first sample in `buffer`.
    committed: u64,
    /// Frame index one past the last sample in `buffer`.
    head: u64,
    markers: Vec<Marker>,
    tracker: Tracker,
    active: Option<ActiveSegment>,
    next_id: u64,
}

impl Segmenter {
    pub(crate) fn new(spec: PcmSpec, config: SegmenterConfig) -> Self {
        let delay_frames = spec.duration_to_frames(config.delay);
        Self {
            channels: spec.channels as usize,
            sample_rate: spec.sample_rate as f64,
            delay_frames,
            buffer: VecDeque::with_capacity(
                (delay_frames as usize + 1) * spec.channels as usize * 2,
            ),
            committed: 0,
            head: 0,
            markers: Vec::new(),
            tracker: Tracker::default(),
            active: None,
            next_id: 1,
            config,
        }
    }

    #[cfg(test)]
    pub(crate) fn head(&self) -> u64 {
        self.head
    }

    /// The segment that currently receives committed audio, if any.
    pub(crate) fn active_segment(&self) -> Option<SegmentId> {
        self.active.as_ref().map(|a| a.id)
    }

    /// The followed player is playing a recordable item right now (ahead of the delay line).
    pub(crate) fn is_live(&self) -> bool {
        !self.tracker.foreign
            && self
                .tracker
                .last
                .as_ref()
                .is_some_and(|np| np.playing && !np.is_advertisement)
    }

    pub(crate) fn push(&mut self, samples: &[f32]) {
        debug_assert_eq!(samples.len() % self.channels, 0);
        self.buffer.extend(samples.iter().copied());
        self.head += (samples.len() / self.channels) as u64;
    }

    fn frames(&self, duration: Duration) -> u64 {
        (duration.as_secs_f64() * self.sample_rate).round() as u64
    }

    /// Clamps a mapped frame into the part of the timeline that can still be changed.
    fn clamp(&self, raw: Option<i64>) -> u64 {
        match raw {
            Some(frame) => (frame.max(self.committed as i64) as u64).min(self.head),
            None => self.head,
        }
    }

    fn schedule(&mut self, frame: u64, kind: MarkerKind) {
        // Keep markers ordered by frame, preserving arrival order for equal frames.
        let index = self.markers.partition_point(|m| m.frame <= frame);
        self.markers.insert(index, Marker { frame, kind });
    }

    fn schedule_start(&mut self, np: &NowPlaying, clock: &dyn FrameClock) {
        let raw = clock.frame_at(np.started_at());
        let frame = self.clamp(raw);
        let lost = raw.map_or(0, |raw| (frame as i64 - raw).max(0) as u64);
        let partial_start = lost > self.frames(self.config.partial_start_tolerance);
        self.schedule(
            frame,
            MarkerKind::Start {
                track: Box::new(np.clone()),
                partial_start,
                refined: false,
            },
        );
        self.tracker.scheduled = Some(np.track_id.clone());
    }

    fn is_followed(&self, np: &NowPlaying) -> bool {
        self.config
            .follow_player
            .as_deref()
            .is_none_or(|id| id == np.player.id)
    }

    /// Interprets a now-playing update.
    pub(crate) fn on_update(&mut self, np: &NowPlaying, clock: &dyn FrameClock) {
        if !self.is_followed(np) {
            // Another app took over "now playing"; its audio must not end up in our track.
            if !self.tracker.foreign && self.tracker.scheduled.is_some() {
                let frame = self.clamp(clock.frame_at(np.received_at));
                self.schedule(frame, MarkerKind::Pause { interrupted: true });
            }
            self.tracker.foreign = true;
            if let Some(last) = &mut self.tracker.last {
                last.playing = false;
            }
            return;
        }
        let returning_from_foreign = std::mem::take(&mut self.tracker.foreign);

        let same_track = self.tracker.scheduled.as_ref() == Some(&np.track_id);
        let event_frame = self.clamp(clock.frame_at(np.reliable_measured_at()));

        if np.is_advertisement {
            if self.tracker.scheduled.take().is_some() {
                let frame = self.clamp(clock.frame_at(np.started_at()));
                self.schedule(frame, MarkerKind::End);
            }
        } else if !same_track {
            if np.playing {
                self.schedule_start(np, clock);
            } else if self.tracker.scheduled.take().is_some() {
                // The next item was loaded but is paused: the previous track is over.
                self.schedule(event_frame, MarkerKind::End);
            }
        } else if let Some(last) = self.tracker.last.clone() {
            match (last.playing, np.playing) {
                (false, true) => {
                    let jumped = np.elapsed.abs_diff(last.elapsed) > self.config.seek_tolerance;
                    if jumped || returning_from_foreign {
                        self.schedule(event_frame, MarkerKind::Interrupt);
                    }
                    self.schedule(event_frame, MarkerKind::Resume);
                }
                (true, false) => {
                    self.schedule(event_frame, MarkerKind::Pause { interrupted: false });
                }
                (true, true) => {
                    let expected = last.position_at(np.measured_at);
                    let restarted =
                        np.elapsed < Duration::from_secs(3) && expected > Duration::from_secs(10);
                    if restarted {
                        self.schedule_start(np, clock);
                    } else if np.elapsed.abs_diff(expected) > self.config.seek_tolerance {
                        self.schedule(event_frame, MarkerKind::Interrupt);
                    }
                }
                (false, false) => {}
            }
        } else if np.playing {
            self.schedule_start(np, clock);
        }
        self.tracker.last = Some(np.clone());
    }

    /// Nothing is playing anymore.
    pub(crate) fn on_cleared(&mut self, at: SystemTime, clock: &dyn FrameClock) {
        if self.tracker.scheduled.take().is_some() {
            let frame = self.clamp(clock.frame_at(at));
            self.schedule(frame, MarkerKind::End);
        }
        self.tracker.last = None;
        self.tracker.foreign = false;
    }

    /// Commits audio that has left the delay line.
    pub(crate) fn advance(&mut self, sink: &mut dyn SegmentSink) {
        let limit = self.head.saturating_sub(self.delay_frames);
        self.commit_until(limit, sink);
    }

    /// Commits everything and ends the active segment.
    pub(crate) fn flush(&mut self, sink: &mut dyn SegmentSink) {
        let head = self.head;
        self.commit_until(head, sink);
        self.markers.clear();
        if let Some(active) = self.active.take() {
            sink.end(active.id, SegmentEndCause::SessionStopped);
        }
        self.tracker = Tracker::default();
    }

    fn commit_until(&mut self, limit: u64, sink: &mut dyn SegmentSink) {
        let window = self.frames(self.config.refine_window);
        loop {
            self.refine_reachable_starts(limit, window);
            let next = self.markers.first().map(|m| m.frame);
            let target = next
                .map_or(limit, |frame| frame.min(limit))
                .max(self.committed);
            self.commit_frames(target, sink);
            match next {
                Some(frame) if frame <= self.committed => {
                    let marker = self.markers.remove(0);
                    self.apply(marker.kind, sink);
                }
                _ => break,
            }
        }
    }

    fn commit_frames(&mut self, target: u64, sink: &mut dyn SegmentSink) {
        if target <= self.committed {
            return;
        }
        let n = (target - self.committed) as usize * self.channels;
        if let Some(active) = self.active.as_ref().filter(|a| a.routing) {
            let (front, back) = self.buffer.as_slices();
            let first = n.min(front.len());
            sink.write(active.id, &front[..first]);
            if n > first {
                sink.write(active.id, &back[..n - first]);
            }
        }
        self.buffer.drain(..n);
        self.committed = target;
    }

    fn apply(&mut self, kind: MarkerKind, sink: &mut dyn SegmentSink) {
        match kind {
            MarkerKind::Start {
                track,
                partial_start,
                ..
            } => {
                if let Some(previous) = self.active.take() {
                    sink.end(previous.id, SegmentEndCause::NextTrack);
                }
                let id = SegmentId(self.next_id);
                self.next_id += 1;
                sink.begin(id, &track, partial_start);
                self.active = Some(ActiveSegment { id, routing: true });
            }
            MarkerKind::Pause { interrupted } => {
                if let Some(active) = &mut self.active {
                    if interrupted {
                        sink.interrupt(active.id);
                    }
                    if active.routing {
                        active.routing = false;
                        sink.set_paused(active.id, true);
                    }
                }
            }
            MarkerKind::Resume => {
                if let Some(active) = self.active.as_mut().filter(|a| !a.routing) {
                    active.routing = true;
                    sink.set_paused(active.id, false);
                }
            }
            MarkerKind::Interrupt => {
                if let Some(active) = &self.active {
                    sink.interrupt(active.id);
                }
            }
            MarkerKind::End => {
                if let Some(active) = self.active.take() {
                    sink.end(active.id, SegmentEndCause::PlaybackEnded);
                }
            }
        }
    }

    /// Snaps track starts whose search window is about to be committed to nearby silence.
    fn refine_reachable_starts(&mut self, limit: u64, window: u64) {
        for index in 0..self.markers.len() {
            let frame = self.markers[index].frame;
            if frame > limit + window {
                break;
            }
            let MarkerKind::Start { refined, .. } = &self.markers[index].kind else {
                continue;
            };
            if *refined {
                continue;
            }
            let lower = index
                .checked_sub(1)
                .map_or(self.committed, |i| self.markers[i].frame)
                .max(frame.saturating_sub(window))
                .max(self.committed);
            let upper = self
                .markers
                .get(index + 1)
                .map_or(self.head, |m| m.frame)
                .min(frame + window)
                .min(self.head);
            if let Some(quiet) = self.nearest_gap(frame, lower, upper) {
                self.markers[index].frame = quiet;
            }
            if let MarkerKind::Start { refined, .. } = &mut self.markers[index].kind {
                *refined = true;
            }
        }
    }

    /// Center of the silent gap in `lower..upper` closest to `estimate`, if there is one.
    ///
    /// Silence is measured in 10 ms windows with a 5 ms hop; overlapping quiet windows form a
    /// gap. Ties in distance prefer the longer gap.
    fn nearest_gap(&self, estimate: u64, lower: u64, upper: u64) -> Option<u64> {
        let win = ((self.sample_rate * 0.010) as u64).max(1);
        let hop = (win / 2).max(1);
        let len = win as usize * self.channels;
        let mut gaps: Vec<(u64, u64)> = Vec::new();
        let mut start = lower;
        while start + win <= upper {
            let offset = (start - self.committed) as usize * self.channels;
            let energy: f32 = self.buffer.range(offset..offset + len).map(|s| s * s).sum();
            if (energy / len as f32).sqrt() < self.config.silence_threshold {
                match gaps.last_mut() {
                    Some(gap) if gap.1 >= start => gap.1 = start + win,
                    _ => gaps.push((start, start + win)),
                }
            }
            start += hop;
        }
        let distance = |&(s, e): &(u64, u64)| {
            if (s..e).contains(&estimate) {
                0
            } else {
                estimate.abs_diff(s).min(estimate.abs_diff(e))
            }
        };
        gaps.into_iter()
            .min_by(|a, b| {
                distance(a)
                    .cmp(&distance(b))
                    .then((b.1 - b.0).cmp(&(a.1 - a.0)))
            })
            .map(|(s, e)| (s + e) / 2)
    }
}

#[cfg(test)]
mod tests {
    use super::super::timeline::tests::LinearClock;
    use super::*;
    use crate::model::{PlayerInfo, TrackMetadata};

    const RATE: u32 = 1_000;

    #[derive(Debug, PartialEq)]
    enum Event {
        Begin(u64, String, bool),
        Paused(u64, bool),
        Interrupted(u64),
        End(u64, SegmentEndCause),
    }

    #[derive(Default)]
    struct Recorder {
        events: Vec<Event>,
        /// Samples written per segment.
        audio: std::collections::BTreeMap<u64, Vec<f32>>,
    }

    impl SegmentSink for Recorder {
        fn begin(&mut self, id: SegmentId, track: &NowPlaying, partial_start: bool) {
            self.events
                .push(Event::Begin(id.0, track.track.title.clone(), partial_start));
        }
        fn write(&mut self, id: SegmentId, samples: &[f32]) {
            self.audio
                .entry(id.0)
                .or_default()
                .extend_from_slice(samples);
        }
        fn set_paused(&mut self, id: SegmentId, paused: bool) {
            self.events.push(Event::Paused(id.0, paused));
        }
        fn interrupt(&mut self, id: SegmentId) {
            self.events.push(Event::Interrupted(id.0));
        }
        fn end(&mut self, id: SegmentId, cause: SegmentEndCause) {
            self.events.push(Event::End(id.0, cause));
        }
    }

    struct Harness {
        segmenter: Segmenter,
        clock: LinearClock,
        sink: Recorder,
    }

    impl Harness {
        fn new(follow: Option<&str>) -> Self {
            let spec = PcmSpec {
                sample_rate: RATE,
                channels: 1,
            };
            let config = SegmenterConfig {
                follow_player: follow.map(str::to_owned),
                ..Default::default()
            };
            Self {
                segmenter: Segmenter::new(spec, config),
                clock: LinearClock {
                    origin: SystemTime::UNIX_EPOCH + Duration::from_secs(10_000),
                    sample_rate: RATE as f64,
                },
                sink: Recorder::default(),
            }
        }

        fn time(&self, secs: f64) -> SystemTime {
            self.clock.origin + Duration::from_secs_f64(secs)
        }

        /// Feeds `secs` of audio where each sample's value is its frame index (for checks)
        /// unless `silent` is set.
        fn feed(&mut self, secs: f64, silent: bool) {
            let frames = (secs * RATE as f64) as u64;
            let start = self.segmenter.head();
            let samples: Vec<f32> = (start..start + frames)
                .map(|f| if silent { 0.0 } else { 0.1 + f as f32 * 1e-6 })
                .collect();
            for chunk in samples.chunks(37) {
                self.segmenter.push(chunk);
                self.segmenter.advance(&mut self.sink);
            }
        }

        fn update(&mut self, np: NowPlaying) {
            self.segmenter.on_update(&np, &self.clock);
        }

        fn np(
            &self,
            player: &str,
            title: &str,
            playing: bool,
            elapsed: f64,
            at: f64,
        ) -> NowPlaying {
            let at = self.time(at);
            NowPlaying {
                player: PlayerInfo::new(player, player),
                track_id: TrackId::new(title),
                track: TrackMetadata {
                    title: title.into(),
                    duration: Some(Duration::from_secs(60)),
                    ..Default::default()
                },
                playing,
                rate: 1.0,
                elapsed: Duration::from_secs_f64(elapsed),
                measured_at: at,
                received_at: at,
                is_advertisement: false,
            }
        }

        fn finish(mut self) -> Recorder {
            self.segmenter.flush(&mut self.sink);
            self.sink
        }

        /// Frame index of the first sample written to `segment`.
        fn first_frame(sink: &Recorder, segment: u64) -> u64 {
            let first = sink.audio[&segment][0];
            ((first - 0.1) / 1e-6).round() as u64
        }
    }

    #[test]
    fn late_track_changes_split_at_the_reported_start() {
        let mut h = Harness::new(None);
        h.feed(1.0, false);
        // Track A started at t=1.0 (reported immediately).
        h.update(h.np("p", "A", true, 0.0, 1.0));
        h.feed(4.0, false);
        // Track B started at t=5.0 but the update arrives 0.8 s later.
        let mut b = h.np("p", "B", true, 0.8, 5.8);
        b.received_at = h.time(5.8);
        h.feed(0.8, false);
        h.update(b);
        h.feed(5.0, false);
        let sink = h.finish();

        assert_eq!(
            sink.events,
            vec![
                Event::Begin(1, "A".into(), false),
                Event::End(1, SegmentEndCause::NextTrack),
                Event::Begin(2, "B".into(), false),
                Event::End(2, SegmentEndCause::SessionStopped),
            ]
        );
        assert_eq!(sink.audio[&1].len(), 4_000);
        assert_eq!(Harness::first_frame(&sink, 1), 1_000);
        assert_eq!(Harness::first_frame(&sink, 2), 5_000);
    }

    #[test]
    fn starts_snap_to_nearby_silence() {
        let mut h = Harness::new(None);
        h.update(h.np("p", "A", true, 0.0, 0.0));
        h.feed(2.0, false);
        h.feed(0.1, true); // gap 2.0..2.1
        h.feed(2.0, false);
        // B reported as starting at 2.2, 100 ms after the gap.
        h.update(h.np("p", "B", true, 0.0, 2.2));
        h.feed(4.0, false);
        let sink = h.finish();
        let a = sink.audio[&1].len() as i64;
        assert!((a - 2_050).abs() <= 10, "cut at {a}");
    }

    #[test]
    fn joining_mid_track_marks_the_start_partial() {
        let mut h = Harness::new(None);
        h.update(h.np("p", "A", true, 42.0, 0.0));
        h.feed(4.0, false);
        let sink = h.finish();
        assert_eq!(sink.events[0], Event::Begin(1, "A".into(), true));
    }

    #[test]
    fn pauses_drop_audio_and_resume_continues_the_segment() {
        let mut h = Harness::new(None);
        h.update(h.np("p", "A", true, 0.0, 0.0));
        h.feed(2.0, false);
        h.update(h.np("p", "A", false, 2.0, 2.0));
        h.feed(3.0, true);
        h.update(h.np("p", "A", true, 2.0, 5.0));
        h.feed(4.0, false);
        let sink = h.finish();
        assert_eq!(
            sink.events,
            vec![
                Event::Begin(1, "A".into(), false),
                Event::Paused(1, true),
                Event::Paused(1, false),
                Event::End(1, SegmentEndCause::SessionStopped),
            ]
        );
        assert_eq!(sink.audio[&1].len(), 2_000 + 4_000);
    }

    #[test]
    fn foreign_players_interrupt_the_followed_track() {
        let mut h = Harness::new(Some("music"));
        h.update(h.np("music", "A", true, 0.0, 0.0));
        h.feed(2.0, false);
        h.update(h.np("browser", "Video", true, 0.0, 2.0));
        h.feed(2.0, false);
        h.update(h.np("music", "A", true, 4.0, 4.0));
        h.feed(4.0, false);
        let sink = h.finish();
        assert_eq!(
            sink.events,
            vec![
                Event::Begin(1, "A".into(), false),
                Event::Interrupted(1),
                Event::Paused(1, true),
                Event::Interrupted(1),
                Event::Paused(1, false),
                Event::End(1, SegmentEndCause::SessionStopped),
            ]
        );
        assert_eq!(sink.audio[&1].len(), 2_000 + 4_000);
    }

    #[test]
    fn seeks_interrupt_and_restarts_start_a_new_segment() {
        let mut h = Harness::new(None);
        h.update(h.np("p", "A", true, 0.0, 0.0));
        h.feed(4.0, false);
        h.update(h.np("p", "A", true, 30.0, 4.0)); // seek forward
        h.feed(8.0, false);
        h.update(h.np("p", "A", true, 0.0, 12.0)); // repeat-one restart
        h.feed(4.0, false);
        let sink = h.finish();
        assert_eq!(
            sink.events,
            vec![
                Event::Begin(1, "A".into(), false),
                Event::Interrupted(1),
                Event::End(1, SegmentEndCause::NextTrack),
                Event::Begin(2, "A".into(), false),
                Event::End(2, SegmentEndCause::SessionStopped),
            ]
        );
    }

    #[test]
    fn clearing_ends_the_segment_and_ads_are_skipped() {
        let mut h = Harness::new(None);
        h.update(h.np("p", "A", true, 0.0, 0.0));
        h.feed(3.0, false);
        let mut ad = h.np("p", "Ad", true, 0.0, 3.0);
        ad.is_advertisement = true;
        h.update(ad);
        h.feed(3.0, false);
        h.segmenter.on_cleared(h.time(6.0), &h.clock);
        h.feed(4.0, false);
        let sink = h.finish();
        assert_eq!(
            sink.events,
            vec![
                Event::Begin(1, "A".into(), false),
                Event::End(1, SegmentEndCause::PlaybackEnded),
            ]
        );
        assert_eq!(sink.audio[&1].len(), 3_000);
    }
}
