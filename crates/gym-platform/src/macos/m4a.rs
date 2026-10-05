//! AAC and ALAC in an MPEG-4 container, encoded by Apple's AudioToolbox.

use std::ffi::c_void;
use std::path::Path;
use std::ptr::{self, NonNull};

use objc2_audio_toolbox::{
    AudioConverterRef, AudioConverterSetProperty, AudioFileFlags, ExtAudioFileCreateWithURL,
    ExtAudioFileDispose, ExtAudioFileGetProperty, ExtAudioFileRef, ExtAudioFileSetProperty,
    ExtAudioFileWrite, kAudioConverterCodecQuality, kAudioConverterEncodeBitRate,
    kAudioConverterQuality_Max, kAudioFileM4AType, kExtAudioFileProperty_AudioConverter,
    kExtAudioFileProperty_ClientDataFormat, kExtAudioFileProperty_ConverterConfig,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription,
    kAppleLosslessFormatFlag_16BitSourceData, kAppleLosslessFormatFlag_24BitSourceData,
    kAudioFormatAppleLossless, kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM, kAudioFormatMPEG4AAC,
};
use objc2_core_foundation::CFURL;

use gym_core::encode::{
    AudioEncoder, BitDepth, EncodeError, EncodeSettings, LOSSY_SAMPLE_RATES, M4aCodec, OutputFormat,
};
use gym_core::pcm::PcmReader;

const CHUNK_FRAMES: usize = 4096;
/// ALAC is specified up to 384 kHz.
const ALAC_SAMPLE_RATES: &[u32] = &[
    8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400,
    192_000, 352_800, 384_000,
];

#[derive(Debug, Default)]
pub struct M4aEncoder;

/// Formats an `OSStatus`, showing four-character codes where possible.
fn os_error(context: &str, status: i32) -> EncodeError {
    let bytes = status.to_be_bytes();
    let code = if bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        format!("'{}'", String::from_utf8_lossy(&bytes))
    } else {
        status.to_string()
    };
    EncodeError::Encoder(format!("{context} failed: {code}"))
}

fn check(context: &str, status: i32) -> Result<(), EncodeError> {
    if status == 0 {
        Ok(())
    } else {
        Err(os_error(context, status))
    }
}

/// Disposes the ExtAudioFile on every exit path.
struct FileGuard(ExtAudioFileRef);

impl FileGuard {
    fn close(mut self) -> Result<(), EncodeError> {
        let file = std::mem::replace(&mut self.0, ptr::null_mut());
        // SAFETY: `file` is a valid ExtAudioFileRef created by ExtAudioFileCreateWithURL.
        check("ExtAudioFileDispose", unsafe { ExtAudioFileDispose(file) })
    }
}

impl Drop for FileGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: as above; only reached when `close` was not called.
            unsafe { ExtAudioFileDispose(self.0) };
        }
    }
}

impl AudioEncoder for M4aEncoder {
    fn format(&self) -> OutputFormat {
        OutputFormat::M4a
    }

