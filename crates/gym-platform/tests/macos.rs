#![cfg(target_os = "macos")]

use std::time::Duration;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;

use gym_core::capture::PcmSpec;
use gym_core::encode::{
    AacBitrate, AudioEncoder, BitDepth, EncodeSettings, M4aCodec, OutputFormat, choose_sample_rate,
};
use gym_core::model::{PlayerInfo, TrackMetadata};
use gym_core::now_playing::NowPlayingEvent;
use gym_core::pcm::{SliceReader, conform};
use gym_core::tags::write_tags;
use gym_platform::macos::{AdapterNowPlaying, M4aEncoder};

fn tone(spec: PcmSpec, seconds: f32) -> Vec<f32> {
    let frames = (spec.sample_rate as f32 * seconds) as usize;
    (0..frames)
        .flat_map(|i| {
            let s =
                (i as f32 * 440.0 * std::f32::consts::TAU / spec.sample_rate as f32).sin() * 0.5;
            [s, s]
        })
        .collect()
}

fn encode_m4a(settings: EncodeSettings, expected_rate: u32) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.m4a");
    let spec = PcmSpec {
        sample_rate: 96_000,
        channels: 2,
    };
    let rate = choose_sample_rate(
        spec.sample_rate,
        M4aEncoder.supported_sample_rates(&settings),
    );
    let reader = Box::new(SliceReader::new(tone(spec, 3.0), spec));
    let mut reader = conform(reader, rate, M4aEncoder.max_channels()).unwrap();
    M4aEncoder
        .encode(&mut *reader, &settings, &path, &mut |_| {})
        .unwrap();
    let track = TrackMetadata {
        title: "Tone".into(),
        artist: Some("Oscillator".into()),
        ..Default::default()
    };
    write_tags(&path, &track, &PlayerInfo::new("t", "Test")).unwrap();

    let file = lofty::read_from_path(&path).unwrap();
    let properties = file.properties();
    assert!(
        properties.duration().abs_diff(Duration::from_secs(3)) < Duration::from_millis(60),
        "duration {:?}",
        properties.duration()
    );
    assert_eq!(properties.sample_rate(), Some(expected_rate));
    assert_eq!(properties.channels(), Some(2));
    let tag = file.primary_tag().unwrap();
    assert_eq!(tag.title().as_deref(), Some("Tone"));
    assert_eq!(tag.artist().as_deref(), Some("Oscillator"));
}

#[test]
fn aac_downsamples_hi_res_input() {
    encode_m4a(
        EncodeSettings {
            format: OutputFormat::M4a,
            m4a_codec: M4aCodec::Aac,
            aac_bitrate: AacBitrate::Kbps256,
            ..Default::default()
        },
        48_000,
    );
}

#[test]
fn alac_keeps_hi_res_input() {
    encode_m4a(
        EncodeSettings {
            format: OutputFormat::M4a,
            m4a_codec: M4aCodec::Alac,
            bit_depth: BitDepth::Bits24,
            ..Default::default()
        },
        96_000,
    );
}

/// Requires media playing in some app. Run with `cargo test -- --ignored`.
#[test]
#[ignore]
fn adapter_reports_the_current_player() {
    use gym_core::now_playing::NowPlayingSource;
    let source = AdapterNowPlaying::locate().unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    let _subscription = source.start(tx).unwrap();
    // The helper first reports an empty state, then the current item.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while let Ok(event) = rx.recv_deadline(deadline) {
        if let NowPlayingEvent::Updated(np) = event {
            println!(
                "{} ({}) — {} playing={} elapsed={:?} artwork={}",
                np.player.name,
                np.player.id,
                np.track.display_name(),
                np.playing,
                np.elapsed,
                np.track.artwork.is_some()
            );
            return;
        }
    }
    panic!("no media is playing; start playback in any app and retry");
}

/// Lists capture sources. Run with `cargo test -- --ignored --nocapture`.
#[test]
#[ignore]
fn lists_capture_sources() {
    let platform = gym_platform::current();
    for source in platform.capture_backend().list_sources().unwrap() {
        println!("{source:?}");
    }
}
