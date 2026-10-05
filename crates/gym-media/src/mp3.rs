//! MP3 encoder built on LAME.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use mp3lame_encoder::{
    Bitrate, Builder, FlushGap, InterleavedPcm, MonoPcm, Quality, VbrMode, max_required_buffer_size,
};

use gym_core::encode::{
    AudioEncoder, EncodeError, EncodeSettings, LOSSY_SAMPLE_RATES, Mp3Quality, OutputFormat,
};
use gym_core::pcm::PcmReader;

const CHUNK_FRAMES: usize = 4096;

#[derive(Debug, Default)]
pub struct Mp3Encoder;

fn lame_error(error: impl std::fmt::Debug) -> EncodeError {
    EncodeError::Encoder(format!("lame: {error:?}"))
}

/// Appends LAME output written into the spare capacity of `buf`.
fn append(
    buf: &mut Vec<u8>,
    write: impl FnOnce(&mut [std::mem::MaybeUninit<u8>]) -> Result<usize, EncodeError>,
) -> Result<(), EncodeError> {
    let written = write(buf.spare_capacity_mut())?;
    // SAFETY: LAME initialized exactly `written` bytes of the spare capacity.
    unsafe { buf.set_len(buf.len() + written) };
    Ok(())
}

impl AudioEncoder for Mp3Encoder {
    fn format(&self) -> OutputFormat {
        OutputFormat::Mp3
    }

    fn supported_sample_rates(&self, _settings: &EncodeSettings) -> Option<&'static [u32]> {
        Some(LOSSY_SAMPLE_RATES)
    }

    fn encode(
        &self,
        input: &mut dyn PcmReader,
        settings: &EncodeSettings,
        output: &Path,
        progress: &mut dyn FnMut(f32),
    ) -> Result<(), EncodeError> {
        let spec = input.spec();
        if !(1..=2).contains(&spec.channels) {
            return Err(EncodeError::Unsupported(format!(
                "MP3 supports 1 or 2 channels, got {}",
                spec.channels
            )));
        }
        let mut builder = Builder::new().ok_or_else(|| lame_error("cannot allocate encoder"))?;
        builder
            .set_num_channels(spec.channels as u8)
            .map_err(lame_error)?;
        builder
            .set_sample_rate(spec.sample_rate)
            .map_err(lame_error)?;
        builder.set_quality(Quality::Best).map_err(lame_error)?;
        builder.set_to_write_vbr_tag(true).map_err(lame_error)?;
        match settings.mp3_quality {
            Mp3Quality::VbrV0 | Mp3Quality::VbrV2 => {
                builder.set_vbr_mode(VbrMode::Mtrh).map_err(lame_error)?;
                let quality = if settings.mp3_quality == Mp3Quality::VbrV0 {
                    Quality::Best
                } else {
                    Quality::NearBest
                };
                builder.set_vbr_quality(quality).map_err(lame_error)?;
            }
            cbr => {
                builder.set_vbr_mode(VbrMode::Off).map_err(lame_error)?;
                let bitrate = match cbr {
                    Mp3Quality::Cbr128 => Bitrate::Kbps128,
                    Mp3Quality::Cbr192 => Bitrate::Kbps192,
                    Mp3Quality::Cbr256 => Bitrate::Kbps256,
                    _ => Bitrate::Kbps320,
                };
                builder.set_brate(bitrate).map_err(lame_error)?;
            }
        }
        let mut encoder = builder.build().map_err(lame_error)?;

        let channels = spec.channels as usize;
        let total = input.total_frames().max(1) as f32;
        let mut file = BufWriter::new(File::create(output)?);
        let mut pcm = vec![0f32; CHUNK_FRAMES * channels];
        let mut mp3 = Vec::with_capacity(max_required_buffer_size(CHUNK_FRAMES));
        let mut frames_done = 0u64;
        loop {
            let n = input.read(&mut pcm)?;
            if n == 0 {
                break;
            }
            mp3.clear();
            mp3.reserve(max_required_buffer_size(n / channels));
            append(&mut mp3, |out| {
                if channels == 1 {
                    encoder.encode(MonoPcm(&pcm[..n]), out)
                } else {
                    encoder.encode(InterleavedPcm(&pcm[..n]), out)
                }
                .map_err(lame_error)
            })?;
            file.write_all(&mp3)?;
            frames_done += (n / channels) as u64;
            progress(frames_done as f32 / total);
        }
        mp3.clear();
        mp3.reserve(max_required_buffer_size(CHUNK_FRAMES));
        append(&mut mp3, |out| {
            encoder.flush::<FlushGap>(out).map_err(lame_error)
        })?;
        file.write_all(&mp3)?;

        // The first frame is reserved for the Xing/LAME tag (duration, gapless info).
        let mut tag = Vec::with_capacity(encoder.lame_tag_size());
        if encoder.lame_tag_encode_to_vec(&mut tag).is_some() {
            let mut file = file.into_inner().map_err(|e| e.into_error())?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&tag)?;
            file.sync_all()?;
        } else {
            file.flush()?;
            file.get_ref().sync_all()?;
        }
        progress(1.0);
        Ok(())
    }
}
