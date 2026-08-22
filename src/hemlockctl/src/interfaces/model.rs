//! The `show interfaces` data model.
//!
//! One set of structs backs every command in the family: the text
//! renderers and the `| json` serializer both consume these, so the two
//! outputs can never drift. The daemons populate what they know; every
//! optional field degrades gracefully in the renderers (line omitted, or
//! `N/A` where the layout requires a cell).

use std::collections::BTreeMap;

use serde::Serialize;

use super::name::{InterfaceId, Kind};

/// Administrative state, as shown on the detail-block state line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AdminState {
    #[default]
    Up,
    Down,
    AdminDown,
}

impl AdminState {
    /// State-line wording: `up` / `down` / `administratively down`.
    pub fn state_line(self) -> &'static str {
        match self {
            AdminState::Up => "up",
            AdminState::Down => "down",
            AdminState::AdminDown => "administratively down",
        }
    }

    /// `show interfaces description` Status column wording.
    pub fn table_word(self) -> &'static str {
        match self {
            AdminState::Up => "up",
            AdminState::Down => "down",
            AdminState::AdminDown => "admin down",
        }
    }
}

/// Line-protocol state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LineProtocol {
    #[default]
    Up,
    Down,
    // Part of the EOS state model; the daemons produce them once LAG /
    // subinterface lower-layer tracking lands.
    #[allow(dead_code)]
    LowerLayerDown,
    #[allow(dead_code)]
    NotPresent,
}

impl LineProtocol {
    pub fn word(self) -> &'static str {
        match self {
            LineProtocol::Up => "up",
            LineProtocol::Down => "down",
            LineProtocol::LowerLayerDown => "lowerlayerdown",
            LineProtocol::NotPresent => "notpresent",
        }
    }
}

/// The parenthesized interface status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum IfStatus {
    #[default]
    Connected,
    NotConnect,
    Disabled,
    ErrDisabled,
    Inactive,
}

impl IfStatus {
    pub fn word(self) -> &'static str {
        match self {
            IfStatus::Connected => "connected",
            IfStatus::NotConnect => "notconnect",
            IfStatus::Disabled => "disabled",
            IfStatus::ErrDisabled => "errdisabled",
            IfStatus::Inactive => "inactive",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Duplex {
    #[default]
    Full,
    Half,
}

impl Duplex {
    /// Detail-block wording (`Full-duplex`).
    pub fn detail(self) -> &'static str {
        match self {
            Duplex::Full => "Full-duplex",
            Duplex::Half => "Half-duplex",
        }
    }

    /// Tabular cell (`full`).
    pub fn cell(self) -> &'static str {
        match self {
            Duplex::Full => "full",
            Duplex::Half => "half",
        }
    }
}

/// Physical-layer facts for the `Full-duplex, 1Gb/s, ...` line; absent on
/// virtual interfaces.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Phys {
    pub duplex: Duplex,
    /// `None` = link down with no forced speed (`Unconfigured`).
    pub speed_mbps: Option<u64>,
    pub autoneg: bool,
    /// `a-` prefix in tabular duplex/speed cells (values resolved by
    /// auto-negotiation rather than forced).
    pub speed_from_autoneg: bool,
    /// Unidirectional-link state; `None` renders `n/a`.
    pub uni_link: Option<String>,
}

/// Primary L3 addressing, when the interface is routed.
#[derive(Debug, Clone, Serialize)]
pub struct IpInfo {
    /// CIDR form, e.g. `10.10.0.21/24`.
    pub address: String,
    pub broadcast: String,
}

/// Counter bookkeeping shown in the detail block.
#[derive(Debug, Clone, Serialize, Default)]
pub struct CounterMeta {
    pub link_changes: u64,
    /// `None` = never cleared.
    pub last_clear_secs: Option<u64>,
}

/// Load-interval rates, daemon-computed (EWMA over the load interval;
/// utilization includes the 20 bytes/frame preamble + IFG overhead).
#[derive(Debug, Clone, Serialize, Default)]
pub struct Rates {
    pub interval_secs: u32,
    pub in_bps: f64,
    pub in_pps: u64,
    pub in_util_pct: f64,
    pub out_bps: f64,
    pub out_pps: u64,
    pub out_util_pct: f64,
}

