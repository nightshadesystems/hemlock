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
    /// The default 802.1Q VLAN's object id, so FDB events on it (their
    /// `bv_id`) can be mapped back to VLAN 1.
    pub default_vlan_oid: u64,
}

/// What the platform's SAI supports, probed once at `create_switch`.
/// The pinned Helix4 libsaibcm (8.4.50.0 / SAI v1.11.0) predates several
/// optional families; a commit that needs an absent capability must fail
/// cleanly, never silently no-op.
#[derive(Debug, Clone, Copy)]
pub struct SaiCapabilities {
    pub lag: bool,
    pub stp: bool,
    pub fdb_flush: bool,
    pub fdb_aging: bool,
    pub l2mc: bool,
    pub storm_control: bool,
    pub mirror: bool,
    /// Sessions the ASIC can host concurrently (0 = unsupported).
    pub mirror_sessions_max: u32,
    pub port_tpid: bool,
}

impl SaiCapabilities {
    /// Everything supported — the mock's default posture.
    pub fn all() -> Self {
        Self {
            lag: true,
            stp: true,
            fdb_flush: true,
            fdb_aging: true,
            l2mc: true,
            storm_control: true,
            mirror: true,
            mirror_sessions_max: 4,
            port_tpid: true,
        }
    }
}

/// Storm-control traffic class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StormClass {
    Broadcast,
    Multicast,
    UnknownUnicast,
}

/// Where a static FDB entry sends its frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdbAction {
    Forward(PortId),
    Drop,
}

/// What happened to a dynamic FDB entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdbEventKind {
    Learned,
    Aged,
    Moved,
    Flushed,
}

