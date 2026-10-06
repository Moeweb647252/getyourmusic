//! The finalize worker: cached audio → (trim, resample) → encode → tag → storage → library.

use std::io;
use std::path::Path;
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};
use crossbeam_channel::{Sender, unbounded};

use crate::encode::{EncodeError, choose_sample_rate};
use crate::library::RecordingEntry;
use crate::model::NowPlaying;
use crate::pcm::{PcmReader, conform};
use crate::settings::IncompletePolicy;
use crate::storage::{StorageError, StoreOutcome};
use crate::tags::{TagError, write_tags};

use super::segmenter::SegmentId;
use super::spool::SpoolData;
use super::{EngineEvent, EngineServices, OutputConfig, PartialReason, SkipReason, StopReason};

/// Segments whose peak never exceeds this (-80 dBFS) contain no audio.
const SILENT_PEAK: f32 = 1e-4;
/// Segments shorter than this (after trimming) are discarded.
const MIN_TRACK: Duration = Duration::from_secs(5);
/// Peak below which audio at the edges counts as silence (-60 dBFS).
const TRIM_THRESHOLD: f32 = 0.001;
/// Players often leave a gap of a few seconds between tracks.
const MAX_TRIM: Duration = Duration::from_secs(5);
const TRIM_MARGIN: Duration = Duration::from_millis(10);

pub(super) struct FinalizeJob {
    pub id: SegmentId,
    pub audio: SpoolData,
    pub frames: u64,
    /// Highest absolute sample value written to the segment.
    pub peak: f32,
    pub track: NowPlaying,
    pub partial: Option<PartialReason>,
    pub recorded_at: DateTime<Utc>,
    pub output: OutputConfig,
}

pub(super) enum Job {
    Finalize(Box<FinalizeJob>),
    /// No more jobs will follow; report the engine as stopped.
    Shutdown(StopReason),
}