/// The full hardware counter set. Port-channels carry the same struct but
/// render the reduced block (no runts/giants/CRC lines).
#[derive(Debug, Clone, Serialize, Default)]
pub struct Counters {
    /// Total packets, as the detail block prints (`N packets input`).
    pub in_pkts: u64,
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
    pub out_pkts: u64,
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
}

/// A port-channel member as shown in the detail block.
#[derive(Debug, Clone, Serialize)]
pub struct Member {
    pub id: InterfaceId,
    pub duplex: Duplex,
    pub speed_mbps: u64,
}

/// The `Vlan` column of `show interfaces status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum VlanCell {
    Routed,
    #[default]
    Trunk,
    Access(u32),
    // Produced once LAG membership is modeled (`in Po<N>`).
    #[allow(dead_code)]
    InPortChannel(u32),
}

impl VlanCell {
    pub fn cell(&self) -> String {
        match self {
            VlanCell::Routed => "routed".into(),
            VlanCell::Trunk => "trunk".into(),
            VlanCell::Access(vlan) => format!("{vlan}"),
            VlanCell::InPortChannel(po) => format!("in Po{po}"),
        }
    }
}

/// One egress queue's counters (`show interfaces counters queue`).
#[derive(Debug, Clone, Serialize, Default)]
pub struct QueueCounters {
    /// `UC0`..`UC7`, `MC0`.. — labels come from the platform definition.
    pub queue: String,
    pub pkts: u64,
    pub bytes: u64,
    pub dropped_pkts: u64,
    pub dropped_bytes: u64,
}

/// RMON frame-size distribution (`show interfaces counters bins`). The
/// seven EOS bins; platforms lacking a bin report 0.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Bins {
    pub rx: [u64; 7],
    pub tx: [u64; 7],
}

pub const BIN_LABELS: [&str; 7] = [
    "64 bytes:",
    "65-127 bytes:",
    "128-255 bytes:",
    "256-511 bytes:",
    "512-1023 bytes:",
    "1024-1522 bytes:",
    "1523-max bytes:",
];

/// `show interfaces capabilities` facts, straight from the platform
/// definition layer.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Capabilities {
    pub model: String,
    pub media_type: String,
    /// e.g. `10M/half,10M/full,100M/half,100M/full,1G/full,auto`
    pub speed_duplex: String,
    /// e.g. `rx-(off,on,desired),tx-(off,on,desired)`
    pub flowcontrol: String,
}

/// Flow-control admin/oper state; pause frame counts live in [`Counters`].
#[derive(Debug, Clone, Serialize, Default)]
pub struct FlowControl {
    pub send_admin: String,
    pub send_oper: String,
    pub recv_admin: String,
    pub recv_oper: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AutonegMode {
    Ieee8023,
    #[default]
    Off,
}

impl AutonegMode {
    pub fn cell(self) -> &'static str {
        match self {
            AutonegMode::Ieee8023 => "802.3",
            AutonegMode::Off => "off",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            AutonegMode::Ieee8023 => "IEEE 802.3",
            AutonegMode::Off => "Off",
        }
    }
}

/// A local or link-partner advertisement.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Advertisement {
    /// e.g. `["10M/half", "10M/full", "100M/half", "100M/full", "1G/full"]`
    pub speed_duplex: Vec<String>,
    /// `None`, `Symmetric`, `Asymmetric`, ...
    pub pause: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct NegotiationResolution {
    /// e.g. `1G/full`
    pub speed_duplex: String,
    /// e.g. `rx off, tx off`
    pub pause: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Negotiation {
    pub mode: AutonegMode,
    /// `success`, `in progress`, `failed`; `None` renders `n/a`.
    pub status: Option<String>,
    pub local: Option<Advertisement>,
    pub partner: Option<Advertisement>,
    pub resolution: Option<NegotiationResolution>,
}

/// PHY diagnostic state (`show interfaces phy detail`). Field
/// availability is platform/PHY-dependent; `None` rows are omitted.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Phy {
    pub state: Option<String>,
    pub interface_state: Option<String>,
    pub hw_resets: Option<u64>,
    pub transceiver: Option<String>,
    /// e.g. `1Gbps`
    pub oper_speed: Option<String>,
    pub interrupt_count: Option<u64>,
    pub diags_mode: Option<String>,
    pub model: Option<String>,
    pub reset_count: Option<u64>,
    pub state_changes: Option<u64>,
    pub last_change_secs: Option<u64>,
    /// e.g. `auto`, `1G`
    pub configured_speed: Option<String>,
    pub autoneg_config: Option<bool>,
}

