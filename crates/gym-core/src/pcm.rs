//! Streaming PCM readers used between the spool files and the encoders.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

use crate::capture::PcmSpec;
use crate::encode::EncodeError;

/// A pull-based source of interleaved `f32` samples.
pub trait PcmReader: Send {
    fn spec(&self) -> PcmSpec;

    /// Total number of frames this reader will produce (used for progress reporting).
    fn total_frames(&self) -> u64;

    /// Fills `buf` with whole frames and returns the number of samples written; `0` means end.
    fn read(&mut self, buf: &mut [f32]) -> Result<usize, EncodeError>;
}

/// Reads a range of frames from a 32-bit float WAV file.
pub struct WavFileReader {
    reader: hound::WavReader<BufReader<File>>,
    spec: PcmSpec,
    remaining: u64,
    total: u64,
}

impl WavFileReader {
    /// Opens `path` and limits reading to frames `start..end` (`end` = `None` reads to the end).
    pub fn open(path: &Path, start: u64, end: Option<u64>) -> Result<Self, EncodeError> {
        let mut reader = hound::WavReader::open(path).map_err(wav_error)?;
        let wav = reader.spec();
        if wav.sample_format != hound::SampleFormat::Float || wav.bits_per_sample != 32 {
            return Err(EncodeError::Unsupported(
                "spool files must be 32-bit float WAV".into(),
            ));
        }
        let available = reader.duration() as u64;
        let start = start.min(available);
        let end = end.unwrap_or(available).clamp(start, available);
        reader.seek(start as u32).map_err(EncodeError::Io)?;
        let spec = PcmSpec {
            sample_rate: wav.sample_rate,
            channels: wav.channels,
        };
        Ok(Self {
            reader,
            spec,
            remaining: end - start,
            total: end - start,
        })
    }
}

impl PcmReader for WavFileReader {
    fn spec(&self) -> PcmSpec {
        self.spec
    }

    fn total_frames(&self) -> u64 {
        self.total
    }

    fn read(&mut self, buf: &mut [f32]) -> Result<usize, EncodeError> {
        let channels = self.spec.channels as usize;
        let frames = ((buf.len() / channels) as u64).min(self.remaining) as usize;
        let wanted = frames * channels;
        let mut written = 0;
        for (slot, sample) in buf[..wanted].iter_mut().zip(self.reader.samples::<f32>()) {
            *slot = sample.map_err(wav_error)?;
            written += 1;
        }
        let written = written - written % channels;
        self.remaining -= (written / channels) as u64;
        Ok(written)
    }
}

fn wav_error(error: hound::Error) -> EncodeError {
    match error {
        hound::Error::IoError(io) => EncodeError::Io(io),
        other => EncodeError::Encoder(other.to_string()),
    }
}

/// An in-memory reader, mainly for tests.
pub struct SliceReader {
    samples: Vec<f32>,
    position: usize,
    spec: PcmSpec,
}

impl SliceReader {
    pub fn new(samples: Vec<f32>, spec: PcmSpec) -> Self {
        Self {
            samples,
            position: 0,
            spec,
        }
    }
}

impl PcmReader for SliceReader {
    fn spec(&self) -> PcmSpec {
        self.spec
    }

    fn total_frames(&self) -> u64 {
        (self.samples.len() / self.spec.channels as usize) as u64
    }

    fn read(&mut self, buf: &mut [f32]) -> Result<usize, EncodeError> {
        let channels = self.spec.channels as usize;
        let n = (buf.len().min(self.samples.len() - self.position) / channels) * channels;
        buf[..n].copy_from_slice(&self.samples[self.position..self.position + n]);
        self.position += n;
        Ok(n)
    }
}

/// Keeps the first `max` channels of a reader (front left/right for surround layouts).
pub struct ChannelLimiter {
    inner: Box<dyn PcmReader>,
    out_channels: u16,
    scratch: Vec<f32>,
}

impl ChannelLimiter {
    pub fn new(inner: Box<dyn PcmReader>, max: u16) -> Self {
        let out_channels = inner.spec().channels.min(max);
        Self {
            inner,
            out_channels,
            scratch: Vec::new(),
        }
    }
}

impl PcmReader for ChannelLimiter {
    fn spec(&self) -> PcmSpec {
        PcmSpec {
            channels: self.out_channels,
            ..self.inner.spec()
        }
    }

