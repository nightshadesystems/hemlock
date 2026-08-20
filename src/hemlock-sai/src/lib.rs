//! SAI backend abstraction.
//!
//! Everything above this crate talks to the switch ASIC through the safe
//! [`SaiBackend`] trait. The `real-sai` feature provides the vendor-library
//! implementation (bindgen FFI + dlopen); the default `mock-sai` feature
//! provides a pure-Rust in-memory implementation so the whole stack builds
//! and tests without hardware or vendor blobs.
//!
//! Phase 1 surface: switch create, port enumeration, port admin state, and
//! port oper-status notifications. L2/L3 object families arrive with
//! hemlock-orch in later phases.

#[cfg(feature = "mock-sai")]
pub mod mock;

#[cfg(feature = "real-sai")]
mod ffi;
#[cfg(feature = "real-sai")]
pub mod vendor;

use std::path::PathBuf;

use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum SaiError {
    #[error("SAI library load failed: {0}")]
    Load(String),

    #[error("SAI {call} failed with status {status}")]
    Status { call: &'static str, status: i32 },

    #[error("switch not created yet")]
    NoSwitch,

    #[error("unknown port {0:?}")]
    UnknownPort(PortId),

    #[error("{0}")]
    Other(String),
}

/// A SAI object id for a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PortId(pub u64);

impl std::fmt::Display for PortId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

/// What a backend needs to bring the switch up. Resolved by syncd from the
/// platform manifest; the backend knows nothing else about the board.
#[derive(Debug, Clone)]
pub struct SwitchInit {
    /// Vendor library to load (real backend only).
    pub libsai_path: PathBuf,
    /// ASIC init config handed to the vendor library via the SAI profile
    /// (`SAI_INIT_CONFIG_FILE`).
    pub config_bcm_path: PathBuf,
}

#[derive(Debug, Clone, Copy)]
pub struct SwitchInfo {
    pub oid: u64,
}

/// A port as the ASIC reports it. Correlated with the platform port table
/// by lane set (SAI creates ports from config.bcm, in its own order).
#[derive(Debug, Clone)]
pub struct SaiPort {
    pub id: PortId,
    pub lanes: Vec<u32>,
    pub speed_mbps: u32,
    pub admin_up: bool,
    pub oper_up: bool,
}

/// Asynchronous SAI notifications, delivered off the vendor callback thread.
#[derive(Debug, Clone)]
pub enum SaiEvent {
    PortOperStatus { port: PortId, up: bool },
}

/// The safe wrapper trait over a SAI implementation.
///
/// Calls are synchronous (SAI itself is); syncd serializes access and moves
/// blocking calls off the async executor. Implementations must be `Send` so
/// the backend can live in a dedicated task.
pub trait SaiBackend: Send {
    /// Human-readable identity, e.g. `mock` or `vendor:/usr/lib/libsai.so.1`.
    fn name(&self) -> String;

    /// Initialize the SAI library and create the switch object. Must be
    /// called exactly once, before any other call.
    fn create_switch(&mut self) -> Result<SwitchInfo, SaiError>;

    /// Enumerate the ports the switch created from its init config.
    fn ports(&mut self) -> Result<Vec<SaiPort>, SaiError>;

    fn set_port_admin_state(&mut self, port: PortId, up: bool) -> Result<(), SaiError>;

    /// Take the notification receiver. Yields `Some` on first call only.
    fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<SaiEvent>>;
}