/// MAC-layer state (`show interfaces mac [detail]`).
#[derive(Debug, Clone, Serialize, Default)]
pub struct MacDetail {
    /// `linkUp`, `phyOff`, ...
    pub state: String,
    pub local_fault: Option<bool>,
    pub remote_fault: Option<bool>,
    /// e.g. `Disabled`, `Reed-Solomon`
    pub fec_mode: Option<String>,
    pub fec_corrected: Option<u64>,
    pub fec_uncorrected: Option<u64>,
}

/// L2 switchport state (`show interfaces switchport` and friends).
/// `enabled = false` renders the two-line routed-port block.
#[derive(Debug, Clone, Serialize)]
pub struct Switchport {
    pub enabled: bool,
    /// `static access` | `trunk`
    pub admin_mode: String,
    pub oper_mode: String,
    pub mac_learning: bool,
    pub tpid: String,
    pub tpid_active: bool,
    pub source_port_filter: String,
    pub access_vlan: u32,
    pub native_vlan: u32,
    pub native_tagging: bool,
    /// `None` = ALL.
    pub trunk_vlans: Option<Vec<u32>>,
    pub static_trunk_groups: Vec<String>,
    pub dynamic_trunk_groups: Vec<String>,
    pub source_interface_filtering: bool,
}

impl Default for Switchport {
    fn default() -> Self {
        Self {
            enabled: true,
            admin_mode: "static access".into(),
            oper_mode: "static access".into(),
            mac_learning: true,
            tpid: "0x8100".into(),
            tpid_active: true,
            source_port_filter: "Disabled".into(),
            access_vlan: 1,
            native_vlan: 1,
            native_tagging: false,
            trunk_vlans: None,
            static_trunk_groups: Vec::new(),
            dynamic_trunk_groups: Vec::new(),
            source_interface_filtering: true,
        }
    }
}

/// Everything the CLI knows about one interface.
#[derive(Debug, Clone, Serialize)]
pub struct Interface {
    pub id: InterfaceId,
    pub admin: AdminState,
    pub proto: LineProtocol,
    pub status: IfStatus,
    /// Errdisable cause (`link-flap`, ...), when status is `errdisabled`.
    pub errdisable_reason: Option<String>,
    pub description: Option<String>,
    /// Effective MAC (configured override, if any), colon-lowercase.
    pub mac: Option<String>,
    /// Burned-in address.
    pub bia: Option<String>,
    pub ip: Option<IpInfo>,
    pub mtu: u32,
    /// Routed interface: prints `Internet address` / `IP MTU`.
    pub l3: bool,
    pub bandwidth_kbit: Option<u64>,
    pub phys: Option<Phys>,
    /// Seconds in the current up/down state.
    pub last_change_secs: Option<u64>,
    /// `Loopback Mode : <mode>` line (physical ports).
    pub loopback_mode: Option<String>,
    pub counter_meta: Option<CounterMeta>,
    pub rates: Option<Rates>,
    pub counters: Option<Counters>,
    /// Port-channel members.
    pub members: Vec<Member>,
    /// Port-channel fallback mode (`off`).
    pub fallback_mode: Option<String>,
    pub vlan_membership: VlanCell,
    /// Physical media / type cell (`1000BASE-T`, `10GBASE-SR`,
    /// `10/100/1000`); `None` renders `N/A` where a cell is required.
    pub media: Option<String>,
    pub queues: Vec<QueueCounters>,
    pub bins: Option<Bins>,
    pub caps: Option<Capabilities>,
    pub flowcontrol: Option<FlowControl>,
    pub negotiation: Option<Negotiation>,
    pub phy: Option<Phy>,
    pub mac_detail: Option<MacDetail>,
    pub switchport: Option<Switchport>,
}

