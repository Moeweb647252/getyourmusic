//! Presentation helpers: human-readable durations, sizes, formats and states.

use std::time::Duration;

use gpui_kit::SharedString;

use gym_core::capture::{CaptureSource, CaptureSourceKind};
use gym_core::encode::{EncodeSettings, M4aCodec, Mp3Quality, OutputFormat};
use gym_core::engine::{PartialReason, SkipReason};

/// `m:ss`, or `h:mm:ss` from one hour.
pub fn duration(d: Duration) -> SharedString {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}").into()
    } else {
        format!("{m}:{s:02}").into()
    }
}

/// Decimal (SI) byte sizes, as Finder shows them.
pub fn bytes(size: u64) -> SharedString {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B").into()
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit]).into()
    } else {
        format!("{value:.0} {}", UNITS[unit]).into()
    }
}

pub fn date_time(at: chrono::DateTime<chrono::Utc>) -> SharedString {
    at.with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
        .into()
}

pub fn format_name(format: OutputFormat) -> SharedString {
    match format {
        OutputFormat::Flac => tr!("format.flac"),
        OutputFormat::Mp3 => tr!("format.mp3"),
        OutputFormat::M4a => tr!("format.m4a"),
    }
}

pub fn mp3_kbps(quality: Mp3Quality) -> Option<u32> {
    match quality {
        Mp3Quality::Cbr128 => Some(128),
        Mp3Quality::Cbr192 => Some(192),
        Mp3Quality::Cbr256 => Some(256),
        Mp3Quality::Cbr320 => Some(320),
        Mp3Quality::VbrV0 | Mp3Quality::VbrV2 => None,
    }
}

/// e.g. "FLAC · 24-bit", "AAC · 256 kbps", "MP3 · VBR V0".
pub fn encoding(settings: &EncodeSettings) -> SharedString {
    let bits = settings.bit_depth.bits();
    match settings.format {
        OutputFormat::Flac => tr!("format.flac_detail", bits = bits),
        OutputFormat::M4a => match settings.m4a_codec {
            M4aCodec::Alac => tr!("format.alac_detail", bits = bits),
            M4aCodec::Aac => tr!(
                "format.aac_detail",
                kbps = settings.aac_bitrate.bits_per_second() / 1000
            ),
        },
        OutputFormat::Mp3 => match mp3_kbps(settings.mp3_quality) {
            Some(kbps) => tr!("format.mp3_cbr_detail", kbps = kbps),
            None => tr!(
                "format.mp3_vbr_detail",
                level = if settings.mp3_quality == Mp3Quality::VbrV0 {
                    "V0"
                } else {
                    "V2"
                }
            ),
        },
    }
}

pub fn source_label(source: &CaptureSource) -> SharedString {
    match source.kind {
        CaptureSourceKind::OutputLoopback => tr!("source.loopback", name = source.name),
        CaptureSourceKind::Input => tr!("source.input", name = source.name),
    }
}

/// A full sentence explaining why a track is incomplete.
pub fn partial_reason(reason: PartialReason) -> SharedString {
    match reason {
        PartialReason::StartedMidTrack => tr!("track.reason.started_mid_track"),
        PartialReason::Interrupted => tr!("track.reason.interrupted"),
        PartialReason::EndedEarly => tr!("track.reason.ended_early"),
        PartialReason::StoppedEarly => tr!("track.reason.stopped_early"),
    }
}

pub fn skip_reason(reason: SkipReason) -> SharedString {
    match reason {
        SkipReason::Incomplete(partial) => partial_reason(partial),
        SkipReason::TooShort => tr!("track.reason.too_short"),
        SkipReason::Silent => tr!("track.reason.silent"),
        SkipReason::AlreadyExists => tr!("track.reason.exists"),
    }
}

/// The notification shown when a track is not saved, or `None` when it is not worth one.
pub fn skip_message(reason: SkipReason, title: &str) -> Option<SharedString> {
    Some(match reason {
        SkipReason::Incomplete(PartialReason::StartedMidTrack) => {
            tr!("toast.skipped_started_mid_track", title = title)
        }
        SkipReason::Incomplete(PartialReason::Interrupted) => {
            tr!("toast.skipped_interrupted", title = title)
        }
        SkipReason::Incomplete(PartialReason::EndedEarly) => {
            tr!("toast.skipped_ended_early", title = title)
        }
        SkipReason::Incomplete(PartialReason::StoppedEarly) => {
            tr!("toast.skipped_stopped_early", title = title)
        }
        SkipReason::Silent => tr!("toast.skipped_silent", title = title),
        SkipReason::AlreadyExists => tr!("toast.skipped_exists", title = title),
        SkipReason::TooShort => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration(Duration::from_secs(5)), "0:05");
        assert_eq!(duration(Duration::from_secs(245)), "4:05");
        assert_eq!(duration(Duration::from_secs(3_725)), "1:02:05");
    }

    #[test]
    fn sizes() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1_500), "1.5 KB");
        assert_eq!(bytes(34_200_000), "34 MB");
    }
}
