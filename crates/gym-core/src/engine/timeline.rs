use std::sync::Arc;
use std::time::SystemTime;

use crate::capture::CaptureClock;

/// Maps between wall-clock time and capture frame indices.
pub(crate) trait FrameClock {
    /// The (possibly negative) frame index captured at `at`, or `None` before capture started.
    fn frame_at(&self, at: SystemTime) -> Option<i64>;
}

/// [`FrameClock`] backed by the anchors a capture backend publishes.
pub(crate) struct CaptureTimeline {
    clock: Arc<CaptureClock>,
    sample_rate: f64,
}

impl CaptureTimeline {
    pub(crate) fn new(clock: Arc<CaptureClock>, sample_rate: u32) -> Self {
        Self {
            clock,
            sample_rate: sample_rate as f64,
        }
    }

    /// Whether the first audio has arrived, so times can be mapped to frames.
    pub(crate) fn is_ready(&self) -> bool {
        self.clock.latest().is_some()
    }
}

impl FrameClock for CaptureTimeline {
    fn frame_at(&self, at: SystemTime) -> Option<i64> {
        let (anchor_frame, anchor_time) = self.clock.latest()?;
        let offset_secs = match at.duration_since(anchor_time) {
            Ok(ahead) => ahead.as_secs_f64(),
            Err(behind) => -behind.duration().as_secs_f64(),
        };
        Some(anchor_frame as i64 + (offset_secs * self.sample_rate).round() as i64)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use super::*;

    /// A clock where frame 0 was captured at `origin`.
    pub(crate) struct LinearClock {
        pub origin: SystemTime,
        pub sample_rate: f64,
    }

    impl FrameClock for LinearClock {
        fn frame_at(&self, at: SystemTime) -> Option<i64> {
            let secs = match at.duration_since(self.origin) {
                Ok(d) => d.as_secs_f64(),
                Err(e) => -e.duration().as_secs_f64(),
            };
            Some((secs * self.sample_rate).round() as i64)
        }
    }

    #[test]
    fn capture_timeline_extrapolates_from_the_latest_anchor() {
        let clock = Arc::new(CaptureClock::default());
        let timeline = CaptureTimeline::new(Arc::clone(&clock), 48_000);
        assert_eq!(timeline.frame_at(SystemTime::now()), None);

        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        clock.anchor(48_000, t0);
        assert_eq!(
            timeline.frame_at(t0 + Duration::from_millis(500)),
            Some(72_000)
        );
        assert_eq!(
            timeline.frame_at(t0 - Duration::from_secs(2)),
            Some(-48_000)
        );
    }
}
