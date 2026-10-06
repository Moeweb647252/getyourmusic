//! Audio of segments between recording and encoding: in the cache folder, in memory, or both.
//!
//! In memory mode a segment fills fixed-size chunks until the shared [`MemoryBudget`] runs out,
//! then either continues in a WAV file (the memory head is kept) or fails.

use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::capture::PcmSpec;
use crate::encode::EncodeError;
use crate::pcm::{PcmReader, SliceReader, WavFileReader};
use crate::settings::{CacheMode, MemoryOverflow};

use super::CacheConfig;
use super::segmenter::SegmentId;

/// Frames per memory chunk (256 KiB for stereo).
const CHUNK_FRAMES: usize = 32 * 1024;
const SAMPLE_BYTES: u64 = size_of::<f32>() as u64;

/// A random prefix that keeps cache files of different sessions apart.
pub(super) fn new_session() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_owned()
}

/// The cache file of a segment; encoded files reuse its stem with their own extension.
pub(super) fn spool_file_name(session: &str, id: SegmentId) -> String {
    format!("{session}-{:04}.wav", id.0)
}

/// Whether `path` is named like a file from [`spool_file_name`] (any extension).
pub(super) fn is_spool_file(path: &Path) -> bool {
    let (Some(stem), Some(_)) = (path.file_stem().and_then(|s| s.to_str()), path.extension())
    else {
        return false;
    };
    let Some((session, number)) = stem.split_once('-') else {
        return false;
    };
    session.len() == 8
        && session
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && number.len() >= 4
        && number.bytes().all(|b| b.is_ascii_digit())
}

/// Memory that in-memory segments of one session may use together.
#[derive(Debug)]
pub(super) struct MemoryBudget {
    limit: Option<u64>,
    used: AtomicU64,
}

impl MemoryBudget {
    pub(super) fn new(limit: Option<u64>) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicU64::new(0),
        })
    }

    fn try_reserve(&self, bytes: u64) -> bool {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let next = used + bytes;
            if self.limit.is_some_and(|limit| next > limit) {
                return false;
            }
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(actual) => used = actual,
            }
        }
    }

    fn release(&self, bytes: u64) {
        self.used.fetch_sub(bytes, Ordering::AcqRel);
    }

    #[cfg(test)]
    fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }
}

/// Samples in fixed-size chunks, so growing never copies what is already stored.
///
/// Chunks hold whole frames, so a segment that continues on disk splits between frames.
#[derive(Debug)]
struct MemorySpool {
    chunks: Vec<Vec<f32>>,
    chunk_samples: usize,
    len: usize,
    channels: usize,
    budget: Arc<MemoryBudget>,
    reserved: u64,
}

impl MemorySpool {
    fn new(budget: Arc<MemoryBudget>, channels: u16) -> Self {
        let channels = channels.max(1) as usize;
        Self {
            chunks: Vec::new(),
            chunk_samples: CHUNK_FRAMES * channels,
            len: 0,
            channels,
            budget,
            reserved: 0,
        }
    }

    /// Appends as many samples as the budget allows and returns how many were taken.
    fn push(&mut self, samples: &[f32]) -> usize {
        let mut taken = 0;
        while taken < samples.len() {
            if self
                .chunks
                .last()
                .is_none_or(|chunk| chunk.len() == self.chunk_samples)
            {
                let bytes = self.chunk_samples as u64 * SAMPLE_BYTES;
                if !self.budget.try_reserve(bytes) {
                    break;
                }
                self.reserved += bytes;
                self.chunks.push(Vec::with_capacity(self.chunk_samples));
            }
            let chunk = self
                .chunks
                .last_mut()
                .expect("a chunk with room was just ensured");
            let n = (self.chunk_samples - chunk.len()).min(samples.len() - taken);
            chunk.extend_from_slice(&samples[taken..taken + n]);
            taken += n;
        }
        self.len += taken;
        taken
    }

    fn frames(&self) -> u64 {
        (self.len / self.channels) as u64
    }
}

