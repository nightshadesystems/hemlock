//! Shared port state between the SAI actor and the gRPC service.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, RwLock};

use hemlock_common::link::Duplex;
use hemlock_platform::PortDef;
use hemlock_sai::{
    AclFamily, AclFields, AclPacketAction, AclStage, Oid, PolicerSpec, PolicerStats, PortId,
    QosMapType, SchedulerSpec, StormClass, WredSpec,
};

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
    /// Operator-pinned link parameters; unset fields run at the
    /// platform default from `def`.
    pub link: LinkConfig,
}

impl PortState {
    /// The rate to derive percent-relative rates from and to report:
    /// the pinned speed when the operator forced one, otherwise the
    /// platform's definition for the port.
    pub fn speed_mbps(&self) -> u32 {
        self.link.speed_mbps.unwrap_or(self.def.speed_mbps)
    }

    /// Auto-negotiation as programmed, falling back to the manifest's
    /// declaration for the port.
    pub fn autoneg(&self) -> bool {
        self.link.autoneg.unwrap_or(self.def.autoneg)
    }

    /// The port's duplex for display. Nothing forced = full, which is
    /// what every rate above 100M runs at anyway.
    pub fn duplex(&self) -> Duplex {
        self.link.duplex.unwrap_or(Duplex::Full)
    }
}

/// Link parameters an operator pinned on a port. `None` everywhere is
/// the boot state: the ASIC runs whatever config.bcm created and the
/// hostif netdev keeps the KNET default MTU.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkConfig {
    /// Pinned rate in Mb/s; None = not pinned.
    pub speed_mbps: Option<u32>,
    /// Forced duplex; None = not forced.
    pub duplex: Option<Duplex>,
    /// Auto-negotiation as programmed; None = platform default.
    pub autoneg: Option<bool>,
    /// L2 MTU as programmed; None = platform default.
    pub mtu: Option<u32>,
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

/// The ACL world: user ACL programs and bindings as pushed by mgmtd,
/// feature-internal entries (dot1x enforcement, DAI/DHCP-snooping
/// redirects), and the per-(port, stage) hardware tables they
/// materialize into.
///
/// # Priority bands
///
/// Each (port, stage) table holds one flat priority space (higher
/// wins). syncd partitions it so internal entries always beat user
/// rules and nothing beats the implicit deny from below:
///
/// - internal entries: `2_000_000_000 - seq` — dot1x enforcement and
///   snooping/DAI redirects; a user rule can never shadow them,
/// - user rules: `1_000_000_000 - ordinal` (first rule by number =
///   ordinal 0),
/// - the implicit deny: priority `1`, with its own match counter.
#[derive(Debug, Default)]
pub struct AclWorld {
    /// User ACLs keyed by name.
    pub acls: BTreeMap<String, AclProgram>,
    /// Bindings keyed by (display name — a port or a Port-Channel,
    /// stage) -> ACL name. Port-Channel bindings expand to their
    /// members at materialization.
    pub bindings: BTreeMap<(String, AclStage), String>,
    /// Internal entries per (physical port, stage). dot1x enforcement
    /// outranks the snooping/DAI redirects, which outrank user rules.
    pub internal: BTreeMap<(String, AclStage), InternalAcl>,
    /// Materialized hardware tables per (physical port, stage).
    pub tables: BTreeMap<(String, AclStage), PortAclTable>,
    /// `clear acl counters` baselines keyed by counter oid.
    pub counter_base: HashMap<u64, u64>,
}

/// One user ACL as pushed by `EnsureAcl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclProgram {
    pub family: AclFamily,
    pub rules: BTreeMap<u32, AclRuleState>,
}

/// One user rule: match fields plus the operator-facing action set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclRuleState {
    pub permit: bool,
    pub log: bool,
    pub fields: AclFields,
    pub police: Option<PolicerSpec>,
}

/// One feature-internal entry (dot1x EAPOL permit / deny-all, snooping
/// and DAI CPU redirects).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalAclEntry {
    pub fields: AclFields,
    pub action: AclPacketAction,
}

/// One (port, stage)'s internal entries, per owning feature.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InternalAcl {
    pub dot1x: Vec<InternalAclEntry>,
    pub snoop: Vec<InternalAclEntry>,
}

impl InternalAcl {
    pub fn is_empty(&self) -> bool {
        self.dot1x.is_empty() && self.snoop.is_empty()
    }

    /// Band order: dot1x first (highest priority), then snooping/DAI.
    pub fn ordered(&self) -> impl Iterator<Item = &InternalAclEntry> {
        self.dot1x.iter().chain(self.snoop.iter())
    }
}

