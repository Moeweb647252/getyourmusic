use std::path::Path;
use std::time::Duration;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;

use gym_core::capture::PcmSpec;
use gym_core::encode::{
    AudioEncoder, BitDepth, DEFAULT_FLAC_LEVEL, EncodeSettings, MAX_FLAC_LEVEL, Mp3Quality,
    OutputFormat,
};
use gym_core::model::{Artwork, PlayerInfo, TrackMetadata};
use gym_core::pcm::{SliceReader, conform};
use gym_core::tags::write_tags;
use gym_media::{FlacEncoder, Mp3Encoder};

fn tone(spec: PcmSpec, seconds: f32) -> Vec<f32> {
    let frames = (spec.sample_rate as f32 * seconds) as usize;
    (0..frames)
        .flat_map(|i| {
            let s =
                (i as f32 * 440.0 * std::f32::consts::TAU / spec.sample_rate as f32).sin() * 0.5;
            std::iter::repeat_n(s, spec.channels as usize)
        })
        .collect()
}

fn track() -> TrackMetadata {
    TrackMetadata {
        title: "Tone".into(),
        artist: Some("Oscillator".into()),
        album: Some("Test Signals".into()),
        artwork: Some(Artwork {
            mime_type: "image/png".into(),
            // A 1x1 transparent PNG.
            data: std::sync::Arc::from(
                &b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89\0\0\0\rIDATx\x9cc\xf8\x0f\0\0\x01\x01\0\x05\x18\xd8N\0\0\0\0IEND\xaeB`\x82"[..],
            ),
        }),
        ..Default::default()
    }
}

fn encode(encoder: &dyn AudioEncoder, settings: &EncodeSettings, spec: PcmSpec, path: &Path) {
    let rate = gym_core::encode::choose_sample_rate(
        spec.sample_rate,
        encoder.supported_sample_rates(settings),
    );
    let reader = Box::new(SliceReader::new(tone(spec, 3.0), spec));
    let mut reader = conform(reader, rate, encoder.max_channels()).unwrap();
    let mut last = 0.0;
    encoder
        .encode(&mut *reader, settings, path, &mut |p| {
            assert!(p >= last, "progress must not go backwards");
            last = p;
        })
        .unwrap();
    assert_eq!(last, 1.0);
    write_tags(path, &track(), &PlayerInfo::new("test", "Test Player")).unwrap();
}

fn assert_file(path: &Path, sample_rate: u32, bit_depth: Option<u8>) {
    let file = lofty::read_from_path(path).unwrap();
    let properties = file.properties();
    let duration = properties.duration();
    assert!(
        duration.abs_diff(Duration::from_secs(3)) < Duration::from_millis(60),
        "duration {duration:?}"
    );
    assert_eq!(properties.sample_rate(), Some(sample_rate));
    assert_eq!(properties.channels(), Some(2));
    if bit_depth.is_some() {
        assert_eq!(properties.bit_depth(), bit_depth);
    }
    let tag = file.primary_tag().expect("tag written");
    assert_eq!(tag.title().as_deref(), Some("Tone"));
    assert_eq!(tag.artist().as_deref(), Some("Oscillator"));
    assert_eq!(tag.album().as_deref(), Some("Test Signals"));
    assert_eq!(tag.pictures().len(), 1);
}

#[test]
fn flac_round_trip_24_bit_hi_res() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.flac");
    let spec = PcmSpec {
        sample_rate: 96_000,
        channels: 2,
    };
    let settings = EncodeSettings {
        format: OutputFormat::Flac,
        bit_depth: BitDepth::Bits24,
        ..Default::default()
    };
    encode(&FlacEncoder, &settings, spec, &path);
    assert_file(&path, 96_000, Some(24));
}

#[test]
fn flac_round_trip_16_bit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.flac");
    let spec = PcmSpec {
        sample_rate: 44_100,
        channels: 2,
    };
    let settings = EncodeSettings {
        format: OutputFormat::Flac,
        bit_depth: BitDepth::Bits16,
        ..Default::default()
    };
    encode(&FlacEncoder, &settings, spec, &path);
    assert_file(&path, 44_100, Some(16));
}

#[test]
fn flac_levels_make_valid_files_that_never_grow() {
    let dir = tempfile::tempdir().unwrap();
    let spec = PcmSpec {
        sample_rate: 44_100,
        channels: 2,
    };
    // 24-bit, so no random dither makes sizes differ between runs.
    let sizes: Vec<u64> = [0, DEFAULT_FLAC_LEVEL, 6, 7, MAX_FLAC_LEVEL]
        .into_iter()
        .map(|level| {
            let path = dir.path().join(format!("t{level}.flac"));
            let settings = EncodeSettings {
                format: OutputFormat::Flac,
                bit_depth: BitDepth::Bits24,
                flac_level: level,
                ..Default::default()
            };
            encode(&FlacEncoder, &settings, spec, &path);
            assert_file(&path, 44_100, Some(24));
            std::fs::metadata(&path).unwrap().len()
        })
        .collect();
    assert!(sizes.windows(2).all(|w| w[1] <= w[0]), "{sizes:?}");
}

#[test]
fn mp3_resamples_hi_res_input() {
    let dir = tempfile::tempdir().unwrap();
    for quality in [Mp3Quality::Cbr320, Mp3Quality::VbrV0] {
        let path = dir.path().join(format!("t-{quality:?}.mp3"));
        let spec = PcmSpec {
            sample_rate: 96_000,
            channels: 2,
        };
        let settings = EncodeSettings {
            format: OutputFormat::Mp3,
            mp3_quality: quality,
            ..Default::default()
        };
        encode(&Mp3Encoder, &settings, spec, &path);
        assert_file(&path, 48_000, None);
    }
}
