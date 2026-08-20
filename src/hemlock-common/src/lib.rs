//! Shared foundations for all Hemlock daemons and tools:
//! error conventions, tracing setup, and gRPC/IPC scaffolding.

pub mod error;
pub mod ipc;
pub mod logging;

pub use error::HemlockError;

/// Generated gRPC/protobuf types for the Hemlock daemon APIs.
pub mod proto {
    pub mod v1 {
        #![allow(clippy::all)]
        tonic::include_proto!("hemlock.v1");
    }
}