/// One materialized (port, stage) table and its live entry objects.
#[derive(Debug)]
pub struct PortAclTable {
    pub table: Oid,
    pub family: AclFamily,
    /// The user ACL contributing this table's user band, when bound.
    pub user_acl: Option<String>,
    pub entries: BTreeMap<AclEntryKey, AclEntryObjs>,
}

/// Entry identity within one (port, stage) table. The variant order is
/// the band order (internal above user above the implicit deny).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AclEntryKey {
    Internal(u32),
    User(u32),
    ImplicitDeny,
}

/// The SAI objects behind one entry, plus what was programmed so a
/// re-materialization can leave untouched rules (and their counters)
/// alone.
#[derive(Debug, Clone)]
pub struct AclEntryObjs {
    pub entry: Oid,
    pub counter: Option<Oid>,
    pub policer: Option<Oid>,
    pub priority: u32,
    pub fields: AclFields,
    pub action: AclPacketAction,
    pub police: Option<PolicerSpec>,
}

pub type SharedAcls = Arc<RwLock<AclWorld>>;

/// CoPP: per-class trap group + policer state. Class names and
/// membership are the compiled table in `service.rs`; this holds the
/// live objects and configured overrides.
#[derive(Debug, Default)]
pub struct CoppState {
    pub classes: BTreeMap<&'static str, CoppClassState>,
}

/// One CoPP class's live state.
#[derive(Debug, Clone, Default)]
pub struct CoppClassState {
    /// Effective rate/burst (pps/pkts).
    pub rate: u32,
    pub burst: u32,
    /// Rate or burst overridden by config (renders `*` in `show copp`).
    pub overridden: bool,
    pub policer: Option<Oid>,
    /// None for the `default` class (it polices the switch's default
    /// trap group instead of owning one). Ownership records — the
    /// class table lives for the switch's lifetime.
    #[allow(dead_code)]
    pub group: Option<Oid>,
    #[allow(dead_code)]
    pub traps: Vec<Oid>,
    /// `clear copp counters` baseline.
    pub base: PolicerStats,
}

pub type SharedCopp = Arc<RwLock<CoppState>>;

/// Port-security runtime state for one enabled port.
#[derive(Debug, Clone)]
pub struct PortSecurityState {
    pub max: u32,
    /// Violation action: shutdown (errdisable) vs protect (drop only).
    pub shutdown: bool,
    /// Learned secure MACs -> learn time.
    pub learned: BTreeMap<String, std::time::Instant>,
    pub violations: u32,
    pub last_violation: Option<(String, std::time::Instant)>,
}

/// Port-security state keyed by port name.
pub type SharedPortSecurity = Arc<RwLock<BTreeMap<String, PortSecurityState>>>;

/// The QoS world: the four global maps and their SAI objects, the named
/// WRED profiles, the per-port programs mgmtd pushed, and what is
/// actually programmed on each physical port.
///
/// # Object dedup
///
/// Scheduler and WRED objects are shared, refcounted resources exactly
/// like the FIB's next-hop groups: two ports asking for the same queue
/// shape get one SAI scheduler between them, and the object is freed
/// when the last queue unbinds. Keying is by value (a [`SchedulerSpec`]
/// / a profile name), so an edit that leaves a queue's shape unchanged
/// touches no hardware.
#[derive(Debug, Default)]
pub struct QosWorld {
    /// The global maps as pushed by `SetQosMaps`.
    pub maps: QosMaps,
    /// The SAI map object behind each non-empty map table.
    pub map_objects: BTreeMap<QosMapType, Oid>,
    /// Named WRED profiles, keyed by name.
    pub wred_profiles: BTreeMap<String, WredProfileState>,
    /// Per-port programs as pushed, keyed by the display name mgmtd
    /// used — a physical port or a Port-Channel.
    pub programs: BTreeMap<String, PortQosProgram>,
    /// What each *physical* port currently has programmed (a
    /// Port-Channel program expands to its members).
    pub applied: BTreeMap<String, AppliedPortQos>,
    /// Deduplicated scheduler profiles: spec -> (object, refcount).
    pub schedulers: BTreeMap<SchedulerSpec, (Oid, u32)>,
}

/// The four global QoS map tables (`value -> value`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QosMaps {
    pub dscp_to_tc: BTreeMap<u8, u8>,
    pub cos_to_tc: BTreeMap<u8, u8>,
    pub tc_to_dscp: BTreeMap<u8, u8>,
    pub tc_to_cos: BTreeMap<u8, u8>,
}

