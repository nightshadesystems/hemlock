//! The QoS-suite data model: text renderers and the `| json`
//! serializer both consume these, so the two outputs can never drift.

use serde::Serialize;

// ------------------------------------------------- Global maps

/// One `key -> value` mapping in a global map table.
#[derive(Debug, Clone, Serialize)]
pub struct MapEntry {
    pub key: u8,
    pub value: u8,
}

/// One of the four global map tables, pre-labelled for rendering.
#[derive(Debug, Clone, Serialize)]
pub struct MapTable {
    /// The config keyword (`dscp-to-tc`), so `| json` names the table
    /// the way `set` does.
    pub table: String,
    /// Block heading, e.g. "DSCP to Traffic-Class map".
    pub title: String,
    /// Column headings for the key and value columns.
    pub key_label: String,
    pub value_label: String,
    /// What unmapped values do ("0" for classification, "no rewrite"
    /// for the egress maps).
    pub default_note: String,
    pub entries: Vec<MapEntry>,
}

/// `show qos maps`: the four tables in classification-then-rewrite
/// order.
#[derive(Debug, Clone, Serialize, Default)]
pub struct MapState {
    pub tables: Vec<MapTable>,
}

// ------------------------------------------------- WRED profiles

/// One named WRED/ECN profile with the queues referencing it.
#[derive(Debug, Clone, Serialize)]
pub struct WredProfile {
    pub name: String,
    /// Thresholds in KB.
    pub min_threshold: u32,
    pub max_threshold: u32,
    /// Percent at max-threshold.
    pub drop_probability: u32,
    pub ecn: bool,
    /// Referencing queues, pre-rendered as `Et1 (q3)`.
    pub references: Vec<String>,
}

/// `show qos wred`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct WredState {
    pub profiles: Vec<WredProfile>,
    /// The platform's packet buffer in KB (0 = the SAI would not say),
    /// which is the ceiling on `max_threshold`.
    pub buffer_kb: u32,
    /// False where the platform's SAI serves no WRED at all.
    pub supported: bool,
}

// ------------------------------------------------- Per-port QoS

/// One egress queue's effective program plus its live counters.
#[derive(Debug, Clone, Serialize)]
pub struct QueueQos {
    pub queue: u8,
    /// "strict" | "dwrr".
    pub mode: String,
    /// DWRR weight; None for a strict queue (it takes no share).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
    /// Shaper, pre-rendered ("100 Mbps"); None = unshaped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shaper: Option<String>,
    /// Bound WRED profile name; None = plain tail drop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wred_profile: Option<String>,
    /// The bound profile's ECN flag.
    pub ecn: bool,
    pub tx_packets: u64,
    pub tx_bytes: u64,
    pub dropped: u64,
    pub wred_dropped: u64,
    pub ecn_marked: u64,
}

/// One port's effective QoS.
#[derive(Debug, Clone, Serialize)]
pub struct PortQos {
    /// Full display name (`Ethernet1`, `Port-Channel1`).
    pub port: String,
    /// "untrusted" | "dscp" | "cos".
    pub trust: String,
    pub default_tc: u8,
    /// Port shaper, pre-rendered; None = unshaped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shaper: Option<String>,
    pub queues: Vec<QueueQos>,
    /// False for a port running entirely at the platform defaults.
    pub configured: bool,
    /// Set on a physical port whose program comes from its
    /// Port-Channel — the summary grid folds it into the Po row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via_port_channel: Option<String>,
}

impl PortQos {
    /// The strict-priority queues, highest first.
    pub fn strict_queues(&self) -> Vec<u8> {
        let mut queues: Vec<u8> = self
            .queues
            .iter()
            .filter(|q| q.mode == "strict")
            .map(|q| q.queue)
            .collect();
        queues.sort_unstable_by(|a, b| b.cmp(a));
        queues
    }

    /// The queues carrying a WRED profile, highest first.
    pub fn wred_queues(&self) -> Vec<u8> {
        let mut queues: Vec<u8> = self
            .queues
            .iter()
            .filter(|q| q.wred_profile.is_some())
            .map(|q| q.queue)
            .collect();
        queues.sort_unstable_by(|a, b| b.cmp(a));
        queues
    }
}

/// `show qos interface(s)`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct PortQosState {
    pub ports: Vec<PortQos>,
    /// Front-panel ports running entirely at the platform defaults —
    /// the trailing `... N ports with default QoS configuration` line.
    pub default_ports: u32,
}
