//! The switching-suite data model: text renderers and the `| json`
//! serializer both consume these, so the two outputs can never drift.

use serde::Serialize;

use crate::interfaces::name::InterfaceId;

/// One VLAN row of `show vlan`.
#[derive(Debug, Clone, Serialize)]
pub struct VlanRow {
    pub id: u32,
    /// Configured display name; None renders `default` / `VLAN0042`.
    pub name: Option<String>,
    pub suspended: bool,
    /// Member ports (untagged and tagged alike), naturally sorted.
    pub ports: Vec<InterfaceId>,
}

impl VlanRow {
    /// Display name: the configured name, `default` for an unnamed
    /// VLAN 1, `VLAN0042`-style otherwise.
    pub fn display_name(&self) -> String {
        match &self.name {
            Some(name) if !name.is_empty() => name.clone(),
            _ if self.id == 1 => "default".into(),
            _ => format!("VLAN{:04}", self.id),
        }
    }

    pub fn status_word(&self) -> &'static str {
        if self.suspended {
            "suspend"
        } else {
            "active"
        }
    }
}

/// One MAC table entry (`show mac address-table`).
#[derive(Debug, Clone, Serialize)]
pub struct MacEntry {
    pub vlan: u32,
    /// Colon-separated lowercase.
    pub mac: String,
    pub is_static: bool,
    /// None for drop entries.
    pub port: Option<InterfaceId>,
    pub drop: bool,
    pub moves: u32,
    /// Seconds since the entry last moved ports.
    pub last_move_secs: Option<u64>,
}

/// The whole table plus its aging time.
#[derive(Debug, Clone, Serialize, Default)]
pub struct MacTable {
    pub aging_time_secs: u32,
    pub entries: Vec<MacEntry>,
}

/// One `show storm-control` row.
#[derive(Debug, Clone, Serialize)]
pub struct StormRow {
    pub port: InterfaceId,
    /// `broadcast` | `multicast` | `unknown-unicast`.
    pub kind: String,
    /// Percent, two decimals.
    pub level: String,
    pub rate_kbps: u64,
    pub drops: u64,
    pub active: bool,
}

/// One port-channel with its LACP runtime (`show port-channel ...`,
/// `show lacp ...`).
#[derive(Debug, Clone, Serialize)]
pub struct PortChannel {
    pub group: u32,
    pub description: String,
    pub admin_up: bool,
    pub up: bool,
    /// False = static aggregation (`mode on`).
    pub lacp: bool,
    pub active_mode: bool,
    pub bundled: u32,
    pub total: u32,
    pub min_links: u32,
    /// "" | "static" | "individual".
    pub fallback_mode: String,
    pub fallback_timeout_secs: u32,
    pub fallback_active: bool,
    pub mac: String,
    pub members: Vec<PortChannelMember>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PortChannelMember {
    pub id: InterfaceId,
    /// "bundled" | "standby" | "down" | "individual".
    pub status: String,
    pub rate_fast: bool,
    pub actor_state: u8,
    pub partner_state: u8,
    /// "32768,d4:af:f7:12:9c:00"; empty = defaulted.
    pub partner_system: String,
    pub partner_port: u32,
    pub partner_key: u32,
    pub partner_priority: u32,
    pub pdus_rx: u64,
    pub pdus_tx: u64,
    pub churn: u32,
}

/// IEEE state octet rendered EOS-style: the set letters, dash-padded to
/// eight columns (A=activity, F=fast, G=aggregation, S=sync,
/// C=collecting, D=distributing rendered as A,C,D,S per EOS habit).
pub fn lacp_state_word(state: u8) -> String {
    let mut word = String::new();
    if state & 0x01 != 0 {
        word.push('A');
    }
    if state & 0x10 != 0 {
        word.push('C');
    }
    if state & 0x20 != 0 {
        word.push('D');
    }
    if state & 0x08 != 0 {
        word.push('S');
    }
    while word.len() < 8 {
        word.push('-');
    }
    word
}

/// The LACP actor system id (`show lacp sys-id`).
#[derive(Debug, Clone, Serialize, Default)]
pub struct LacpSystem {
    pub system_id: String,
}

/// The spanning-tree bridge view (`show spanning-tree ...`).
#[derive(Debug, Clone, Serialize)]
pub struct StpBridge {
    /// "mstp" | "rstp" | "none".
    pub mode: String,
    pub bridge_priority: u32,
    pub bridge_mac: String,
    pub root_priority: u32,
    pub root_mac: String,
    pub is_root: bool,
    pub root_cost: u32,
    pub root_port: Option<InterfaceId>,
    pub hello_time: u32,
    pub max_age: u32,
    pub forward_time: u32,
    pub mst_name: String,
    pub mst_revision: u32,
    /// (instance, mapped VLANs), instance 0 excluded (implicit).
    pub instances: Vec<(u32, Vec<u32>)>,
    pub topology_changes: u32,
    pub seconds_since_tc: Option<u64>,
    pub last_tc_port: Option<String>,
    pub ports: Vec<StpPortRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StpPortRow {
    pub id: InterfaceId,
    /// "designated" | "root" | "alternate" | "disabled".
    pub role: String,
    /// "forwarding" | "learning" | "discarding".
    pub state: String,
    pub cost: u32,
    pub priority: u32,
    pub portfast: bool,
    pub bpduguard: bool,
    pub bpdus_rx: u64,
    pub bpdus_tx: u64,
    pub errdisabled: bool,
}

impl StpPortRow {
    /// The `Prio.Nbr` cell. Port-channels number from 101 (the EOS
    /// aggregated-port convention keeps them clear of physical ports).
    pub fn prio_nbr(&self) -> String {
        let number = match self.id.kind {
            crate::interfaces::name::Kind::PortChannel => 100 + self.id.num,
            _ => self.id.num,
        };
        format!("{}.{}", self.priority, number)
    }
}

/// One snooping family's view (`show igmp|mld snooping ...`).
#[derive(Debug, Clone, Serialize)]
pub struct SnoopingView {
    /// "IGMP" | "MLD".
    pub family: String,
    pub enabled: bool,
    pub robustness: u32,
    pub vlans: Vec<SnoopVlanView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnoopVlanView {
    pub vlan: u32,
    pub enabled: bool,
    pub fast_leave: bool,
    pub querier_enabled: bool,
    pub querier_address: Option<String>,
    pub querier_active: bool,
    pub static_mrouters: Vec<InterfaceId>,
    pub dynamic_mrouters: Vec<InterfaceId>,
    pub groups: Vec<SnoopGroupView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnoopGroupView {
    pub group: String,
    pub version: u32,
    pub ports: Vec<InterfaceId>,
}

/// One mirror session (`show mirror`).
#[derive(Debug, Clone, Serialize)]
pub struct MirrorSession {
    pub session: u32,
    pub rx: Vec<InterfaceId>,
    pub tx: Vec<InterfaceId>,
    pub both: Vec<InterfaceId>,
    pub destination: Option<InterfaceId>,
    pub destination_active: bool,
}
