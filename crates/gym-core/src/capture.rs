//! Abstraction over audio capture devices.
//!
//! Backends deliver interleaved `f32` samples through a lock-free ring buffer and publish a
//! [`CaptureClock`] that maps frame indices to wall-clock time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CaptureSourceKind {
    /// Records what an output device plays (system loopback).
    OutputLoopback,
    /// Records an input device, e.g. a virtual loopback driver such as BlackHole.
    Input,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureSource {
    /// Stable identifier, persisted in settings.
    pub id: String,
    pub name: String,
    pub kind: CaptureSourceKind,
    pub is_default: bool,
    pub channels: u16,
    pub sample_rate: u32,
}

/// Format of the PCM stream. Samples are always interleaved `f32` in `-1.0..=1.0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PcmSpec {
    pub sample_rate: u32,
    pub channels: u16,
}

impl PcmSpec {
    pub fn frames_to_duration(&self, frames: u64) -> Duration {
        Duration::from_secs_f64(frames as f64 / self.sample_rate as f64)
    }

    pub fn duration_to_frames(&self, duration: Duration) -> u64 {
        (duration.as_secs_f64() * self.sample_rate as f64).round() as u64
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("audio device not found: {0}")]
    DeviceNotFound(String),
    #[error("no audio device available")]
    NoDevice,
    #[error("system audio capture is not supported on this system")]
    LoopbackUnsupported,
    #[error("permission to capture audio was denied")]
    PermissionDenied,
    #[error("unsupported audio format: {0}")]
    UnsupportedFormat(String),
    #[error("audio backend error: {0}")]
    Backend(String),
}

#[derive(Clone, Debug)]
pub enum CaptureEvent {
    /// The device disappeared or the stream stopped unexpectedly.
    Disconnected(String),
    /// A recoverable stream problem (e.g. an overrun).
    Warning(String),
}

/// Lock-free anchor between the capture frame counter and wall-clock time.
///
/// The audio callback is the single writer; readers retry while a write is in progress
/// (a sequence lock), so neither side blocks.
#[derive(Debug, Default)]
pub struct CaptureClock {
    sequence: AtomicU64,
    frame: AtomicU64,
    unix_nanos: AtomicU64,
}

impl CaptureClock {
    /// Records that frame `frame` was captured at `at`. Only call from one thread.
    pub fn anchor(&self, frame: u64, at: SystemTime) {
        let nanos = at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        self.sequence.fetch_add(1, Ordering::AcqRel);
        self.frame.store(frame, Ordering::Release);
        self.unix_nanos.store(nanos, Ordering::Release);
        self.sequence.fetch_add(1, Ordering::AcqRel);
    }

    /// The latest anchor, or `None` before the first callback.
    pub fn latest(&self) -> Option<(u64, SystemTime)> {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let frame = self.frame.load(Ordering::Acquire);
            let nanos = self.unix_nanos.load(Ordering::Acquire);
            if self.sequence.load(Ordering::Acquire) == before {
                return (before > 0)
                    .then(|| (frame, SystemTime::UNIX_EPOCH + Duration::from_nanos(nanos)));
            }
        }
    }
}

/// Keeps the platform stream alive. Dropping it stops capture.
pub trait CaptureStream: Send {}

/// A running capture: format, sample queue, clock and the stream guard.
pub struct CaptureHandle {
    pub source: CaptureSource,
    pub spec: PcmSpec,
    pub samples: rtrb::Consumer<f32>,
    pub clock: std::sync::Arc<CaptureClock>,
    pub events: crossbeam_channel::Receiver<CaptureEvent>,
    pub stream: Box<dyn CaptureStream>,
}

/// A platform audio capture implementation.
pub trait AudioCaptureBackend: Send + Sync {
    /// All sources that can be recorded, loopback sources first.
    fn list_sources(&self) -> Result<Vec<CaptureSource>, CaptureError>;

    /// Starts capturing `source_id`, or the default source when `None`.
    fn start(&self, source_id: Option<&str>) -> Result<CaptureHandle, CaptureError>;

    /// The source used when none is configured.
    fn default_source(&self) -> Result<CaptureSource, CaptureError> {
        let sources = self.list_sources()?;
        sources
            .iter()
            .find(|s| s.is_default && s.kind == CaptureSourceKind::OutputLoopback)
            .or_else(|| sources.iter().find(|s| s.is_default))
            .or_else(|| sources.first())
            .cloned()
            .ok_or(CaptureError::NoDevice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_empty_until_anchored() {
        let clock = CaptureClock::default();
        assert!(clock.latest().is_none());
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(42);
        clock.anchor(480, at);
        assert_eq!(clock.latest(), Some((480, at)));
    }

    #[test]
    fn spec_converts_between_frames_and_time() {
        let spec = PcmSpec {
            sample_rate: 48_000,
            channels: 2,
        };
        assert_eq!(spec.duration_to_frames(Duration::from_millis(500)), 24_000);
        assert_eq!(spec.frames_to_duration(96_000), Duration::from_secs(2));
    }
}
