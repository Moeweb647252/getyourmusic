//! Platform-independent core of GetYourMusic.
//!
//! The core defines the domain model and every abstraction the application depends on:
//! [`now_playing::NowPlayingSource`], [`capture::AudioCaptureBackend`],
//! [`encode::AudioEncoder`], [`storage::StorageProvider`] and [`platform::Platform`].
//! Concrete implementations live in `gym-media` (portable encoders) and `gym-platform`
//! (operating system integrations). The [`engine`] wires them into a recording session.

pub mod capture;
pub mod encode;
pub mod engine;
pub mod library;
pub mod model;
pub mod naming;
pub mod now_playing;
pub mod pcm;
pub mod platform;
pub mod settings;
pub mod storage;
pub mod tags;

#[cfg(any(test, feature = "testing"))]
pub mod testing;
