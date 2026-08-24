//! The routing-suite data model: text renderers and the `| json`
//! serializer both consume these, so the two outputs can never drift.

use serde::Serialize;

/// One next hop of a route.
#[derive(Debug, Clone, Serialize)]
pub struct NextHop {
    pub via: String,
    /// Egress interface, when the next hop resolves onto a connected
    /// subnet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
}

/// One RIB entry.
#[derive(Debug, Clone, Serialize)]
pub struct RouteEntry {
    /// Route source: "connected" | "static" | "kernel" | "ospf" | "bgp".
    pub protocol: String,
    /// Canonical prefix.
    pub prefix: String,
    /// Administrative distance and metric (the `[d/m]` bracket); not
    /// rendered for connected routes.
    pub distance: u32,
    pub metric: u32,
    /// Next hops; empty for connected and drop routes.
    pub next_hops: Vec<NextHop>,
    /// Directly-connected egress: the interface for connected routes,
    /// `Null0` for drop routes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    /// FIB (hardware) state from the orch snapshot: "programmed" |
    /// "punt" | "drop" | "connected" | "kernel". None when the table
    /// was derived from config alone (no RIB pipeline yet).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fib: Option<String>,
}

impl RouteEntry {
    /// The route-code column letter.
    pub fn code(&self) -> &'static str {
        match self.protocol.as_str() {
            "connected" => "C",
            "static" => "S",
            "kernel" => "K",
            "ospf" => "O",
            "bgp" => "B",
            _ => "?",
        }
    }

    /// The gateway-of-last-resort entry, either family.
    pub fn is_default(&self) -> bool {
        matches!(self.prefix.as_str(), "0.0.0.0/0" | "::/0")
    }
}

/// The whole table for one address family, sorted by prefix.
#[derive(Debug, Clone, Serialize, Default)]
pub struct RouteTable {
    pub routes: Vec<RouteEntry>,
}

impl RouteTable {
    /// Roll the table up into `show ip route summary` (fixed source
    /// order; absent sources are omitted).
    pub fn summarize(&self, next_hop_groups: u32) -> RouteSummary {
        let count = |p: &str| self.routes.iter().filter(|r| r.protocol == p).count();
        let sources = ["connected", "static", "kernel", "ospf", "bgp"]
            .iter()
            .map(|p| SourceCount {
                source: p.to_string(),
                routes: count(p),
            })
            .filter(|s| s.routes != 0)
            .collect();
        RouteSummary {
            sources,
            total: self.routes.len(),
            next_hop_groups,
        }
    }
}

/// `show ip route summary`.
#[derive(Debug, Clone, Serialize)]
pub struct RouteSummary {
    pub sources: Vec<SourceCount>,
    pub total: usize,
    /// Hardware next-hop groups; 0 until the FIB pipeline reports them.
    pub next_hop_groups: u32,
}

/// One `show ip route summary` row.
#[derive(Debug, Clone, Serialize)]
pub struct SourceCount {
    pub source: String,
    pub routes: usize,
}

/// One `show arp` / `show ipv6 neighbors` row.
#[derive(Debug, Clone, Serialize)]
pub struct NeighborEntry {
    pub ip: String,
    /// Colon-separated lowercase; empty while unresolved.
    pub mac: String,
    pub interface: String,
    /// A configured static entry (renders `-` in the Age column).
    pub is_static: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_secs: Option<u64>,
}

/// One family's neighbor table, sorted by address.
#[derive(Debug, Clone, Serialize, Default)]
pub struct NeighborTable {
    pub entries: Vec<NeighborEntry>,
}

// ------------------------------------------------- FRR protocol detail

/// `show routing ospf` and its subviews.
#[derive(Debug, Clone, Serialize, Default)]
pub struct OspfState {
    pub router_id: String,
    pub spf_runs: u32,
    pub areas: Vec<OspfArea>,
    pub neighbors: Vec<OspfNeighbor>,
    pub interfaces: Vec<OspfInterface>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OspfArea {
    pub id: String,
    pub interfaces: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct OspfNeighbor {
    pub router_id: String,
    pub priority: u32,
    pub state: String,
    pub dead_time_msecs: u64,
    pub address: String,
    pub interface: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OspfInterface {
    pub name: String,
    pub up: bool,
    pub address: String,
    pub area: String,
    pub router_id: String,
    pub network_type: String,
    pub cost: u32,
    pub state: String,
    pub priority: u32,
    pub dr_id: String,
    pub dr_address: String,
    pub hello_interval: u32,
    pub dead_interval: u32,
    pub neighbors: u32,
    pub adjacent: u32,
}

/// `show routing bgp ...`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct BgpState {
    pub router_id: String,
    pub as_number: u32,
    pub peers: Vec<BgpPeer>,
    pub routes: Vec<BgpRibEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<BgpNeighborDetail>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BgpPeer {
    pub ip: String,
    pub version: u32,
    pub remote_as: u32,
    pub msg_rcvd: u64,
    pub msg_sent: u64,
    pub in_q: u32,
    pub out_q: u32,
    pub up_down: String,
    pub state: String,
    /// -1 = not established (the State column tells the story).
    pub pfx_rcvd: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BgpRibEntry {
    pub network: String,
    pub next_hop: String,
    pub metric: String,
    pub loc_pref: String,
    pub path: String,
    pub valid: bool,
    pub best: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BgpNeighborDetail {
    pub ip: String,
    pub remote_as: u32,
    pub description: String,
    pub state: String,
    pub uptime: String,
    pub msg_rcvd: u64,
    pub msg_sent: u64,
    pub prefixes_received: i64,
    pub prefixes_accepted: i64,
    pub prefixes_advertised: i64,
    pub next_hop_self: bool,
    pub ebgp_multihop: u32,
}

/// `show vrrp [brief]`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct VrrpState {
    pub groups: Vec<VrrpGroup>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VrrpGroup {
    pub interface: String,
    pub group: u32,
    pub priority: u32,
    pub effective_priority: u32,
    pub advertisement_interval_ms: u32,
    pub preempt: bool,
    pub state: String,
    pub addresses: Vec<String>,
    pub virtual_mac: String,
    pub skew_time_ms: u32,
    pub master_down_interval_ms: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds_since_transition: Option<u64>,
}
