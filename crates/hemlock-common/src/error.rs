//! Error conventions.
//!
//! Library crates define precise error enums with `thiserror`; binaries use
//! `anyhow` at their edges. `HemlockError` is the small set of cross-cutting
//! failures shared by more than one crate.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum HemlockError {
    #[error("i/o error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("ipc failure: {0}")]
    Ipc(String),

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
}

impl HemlockError {
    /// Attach a path to an `std::io::Error`, which alone never says *what* failed.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
