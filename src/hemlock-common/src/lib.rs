//! Shared foundations for all Hemlock daemons and tools:
//! error conventions, tracing setup, and gRPC/IPC scaffolding.

pub mod error;
pub mod ipc;
pub mod logging;
pub mod net;

pub use error::HemlockError;

/// The Hemlock product version, sourced from the top-level `VERSION` file.
/// Everything user-visible (daemon/CLI `--version`, image names via
/// `build/mkimage.sh`, os-release) derives from that single file.
pub const VERSION: &str = env!("HEMLOCK_VERSION");

/// Generated gRPC/protobuf types for the Hemlock daemon APIs.
pub mod proto {
    pub mod v1 {
        #![allow(clippy::all)]
        tonic::include_proto!("hemlock.v1");
    }
}
