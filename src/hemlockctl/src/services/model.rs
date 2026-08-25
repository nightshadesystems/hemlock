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
