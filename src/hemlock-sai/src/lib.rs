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

    #[error("SAI {call} failed with status {status} ({})", status_name(*.status))]
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
    /// Extra SAI profile key/values (`[sai.profile]` in the manifest),
    /// e.g. `SAI_NUM_ECMP_MEMBERS`.
    pub profile: Vec<(String, String)>,
}

/// Human name for a SAI status code, per saistatus.h (`SAI_STATUS_CODE(x)`
/// is `-(x)`; attribute-indexed families encode the attr index in the low
/// 16 bits). For log/error readability during hardware bring-up.
pub fn status_name(status: i32) -> String {
    if status == 0 {
        return "SUCCESS".into();
    }
    let magnitude = -(status as i64);
    if !(1..=0x0005_FFFF).contains(&magnitude) {
        return format!("unknown status {status:#x}");
    }
    const SIMPLE: [&str; 0x18] = [
        "FAILURE",
        "NOT_SUPPORTED",
        "NO_MEMORY",
        "INSUFFICIENT_RESOURCES",
        "INVALID_PARAMETER",
        "ITEM_ALREADY_EXISTS",
        "ITEM_NOT_FOUND",
        "BUFFER_OVERFLOW",
        "INVALID_PORT_NUMBER",
        "INVALID_PORT_MEMBER",
        "INVALID_VLAN_ID",
        "UNINITIALIZED",
        "TABLE_FULL",
        "MANDATORY_ATTRIBUTE_MISSING",
        "NOT_IMPLEMENTED",
        "ADDR_NOT_FOUND",
        "OBJECT_IN_USE",
        "INVALID_OBJECT_TYPE",
        "INVALID_OBJECT_ID",
        "INVALID_NV_STORAGE",
        "NV_STORAGE_FULL",
        "SW_UPGRADE_VERSION_MISMATCH",
        "NOT_EXECUTED",
        "STAGE_MISMATCH",
    ];
    let family = magnitude >> 16;
    let index = magnitude & 0xFFFF;
    match family {
        0 => SIMPLE
            .get(magnitude as usize - 1)
            .map(|name| (*name).to_string())
            .unwrap_or_else(|| format!("unknown status {status:#x}")),
        1 => format!("INVALID_ATTRIBUTE_{index}"),
        2 => format!("INVALID_ATTR_VALUE_{index}"),
        3 => format!("ATTR_NOT_IMPLEMENTED_{index}"),
        4 => format!("UNKNOWN_ATTRIBUTE_{index}"),
        5 => format!("ATTR_NOT_SUPPORTED_{index}"),
        _ => format!("unknown status {status:#x}"),
    }
}

#[cfg(test)]
mod status_tests {
    #[test]
    fn decodes_statuses() {
        use super::status_name;
        assert_eq!(status_name(0), "SUCCESS");
        assert_eq!(status_name(-1), "FAILURE");
        assert_eq!(status_name(-3), "NO_MEMORY");
        assert_eq!(status_name(-0x18), "STAGE_MISMATCH");
        assert_eq!(status_name(-0x0001_0000), "INVALID_ATTRIBUTE_0");
        assert_eq!(status_name(-0x0001_0002), "INVALID_ATTRIBUTE_2");
        assert_eq!(status_name(-0x0005_0001), "ATTR_NOT_SUPPORTED_1");
        assert!(status_name(-0x0100_0000).starts_with("unknown"));
    }
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
