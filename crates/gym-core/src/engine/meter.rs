use std::sync::atomic::{AtomicU32, Ordering};

/// Peak and RMS levels (linear, `0.0..=1.0`) for the first two channels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Levels {
    pub peak: [f32; 2],
    pub rms: [f32; 2],
}

/// Lock-free level meter written by the recorder and read by the UI.
///
/// Peaks accumulate until read so short transients are not missed between UI frames.
#[derive(Debug, Default)]
pub struct LevelMeter {
    peak: [AtomicU32; 2],
    rms: [AtomicU32; 2],
}

impl LevelMeter {
    pub(crate) fn publish(&self, samples: &[f32], channels: usize) {
        if samples.is_empty() || channels == 0 {
            return;
        }
        let mut peak = [0f32; 2];
        let mut sum = [0f64; 2];
        let frames = samples.len() / channels;
        for frame in samples.chunks_exact(channels) {
            for (ch, &sample) in frame.iter().take(2).enumerate() {
                let abs = sample.abs();
                peak[ch] = peak[ch].max(abs);
                sum[ch] += (sample as f64) * (sample as f64);
            }
        }
        if channels == 1 {
            peak[1] = peak[0];
            sum[1] = sum[0];
        }
        for ch in 0..2 {
            // Bit patterns of non-negative floats order like the floats themselves.
            self.peak[ch].fetch_max(peak[ch].to_bits(), Ordering::Relaxed);
            let rms = (sum[ch] / frames as f64).sqrt() as f32;
            self.rms[ch].store(rms.to_bits(), Ordering::Relaxed);
        }
    }

    /// Returns the levels since the last call and resets the peak accumulators.
    pub fn take(&self) -> Levels {
        let mut levels = Levels::default();
        for ch in 0..2 {
            levels.peak[ch] = f32::from_bits(self.peak[ch].swap(0, Ordering::Relaxed));
            levels.rms[ch] = f32::from_bits(self.rms[ch].load(Ordering::Relaxed));
        }
        levels
    }

    pub(crate) fn reset(&self) {
        for ch in 0..2 {
            self.peak[ch].store(0, Ordering::Relaxed);
            self.rms[ch].store(0, Ordering::Relaxed);
        }
    }
}

/// Converts a linear amplitude to decibels relative to full scale.
pub fn to_dbfs(linear: f32) -> f32 {
    if linear <= 1e-6 {
        -120.0
    } else {
        20.0 * linear.log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaks_accumulate_until_taken() {
        let meter = LevelMeter::default();
        meter.publish(&[0.5, -0.25], 2);
        meter.publish(&[0.1, 0.1], 2);
        let levels = meter.take();
        assert_eq!(levels.peak, [0.5, 0.25]);
        assert_eq!(meter.take().peak, [0.0, 0.0]);
    }

    #[test]
    fn mono_is_mirrored() {
        let meter = LevelMeter::default();
        meter.publish(&[0.5, -0.5], 1);
        let levels = meter.take();
        assert_eq!(levels.peak[0], levels.peak[1]);
        assert!((levels.rms[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn dbfs() {
        assert!((to_dbfs(1.0)).abs() < 1e-6);
        assert!((to_dbfs(0.5) + 6.02).abs() < 0.01);
        assert_eq!(to_dbfs(0.0), -120.0);
    }
}
