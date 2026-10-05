//! Output formats, encoder settings and the encoder abstraction.

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::pcm::PcmReader;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    Flac,
    Mp3,
    M4a,
}

impl OutputFormat {
    pub const ALL: [OutputFormat; 3] = [OutputFormat::Flac, OutputFormat::Mp3, OutputFormat::M4a];

    pub fn extension(self) -> &'static str {
        match self {
            OutputFormat::Flac => "flac",
            OutputFormat::Mp3 => "mp3",
            OutputFormat::M4a => "m4a",
        }
    }
}

/// Sample resolution for lossless formats (FLAC and ALAC).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BitDepth {
    Bits16,
    #[default]
    Bits24,
}

impl BitDepth {
    pub fn bits(self) -> u32 {
        match self {
            BitDepth::Bits16 => 16,
            BitDepth::Bits24 => 24,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mp3Quality {
    Cbr128,
    Cbr192,
    Cbr256,
    #[default]
    Cbr320,
    /// LAME VBR V2 (~190 kbps).
    VbrV2,
    /// LAME VBR V0 (~245 kbps).
    VbrV0,
}

impl Mp3Quality {
    pub const ALL: [Mp3Quality; 6] = [
        Mp3Quality::Cbr128,
        Mp3Quality::Cbr192,
        Mp3Quality::Cbr256,
        Mp3Quality::Cbr320,
        Mp3Quality::VbrV2,
        Mp3Quality::VbrV0,
    ];
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum M4aCodec {
    #[default]
    Aac,
    Alac,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AacBitrate {
    Kbps128,
    Kbps192,
    #[default]
    Kbps256,
    Kbps320,
}

impl AacBitrate {
    pub const ALL: [AacBitrate; 4] = [
        AacBitrate::Kbps128,
        AacBitrate::Kbps192,
        AacBitrate::Kbps256,
        AacBitrate::Kbps320,
    ];

    pub fn bits_per_second(self) -> u32 {
        match self {
            AacBitrate::Kbps128 => 128_000,
            AacBitrate::Kbps192 => 192_000,
            AacBitrate::Kbps256 => 256_000,
            AacBitrate::Kbps320 => 320_000,
        }
    }
}

/// Everything an encoder needs to know about the requested output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct EncodeSettings {
    pub format: OutputFormat,
    /// Used by FLAC and ALAC.
    pub bit_depth: BitDepth,
    pub mp3_quality: Mp3Quality,
    pub m4a_codec: M4aCodec,
    pub aac_bitrate: AacBitrate,
}

impl Default for EncodeSettings {
    fn default() -> Self {
        Self {
            format: OutputFormat::Flac,
            bit_depth: BitDepth::default(),
            mp3_quality: Mp3Quality::default(),
            m4a_codec: M4aCodec::default(),
            aac_bitrate: AacBitrate::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("no encoder available for {0:?} on this platform")]
    Unavailable(OutputFormat),
    #[error("unsupported input: {0}")]
    Unsupported(String),
    #[error("encoder failure: {0}")]
    Encoder(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Sample rates commonly accepted by lossy codecs (MPEG-1/2 Layer III, AAC-LC).
pub const LOSSY_SAMPLE_RATES: &[u32] = &[
    8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000,
];

/// Encodes PCM into one [`OutputFormat`].
pub trait AudioEncoder: Send + Sync {
    fn format(&self) -> OutputFormat;

    /// Sample rates the encoder accepts with `settings`, or `None` for any rate.
    fn supported_sample_rates(&self, _settings: &EncodeSettings) -> Option<&'static [u32]> {
        None
    }

    /// Maximum number of channels the encoder accepts.
    fn max_channels(&self) -> u16 {
        2
    }

    /// Encodes all of `input` into a new file at `output`, reporting progress in `0.0..=1.0`.
    fn encode(
        &self,
        input: &mut dyn PcmReader,
        settings: &EncodeSettings,
        output: &Path,
        progress: &mut dyn FnMut(f32),
    ) -> Result<(), EncodeError>;
}

/// Picks the output sample rate for an encoder, preferring `requested` and the same rate family.
pub fn choose_sample_rate(requested: u32, supported: Option<&[u32]>) -> u32 {
    let Some(rates) = supported.filter(|r| !r.is_empty()) else {
        return requested;
    };
    if rates.contains(&requested) {
        return requested;
    }
    let family = if requested.is_multiple_of(11_025) {
        11_025
    } else {
        8_000
    };
    let best_below = |filter: &dyn Fn(u32) -> bool| {
        rates
            .iter()
            .copied()
            .filter(|&r| r <= requested && filter(r))
            .max()
    };
    best_below(&|r| r % family == 0)
        .or_else(|| best_below(&|_| true))
        .unwrap_or_else(|| *rates.iter().max().unwrap())
}

/// The set of encoders available on this platform.
#[derive(Clone, Default)]
pub struct EncoderRegistry {
    encoders: Vec<Arc<dyn AudioEncoder>>,
}

impl EncoderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an encoder; a later registration for the same format replaces the earlier one.
    pub fn register(&mut self, encoder: Arc<dyn AudioEncoder>) {
        self.encoders.retain(|e| e.format() != encoder.format());
        self.encoders.push(encoder);
    }

    pub fn get(&self, format: OutputFormat) -> Option<Arc<dyn AudioEncoder>> {
        self.encoders.iter().find(|e| e.format() == format).cloned()
    }

    pub fn is_available(&self, format: OutputFormat) -> bool {
        self.get(format).is_some()
    }

    /// Available formats in canonical order.
    pub fn formats(&self) -> Vec<OutputFormat> {
        OutputFormat::ALL
            .into_iter()
            .filter(|f| self.is_available(*f))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_supported_rates() {
        assert_eq!(choose_sample_rate(44_100, Some(LOSSY_SAMPLE_RATES)), 44_100);
        assert_eq!(choose_sample_rate(96_000, None), 96_000);
    }

    #[test]
    fn steps_down_within_the_rate_family() {
        assert_eq!(choose_sample_rate(96_000, Some(LOSSY_SAMPLE_RATES)), 48_000);
        assert_eq!(choose_sample_rate(88_200, Some(LOSSY_SAMPLE_RATES)), 44_100);
        assert_eq!(
            choose_sample_rate(192_000, Some(LOSSY_SAMPLE_RATES)),
            48_000
        );
    }

    #[test]
    fn odd_rates_fall_back_to_the_closest_lower_rate() {
        assert_eq!(choose_sample_rate(50_000, Some(LOSSY_SAMPLE_RATES)), 48_000);
    }

    #[test]
    fn rates_below_every_supported_rate_use_the_highest() {
        assert_eq!(choose_sample_rate(4_000, Some(LOSSY_SAMPLE_RATES)), 48_000);
    }
}