impl Drop for MemorySpool {
    fn drop(&mut self) {
        self.budget.release(self.reserved);
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum SpoolError {
    #[error("cannot create cache file: {0}")]
    Create(hound::Error),
    #[error("cannot write cache file: {0}")]
    Write(hound::Error),
    #[error("cannot finish cache file: {0}")]
    Finish(hound::Error),
    #[error("the track exceeded the memory limit for recording")]
    MemoryLimit,
}

/// Receives the samples of one segment while it records.
pub(super) struct SpoolWriter {
    spec: PcmSpec,
    path: PathBuf,
    memory: Option<MemorySpool>,
    file: Option<hound::WavWriter<BufWriter<File>>>,
    on_overflow: MemoryOverflow,
}

impl SpoolWriter {
    pub(super) fn create(
        path: PathBuf,
        spec: PcmSpec,
        cache: &CacheConfig,
        budget: &Arc<MemoryBudget>,
    ) -> Result<Self, SpoolError> {
        let mut writer = Self {
            spec,
            path,
            memory: None,
            file: None,
            on_overflow: cache.on_overflow,
        };
        match cache.mode {
            CacheMode::Disk => writer.open_file()?,
            CacheMode::Memory => {
                writer.memory = Some(MemorySpool::new(Arc::clone(budget), spec.channels));
            }
        }
        Ok(writer)
    }

    fn open_file(&mut self) -> Result<(), SpoolError> {
        let spec = hound::WavSpec {
            channels: self.spec.channels,
            sample_rate: self.spec.sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let writer = hound::WavWriter::create(&self.path, spec).map_err(SpoolError::Create)?;
        self.file = Some(writer);
        Ok(())
    }

    pub(super) fn write(&mut self, samples: &[f32]) -> Result<(), SpoolError> {
        let mut rest = samples;
        if self.file.is_none()
            && let Some(memory) = &mut self.memory
        {
            rest = &rest[memory.push(rest)..];
            if rest.is_empty() {
                return Ok(());
            }
            match self.on_overflow {
                MemoryOverflow::SpillToDisk => {
                    tracing::info!(path = %self.path.display(), "memory limit reached; continuing on disk");
                    self.open_file()?;
                }
                MemoryOverflow::Fail => {
                    self.memory = None;
                    return Err(SpoolError::MemoryLimit);
                }
            }
        }
        if let Some(file) = &mut self.file {
            for &sample in rest {
                file.write_sample(sample).map_err(SpoolError::Write)?;
            }
        }
        Ok(())
    }

    /// Completes the segment and hands its audio over for encoding.
    pub(super) fn finish(mut self) -> Result<SpoolData, SpoolError> {
        let on_disk = match self.file.take() {
            Some(writer) => {
                if let Err(err) = writer.finalize() {
                    remove_file(&self.path);
                    return Err(SpoolError::Finish(err));
                }
                true
            }
            None => false,
        };
        Ok(SpoolData {
            spec: self.spec,
            path: self.path,
            memory: self.memory.map(Arc::new),
            on_disk,
        })
    }

    /// Drops the segment's audio, including any file.
    pub(super) fn discard(self) {
        let on_disk = self.file.is_some();
        drop(self.file);
        if on_disk {
            remove_file(&self.path);
        }
    }
}

/// The audio of a finished segment: a memory head followed by a file tail, either may be empty.
pub(super) struct SpoolData {
    spec: PcmSpec,
    /// The cache file (if `on_disk`); encoded files are placed next to it.
    path: PathBuf,
    memory: Option<Arc<MemorySpool>>,
    on_disk: bool,
}

impl SpoolData {
    pub(super) fn spec(&self) -> PcmSpec {
        self.spec
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    fn head_frames(&self) -> u64 {
        self.memory.as_ref().map_or(0, |memory| memory.frames())
    }

    pub(super) fn total_frames(&self) -> Result<u64, EncodeError> {
        let tail = if self.on_disk {
            WavFileReader::open(&self.path, 0, None)?.total_frames()
        } else {
            0
        };
        Ok(self.head_frames() + tail)
    }

    /// Reads frames `start..end`.
    pub(super) fn reader(&self, start: u64, end: u64) -> Result<Box<dyn PcmReader>, EncodeError> {
        let head = self.head_frames();
        let end = end.max(start);
        let memory =
            self.memory.as_ref().filter(|_| start < head).map(|memory| {
                MemoryReader::new(Arc::clone(memory), self.spec, start, end.min(head))
            });
        let file = if self.on_disk && end > head {
            Some(WavFileReader::open(
                &self.path,
                start.saturating_sub(head),
                Some(end - head),
            )?)
        } else {
            None
        };
        Ok(match (memory, file) {
            (Some(memory), Some(file)) => Box::new(ChainReader {
                first: Box::new(memory),
                second: Box::new(file),
                first_done: false,
            }),
            (Some(memory), None) => Box::new(memory),
            (None, Some(file)) => Box::new(file),
            (None, None) => Box::new(SliceReader::new(Vec::new(), self.spec)),
        })
    }

    /// Deletes the cache file; memory is freed once the data and its readers are dropped.
    pub(super) fn remove(&self) {
        if self.on_disk {
            remove_file(&self.path);
        }
    }
}

fn remove_file(path: &Path) {
    if let Err(err) = std::fs::remove_file(path)
        && err.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(%err, path = %path.display(), "cannot remove cache file");
    }
}

struct MemoryReader {
    memory: Arc<MemorySpool>,
    spec: PcmSpec,
    /// Sample positions.
    position: usize,
    end: usize,
}

impl MemoryReader {
    fn new(memory: Arc<MemorySpool>, spec: PcmSpec, start: u64, end: u64) -> Self {
        let channels = spec.channels as usize;
        let end = (end as usize * channels).min(memory.len);
        Self {
            position: (start as usize * channels).min(end),
            end,
            memory,
            spec,
        }
    }
}

impl PcmReader for MemoryReader {
    fn spec(&self) -> PcmSpec {
        self.spec
    }

    fn total_frames(&self) -> u64 {
        ((self.end - self.position) / self.spec.channels as usize) as u64
    }

    fn read(&mut self, buf: &mut [f32]) -> Result<usize, EncodeError> {
        let channels = self.spec.channels as usize;
        let wanted = (buf.len() / channels * channels).min(self.end - self.position);
        let mut written = 0;
        while written < wanted {
            let chunk = &self.memory.chunks[self.position / self.memory.chunk_samples];
            let offset = self.position % self.memory.chunk_samples;
            let n = (chunk.len() - offset).min(wanted - written);
            buf[written..written + n].copy_from_slice(&chunk[offset..offset + n]);
            written += n;
            self.position += n;
        }
        Ok(written)
    }
}

/// Reads `first` to its end, then `second`.
struct ChainReader {
    first: Box<dyn PcmReader>,
    second: Box<dyn PcmReader>,
    first_done: bool,
}

impl PcmReader for ChainReader {
    fn spec(&self) -> PcmSpec {
        self.first.spec()
    }

    fn total_frames(&self) -> u64 {
        self.first.total_frames() + self.second.total_frames()
    }

    fn read(&mut self, buf: &mut [f32]) -> Result<usize, EncodeError> {
        if !self.first_done {
            let n = self.first.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            self.first_done = true;
        }
        self.second.read(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: PcmSpec = PcmSpec {
        sample_rate: 1_000,
        channels: 2,
    };
    const CHUNK_BYTES: u64 = (CHUNK_FRAMES * 2) as u64 * SAMPLE_BYTES;

    fn ramp(frames: usize) -> Vec<f32> {
        (0..frames * 2).map(|i| i as f32).collect()
    }

    fn read_all(mut reader: Box<dyn PcmReader>) -> Vec<f32> {
        let mut out = Vec::new();
        // An odd buffer size exercises reads that straddle chunk and head/tail boundaries.
        let mut buf = vec![0.0; 2 * 999];
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                return out;
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    fn cache(mode: CacheMode, on_overflow: MemoryOverflow) -> CacheConfig {
        CacheConfig {
            mode,
            on_overflow,
            ..Default::default()
        }
    }

    fn record(
        dir: &Path,
        samples: &[f32],
        cache: CacheConfig,
        budget: &Arc<MemoryBudget>,
    ) -> Result<SpoolData, SpoolError> {
        let mut writer = SpoolWriter::create(dir.join("abcd1234-0001.wav"), SPEC, &cache, budget)?;
        // Several writes, like the recorder, some ending mid-frame.
        for part in samples.chunks(7_001) {
            if let Err(err) = writer.write(part) {
                writer.discard();
                return Err(err);
            }
        }
        writer.finish()
    }

    #[test]
    fn every_mode_reads_back_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let frames = CHUNK_FRAMES * 3 + 123;
        let samples = ramp(frames);
        let modes = [
            (cache(CacheMode::Disk, MemoryOverflow::SpillToDisk), None),
            (cache(CacheMode::Memory, MemoryOverflow::SpillToDisk), None),
            // Two chunks fit in memory; the rest continues on disk.
            (
                cache(CacheMode::Memory, MemoryOverflow::SpillToDisk),
                Some(2 * CHUNK_BYTES),
            ),
        ];
        for (cache, limit) in modes {
            let budget = MemoryBudget::new(limit);
            let audio = record(dir.path(), &samples, cache, &budget).unwrap();
            assert_eq!(
                audio.on_disk,
                cache.mode == CacheMode::Disk || limit.is_some()
            );
            assert_eq!(audio.total_frames().unwrap(), frames as u64);
            assert_eq!(read_all(audio.reader(0, frames as u64).unwrap()), samples);

            let (start, end) = (CHUNK_FRAMES - 10, 2 * CHUNK_FRAMES + 10);
            let reader = audio.reader(start as u64, end as u64).unwrap();
            assert_eq!(reader.total_frames(), (end - start) as u64);
            assert_eq!(read_all(reader), samples[start * 2..end * 2]);
            assert!(read_all(audio.reader(5, 5).unwrap()).is_empty());

            audio.remove();
            drop(audio);
            assert_eq!(budget.used(), 0, "memory is returned to the budget");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn failing_over_the_limit_releases_everything() {
        let dir = tempfile::tempdir().unwrap();
        let budget = MemoryBudget::new(Some(CHUNK_BYTES));
        let result = record(
            dir.path(),
            &ramp(CHUNK_FRAMES + 1),
            cache(CacheMode::Memory, MemoryOverflow::Fail),
            &budget,
        );
        assert!(matches!(result, Err(SpoolError::MemoryLimit)));
        assert_eq!(budget.used(), 0);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn the_budget_is_shared_until_audio_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let budget = MemoryBudget::new(Some(CHUNK_BYTES));
        let memory = cache(CacheMode::Memory, MemoryOverflow::Fail);
        let first = record(dir.path(), &ramp(10), memory, &budget).unwrap();
        assert_eq!(budget.used(), CHUNK_BYTES);
        assert!(record(dir.path(), &ramp(10), memory, &budget).is_err());
        drop(first);
        assert!(record(dir.path(), &ramp(10), memory, &budget).is_ok());
    }

    #[test]
    fn recognizes_only_spool_files() {
        assert!(is_spool_file(Path::new("/x/abcd1234-0001.wav")));
        assert!(is_spool_file(Path::new("0f0f0f0f-12345.flac")));
        assert!(!is_spool_file(Path::new("notes.txt")));
        assert!(!is_spool_file(Path::new("song.flac")));
        assert!(!is_spool_file(Path::new("ABCD1234-0001.wav")));
        assert!(!is_spool_file(Path::new("abcd1234-0001")));
        assert!(!is_spool_file(Path::new("Artist - Title-0001.flac")));
        let name = spool_file_name(&new_session(), SegmentId(7));
        assert!(is_spool_file(Path::new(&name)), "{name}");
    }
}
