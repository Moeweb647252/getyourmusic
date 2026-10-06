//! FLAC encoder built on `flacenc`, streaming frames to disk.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, StreamInfo};
use flacenc::config;
use flacenc::error::{Verified, Verify};
use flacenc::source::{Context, Fill, FrameBuf};

use gym_core::encode::{AudioEncoder, EncodeError, EncodeSettings, MAX_FLAC_LEVEL, OutputFormat};
use gym_core::pcm::{PcmReader, Quantizer};

const BLOCK_SIZE: usize = 4096;
/// `fLaC` + metadata block header + STREAMINFO body.
const HEADER_LEN: usize = 4 + 4 + 34;

#[derive(Debug, Default)]
pub struct FlacEncoder;

fn encoder_error(error: impl std::fmt::Debug) -> EncodeError {
    EncodeError::Encoder(format!("flac: {error:?}"))
}

/// Encoder configurations for a compression level; each frame keeps the smallest result.
///
/// flacenc has no presets, and the best LPC order depends on the material (a higher order can
/// make a file bigger). Levels up to 5 (flacenc's default) raise a single order; above that each
/// level tries a superset of the orders below it, so it never produces a bigger file.
fn level_configs(level: u8) -> Result<Vec<Verified<config::Encoder>>, EncodeError> {
    let lpc_orders: &[usize] = match level.min(MAX_FLAC_LEVEL) {
        0 => &[],
        1 => &[2],
        2 => &[4],
        3 => &[6],
        4 => &[8],
        5 => &[10],
        6 => &[10, 12],
        7 => &[8, 10, 12, 16],
        _ => &[6, 8, 10, 12, 16, 20, 24],
    };
    let verify =
        |config: config::Encoder| config.into_verified().map_err(|(_, e)| encoder_error(e));
    if lpc_orders.is_empty() {
        // Fixed predictors only.
        let mut config = config::Encoder::default();
        config.subframe_coding.use_lpc = false;
        return Ok(vec![verify(config)?]);
    }
    lpc_orders
        .iter()
        .map(|&order| {
            let mut config = config::Encoder::default();
            config.subframe_coding.qlpc.lpc_order = order;
            verify(config)
        })
        .collect()
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

        let configs = level_configs(settings.flac_level)?;
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
            let encode = |config| {
                flacenc::encode_fixed_size_frame(config, &fill.0, frame_number, &info)
                    .map_err(encoder_error)
            };
            let mut frame = encode(&configs[0])?;
            for config in &configs[1..] {
                let candidate = encode(config)?;
                if candidate.count_bits() < frame.count_bits() {
                    frame = candidate;
                }
            }
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
