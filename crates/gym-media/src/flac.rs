//! FLAC encoder built on `flacenc`, streaming frames to disk.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, StreamInfo};
use flacenc::error::Verify;
use flacenc::source::{Context, Fill, FrameBuf};

use gym_core::encode::{AudioEncoder, EncodeError, EncodeSettings, OutputFormat};
use gym_core::pcm::{PcmReader, Quantizer};

const BLOCK_SIZE: usize = 4096;
/// `fLaC` + metadata block header + STREAMINFO body.
const HEADER_LEN: usize = 4 + 4 + 34;

#[derive(Debug, Default)]
pub struct FlacEncoder;

fn encoder_error(error: impl std::fmt::Debug) -> EncodeError {
    EncodeError::Encoder(format!("flac: {error:?}"))
}

/// Serializes the stream header with STREAMINFO as the only (last) metadata block.
fn header_bytes(info: &StreamInfo) -> Result<Vec<u8>, EncodeError> {
    let mut sink = ByteSink::new();
    info.write(&mut sink).map_err(encoder_error)?;
    let body = sink.as_slice();
    debug_assert_eq!(body.len(), 34);
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(b"fLaC");
    header.push(0x80); // last-metadata-block flag | type 0 (STREAMINFO)
    header.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    header.extend_from_slice(body);
    Ok(header)
}

impl AudioEncoder for FlacEncoder {
    fn format(&self) -> OutputFormat {
        OutputFormat::Flac
    }

    fn max_channels(&self) -> u16 {
        8
    }

    fn encode(
        &self,
        input: &mut dyn PcmReader,
        settings: &EncodeSettings,
        output: &Path,
        progress: &mut dyn FnMut(f32),
    ) -> Result<(), EncodeError> {
        let spec = input.spec();
        let channels = spec.channels as usize;
        let bits = settings.bit_depth.bits() as usize;
        let total = input.total_frames().max(1) as f32;

        let config = flacenc::config::Encoder::default()
            .into_verified()
            .map_err(|(_, e)| encoder_error(e))?;
        let mut info =
            StreamInfo::new(spec.sample_rate as usize, channels, bits).map_err(encoder_error)?;
        let mut fill = (
            FrameBuf::with_size(channels, BLOCK_SIZE).map_err(encoder_error)?,
            Context::new(bits, channels),
        );

        let mut file = BufWriter::new(File::create(output)?);
        file.write_all(&[0u8; HEADER_LEN])?;

        let mut quantizer = Quantizer::new(bits as u32);
        let mut floats = vec![0f32; BLOCK_SIZE * channels];
        let mut ints = Vec::with_capacity(BLOCK_SIZE * channels);
        let mut sink = ByteSink::new();
        let mut frames_done = 0u64;
        loop {
            // Fill a whole block; readers may return less than requested.
            let mut filled = 0;
            while filled < floats.len() {
                let n = input.read(&mut floats[filled..])?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            ints.clear();
            ints.extend(floats[..filled].iter().map(|&s| quantizer.quantize(s)));
            fill.fill_interleaved(&ints).map_err(encoder_error)?;

            let frame_number = fill.1.current_frame_number().unwrap_or(0);
            let frame = flacenc::encode_fixed_size_frame(&config, &fill.0, frame_number, &info)
                .map_err(encoder_error)?;
            info.update_frame_info(&frame);
            sink.clear();
            frame.write(&mut sink).map_err(encoder_error)?;
            file.write_all(sink.as_slice())?;

            frames_done += (filled / channels) as u64;
            progress(frames_done as f32 / total);
            if filled < floats.len() {
                break;
            }
        }

        info.set_block_sizes(BLOCK_SIZE, BLOCK_SIZE)
            .map_err(encoder_error)?;
        info.set_total_samples(fill.1.total_samples());
        info.set_md5_digest(&fill.1.md5_digest());
        let mut file = file.into_inner().map_err(|e| e.into_error())?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header_bytes(&info)?)?;
        file.sync_all()?;
        progress(1.0);
        Ok(())
    }
}