    fn total_frames(&self) -> u64 {
        self.inner.total_frames()
    }

    fn read(&mut self, buf: &mut [f32]) -> Result<usize, EncodeError> {
        let in_channels = self.inner.spec().channels as usize;
        let out_channels = self.out_channels as usize;
        let frames = buf.len() / out_channels;
        self.scratch.resize(frames * in_channels, 0.0);
        let read = self.inner.read(&mut self.scratch)?;
        let frames_read = read / in_channels;
        for frame in 0..frames_read {
            let src = &self.scratch[frame * in_channels..frame * in_channels + out_channels];
            buf[frame * out_channels..(frame + 1) * out_channels].copy_from_slice(src);
        }
        Ok(frames_read * out_channels)
    }
}

/// Converts the sample rate of a reader with a high-quality FFT resampler.
pub struct ResamplingReader {
    inner: Box<dyn PcmReader>,
    resampler: Fft<f32>,
    out_spec: PcmSpec,
    input: Vec<f32>,
    output: Vec<f32>,
    pending: std::ops::Range<usize>,
    delay_to_skip: usize,
    remaining_out_frames: u64,
    inner_done: bool,
}

impl ResamplingReader {
    const CHUNK_FRAMES: usize = 4096;

    pub fn new(inner: Box<dyn PcmReader>, sample_rate: u32) -> Result<Self, EncodeError> {
        let in_spec = inner.spec();
        let channels = in_spec.channels as usize;
        let resampler = Fft::<f32>::new(
            in_spec.sample_rate as usize,
            sample_rate as usize,
            Self::CHUNK_FRAMES,
            channels,
            FixedSync::Input,
        )
        .map_err(|e| EncodeError::Encoder(format!("resampler: {e}")))?;
        let ratio = sample_rate as f64 / in_spec.sample_rate as f64;
        let remaining_out_frames = (inner.total_frames() as f64 * ratio).round() as u64;
        Ok(Self {
            input: vec![0.0; resampler.input_frames_max() * channels],
            output: vec![0.0; resampler.output_frames_max() * channels],
            delay_to_skip: resampler.output_delay(),
            resampler,
            out_spec: PcmSpec {
                sample_rate,
                channels: in_spec.channels,
            },
            pending: 0..0,
            remaining_out_frames,
            inner_done: false,
            inner,
        })
    }

    fn process_chunk(&mut self) -> Result<(), EncodeError> {
        let channels = self.out_spec.channels as usize;
        let in_frames = self.resampler.input_frames_next();
        let wanted = in_frames * channels;
        let mut filled = 0;
        while !self.inner_done && filled < wanted {
            let n = self.inner.read(&mut self.input[filled..wanted])?;
            if n == 0 {
                self.inner_done = true;
            }
            filled += n;
        }
        self.input[filled..wanted].fill(0.0);

        let out_frames = self.resampler.output_frames_next();
        let input = InterleavedSlice::new(&self.input[..wanted], channels, in_frames)
            .map_err(|e| EncodeError::Encoder(format!("resampler input: {e}")))?;
        let mut output = InterleavedSlice::new_mut(
            &mut self.output[..out_frames * channels],
            channels,
            out_frames,
        )
        .map_err(|e| EncodeError::Encoder(format!("resampler output: {e}")))?;
        let (_, produced) = self
            .resampler
            .process_into_buffer(&input, &mut output, None)
            .map_err(|e| EncodeError::Encoder(format!("resampler: {e}")))?;

        let skip = self.delay_to_skip.min(produced);
        self.delay_to_skip -= skip;
        self.pending = skip * channels..produced * channels;
        Ok(())
    }
}

impl PcmReader for ResamplingReader {
    fn spec(&self) -> PcmSpec {
        self.out_spec
    }

    fn total_frames(&self) -> u64 {
        (self.inner.total_frames() as f64 * self.out_spec.sample_rate as f64
            / self.inner.spec().sample_rate as f64)
            .round() as u64
    }

    fn read(&mut self, buf: &mut [f32]) -> Result<usize, EncodeError> {
        let channels = self.out_spec.channels as usize;
        let mut written = 0;
        while written + channels <= buf.len() && self.remaining_out_frames > 0 {
            if self.pending.is_empty() {
                self.process_chunk()?;
                continue;
            }
            let frames = ((buf.len() - written) / channels)
                .min(self.pending.len() / channels)
                .min(self.remaining_out_frames as usize);
            let n = frames * channels;
            let start = self.pending.start;
            buf[written..written + n].copy_from_slice(&self.output[start..start + n]);
            self.pending.start += n;
            written += n;
            self.remaining_out_frames -= frames as u64;
        }
        Ok(written)
    }
}

