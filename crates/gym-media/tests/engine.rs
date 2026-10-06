//! End-to-end engine test: fake capture + fake player → FLAC files in local storage.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use gym_core::capture::PcmSpec;
use gym_core::encode::{BitDepth, EncodeSettings, EncoderRegistry, OutputFormat};
use gym_core::engine::{
    CacheConfig, EngineConfig, EngineEvent, EngineServices, OutputConfig, PartialReason,
    RecordingEngine, SkipReason, StopReason,
};
use gym_core::library::{Library, RecordingEntry};
use gym_core::model::{NowPlaying, PlayerInfo, TrackId, TrackMetadata};
use gym_core::naming::{DEFAULT_TEMPLATE, NamingFallbacks, NamingTemplate};
use gym_core::now_playing::{NowPlayingEvent, NowPlayingMonitor};
use gym_core::settings::{CacheMode, IncompletePolicy, MemoryOverflow, SampleRatePolicy};
use gym_core::storage::{ConflictPolicy, LocalStorage};
use gym_core::testing::{FakeCapture, FakeFeeder, FakeNowPlaying};

const RATE: u32 = 48_000;

fn snapshot(title: &str, elapsed: f64, at: SystemTime, duration: u64) -> NowPlaying {
    NowPlaying {
        player: PlayerInfo::new("com.example.player", "Example Player"),
        track_id: TrackId::new(title),
        track: TrackMetadata {
            title: title.into(),
            artist: Some("Artist".into()),
            album: Some("Album".into()),
            duration: Some(Duration::from_secs(duration)),
            ..Default::default()
        },
        playing: true,
        rate: 1.0,
        elapsed: Duration::from_secs_f64(elapsed),
        measured_at: at,
        received_at: at,
        is_advertisement: false,
    }
}

/// Feeds `seconds` of a tone in 100 ms chunks along the fake timeline.
fn feed(feeder: &mut FakeFeeder, origin: SystemTime, from: f64, seconds: f64, freq: f32) {
    let chunk = (RATE / 10) as usize;
    let chunks = (seconds * 10.0).round() as usize;
    for c in 0..chunks {
        let start_frame = ((from * RATE as f64) as usize) + c * chunk;
        let samples: Vec<f32> = (start_frame..start_frame + chunk)
            .flat_map(|i| {
                let s = (i as f32 * freq * std::f32::consts::TAU / RATE as f32).sin() * 0.4;
                [s, s]
            })
            .collect();
        let at = origin + Duration::from_secs_f64(start_frame as f64 / RATE as f64);
        feeder.feed(&samples, at);
        std::thread::sleep(Duration::from_millis(2));
    }
}

struct Recorded {
    saved: Vec<RecordingEntry>,
    skipped: Vec<SkipReason>,
    failed: Vec<String>,
    library: Arc<Library>,
    spool: tempfile::TempDir,
    music: tempfile::TempDir,
    _data: tempfile::TempDir,
}