/// A port's forwarding state within one spanning-tree instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StpPortState {
    Blocking,
    Learning,
    Forwarding,
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
    PortOperStatus {
        port: PortId,
        up: bool,
    },
    /// A dynamic FDB entry changed. `bv_id` is the raw bridge/VLAN
    /// object id from the notification; syncd maps it back to a VLAN
    /// number (via [`SwitchInfo::default_vlan_oid`] and its VLAN table).
    Fdb {
        kind: FdbEventKind,
        bv_id: u64,
        mac: [u8; 6],
        /// The entry's port; None on age/flush without one.
        port: Option<PortId>,
    },
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

    /// Create an SVI: a VLAN-type router interface on the default
    /// virtual router. `vlan` is the VLAN's object id; `None` means the
    /// default VLAN. The VLAN keeps bridging; only frames addressed to
    /// the RIF MAC enter L3.
    fn create_vlan_router_interface(&mut self, vlan: Option<Oid>) -> Result<Oid, SaiError>;

    /// Undo [`Self::create_vlan_router_interface`]. Unlike a port RIF
    /// there is no bridge state to restore.
    fn remove_vlan_router_interface(&mut self, rif: Oid) -> Result<(), SaiError>;

    /// Undo [`Self::create_router_interface`]: remove the RIF and
    /// restore default L2 bridging (bridge port + untagged default-VLAN
    /// membership + PVID).
    fn remove_router_interface(&mut self, port: PortId, rif: Oid) -> Result<(), SaiError>;

    /// Program a route on the default virtual router.
    fn create_route(&mut self, dest: IpPrefix, target: RouteTarget) -> Result<(), SaiError>;

    fn remove_route(&mut self, dest: IpPrefix) -> Result<(), SaiError>;

    // --- L2 VLANs ---------------------------------------------------------

    /// Create a VLAN (802.1Q id 2..=4094; 1 is the default VLAN, which
    /// always exists).
    fn create_vlan(&mut self, vlan_id: u16) -> Result<Oid, SaiError>;

    /// Remove a VLAN; its members must already be gone.
    fn remove_vlan(&mut self, vlan: Oid) -> Result<(), SaiError>;

    /// Add `port` as a tagged or untagged member of `vlan`. The port
    /// must be bridged (not routed).
    fn add_vlan_member(&mut self, vlan: Oid, port: PortId, tagged: bool) -> Result<Oid, SaiError>;

    fn remove_vlan_member(&mut self, member: Oid) -> Result<(), SaiError>;

    /// Ingress VLAN classification of untagged frames (PVID).
    fn set_port_pvid(&mut self, port: PortId, vlan_number: u16) -> Result<(), SaiError>;

    /// Take the port out of the default VLAN — the first step of any
    /// non-default switchport program. Idempotent.
    fn remove_port_default_vlan(&mut self, port: PortId) -> Result<(), SaiError>;

    /// Put the port back to default L2: untagged default-VLAN member
    /// with matching PVID. Idempotent.
    fn restore_port_default_vlan(&mut self, port: PortId) -> Result<(), SaiError>;

    // --- Capability probe --------------------------------------------------

    /// What this SAI supports. Valid after `create_switch`.
    fn capabilities(&mut self) -> Result<SaiCapabilities, SaiError>;

    // --- MAC address table (FDB) -------------------------------------------

    /// Dynamic-entry aging time in seconds; 0 disables aging.
    fn set_fdb_aging(&mut self, secs: u32) -> Result<(), SaiError>;

    /// Install a static FDB entry. `vlan` is the VLAN's object id
    /// (None = the default VLAN). Replaces an existing entry for the
    /// same (vlan, mac).
    fn add_fdb_entry(
        &mut self,
        vlan: Option<Oid>,
        mac: [u8; 6],
        action: FdbAction,
    ) -> Result<(), SaiError>;

    fn remove_fdb_entry(&mut self, vlan: Option<Oid>, mac: [u8; 6]) -> Result<(), SaiError>;

    /// Flush dynamic FDB entries, optionally scoped to a VLAN and/or a
    /// port. Static entries stay.
    fn flush_fdb(&mut self, vlan: Option<Oid>, port: Option<PortId>) -> Result<(), SaiError>;

    // --- Storm control -----------------------------------------------------

    /// Meter a traffic class on a port to `kbps`; None removes the
    /// policer. The backend owns policer object lifecycle.
    fn set_port_storm_control(
        &mut self,
        port: PortId,
        class: StormClass,
        kbps: Option<u64>,
    ) -> Result<(), SaiError>;

    /// Cumulative packets a storm-control policer dropped (red-marked);
    /// 0 when no policer is attached.
    fn port_storm_drops(&mut self, port: PortId, class: StormClass) -> Result<u64, SaiError>;

    // --- Port mirroring ----------------------------------------------------

    /// Create a local (SPAN) mirror session towards `monitor`.
    fn create_mirror_session(&mut self, monitor: PortId) -> Result<Oid, SaiError>;

    /// Remove a mirror session; ports must be detached first.
    fn remove_mirror_session(&mut self, session: Oid) -> Result<(), SaiError>;

    /// Attach a port's ingress/egress mirroring to a session (None
    /// detaches that direction).
    fn set_port_mirror(
        &mut self,
        port: PortId,
        ingress: Option<Oid>,
        egress: Option<Oid>,
    ) -> Result<(), SaiError>;

    // --- QinQ --------------------------------------------------------------

    /// The port's outer TPID. 0x8100 is the default; a dot1q-tunnel
    /// port carries 0x88a8 so customer 0x8100 tags ride through as
    /// payload.
    fn set_port_tpid(&mut self, port: PortId, tpid: u16) -> Result<(), SaiError>;

    // --- Link aggregation --------------------------------------------------

    /// Create a LAG. The returned id is port-like: the VLAN-membership
    /// and PVID calls accept it (the backend fronts the LAG with its
    /// own bridge port and dispatches PVID to the LAG attribute).
    fn create_lag(&mut self) -> Result<PortId, SaiError>;

    /// Remove a LAG; its members and VLAN memberships must be gone.
    fn remove_lag(&mut self, lag: PortId) -> Result<(), SaiError>;

    /// Add `port` to `lag`. The port leaves the 802.1Q bridge (its
    /// traffic rides the LAG's bridge port); the member starts gated
    /// closed (not collecting/distributing).
    fn add_lag_member(&mut self, lag: PortId, port: PortId) -> Result<Oid, SaiError>;

    /// Remove a member and restore the port's standalone default L2
    /// bridging.
    fn remove_lag_member(&mut self, member: Oid, port: PortId) -> Result<(), SaiError>;

    /// The collect/distribute gate: an enabled member forwards, a
    /// disabled one only speaks LACP.
    fn set_lag_member_state(&mut self, member: Oid, enabled: bool) -> Result<(), SaiError>;

    // --- Spanning tree -----------------------------------------------------

    /// Create an STP instance (an MST instance beyond the always-present
    /// default one).
    fn create_stp_instance(&mut self) -> Result<Oid, SaiError>;

    /// Remove an STP instance; its VLANs must have moved elsewhere.
    fn remove_stp_instance(&mut self, stp: Oid) -> Result<(), SaiError>;

    /// Assign a VLAN (None = the default VLAN) to an STP instance
    /// (None = the default instance).
    fn set_vlan_stp_instance(
        &mut self,
        vlan: Option<Oid>,
        stp: Option<Oid>,
    ) -> Result<(), SaiError>;

    /// A (port-like) object's forwarding state within an instance
    /// (None = the default instance). The backend owns the stp-port
    /// object lifecycle.
    fn set_stp_port_state(
        &mut self,
        stp: Option<Oid>,
        port: PortId,
        state: StpPortState,
    ) -> Result<(), SaiError>;

    // --- L2 multicast (IGMP/MLD snooping) ----------------------------------

    /// Create an empty L2MC output group.
    fn create_l2mc_group(&mut self) -> Result<Oid, SaiError>;

    /// Remove an L2MC group; members and referencing entries must be
    /// gone.
    fn remove_l2mc_group(&mut self, group: Oid) -> Result<(), SaiError>;

    /// Add a (port-like) output to an L2MC group.
    fn add_l2mc_member(&mut self, group: Oid, port: PortId) -> Result<Oid, SaiError>;

    fn remove_l2mc_member(&mut self, member: Oid) -> Result<(), SaiError>;

    /// Point a VLAN's (*, G) forwarding entry at an L2MC group (None
    /// removes the entry).
    fn set_l2mc_entry(
        &mut self,
        vlan: Option<Oid>,
        group_ip: std::net::IpAddr,
        l2mc: Option<Oid>,
    ) -> Result<(), SaiError>;

    /// Where a VLAN floods unknown multicast: None = everywhere (the
    /// default); Some(group) = only that L2MC group (snooping sends it
    /// to the mrouter ports).
    fn set_vlan_unknown_mcast_group(
        &mut self,
        vlan: Option<Oid>,
        l2mc: Option<Oid>,
    ) -> Result<(), SaiError>;
}
