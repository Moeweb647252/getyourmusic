//! Audio capture through cpal, shared by every platform.
//!
//! Output devices are recorded through cpal's loopback support (a Core Audio process tap on
//! macOS 14.2+, WASAPI loopback on Windows). Devices with inputs, including virtual loopback
//! drivers such as BlackHole, are recorded as regular inputs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use crossbeam_channel::Sender;

use gym_core::capture::{
    AudioCaptureBackend, CaptureClock, CaptureError, CaptureEvent, CaptureHandle, CaptureSource,
    CaptureSourceKind, CaptureStream, PcmSpec,
};

const LOOPBACK_PREFIX: &str = "loopback:";
const INPUT_PREFIX: &str = "input:";
/// Seconds of audio the queue between the audio thread and the recorder can hold.
const QUEUE_SECONDS: usize = 4;

/// Names of virtual drivers whose input mirrors what is played to them.
const VIRTUAL_LOOPBACK_DRIVERS: &[&str] = &["blackhole", "soundflower", "loopback", "vb-cable"];

fn is_virtual_loopback(name: &str) -> bool {
    let name = name.to_lowercase();
    VIRTUAL_LOOPBACK_DRIVERS.iter().any(|d| name.contains(d))
}

pub struct CpalCapture {
    loopback_supported: bool,
}

impl CpalCapture {
    pub fn new(loopback_supported: bool) -> Self {
        Self { loopback_supported }
    }
}

fn backend_error(error: impl std::fmt::Display) -> CaptureError {
    CaptureError::Backend(error.to_string())
}

fn map_cpal_error(error: cpal::Error) -> CaptureError {
    match error.kind() {
        cpal::ErrorKind::PermissionDenied => CaptureError::PermissionDenied,
        cpal::ErrorKind::DeviceNotAvailable => CaptureError::DeviceNotFound(error.to_string()),
        cpal::ErrorKind::UnsupportedConfig => CaptureError::UnsupportedFormat(error.to_string()),
        _ => backend_error(error),
    }
}

impl AudioCaptureBackend for CpalCapture {
    fn list_sources(&self) -> Result<Vec<CaptureSource>, CaptureError> {
        let host = cpal::default_host();
        let default_output = host.default_output_device().and_then(|d| d.id().ok());
        let mut sources = Vec::new();
        for device in host.devices().map_err(map_cpal_error)? {
            let (Ok(id), Ok(description)) = (device.id(), device.description()) else {
                continue;
            };
            let name = description.name().to_owned();
            let is_default_output = default_output.as_ref() == Some(&id);
            if device.supports_input() {
                let Ok(config) = device.default_input_config() else {
                    continue;
                };
                sources.push(CaptureSource {
                    id: format!("{INPUT_PREFIX}{id}"),
                    is_default: is_default_output && is_virtual_loopback(&name),
                    name,
                    kind: CaptureSourceKind::Input,
                    channels: config.channels(),
                    sample_rate: config.sample_rate(),
                });
            } else if device.supports_output() && self.loopback_supported {
                let Ok(config) = device.default_output_config() else {
                    continue;
                };
                sources.push(CaptureSource {
                    id: format!("{LOOPBACK_PREFIX}{id}"),
                    is_default: is_default_output,
                    name,
                    kind: CaptureSourceKind::OutputLoopback,
                    channels: config.channels(),
                    sample_rate: config.sample_rate(),
                });
            }
        }
        // Default first, then loopback devices, then virtual drivers, then other inputs.
        sources.sort_by_key(|s| {
            (
                !s.is_default,
                s.kind != CaptureSourceKind::OutputLoopback,
                !is_virtual_loopback(&s.name),
                s.name.to_lowercase(),
            )
        });
        Ok(sources)
    }

    fn default_source(&self) -> Result<CaptureSource, CaptureError> {
        let sources = self.list_sources()?;
        sources
            .iter()
            .find(|s| s.is_default)
            .or_else(|| {
                sources
                    .iter()
                    .find(|s| s.kind == CaptureSourceKind::OutputLoopback)
            })
            .or_else(|| sources.iter().find(|s| is_virtual_loopback(&s.name)))
            .cloned()
            .ok_or(if self.loopback_supported {
                CaptureError::NoDevice
            } else {
                CaptureError::LoopbackUnsupported
            })
    }

