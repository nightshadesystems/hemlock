//! Shared port state between the SAI actor and the gRPC service.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

use hemlock_platform::PortDef;
use hemlock_sai::{Oid, PortId, StormClass};

/// One front-panel port: manifest definition + live ASIC state + the
/// operator-facing attributes syncd tracks (description, L3 mode).
#[derive(Debug, Clone)]
pub struct PortState {
    pub def: PortDef,
    pub sai_id: PortId,
    pub admin_up: bool,
    pub oper_up: bool,
    pub description: String,
    /// Present when the port is routed (has an address).
    pub l3: Option<L3State>,
    /// Present when the port has explicit switchport config.
    pub switchport: Option<SwitchportState>,
    /// Storm-control levels programmed on the port, keyed by class.
    pub storm: BTreeMap<StormClass, StormState>,
    /// Errdisable cause (`bpduguard`, ...); the port is admin-down
    /// while set.
    pub errdisable_reason: Option<String>,
}

/// One programmed storm-control level: the operator's percent (two
/// decimals) plus the rate derived from the port speed at programming
/// time (re-derived on speed change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StormState {
    pub level: String,
    pub kbps: u64,
}

/// A routed port's L3 objects: its router interface and the address
/// whose IP2ME + subnet routes are programmed.
#[derive(Debug, Clone)]
pub struct L3State {
    pub rif: Oid,
    /// The interface address in CIDR form.
    pub address: String,
}

/// A port's L2 switchport program, as applied: the intent plus the live
/// non-default VLAN memberships it produced.
#[derive(Debug, Clone, Default)]
pub struct SwitchportState {
    pub trunk: bool,
    /// QinQ tunnel port (access-like membership; the access VLAN is the
    /// S-VLAN, and the port's TPID mode is switched).
    pub dot1q_tunnel: bool,
    /// 0 = default VLAN.
    pub access_vlan: u16,
    pub trunk_vlans: Vec<u16>,
    /// 0 = default VLAN.
    pub native_vlan: u16,
    /// (vlan id, member oid, tagged); default-VLAN membership is not
    /// tracked here (the backend owns it, idempotently).
    pub members: Vec<(u16, Oid, bool)>,
}

/// One created VLAN. `oid` is `None` for the default VLAN (it always
/// exists; only its display name is tracked).
#[derive(Debug, Clone)]
pub struct VlanState {
    pub oid: Option<Oid>,
    pub name: String,
    /// `state suspend`: the VLAN exists but forwards nothing.
    pub suspended: bool,
    /// Present when the VLAN has an SVI (a VLAN router interface with
    /// an address).
    pub l3: Option<L3State>,
}

/// VLAN table keyed by 802.1Q id, shared via `Arc<RwLock<...>>`.
pub type SharedVlans = Arc<RwLock<std::collections::BTreeMap<u16, VlanState>>>;

#[derive(Debug, Clone, Copy)]
pub struct SwitchMeta {
    pub oid: u64,
}

/// Port table keyed by port name, shared via `Arc<RwLock<...>>`.
pub type SharedPorts = Arc<RwLock<HashMap<String, PortState>>>;

/// The software mirror of the hardware FDB plus the static entries the
/// config programmed, shared between the event pump and the service.
#[derive(Debug)]
pub struct FdbTable {
    /// Dynamic-entry aging in seconds (0 = no aging).
    pub aging_secs: u32,
    /// Dynamic entries learned from SAI FDB events, keyed by
    /// (vlan, colon-lowercase mac).
    pub dynamics: BTreeMap<(u16, String), FdbDynamicEntry>,
    /// Static entries, same key; None port = drop entry.
    pub statics: BTreeMap<(u16, String), FdbStaticEntry>,
}