/// Records a three-track playlist: one joined mid-track, then two full 8 s tracks.
fn record_playlist(cache: CacheConfig) -> Recorded {
    let spool = tempfile::tempdir().unwrap();
    let music = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    let spec = PcmSpec {
        sample_rate: RATE,
        channels: 2,
    };
    let (capture, feeder_rx) = FakeCapture::new(spec);
    let player = FakeNowPlaying::default();
    let monitor = NowPlayingMonitor::start(&player).unwrap();
    let mut encoders = EncoderRegistry::new();
    gym_media::register_encoders(&mut encoders);
    let library = Arc::new(Library::open(data.path().join("library.json")));

    let services = EngineServices {
        capture: Arc::new(capture),
        now_playing: monitor,
        encoders,
        storage: Arc::new(LocalStorage::new(music.path())),
        library: Arc::clone(&library),
        spool_dir: spool.path().to_path_buf(),
    };
    let config = EngineConfig {
        capture_source: None,
        follow_player: None,
        auto_stop: None,
        output: OutputConfig {
            encode: EncodeSettings {
                format: OutputFormat::Flac,
                bit_depth: BitDepth::Bits16,
                ..Default::default()
            },
            sample_rate: SampleRatePolicy::Source,
            naming: NamingTemplate::parse(DEFAULT_TEMPLATE).unwrap(),
            fallbacks: NamingFallbacks::default(),
            conflict: ConflictPolicy::KeepBoth,
            incomplete: IncompletePolicy::Discard,
            trim_silence: true,
        },
        cache,
    };

    let engine = RecordingEngine::start(config, services).unwrap();
    let events = engine.events();
    let mut feeder = feeder_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let origin = SystemTime::now() - Duration::from_secs(60);
    let pause = || std::thread::sleep(Duration::from_millis(60));

    // Joined "Intro" 30 s in: must be discarded as incomplete.
    player.emit(NowPlayingEvent::Updated(snapshot(
        "Intro", 30.0, origin, 60,
    )));
    pause();
    feed(&mut feeder, origin, 0.0, 6.0, 330.0);
    // "First" plays in full.
    player.emit(NowPlayingEvent::Updated(snapshot(
        "First",
        0.0,
        origin + Duration::from_secs(6),
        8,
    )));
    pause();
    feed(&mut feeder, origin, 6.0, 8.0, 440.0);
    // "Second" is reported 0.5 s late.
    player.emit(NowPlayingEvent::Updated(snapshot(
        "Second",
        0.5,
        origin + Duration::from_secs_f64(14.5),
        8,
    )));
    pause();
    feed(&mut feeder, origin, 14.0, 8.0, 550.0);
    engine.stop();

    let mut saved = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();
    loop {
        match events
            .recv_timeout(Duration::from_secs(60))
            .expect("engine stopped in time")
        {
            EngineEvent::Saved { entry, .. } => saved.push(entry),
            EngineEvent::Skipped { reason, .. } => skipped.push(reason),
            EngineEvent::Failed { error, .. } => failed.push(error),
            EngineEvent::Stopped(reason) => {
                assert_eq!(reason, StopReason::User);
                break;
            }
            _ => {}
        }
    }
    Recorded {
        saved,
        skipped,
        failed,
        library,
        spool,
        music,
        _data: data,
    }
}

fn assert_saved_playlist(cache: CacheConfig) {
    let Recorded {
        saved,
        skipped,
        failed,
        library,
        spool,
        music,
        ..
    } = record_playlist(cache);
    assert!(failed.is_empty(), "{cache:?}: {failed:?}");
    assert_eq!(
        skipped,
        vec![SkipReason::Incomplete(PartialReason::StartedMidTrack)]
    );
    let mut titles: Vec<_> = saved.iter().map(|e| e.track.title.as_str()).collect();
    titles.sort();
    assert_eq!(titles, ["First", "Second"]);
    for entry in &saved {
        assert!(
            entry.duration_ms.abs_diff(8_000) <= 20,
            "{cache:?}: {} lasted {} ms",
            entry.track.title,
            entry.duration_ms
        );
        let path = music.path().join(entry.key.as_str());
        assert!(path.exists(), "{} missing", path.display());
    }
    assert!(
        music
            .path()
            .join("Artist/Album/Artist - First.flac")
            .exists()
    );
    assert_eq!(library.entries().len(), 2);
    assert_eq!(
        std::fs::read_dir(spool.path()).unwrap().count(),
        0,
        "spool files are cleaned up"
    );
}

#[test]
fn records_a_playlist_into_tagged_files() {
    assert_saved_playlist(CacheConfig::default());
}

#[test]
fn records_in_memory() {
    assert_saved_playlist(CacheConfig {
        mode: CacheMode::Memory,
        memory_limit: None,
        on_overflow: MemoryOverflow::Fail,
    });
}

#[test]
fn continues_on_disk_past_the_memory_limit() {
    // About 2.7 s of 48 kHz stereo audio fits; each 8 s track overflows.
    assert_saved_playlist(CacheConfig {
        mode: CacheMode::Memory,
        memory_limit: Some(1024 * 1024),
        on_overflow: MemoryOverflow::SpillToDisk,
    });
}

#[test]
fn fails_tracks_past_the_memory_limit() {
    let recorded = record_playlist(CacheConfig {
        mode: CacheMode::Memory,
        memory_limit: Some(1024 * 1024),
        on_overflow: MemoryOverflow::Fail,
    });
    assert!(recorded.saved.is_empty());
    assert_eq!(recorded.failed.len(), 3, "{:?}", recorded.failed);
    assert!(
        recorded.failed.iter().all(|e| e.contains("memory limit")),
        "{:?}",
        recorded.failed
    );
    assert_eq!(std::fs::read_dir(recorded.spool.path()).unwrap().count(), 0);
}