    fn supported_sample_rates(&self, settings: &EncodeSettings) -> Option<&'static [u32]> {
        Some(match settings.m4a_codec {
            M4aCodec::Aac => LOSSY_SAMPLE_RATES,
            M4aCodec::Alac => ALAC_SAMPLE_RATES,
        })
    }

    fn encode(
        &self,
        input: &mut dyn PcmReader,
        settings: &EncodeSettings,
        output: &Path,
        progress: &mut dyn FnMut(f32),
    ) -> Result<(), EncodeError> {
        let spec = input.spec();
        let channels = spec.channels as u32;
        let file_format = match settings.m4a_codec {
            M4aCodec::Aac => AudioStreamBasicDescription {
                mSampleRate: spec.sample_rate as f64,
                mFormatID: kAudioFormatMPEG4AAC,
                mFormatFlags: 0,
                mBytesPerPacket: 0,
                mFramesPerPacket: 0,
                mBytesPerFrame: 0,
                mChannelsPerFrame: channels,
                mBitsPerChannel: 0,
                mReserved: 0,
            },
            M4aCodec::Alac => AudioStreamBasicDescription {
                mSampleRate: spec.sample_rate as f64,
                mFormatID: kAudioFormatAppleLossless,
                mFormatFlags: match settings.bit_depth {
                    BitDepth::Bits16 => kAppleLosslessFormatFlag_16BitSourceData,
                    BitDepth::Bits24 => kAppleLosslessFormatFlag_24BitSourceData,
                },
                mBytesPerPacket: 0,
                mFramesPerPacket: 4096,
                mBytesPerFrame: 0,
                mChannelsPerFrame: channels,
                mBitsPerChannel: 0,
                mReserved: 0,
            },
        };
        let client_format = AudioStreamBasicDescription {
            mSampleRate: spec.sample_rate as f64,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: 4 * channels,
            mFramesPerPacket: 1,
            mBytesPerFrame: 4 * channels,
            mChannelsPerFrame: channels,
            mBitsPerChannel: 32,
            mReserved: 0,
        };

        let url = CFURL::from_file_path(output)
            .ok_or_else(|| EncodeError::Encoder(format!("invalid path {}", output.display())))?;
        let mut file: ExtAudioFileRef = ptr::null_mut();
        // SAFETY: all pointers reference live stack values for the duration of the call.
        check("ExtAudioFileCreateWithURL", unsafe {
            ExtAudioFileCreateWithURL(
                &url,
                kAudioFileM4AType,
                NonNull::from(&file_format),
                ptr::null(),
                AudioFileFlags::EraseFile.0,
                NonNull::from(&mut file),
            )
        })?;
        let guard = FileGuard(file);

        // SAFETY: `client_format` outlives the call and the size matches its type.
        check("set client format", unsafe {
            ExtAudioFileSetProperty(
                file,
                kExtAudioFileProperty_ClientDataFormat,
                size_of::<AudioStreamBasicDescription>() as u32,
                NonNull::from(&client_format).cast(),
            )
        })?;
        if settings.m4a_codec == M4aCodec::Aac {
            configure_aac(file, settings.aac_bitrate.bits_per_second());
        }

        let total = input.total_frames().max(1) as f32;
        let mut buffer = vec![0f32; CHUNK_FRAMES * channels as usize];
        let mut frames_done = 0u64;
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            let frames = (n / channels as usize) as u32;
            let mut list = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer {
                    mNumberChannels: channels,
                    mDataByteSize: (n * size_of::<f32>()) as u32,
                    mData: buffer.as_mut_ptr().cast(),
                }],
            };
            // SAFETY: `list` points at `n` initialized samples in `buffer`.
            check("ExtAudioFileWrite", unsafe {
                ExtAudioFileWrite(file, frames, NonNull::from(&mut list))
            })?;
            frames_done += frames as u64;
            progress(frames_done as f32 / total);
        }
        guard.close()?;
        progress(1.0);
        Ok(())
    }
}

/// Sets the AAC bitrate and best quality; failures fall back to encoder defaults.
fn configure_aac(file: ExtAudioFileRef, bitrate: u32) {
    let mut converter: AudioConverterRef = ptr::null_mut();
    let mut size = size_of::<AudioConverterRef>() as u32;
    // SAFETY: `converter` and `size` are valid out-pointers of the right size.
    let status = unsafe {
        ExtAudioFileGetProperty(
            file,
            kExtAudioFileProperty_AudioConverter,
            NonNull::from(&mut size),
            NonNull::from(&mut converter).cast(),
        )
    };
    if status != 0 || converter.is_null() {
        tracing::warn!(status, "cannot access the AAC converter; using defaults");
        return;
    }
    let quality = kAudioConverterQuality_Max;
    // SAFETY: the converter belongs to `file`; values are u32 as the properties require.
    unsafe {
        let status = AudioConverterSetProperty(
            converter,
            kAudioConverterCodecQuality,
            size_of::<u32>() as u32,
            NonNull::from(&quality).cast(),
        );
        if status != 0 {
            tracing::warn!(status, "cannot set AAC quality");
        }
        let status = AudioConverterSetProperty(
            converter,
            kAudioConverterEncodeBitRate,
            size_of::<u32>() as u32,
            NonNull::from(&bitrate).cast(),
        );
        if status != 0 {
            tracing::warn!(status, bitrate, "AAC bitrate not applicable; using default");
        }
        // Applying the converter settings requires resetting the converter config.
        let config: *const c_void = ptr::null();
        let status = ExtAudioFileSetProperty(
            file,
            kExtAudioFileProperty_ConverterConfig,
            size_of::<*const c_void>() as u32,
            NonNull::from(&config).cast(),
        );
        if status != 0 {
            tracing::warn!(status, "cannot apply AAC converter settings");
        }
    }
}