    fn start(&self, source_id: Option<&str>) -> Result<CaptureHandle, CaptureError> {
        let source = match source_id {
            Some(id) => self
                .list_sources()?
                .into_iter()
                .find(|s| s.id == id)
                .ok_or_else(|| CaptureError::DeviceNotFound(id.to_owned()))?,
            None => self.default_source()?,
        };
        let device_id = source
            .id
            .strip_prefix(LOOPBACK_PREFIX)
            .or_else(|| source.id.strip_prefix(INPUT_PREFIX))
            .ok_or_else(|| CaptureError::DeviceNotFound(source.id.clone()))?;
        let host = cpal::default_host();
        let device = device_id
            .parse()
            .ok()
            .and_then(|id| host.device_by_id(&id))
            .ok_or_else(|| CaptureError::DeviceNotFound(source.name.clone()))?;
        let config = match source.kind {
            CaptureSourceKind::Input => device.default_input_config(),
            CaptureSourceKind::OutputLoopback => device.default_output_config(),
        }
        .map_err(map_cpal_error)?;

        let spec = PcmSpec {
            sample_rate: config.sample_rate(),
            channels: config.channels(),
        };
        let channels = spec.channels as usize;
        let capacity = spec.sample_rate as usize * channels * QUEUE_SECONDS;
        let (producer, consumer) = rtrb::RingBuffer::new(capacity);
        let clock = Arc::new(CaptureClock::default());
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let writer = Writer {
            producer,
            clock: Arc::clone(&clock),
            channels,
            frames: 0,
            overrun_reported: Arc::new(AtomicBool::new(false)),
            events: events_tx.clone(),
        };

        let stream_config = config.config();
        let stream = match config.sample_format() {
            SampleFormat::F32 => build::<f32>(&device, &stream_config, writer, events_tx),
            SampleFormat::I16 => build::<i16>(&device, &stream_config, writer, events_tx),
            SampleFormat::I32 => build::<i32>(&device, &stream_config, writer, events_tx),
            SampleFormat::U16 => build::<u16>(&device, &stream_config, writer, events_tx),
            other => {
                return Err(CaptureError::UnsupportedFormat(format!("{other:?}")));
            }
        }?;
        stream.play().map_err(map_cpal_error)?;
        tracing::info!(source = %source.name, ?spec, "capture started");

        Ok(CaptureHandle {
            source,
            spec,
            samples: consumer,
            clock,
            events: events_rx,
            stream: Box::new(CpalStream(stream)),
        })
    }
}

struct CpalStream(#[allow(dead_code)] cpal::Stream);
impl CaptureStream for CpalStream {}

/// State owned by the real-time audio callback. Never allocates or locks.
struct Writer {
    producer: rtrb::Producer<f32>,
    clock: Arc<CaptureClock>,
    channels: usize,
    frames: u64,
    overrun_reported: Arc<AtomicBool>,
    events: Sender<CaptureEvent>,
}

impl Writer {
    fn write<T>(&mut self, data: &[T], latency: Duration)
    where
        T: Sample,
        f32: FromSample<T>,
    {
        let n = data.len() - data.len() % self.channels;
        if n == 0 {
            return;
        }
        let captured_at = SystemTime::now()
            .checked_sub(latency)
            .unwrap_or_else(SystemTime::now);
        self.clock.anchor(self.frames, captured_at);
        match self.producer.write_chunk_uninit(n) {
            Ok(mut chunk) => {
                let (first, second) = chunk.as_mut_slices();
                let split = first.len();
                for (slot, &sample) in first.iter_mut().zip(&data[..split]) {
                    slot.write(f32::from_sample(sample));
                }
                for (slot, &sample) in second.iter_mut().zip(&data[split..n]) {
                    slot.write(f32::from_sample(sample));
                }
                // SAFETY: all `n` slots were initialized above.
                unsafe { chunk.commit_all() };
                self.frames += (n / self.channels) as u64;
            }
            Err(_) => {
                // The recorder fell behind; drop this buffer rather than block the audio thread.
                if !self.overrun_reported.swap(true, Ordering::Relaxed) {
                    let _ = self
                        .events
                        .send(CaptureEvent::Warning("audio queue overrun".into()));
                }
            }
        }
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut writer: Writer,
    events: Sender<CaptureEvent>,
) -> Result<cpal::Stream, CaptureError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device
        .build_input_stream::<T, _, _>(
            *config,
            move |data: &[T], info: &cpal::InputCallbackInfo| {
                let timestamp = info.timestamp();
                let latency = timestamp
                    .callback
                    .checked_duration_since(timestamp.capture)
                    .unwrap_or_default();
                writer.write(data, latency);
            },
            move |error: cpal::Error| {
                let event = match error.kind() {
                    cpal::ErrorKind::DeviceNotAvailable
                    | cpal::ErrorKind::StreamInvalidated
                    | cpal::ErrorKind::HostUnavailable => {
                        CaptureEvent::Disconnected(error.to_string())
                    }
                    _ => CaptureEvent::Warning(error.to_string()),
                };
                let _ = events.send(event);
            },
            Some(Duration::from_secs(5)),
        )
        .map_err(map_cpal_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_virtual_drivers() {
        assert!(is_virtual_loopback("BlackHole 2ch"));
        assert!(!is_virtual_loopback("MacBook Air Speakers"));
    }
}
