//! SAI backend abstraction.
//!
//! Everything above this crate talks to the switch ASIC through the safe
//! [`SaiBackend`] trait. The `real-sai` feature provides the vendor-library
//! implementation (bindgen FFI + dlopen); the default `mock-sai` feature
//! provides a pure-Rust in-memory implementation so the whole stack builds
//! and tests without hardware or vendor blobs.
//!
//! Surface: switch create, port enumeration, port admin state, port
//! oper-status notifications, and the host-services L3 family — NETDEV
//! host interfaces, CPU punt traps, port router interfaces, and routes
//! (IP2ME + connected subnets). Transit routing orchestration (FRR,
//! neighbors, next-hop groups) arrives with hemlock-orch in later phases.

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

/// A SAI object id for a non-port object (hostif, router interface).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Oid(pub u64);

impl std::fmt::Display for Oid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

/// An IP destination prefix (`address`, `prefix length`).
pub type IpPrefix = (std::net::IpAddr, u8);

/// Where a route sends its packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteTarget {
    /// Punt to the CPU port (IP2ME: one of the switch's own addresses;
    /// the IP2ME hostif trap delivers it to the kernel).
    Cpu,
    /// A connected subnet via a router interface.
    Rif(Oid),
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
    /// Switch source MAC, passed as `SAI_SWITCH_ATTR_SRC_MAC_ADDRESS`.
    /// Resolved by syncd from the platform (ONIE syseeprom, management
    /// netdev). Optional in the SAI spec, but some vendor libraries have
    /// no working fallback — Broadcom's aborts create_switch on the E1031
    /// without it ("get local MAC address failed").
    pub src_mac: Option<[u8; 6]>,
    /// Enable the vendor diagnostic shell
    /// (`SAI_SWITCH_ATTR_SWITCH_SHELL_ENABLE`). Broadcom's SAI then runs
    /// its `BCM.0>` diag shell on the process's stdin/stdout — bench
    /// bring-up only (LED scan-chain probing, register pokes); never in
    /// the production service.
    pub diag_shell: bool,
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

/// Cumulative hardware counters for one port, as the ASIC reports them
/// (`sai_get_port_stats`). Counters a platform cannot supply stay 0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortCounters {
    pub in_octets: u64,
    pub in_ucast_pkts: u64,
    pub in_mcast_pkts: u64,
    pub in_bcast_pkts: u64,
    pub in_discards: u64,
    pub in_errors: u64,
    pub in_crc_errors: u64,
    pub in_alignment_errors: u64,
    pub in_symbol_errors: u64,
    pub in_runts: u64,
    pub in_giants: u64,
    pub in_pause: u64,
    pub out_octets: u64,
    pub out_ucast_pkts: u64,
    pub out_mcast_pkts: u64,
    pub out_bcast_pkts: u64,
    pub out_discards: u64,
    pub out_errors: u64,
    pub out_pause: u64,
    pub collisions: u64,
    pub late_collisions: u64,
    pub deferred: u64,
    /// RMON frame-size bins, EOS layout:
    /// 64 / 65-127 / 128-255 / 256-511 / 512-1023 / 1024-1522 / 1523-max.
    pub rx_bins: [u64; 7],
    pub tx_bins: [u64; 7],
}

/// One egress queue's stat counters. The label (UC0/MC0) is derived by
/// syncd from `unicast` + `index` against the platform definition.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueCounters {
    pub unicast: bool,
    pub index: u32,
    pub pkts: u64,
    pub bytes: u64,
    pub dropped_pkts: u64,
    pub dropped_bytes: u64,
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

    /// Cumulative hardware counters for one port. Polled by syncd's stats
    /// engine; must be cheap enough for a 5s all-ports sweep.
    fn port_counters(&mut self, port: PortId) -> Result<PortCounters, SaiError>;

    /// Per-egress-queue stat counters for one port. Backends without
    /// queue stat support return an empty list; syncd renders the
    /// platform-declared queues as zeros.
    fn port_queue_counters(&mut self, port: PortId) -> Result<Vec<QueueCounters>, SaiError>;

    /// Take the notification receiver. Yields `Some` on first call only.
    fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<SaiEvent>>;

    // --- Host-services L3 -----------------------------------------------

    /// Install the CPU punt path: ARP request/response copies and an
    /// IP2ME trap into the default trap group, delivered to the ingress
    /// port's netdev via a wildcard hostif table entry. Called once
    /// after `create_switch`.
    fn setup_host_punt(&mut self) -> Result<(), SaiError>;

    /// Create a NETDEV host interface for a port; the kernel sees a
    /// netdev called `name` (SAI caps it at 15 chars + NUL) that
    /// receives punted packets and transmits raw out the port.
    fn create_hostif(&mut self, port: PortId, name: &str) -> Result<Oid, SaiError>;

    /// Route a port: pull it out of the default 802.1Q bridge and
    /// create a router interface on the default virtual router.
    fn create_router_interface(&mut self, port: PortId) -> Result<Oid, SaiError>;

    /// Undo [`Self::create_router_interface`]: remove the RIF and
    /// restore default L2 bridging (bridge port + untagged default-VLAN
    /// membership + PVID).
    fn remove_router_interface(&mut self, port: PortId, rif: Oid) -> Result<(), SaiError>;

    /// Program a route on the default virtual router.
    fn create_route(&mut self, dest: IpPrefix, target: RouteTarget) -> Result<(), SaiError>;

    fn remove_route(&mut self, dest: IpPrefix) -> Result<(), SaiError>;
}