/// Wraps `reader` so that it produces `sample_rate` and at most `max_channels` channels.
pub fn conform(
    reader: Box<dyn PcmReader>,
    sample_rate: u32,
    max_channels: u16,
) -> Result<Box<dyn PcmReader>, EncodeError> {
    let mut reader = reader;
    if reader.spec().channels > max_channels {
        reader = Box::new(ChannelLimiter::new(reader, max_channels));
    }
    if reader.spec().sample_rate != sample_rate {
        reader = Box::new(ResamplingReader::new(reader, sample_rate)?);
    }
    Ok(reader)
}

/// Converts float samples to signed integers, with TPDF dither below 24 bits.
pub struct Quantizer {
    bits: u32,
    scale: f32,
    max: i32,
    dither: bool,
    state: u32,
}

impl Quantizer {
    pub fn new(bits: u32) -> Self {
        let max = (1i32 << (bits - 1)) - 1;
        Self {
            bits,
            scale: max as f32 + 1.0,
            max,
            dither: bits < 24,
            state: 0x9E37_79B9,
        }
    }

    pub fn bits(&self) -> u32 {
        self.bits
    }

    fn next_uniform(&mut self) -> f32 {
        // xorshift32
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        (x as f32 / u32::MAX as f32) - 0.5
    }

    pub fn quantize(&mut self, sample: f32) -> i32 {
        let mut value = sample.clamp(-1.0, 1.0) * self.scale;
        if self.dither {
            value += self.next_uniform() + self.next_uniform();
        }
        (value.round() as i32).clamp(-self.max - 1, self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(spec: PcmSpec, seconds: f32, freq: f32) -> Vec<f32> {
        let frames = (spec.sample_rate as f32 * seconds) as usize;
        let mut out = Vec::with_capacity(frames * spec.channels as usize);
        for i in 0..frames {
            let s = (i as f32 * freq * std::f32::consts::TAU / spec.sample_rate as f32).sin() * 0.5;
            for _ in 0..spec.channels {
                out.push(s);
            }
        }
        out
    }

    fn drain(reader: &mut dyn PcmReader) -> Vec<f32> {
        let mut out = Vec::new();
        let mut buf = vec![0.0; 1000 * reader.spec().channels as usize];
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                return out;
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    #[test]
    fn resampler_produces_the_expected_length_and_level() {
        let spec = PcmSpec {
            sample_rate: 96_000,
            channels: 2,
        };
        let input = sine(spec, 1.0, 1_000.0);
        let reader = Box::new(SliceReader::new(input, spec));
        let mut resampled = ResamplingReader::new(reader, 48_000).unwrap();
        assert_eq!(resampled.total_frames(), 48_000);
        let out = drain(&mut resampled);
        assert_eq!(out.len(), 48_000 * 2);
        let peak = out[4_000..90_000].iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.01, "peak {peak}");
    }

    #[test]
    fn channel_limiter_keeps_front_channels() {
        let spec = PcmSpec {
            sample_rate: 48_000,
            channels: 4,
        };
        let reader = Box::new(SliceReader::new(
            vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0],
            spec,
        ));
        let mut limited = ChannelLimiter::new(reader, 2);
        assert_eq!(drain(&mut limited), vec![1.0, 2.0, 5.0, 6.0]);
    }

    #[test]
    fn wav_reader_honours_the_frame_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..100 {
            writer.write_sample(i as f32).unwrap();
        }
        writer.finalize().unwrap();
        let mut reader = WavFileReader::open(&path, 10, Some(20)).unwrap();
        assert_eq!(reader.total_frames(), 10);
        let samples = drain(&mut reader);
        assert_eq!(samples, (10..20).map(|i| i as f32).collect::<Vec<_>>());
    }

    #[test]
    fn quantizer_clamps_full_scale() {
        let mut q = Quantizer::new(24);
        assert_eq!(q.quantize(1.5), (1 << 23) - 1);
        assert_eq!(q.quantize(-1.0), -(1 << 23));
    }
}
