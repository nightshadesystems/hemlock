//! The services-suite data model: text renderers and the `| json`
//! serializer both consume these, so the two outputs can never drift.

use serde::Serialize;

/// One neighbor as LLDP last heard it.
#[derive(Debug, Clone, Serialize)]
pub struct LldpNeighbor {
    /// The port the neighbor was heard on (full display name).
    pub port: String,
    pub chassis_id: String,
    /// "mac" | "interface-name" | "local" | ... — the TLV subtype, as
    /// the wire named it.
    pub chassis_id_subtype: String,
    pub port_id: String,
    pub port_id_subtype: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub port_description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub system_name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub system_description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub management_address: String,
    /// The TTL the neighbor advertised, in seconds.
    pub ttl: u32,
    /// Seconds since its last advertisement.
    pub age_secs: u64,
}

/// One local port's LLDP state.
#[derive(Debug, Clone, Serialize)]
pub struct LldpPort {
    /// Full display name (`Ethernet1`).
    pub port: String,
    /// False when the global switch or `lldp disable` turns it off.
    pub enabled: bool,
    pub frames_tx: u64,
    pub frames_rx: u64,
    pub frames_discarded: u64,
    pub ageouts: u64,
    pub neighbors: Vec<LldpNeighbor>,
}

/// `show lldp` and its neighbor views.
#[derive(Debug, Clone, Serialize, Default)]
pub struct LldpState {
    pub enabled: bool,
    pub tx_interval: u32,
    pub hold_multiplier: u32,
    pub chassis_id: String,
    pub system_name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub system_description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub management_address: String,
    pub ports: Vec<LldpPort>,
}

impl LldpState {
    /// The advertised hold time: the interval times the multiplier.
    pub fn ttl(&self) -> u32 {
        self.tx_interval.saturating_mul(self.hold_multiplier)
    }

    /// Every neighbor across every port, in port order — the flat
    /// `show lldp neighbors` grid.
    pub fn neighbors(&self) -> Vec<&LldpNeighbor> {
        self.ports
            .iter()
            .flat_map(|port| port.neighbors.iter())
            .collect()
    }
}

// ------------------------------------------------- NTP

/// `show ntp`: the configured servers plus timesyncd's live sync.
#[derive(Debug, Clone, Serialize, Default)]
pub struct NtpState {
    /// systemd-timesyncd is running (no servers = mgmtd stops it).
    pub enabled: bool,
    /// Servers in config order.
    pub servers: Vec<String>,
    pub synchronized: bool,
    /// The server actually in use; empty while unsynchronized.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub server: String,
    pub stratum: u32,
    pub poll_interval_secs: u32,
    /// Microseconds; the offset is signed (the local clock can lead).
    pub offset_usecs: i64,
    pub delay_usecs: u64,
    pub jitter_usecs: u64,
    /// Seconds since the last accepted reply; None = never, or the
    /// timestamp was unreadable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync_secs_ago: Option<u64>,
}

// ------------------------------------------------- SNMP

/// One v2c read-only community.
#[derive(Debug, Clone, Serialize)]
pub struct SnmpCommunity {
    pub name: String,
    /// Source prefix the community answers on; None = anywhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// `show snmp`: agent settings plus the subagent's request counters.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SnmpState {
    pub enabled: bool,
    /// The AgentX subagent holds a session with snmpd's master, so the
    /// IF-MIB is actually answerable.
    pub agentx_connected: bool,
    pub listen_interface: String,
    pub listen_address: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub location: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub contact: String,
    pub communities: Vec<SnmpCommunity>,
    /// v3 USM user names (read-only authPriv; passphrases never leave
    /// the config).
    pub users: Vec<String>,
    pub packets_in: u64,
    pub packets_out: u64,
    pub get_requests: u64,
    pub getnext_requests: u64,
    pub errors: u64,
}

// ------------------------------------------------- sFlow

/// `show sflow`: the sampler's settings and the exporter's counters.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SflowState {
    /// False = no collector configured, so nothing is sampled.
    pub enabled: bool,
    /// False where the platform's SAI serves no samplepacket objects.
    pub supported: bool,
    /// The agent address the datagrams carry, and the interface it
    /// belongs to.
    pub agent_address: String,
    pub agent_interface: String,
    /// 1-in-N.
    pub sample_rate: u32,
    pub polling_interval: u32,
    /// Pre-rendered `10.42.0.20:6343` collector endpoints, in config
    /// order.
    pub collectors: Vec<String>,
    /// Ports sampling is programmed on, and ports carrying
    /// `sflow disable`.
    pub enabled_ports: Vec<String>,
    pub disabled_ports: Vec<String>,
    pub samples_taken: u64,
    pub counter_samples: u64,
    pub datagrams_sent: u64,
    pub datagrams_failed: u64,
}

// ------------------------------------------------- DHCP relay

/// One relay-enabled SVI.
#[derive(Debug, Clone, Serialize)]
pub struct DhcpRelayVlan {
    pub vlan: u16,
    /// Servers in config order.
    pub servers: Vec<String>,
    /// The SVI address stamped into giaddr.
    pub giaddr: String,
    pub to_server: u64,
    pub to_client: u64,
    pub dropped: u64,
}

/// `show dhcp relay`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct DhcpRelayState {
    pub vlans: Vec<DhcpRelayVlan>,
}

// ------------------------------------------------- DHCP server

/// One configured pool with its live utilisation.
#[derive(Debug, Clone, Serialize)]
pub struct DhcpPool {
    pub name: String,
    pub network: String,
    /// Pre-rendered `10.0.10.100 - 10.0.10.200`.
    pub range: String,
    pub gateway: String,
    pub lease_time: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dns_servers: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub domain_name: String,
    /// Dynamic leases held out of the range, and how many it holds.
    pub in_use: u32,
    pub capacity: u32,
}

/// One lease or reservation.
#[derive(Debug, Clone, Serialize)]
pub struct DhcpLease {
    pub address: String,
    pub mac: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub hostname: String,
    /// Unix seconds; None for a reservation with no active lease.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// "dynamic" | "reservation".
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub pool: String,
}

/// `show dhcp server` and `show dhcp server leases`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct DhcpServerState {
    pub pools: Vec<DhcpPool>,
    pub leases: Vec<DhcpLease>,
}
