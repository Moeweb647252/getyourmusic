//! Deterministic fakes for exercising the engine without hardware or a media player.

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crossbeam_channel::Sender;

use crate::capture::{
    AudioCaptureBackend, CaptureClock, CaptureError, CaptureEvent, CaptureHandle, CaptureSource,
    CaptureSourceKind, CaptureStream, PcmSpec,
};
use crate::now_playing::{NowPlayingError, NowPlayingEvent, NowPlayingSource, Subscription};

/// A now-playing source driven by the test.
#[derive(Clone, Default)]
pub struct FakeNowPlaying {
    sinks: Arc<Mutex<Vec<Sender<NowPlayingEvent>>>>,
}

impl FakeNowPlaying {
    pub fn emit(&self, event: NowPlayingEvent) {
        for sink in self.sinks.lock().unwrap().iter() {
            let _ = sink.send(event.clone());
        }
    }
}

impl NowPlayingSource for FakeNowPlaying {
    fn start(&self, sink: Sender<NowPlayingEvent>) -> Result<Subscription, NowPlayingError> {
        self.sinks.lock().unwrap().push(sink);
        Ok(Subscription::new(()))
    }
}

struct FakeStream;
impl CaptureStream for FakeStream {}

/// Pushes audio into a running engine as if it came from a device.
pub struct FakeFeeder {
    producer: rtrb::Producer<f32>,
    clock: Arc<CaptureClock>,
    spec: PcmSpec,
    frames: u64,
    pub events: Sender<CaptureEvent>,
}

impl FakeFeeder {
    /// Pushes interleaved samples whose first frame was captured at `at`.
    ///
    /// Blocks (spinning) while the queue is full, so tests never drop audio.
    pub fn feed(&mut self, samples: &[f32], at: SystemTime) {
        self.clock.anchor(self.frames, at);
        let mut rest = samples;
        while !rest.is_empty() {
            let n = self.producer.slots().min(rest.len());
            let n = n - n % self.spec.channels as usize;
            if n == 0 {
                std::thread::yield_now();
                continue;
            }
            let mut chunk = self.producer.write_chunk_uninit(n).unwrap();
            let (first, second) = chunk.as_mut_slices();
            let split = first.len();
            for (slot, &s) in first.iter_mut().zip(&rest[..split]) {
                slot.write(s);
            }
            for (slot, &s) in second.iter_mut().zip(&rest[split..n]) {
                slot.write(s);
            }
            // SAFETY: every slot of the chunk was initialized above.
            unsafe { chunk.commit_all() };
            rest = &rest[n..];
        }
        self.frames += (samples.len() / self.spec.channels as usize) as u64;
    }
}

/// A capture backend with one source; `start` hands the feeder to the test.
pub struct FakeCapture {
    spec: PcmSpec,
    feeder: Mutex<Option<Sender<FakeFeeder>>>,
}

impl FakeCapture {
    /// Returns the backend and a receiver that yields the feeder once capture starts.
    pub fn new(spec: PcmSpec) -> (Self, crossbeam_channel::Receiver<FakeFeeder>) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        (
            Self {
                spec,
                feeder: Mutex::new(Some(tx)),
            },
            rx,
        )
    }

    fn source(&self) -> CaptureSource {
        CaptureSource {
            id: "fake".into(),
            name: "Fake".into(),
            kind: CaptureSourceKind::OutputLoopback,
            is_default: true,
            channels: self.spec.channels,
            sample_rate: self.spec.sample_rate,
        }
    }
}

impl AudioCaptureBackend for FakeCapture {
    fn list_sources(&self) -> Result<Vec<CaptureSource>, CaptureError> {
        Ok(vec![self.source()])
    }

    fn start(&self, _source_id: Option<&str>) -> Result<CaptureHandle, CaptureError> {
        let capacity = self.spec.sample_rate as usize * self.spec.channels as usize * 4;
        let (producer, consumer) = rtrb::RingBuffer::new(capacity);
        let clock = Arc::new(CaptureClock::default());
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let feeder = FakeFeeder {
            producer,
            clock: Arc::clone(&clock),
            spec: self.spec,
            frames: 0,
            events: events_tx,
        };
        if let Some(tx) = self.feeder.lock().unwrap().take() {
            let _ = tx.send(feeder);
        }
        Ok(CaptureHandle {
            source: self.source(),
            spec: self.spec,
            samples: consumer,
            clock,
            events: events_rx,
            stream: Box::new(FakeStream),
        })
    }
}
