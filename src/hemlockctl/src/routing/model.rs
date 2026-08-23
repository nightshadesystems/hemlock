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
