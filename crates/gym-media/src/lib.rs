//! Portable audio encoders.

mod flac;
mod mp3;

use std::sync::Arc;

use gym_core::encode::EncoderRegistry;

pub use flac::FlacEncoder;
pub use mp3::Mp3Encoder;

/// Registers every portable encoder.
pub fn register_encoders(registry: &mut EncoderRegistry) {
    registry.register(Arc::new(FlacEncoder));
    registry.register(Arc::new(Mp3Encoder));
}