impl Default for FdbTable {
    fn default() -> Self {
        Self {
            aging_secs: 300,
            dynamics: BTreeMap::new(),
            statics: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FdbDynamicEntry {
    pub port: String,
    /// Times the entry changed ports (first learn counts as move 1,
    /// matching the EOS Moves column).
    pub moves: u32,
    pub last_move: Option<std::time::Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FdbStaticEntry {
    /// None = drop entry.
    pub port: Option<String>,
}

pub type SharedFdb = Arc<RwLock<FdbTable>>;

/// One mirror session as programmed: destination, its SAI object, and
/// the attached sources.
#[derive(Debug, Clone)]
pub struct MirrorState {
    pub destination: String,
    pub oid: Oid,
    /// Source port -> mirrored direction ("rx" | "tx" | "both").
    pub sources: BTreeMap<String, MirrorDir>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorDir {
    Rx,
    Tx,
    Both,
}

/// Mirror sessions keyed by operator-visible session id.
pub type SharedMirrors = Arc<RwLock<BTreeMap<u32, MirrorState>>>;

/// One port-channel as programmed: its port-like SAI id, members with
/// their collect/distribute gates, and the port-shaped state the LAG
/// carries (switchport program, storm levels).
#[derive(Debug, Clone)]
pub struct LagState {
    pub group: u16,
    pub sai_id: PortId,
    pub description: String,
    pub admin_up: bool,
    /// Member port name -> (member oid, gate).
    pub members: BTreeMap<String, LagMemberState>,
    pub switchport: Option<SwitchportState>,
    pub storm: BTreeMap<StormClass, StormState>,
}

#[derive(Debug, Clone)]
pub struct LagMemberState {
    pub oid: Oid,
    pub enabled: bool,
}

/// Port-channels keyed by group number.
pub type SharedLags = Arc<RwLock<BTreeMap<u16, LagState>>>;

/// One created MST instance and its VLAN mapping.
#[derive(Debug, Clone)]
pub struct StpInstanceState {
    pub oid: Oid,
    pub vlans: Vec<u16>,
}

/// MST instances keyed by instance number (1..15; 0 is the
/// always-present default instance).
pub type SharedStps = Arc<RwLock<BTreeMap<u8, StpInstanceState>>>;

/// One L2MC output group backing a (vlan, group-IP) forwarding entry.
#[derive(Debug, Clone)]
pub struct L2mcGroupState {
    pub oid: Oid,
    /// Output port name -> member oid.
    pub members: BTreeMap<String, Oid>,
}

/// Snooping-programmed multicast groups keyed by (vlan, group IP,
/// canonical text form).
pub type SharedL2mc = Arc<RwLock<BTreeMap<(u16, String), L2mcGroupState>>>;

/// Per-VLAN unknown-multicast restriction: the L2MC group holding the
/// mrouter set.
pub type SharedUnknownMcast = Arc<RwLock<BTreeMap<u16, L2mcGroupState>>>;

/// The transit FIB as programmed by orch: per-prefix targets plus the
/// deduplicated, refcounted next-hop and ECMP-group objects backing
/// them. Connected/IP2ME routes ride the interface-address path and are
/// not tracked here.
#[derive(Debug, Default)]
pub struct FibTable {
    /// Installed transit routes keyed by canonical prefix text.
    pub routes: BTreeMap<String, FibRoute>,
    /// Deduplicated next hops keyed by (rif, next-hop ip text):
    /// (next-hop oid, reference count).
    pub next_hops: HashMap<(Oid, String), (Oid, u32)>,
    /// Deduplicated ECMP groups keyed by their sorted member next-hop
    /// oid set.
    pub groups: HashMap<Vec<Oid>, FibGroup>,
    /// Neighbor entries keyed by (interface, ip text) -> (rif, mac).
    pub neighbors: BTreeMap<(String, String), (Oid, String)>,
    /// My-MAC entries keyed by (vlan, colon mac) -> oid (vlan 0 =
    /// unscoped).
    pub my_macs: BTreeMap<(u16, String), Oid>,
}

/// One installed transit route: what it targets and which shared
/// objects it holds references on.
#[derive(Debug, Clone)]
pub struct FibRoute {
    /// The (rif, ip) next-hop keys this route holds references on
    /// (empty for punt and drop routes).
    pub hop_keys: Vec<(Oid, String)>,
    /// The sorted member-oid group key, when ECMP.
    pub group_key: Option<Vec<Oid>>,
}

/// One ECMP group: its oid, member objects, and how many routes use it.
#[derive(Debug, Clone)]
pub struct FibGroup {
    pub oid: Oid,
    pub members: Vec<Oid>,
    pub refs: u32,
}

pub type SharedFib = Arc<RwLock<FibTable>>;

/// Resolve a SAI port id back to a port name (for event handling).
pub fn name_for(ports: &HashMap<String, PortState>, id: PortId) -> Option<String> {
    ports
        .iter()
        .find(|(_, p)| p.sai_id == id)
        .map(|(name, _)| name.clone())
}
