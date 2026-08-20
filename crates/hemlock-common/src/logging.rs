//! Tracing initialization shared by every Hemlock binary.

use tracing_subscriber::{fmt, EnvFilter};

/// Initialize tracing for a daemon or CLI binary.
///
/// Filter resolution order: `HEMLOCK_LOG` env var, then the provided default
/// (e.g. `"info"`). Daemons log to stderr so stdout stays clean for tooling.
pub fn init(default_filter: &str) {
    let filter =
        EnvFilter::try_from_env("HEMLOCK_LOG").unwrap_or_else(|_| EnvFilter::new(default_filter));
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