impl QosMaps {
    /// One table by map type.
    pub fn table(&self, kind: QosMapType) -> &BTreeMap<u8, u8> {
        match kind {
            QosMapType::DscpToTc => &self.dscp_to_tc,
            QosMapType::Dot1pToTc => &self.cos_to_tc,
            QosMapType::TcToDscp => &self.tc_to_dscp,
            QosMapType::TcToDot1p => &self.tc_to_cos,
        }
    }
}

/// A port's ingress classification mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QosTrust {
    /// Everything lands in the port's default traffic class.
    #[default]
    Untrusted,
    Dscp,
    Cos,
}

impl QosTrust {
    pub fn word(self) -> &'static str {
        match self {
            QosTrust::Untrusted => "untrusted",
            QosTrust::Dscp => "dscp",
            QosTrust::Cos => "cos",
        }
    }

    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "untrusted" | "" => Some(QosTrust::Untrusted),
            "dscp" => Some(QosTrust::Dscp),
            "cos" => Some(QosTrust::Cos),
            _ => None,
        }
    }

    /// The ingress classification map this trust mode needs bound.
    pub fn map(self) -> Option<QosMapType> {
        match self {
            QosTrust::Untrusted => None,
            QosTrust::Dscp => Some(QosMapType::DscpToTc),
            QosTrust::Cos => Some(QosMapType::Dot1pToTc),
        }
    }
}

/// One port's whole QoS program, as pushed by `SetPortQos`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PortQosProgram {
    pub trust: QosTrust,
    pub default_tc: u8,
    /// Port shaper in bits/sec.
    pub shape_bps: Option<u64>,
    /// Non-default queues, keyed by queue index.
    pub queues: BTreeMap<u8, QueueQosProgram>,
}

/// One egress queue's program. Absent from
/// [`PortQosProgram::queues`] = the platform default (DWRR weight 1,
/// unshaped, no WRED).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueueQosProgram {
    pub strict: bool,
    /// DWRR weight; 1 unless set.
    pub weight: u8,
    pub shape_bps: Option<u64>,
    /// WRED profile name; empty = none.
    pub wred_profile: String,
}

impl QueueQosProgram {
    /// The scheduler shape this queue asks for.
    pub fn scheduler(&self) -> SchedulerSpec {
        SchedulerSpec {
            strict: self.strict,
            weight: if self.weight == 0 { 1 } else { self.weight },
            max_rate_bps: self.shape_bps,
        }
    }
}

/// What one physical port currently has programmed, so a re-push can
/// touch only what changed and a clear knows what to undo.
#[derive(Debug, Clone, Default)]
pub struct AppliedPortQos {
    /// The display name whose program this is — the port itself, or the
    /// Port-Channel it belongs to.
    pub source: String,
    pub trust: QosTrust,
    pub default_tc: u8,
    pub shape_bps: Option<u64>,
    /// Per-queue scheduler objects this port holds a reference on.
    pub queue_schedulers: BTreeMap<u8, (SchedulerSpec, Oid)>,
    /// Per-queue WRED bindings, by profile name.
    pub queue_wreds: BTreeMap<u8, String>,
    /// Map types currently bound on the port.
    pub bound_maps: BTreeSet<QosMapType>,
}

/// One named WRED profile: its config and, once a queue references it,
/// the refcounted SAI object behind it.
#[derive(Debug, Clone, Default)]
pub struct WredProfileState {
    pub spec: WredSpec,
    pub oid: Option<Oid>,
    /// Queues currently bound to it.
    pub refs: u32,
}

pub type SharedQos = Arc<RwLock<QosWorld>>;

/// Resolve a SAI port id back to a port name (for event handling).
pub fn name_for(ports: &HashMap<String, PortState>, id: PortId) -> Option<String> {
    ports
        .iter()
        .find(|(_, p)| p.sai_id == id)
        .map(|(name, _)| name.clone())
}

// ---------------------------------------------------------- cable diag

/// One TDR sweep result, kept so `show interfaces <port>
/// cable-diagnostics` can replay what the last `request` found. In
/// memory only: a sweep is a measurement of a moment, and a stale one
/// surviving a reboot would be worse than none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CableDiagResult {
    /// Unix seconds the sweep ran.
    pub run_at: i64,
    pub pairs: Vec<hemlock_sai::CablePair>,
}

/// Last sweep per port display name.
pub type SharedCableDiag = Arc<RwLock<BTreeMap<String, CableDiagResult>>>;