#[derive(Debug, thiserror::Error)]
enum FinalizeError {
    #[error(transparent)]
    Encode(#[from] EncodeError),
    #[error(transparent)]
    Tag(#[from] TagError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

enum Outcome {
    Saved(Box<RecordingEntry>),
    Skipped(SkipReason),
}

pub(super) fn spawn_worker(
    services: EngineServices,
    events: Sender<EngineEvent>,
) -> io::Result<Sender<Job>> {
    let (tx, rx) = unbounded::<Job>();
    thread::Builder::new()
        .name("finalize".into())
        .spawn(move || {
            for job in rx {
                match job {
                    Job::Finalize(job) => process(&job, &services, &events),
                    Job::Shutdown(reason) => {
                        let _ = events.send(EngineEvent::Stopped(reason));
                        break;
                    }
                }
            }
        })?;
    Ok(tx)
}

fn process(job: &FinalizeJob, services: &EngineServices, events: &Sender<EngineEvent>) {
    let title = job.track.track.display_name();
    let event = match finalize(job, services, events) {
        Ok(Outcome::Saved(entry)) => {
            tracing::info!(track = %title, key = %entry.key, ms = entry.duration_ms, "saved");
            EngineEvent::Saved {
                id: job.id,
                entry: *entry,
            }
        }
        Ok(Outcome::Skipped(reason)) => {
            tracing::info!(track = %title, ?reason, "not saved");
            EngineEvent::Skipped { id: job.id, reason }
        }
        Err(err) => {
            tracing::error!(%err, track = %job.track.track.display_name(), "finalizing failed");
            EngineEvent::Failed {
                id: job.id,
                error: err.to_string(),
            }
        }
    };
    job.audio.remove();
    let _ = events.send(event);
}

fn finalize(
    job: &FinalizeJob,
    services: &EngineServices,
    events: &Sender<EngineEvent>,
) -> Result<Outcome, FinalizeError> {
    if job.peak < SILENT_PEAK {
        return Ok(Outcome::Skipped(SkipReason::Silent));
    }
    if let Some(reason) = job.partial
        && job.output.incomplete == IncompletePolicy::Discard
    {
        return Ok(Outcome::Skipped(SkipReason::Incomplete(reason)));
    }

    let spec = job.audio.spec();
    let frames = job.frames.min(job.audio.total_frames()?);
    let (start, end) = if job.output.trim_silence {
        audible_range(&job.audio, frames)?
    } else {
        (0, frames)
    };
    if end - start < spec.duration_to_frames(MIN_TRACK) {
        return Ok(Outcome::Skipped(SkipReason::TooShort));
    }

    let settings = job.output.encode;
    let encoder = services
        .encoders
        .get(settings.format)
        .ok_or(EncodeError::Unavailable(settings.format))?;
    let rate = choose_sample_rate(
        job.output.sample_rate.resolve(spec.sample_rate),
        encoder.supported_sample_rates(&settings),
    );
    let mut reader = conform(job.audio.reader(start, end)?, rate, encoder.max_channels())?;
    let out_spec = reader.spec();
    let out_frames = reader.total_frames();

    let extension = settings.format.extension();
    let encoded = job.audio.path().with_extension(extension);
    let result = encode_tag_store(job, services, events, &mut *reader, &encoded);
    if result.is_err() {
        let _ = std::fs::remove_file(&encoded);
    }
    let stored = match result? {
        StoreOutcome::Stored(stored) => stored,
        StoreOutcome::SkippedExisting(_) => return Ok(Outcome::Skipped(SkipReason::AlreadyExists)),
    };

    let entry = RecordingEntry {
        id: uuid::Uuid::new_v4(),
        provider_id: stored.provider_id,
        key: stored.key,
        track: job.track.track.summary(),
        player: job.track.player.clone(),
        format: settings.format,
        encode: settings,
        sample_rate: out_spec.sample_rate,
        channels: out_spec.channels,
        duration_ms: out_spec.frames_to_duration(out_frames).as_millis() as u64,
        size_bytes: stored.size,
        recorded_at: job.recorded_at,
        partial: job.partial.is_some(),
    };
    services.library.insert(entry.clone())?;
    Ok(Outcome::Saved(Box::new(entry)))
}

fn encode_tag_store(
    job: &FinalizeJob,
    services: &EngineServices,
    events: &Sender<EngineEvent>,
    reader: &mut dyn PcmReader,
    encoded: &Path,
) -> Result<StoreOutcome, FinalizeError> {
    let settings = &job.output.encode;
    let encoder = services
        .encoders
        .get(settings.format)
        .ok_or(EncodeError::Unavailable(settings.format))?;
    let mut last_reported = 0.0f32;
    let mut progress = |fraction: f32| {
        if fraction - last_reported >= 0.02 || fraction >= 1.0 {
            last_reported = fraction;
            let _ = events.send(EngineEvent::Finalizing {
                id: job.id,
                progress: fraction.clamp(0.0, 1.0),
            });
        }
    };
    progress(0.0);
    encoder.encode(reader, settings, encoded, &mut progress)?;
    write_tags(encoded, &job.track.track, &job.track.player)?;

    let key = job.output.naming.render(
        &job.track.track,
        &job.track.player,
        settings.format.extension(),
        &job.output.fallbacks,
    );
    Ok(services.storage.store(&key, encoded, job.output.conflict)?)
}

/// Frames `start..end` without leading and trailing digital silence (at most [`MAX_TRIM`] each).
fn audible_range(audio: &SpoolData, frames: u64) -> Result<(u64, u64), EncodeError> {
    let spec = audio.spec();
    let channels = spec.channels as usize;
    let max_trim = spec.duration_to_frames(MAX_TRIM).min(frames / 2);
    let margin = spec.duration_to_frames(TRIM_MARGIN);
    let loud = |frame: &[f32]| frame.iter().any(|s| s.abs() > TRIM_THRESHOLD);
    let mut buf = vec![0.0f32; 4096 * channels];

    let mut start = max_trim;
    let mut reader = audio.reader(0, max_trim)?;
    let mut position = 0u64;
    'leading: loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for (i, frame) in buf[..n].chunks_exact(channels).enumerate() {
            if loud(frame) {
                start = position + i as u64;
                break 'leading;
            }
        }
        position += (n / channels) as u64;
    }

    let tail_start = frames.saturating_sub(max_trim).max(start);
    let mut end = tail_start;
    let mut reader = audio.reader(tail_start, frames)?;
    let mut position = tail_start;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for (i, frame) in buf[..n].chunks_exact(channels).enumerate() {
            if loud(frame) {
                end = position + i as u64 + 1;
            }
        }
        position += (n / channels) as u64;
    }
    Ok((start.saturating_sub(margin), (end + margin).min(frames)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::PcmSpec;
    use crate::engine::CacheConfig;
    use crate::engine::spool::{MemoryBudget, SpoolWriter};
    use crate::settings::CacheMode;

    fn spool(dir: &Path, name: &str, samples: &[f32], mode: CacheMode) -> SpoolData {
        let spec = PcmSpec {
            sample_rate: 1_000,
            channels: 1,
        };
        let cache = CacheConfig {
            mode,
            ..Default::default()
        };
        let mut writer =
            SpoolWriter::create(dir.join(name), spec, &cache, &MemoryBudget::new(None)).unwrap();
        writer.write(samples).unwrap();
        writer.finish().unwrap()
    }

    #[test]
    fn trims_edges_within_limits() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [CacheMode::Disk, CacheMode::Memory] {
            let mut samples = vec![0.0; 500];
            samples.extend(std::iter::repeat_n(0.5, 9_000));
            samples.extend(vec![0.0; 3_000]);
            let audio = spool(dir.path(), "t.wav", &samples, mode);
            let (start, end) = audible_range(&audio, samples.len() as u64).unwrap();
            assert_eq!(
                start, 490,
                "leading silence is trimmed up to a 10 ms margin ({mode:?})"
            );
            assert_eq!(
                end, 9_510,
                "trailing silence is trimmed up to a 10 ms margin ({mode:?})"
            );

            let mut long = vec![0.5; 20_000];
            long.extend(vec![0.0; 7_000]);
            let audio = spool(dir.path(), "long.wav", &long, mode);
            let (_, end) = audible_range(&audio, long.len() as u64).unwrap();
            assert_eq!(
                end, 22_010,
                "at most 5 s of trailing silence is trimmed ({mode:?})"
            );
        }
    }
}
