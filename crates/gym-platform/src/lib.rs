//! Operating system integrations behind the `gym_core::platform::Platform` seam.
//!
//! macOS is fully implemented. Other systems get a stub that compiles and reports features
//! as unavailable, so the rest of the application can be developed and tested anywhere.

mod cpal_capture;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(not(target_os = "macos"))]
mod unsupported;

use std::sync::Arc;

use gym_core::platform::Platform;

pub use cpal_capture::CpalCapture;

/// The platform implementation for the operating system this binary was built for.
pub fn current() -> Arc<dyn Platform> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(macos::MacPlatform::new())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(unsupported::UnsupportedPlatform::new())
    }
}