impl Interface {
    /// A blank interface of the given identity; fixtures and daemon
    /// conversions fill in what they know.
    pub fn new(id: InterfaceId) -> Self {
        Self {
            id,
            admin: AdminState::Up,
            proto: LineProtocol::Up,
            status: IfStatus::Connected,
            errdisable_reason: None,
            description: None,
            mac: None,
            bia: None,
            ip: None,
            mtu: 0,
            l3: false,
            bandwidth_kbit: None,
            phys: None,
            last_change_secs: None,
            loopback_mode: None,
            counter_meta: None,
            rates: None,
            counters: None,
            members: Vec::new(),
            fallback_mode: None,
            vlan_membership: VlanCell::default(),
            media: None,
            queues: Vec::new(),
            bins: None,
            caps: None,
            flowcontrol: None,
            negotiation: None,
            phy: None,
            mac_detail: None,
            switchport: None,
        }
    }

    /// `Hardware is <...>` wording. Management ports are Ethernet
    /// hardware.
    pub fn hardware(&self) -> &'static str {
        match self.id.kind {
            Kind::Ethernet | Kind::Management => "Ethernet",
            Kind::Loopback => "Loopback",
            Kind::Vlan => "Vlan",
            Kind::PortChannel => "Port-Channel",
        }
    }

    /// Best-available speed in Mb/s (physical speed, else bandwidth).
    pub fn speed_mbps(&self) -> Option<u64> {
        self.phys
            .as_ref()
            .and_then(|p| p.speed_mbps)
            .or(self.bandwidth_kbit.map(|kb| kb / 1000))
    }

    /// MAC state for `show interfaces mac`, derived when the daemon did
    /// not report one.
    pub fn mac_state(&self) -> String {
        if let Some(detail) = &self.mac_detail {
            if !detail.state.is_empty() {
                return detail.state.clone();
            }
        }
        match (self.admin, self.proto) {
            (AdminState::AdminDown, _) => "phyOff".into(),
            (_, LineProtocol::Up) => "linkUp".into(),
            _ => "linkDown".into(),
        }
    }
}

/// Transceiver DOM thresholds, per SFF-8472.
#[derive(Debug, Clone, Copy, Serialize, Default)]
pub struct Thresholds {
    pub high_alarm: f64,
    pub high_warn: f64,
    pub low_alarm: f64,
    pub low_warn: f64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DomThresholds {
    pub temperature: Thresholds,
    pub voltage: Thresholds,
    pub bias: Thresholds,
    pub tx_power: Thresholds,
    pub rx_power: Thresholds,
}

/// One transceiver, as pmon reports it (`show interfaces transceiver`).
#[derive(Debug, Clone, Serialize)]
pub struct Transceiver {
    pub id: InterfaceId,
    /// e.g. `10GBASE-SR`
    pub media_type: String,
    pub vendor: String,
    pub part_number: String,
    pub serial: String,
    pub date_code: String,
    pub temp_c: Option<f64>,
    pub voltage_v: Option<f64>,
    pub bias_ma: Option<f64>,
    pub tx_dbm: Option<f64>,
    pub rx_dbm: Option<f64>,
    pub thresholds: Option<DomThresholds>,
    /// Raw SFF pages for `transceiver eeprom`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub eeprom_a0: Vec<u8>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub eeprom_a2: Vec<u8>,
    /// Cache age (`Last Update`).
    pub age_secs: u64,
}

/// System-level facts renderers need alongside the interface list.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// `Default switchport mode: <mode>` header.
    pub default_switchport_mode: String,
    /// VLAN id -> configured name (`10 -> LAN-USERS`).
    pub vlan_names: BTreeMap<u32, String>,
    /// VLANs that exist/are active, for the trunk tables.
    pub active_vlans: Vec<u32>,
    /// Preformatted `Current System Time` for `phy detail`.
    pub system_time: Option<String>,
}

impl Context {
    /// `10 (LAN-USERS)` / `1 (default)` — VLAN with display name.
    pub fn vlan_with_name(&self, vlan: u32) -> String {
        match self.vlan_names.get(&vlan) {
            Some(name) => format!("{vlan} ({name})"),
            None if vlan == 1 => "1 (default)".into(),
            None => format!("{vlan}"),
        }
    }
}
