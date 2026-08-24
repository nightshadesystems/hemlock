//! The security-suite data model: text renderers and the `| json`
//! serializer both consume these, so the two outputs can never drift.

use serde::Serialize;

// ------------------------------------------------- ACLs

/// One user rule, pre-rendered to its EOS words.
#[derive(Debug, Clone, Serialize)]
pub struct AclRule {
    pub number: u32,
    pub permit: bool,
    /// Protocol word ("tcp", "udp", a number, or "ip"/"ipv6" when the
    /// rule matches any protocol); None for MAC rules, which carry no
    /// protocol column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Source/destination match ("any" when unconstrained): a prefix
    /// for the IP families, `mac/mask` for MAC lists.
    pub source: String,
    pub destination: String,
    /// Destination-port match, joined ("443", "67-68").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    pub log: bool,
    /// Per-rule policer as rendered: "<rate> <burst>" ("10m 256k").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub police: Option<String>,
    /// Hardware match counter (summed across bindings).
    pub matches: u64,
}

/// One ACL binding: a port and direction.
#[derive(Debug, Clone, Serialize)]
pub struct AclBinding {
    /// Full display name (`Ethernet1`).
    pub port: String,
    /// "in" | "out".
    pub direction: String,
}

/// One access list with its counters and bindings.
#[derive(Debug, Clone, Serialize)]
pub struct AclTable {
    pub name: String,
    /// "ipv4" | "ipv6" | "mac".
    pub family: String,
    pub rules: Vec<AclRule>,
    pub implicit_deny_matches: u64,
    pub bindings: Vec<AclBinding>,
}

impl AclTable {
    /// The family word of the block header ("IPv4 access list ...").
    pub fn family_display(&self) -> &'static str {
        match self.family.as_str() {
            "ipv6" => "IPv6",
            "mac" => "MAC",
            _ => "IPv4",
        }
    }

    /// The protocol word of the implicit-deny line; None for MAC lists
    /// (which render `implicit deny any any`).
    pub fn implicit_protocol(&self) -> Option<&'static str> {
        match self.family.as_str() {
            "ipv4" => Some("ip"),
            "ipv6" => Some("ipv6"),
            _ => None,
        }
    }
}

/// One TCAM stage's utilization.
#[derive(Debug, Clone, Serialize)]
pub struct TcamStage {
    /// "ingress" | "egress".
    pub stage: String,
    pub used: u32,
    pub available: u32,
}

/// Everything `show acl` renders: the lists plus TCAM utilization.
#[derive(Debug, Clone, Serialize, Default)]
pub struct AclState {
    pub acls: Vec<AclTable>,
    pub tcam: Vec<TcamStage>,
}

// ------------------------------------------------- Control-plane policing

/// One CoPP class row.
#[derive(Debug, Clone, Serialize)]
pub struct CoppClass {
    pub class: String,
    /// Packets per second and burst packets.
    pub rate: u32,
    pub burst: u32,
    /// Config overrides the compiled default (renders `*`).
    pub overridden: bool,
    pub conforming: u64,
    pub dropped: u64,
}

/// `show copp`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct CoppState {
    pub classes: Vec<CoppClass>,
}

// ------------------------------------------------- Port security

/// One learned secure MAC.
#[derive(Debug, Clone, Serialize)]
pub struct SecureMac {
    pub mac: String,
    pub age_secs: u64,
}

/// One port-security-enabled port.
#[derive(Debug, Clone, Serialize)]
pub struct PortSecurityEntry {
    /// Full display name (`Ethernet5`).
    pub port: String,
    pub maximum: u32,
    /// Violation action: true = shutdown (errdisable), false = protect.
    pub shutdown: bool,
    pub learned: Vec<SecureMac>,
    pub violations: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_violation_mac: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_violation_secs_ago: Option<u64>,
    pub errdisabled: bool,
}

impl PortSecurityEntry {
    /// The Action column word.
    pub fn action(&self) -> &'static str {
        if self.shutdown {
            "shutdown"
        } else {
            "protect"
        }
    }
}

// ------------------------------------------------- 802.1X

/// One authenticator port.
#[derive(Debug, Clone, Serialize)]
pub struct Dot1xPort {
    /// Full display name (`Ethernet10`).
    pub port: String,
    /// "authorized" | "unauthorized".
    pub status: String,
    /// Colon-lowercase; None while no supplicant is heard.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supplicant_mac: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_auth_secs_ago: Option<u64>,
    pub failures: u32,
}

/// `show dot1x` (ports already filtered for the interface form).
#[derive(Debug, Clone, Serialize, Default)]
pub struct Dot1xState {
    /// Display forms ("10.42.0.5:1812"), in config order.
    pub radius_servers: Vec<String>,
    /// Seconds; 0 = reauthentication off.
    pub reauth_interval_secs: u32,
    pub ports: Vec<Dot1xPort>,
}

// ------------------------------------------------- DHCP snooping + DAI

/// One snooping binding-table row.
#[derive(Debug, Clone, Serialize)]
pub struct SnoopBinding {
    pub mac: String,
    pub ip: String,
    /// Remaining lease; None for statics (renders `-`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_secs: Option<u64>,
    pub is_static: bool,
    pub vlan: u32,
    /// Full display name (`Ethernet1`).
    pub interface: String,
}

/// One VLAN's DHCP snooping counters.
#[derive(Debug, Clone, Serialize)]
pub struct SnoopVlanStats {
    pub vlan: u32,
    pub packets: u64,
    pub dropped: u64,
}

/// `show dhcp snooping statistics`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct DhcpStatistics {
    pub vlans: Vec<SnoopVlanStats>,
    pub untrusted_server_drops: u64,
}

/// The DHCP snooping half of the snooping-security state.
#[derive(Debug, Clone, Serialize, Default)]
pub struct DhcpSnooping {
    pub vlans: Vec<u32>,
    /// Trusted interfaces, full display names.
    pub trusted: Vec<String>,
    pub bindings: Vec<SnoopBinding>,
    pub statistics: DhcpStatistics,
}

/// One VLAN's ARP inspection counters.
#[derive(Debug, Clone, Serialize)]
pub struct DaiVlanStats {
    pub vlan: u32,
    pub forwarded: u64,
    pub dropped: u64,
    pub bad_binding: u64,
    pub bad_src_mac: u64,
}

/// The ARP inspection half of the snooping-security state.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ArpInspection {
    pub vlans: Vec<u32>,
    /// Extra validations ("src-mac", "dst-mac", "ip").
    pub validate: Vec<String>,
    /// Trusted interfaces, full display names.
    pub trusted: Vec<String>,
    pub statistics: Vec<DaiVlanStats>,
}

/// The whole snooping-security state as orch reports it; the DHCP and
/// ARP shows each render their half.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SnoopState {
    pub dhcp: DhcpSnooping,
    pub arp: ArpInspection,
}
