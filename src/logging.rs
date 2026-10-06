//! Log to a daily-rotated file in the platform log directory (and stderr in debug builds).

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt as _, util::SubscriberInitExt as _};

/// Installs the global subscriber. Keep the guard alive for the life of the process.
pub fn init(dir: &Path) -> Option<WorkerGuard> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,gpui=warn,gpui_component=warn,lofty=error,ureq=warn")
    });
    let (file_layer, guard) = match std::fs::create_dir_all(dir) {
        Ok(()) => {
            let appender = tracing_appender::rolling::Builder::new()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix("getyourmusic")
                .filename_suffix("log")
                .max_log_files(7)
                .build(dir)
                .ok();
            match appender {
                Some(appender) => {
                    let (writer, guard) = tracing_appender::non_blocking(appender);
                    (
                        Some(fmt::layer().with_ansi(false).with_writer(writer)),
                        Some(guard),
                    )
                }
                None => (None, None),
            }
        }
        Err(_) => (None, None),
    };
    let stderr_layer = cfg!(debug_assertions).then(|| fmt::layer().with_writer(std::io::stderr));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init();
    guard
}
