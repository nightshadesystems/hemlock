//! Config intents: the typed slices of the config tree mgmtd knows how
//! to apply. ASIC ports (`interfaces { Ethernet1 { ... } }`) are pushed
//! to syncd; the OS-side families â€” management addressing (`interfaces
//! { Management1 { address ... } }`), static routes (`routing { static
//! { <prefix> <next-hop> } }`) and the SSH service (`system { ssh {
//! ... } }`) â€” go through the OS applier (`osapply`). The legacy
//! `ethernet <name>` keyed form is still accepted for configs persisted
//! before the format change.
//!
//! Each family stays a pure function from config tree to typed intent,
//! diffed against the running tree and pushed to the owning applier.

use std::collections::{BTreeMap, BTreeSet};

use hemlock_common::link::{self, Duplex};
use hemlock_config::{ConfigTree, Item};

/// Every intent family extracted from one config tree.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Intents {
    /// ASIC ports, keyed by interface name.
    pub ports: BTreeMap<String, InterfaceIntent>,
    /// Management (OS netdev) interfaces, keyed by interface name.
    pub management: BTreeMap<String, MgmtIntent>,
    pub ssh: SshIntent,
    /// Web console listeners (`system { http }` / `system { https }`).
    pub web: WebIntent,
    /// Static routes, keyed by canonical prefix. Repeated config lines
    /// per prefix merge into one ECMP next-hop set.
    pub routes: BTreeMap<String, StaticRoute>,
    /// Static ARP/ND entries (`routing { arp { ... } }`), keyed by
    /// canonical address text.
    pub arp_statics: BTreeMap<String, ArpStatic>,
    /// Global router identity (`routing { router-id ... }`). Absent =
    /// derived at render time (highest SVI address, then Management),
    /// never persisted.
    pub router_id: Option<String>,
    /// OSPFv2 (`routing { ospf { ... } }`), rendered into FRR.
    pub ospf: Option<OspfIntent>,
    /// BGP IPv4 unicast (`routing { bgp { ... } }`), rendered into FRR.
    pub bgp: Option<BgpIntent>,
    /// VRRP groups (`interfaces { <name> { vrrp <group> { ... } } }`),
    /// keyed by (interface, group): FRR's vrrpd + the OS macvlan + the
    /// syncd My-MAC entry.
    pub vrrp: BTreeMap<(String, u8), VrrpIntent>,
    /// VLANs (`vlans { vlan <id> { ... } }`), keyed by 802.1Q id.
    pub vlans: BTreeMap<u16, VlanIntent>,
    /// SVIs (`interfaces { Vlan<id> { address ... } }`), keyed by
    /// interface name.
    pub svis: BTreeMap<String, SviIntent>,
    /// Port-channels (`interfaces { Port-Channel<n> { ... } }`), keyed
    /// by channel-group number. Members live on the port intents
    /// (`channel_group`); [`Intents::lag_members`] assembles the map.
    pub lags: BTreeMap<u16, LagIntent>,
    /// Global LACP config (`protocols { lacp { ... } }`).
    pub lacp: LacpGlobalIntent,
    /// Spanning tree (`protocols { spanning-tree { ... } }`).
    pub stp: StpIntent,
    /// IGMP snooping (`protocols { igmp-snooping { ... } }`).
    pub igmp_snooping: SnoopingIntent,
    /// MLD snooping (`protocols { mld-snooping { ... } }`).
    pub mld_snooping: SnoopingIntent,
    /// MAC address table (`switching { mac-table { ... } }`).
    pub mac_table: MacTableIntent,
    /// Mirror sessions (`switching { mirror { session <n> { ... } } }`).
    pub mirror: BTreeMap<u8, MirrorIntent>,
    /// ACLs (`security { acl { <ipv4|ipv6|mac> <name> { ... } } }`),
    /// keyed by name â€” unique across all three families, because the
    /// binding syntax carries no family keyword.
    pub acls: BTreeMap<String, AclIntent>,
    /// CoPP class overrides (`security { copp { class <name> { ... } } }`),
    /// keyed by class name. Absent classes run at compiled defaults.
    pub copp: BTreeMap<String, CoppClassIntent>,
    /// 802.1X (`security { dot1x { ... } }`; per-port enables live on
    /// the port intents).
    pub dot1x: Dot1xIntent,
    /// DHCP snooping + dynamic ARP inspection (`security {
    /// dhcp-snooping ... arp-inspection ... }`; per-port trust flags
    /// live on the port intents).
    pub snoop_sec: SnoopSecIntent,
    /// LLDP (`services { lldp { ... } }`; per-port disables live on
    /// the port intents).
    pub lldp: LldpIntent,
    /// NTP client (`services { ntp { ... } }`), rendered into
    /// systemd-timesyncd.
    pub ntp: NtpIntent,
    /// SNMP agent (`services { snmp { ... } }`), rendered into
    /// snmpd.conf; orch's AgentX subagent serves the IF-MIB.
    pub snmp: SnmpIntent,
    /// sFlow (`services { sflow { ... } }`); syncd programs the ASIC
    /// sampler, orch exports the datagrams. Per-port disables live on
    /// the port intents.
    pub sflow: SflowIntent,
    /// DHCP relay servers per SVI (`interfaces { Vlan<id> {
    /// dhcp-relay server ... } }`), keyed by VLAN id. The relay is a
    /// capability of the snooping engine, so it rides that push.
    pub dhcp_relay: BTreeMap<u16, Vec<std::net::Ipv4Addr>>,
    /// The four global QoS maps (`qos { map { ... } }`).
    pub qos_maps: QosMapsIntent,
    /// Named WRED/ECN profiles (`qos { wred-profile <name> { ... } }`).
    pub wred_profiles: BTreeMap<String, WredProfileIntent>,
    /// Non-fatal commit notes (empty port-channels, MTU hints).
    pub warnings: Vec<String>,
}

/// One static ARP/ND entry: the address answers on `interface` at
/// `mac` (kernel: `ip neigh replace ... nud permanent`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArpStatic {
    pub interface: String,
    /// Colon-separated lowercase unicast MAC.
    pub mac: String,
}

/// OSPFv2 process config (`routing { ospf { ... } }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OspfIntent {
    /// Overrides the global router-id.
    pub router_id: Option<String>,
    /// Canonical dotted area id -> canonical network prefixes.
    pub areas: BTreeMap<String, BTreeSet<String>>,
    pub passive_interfaces: BTreeSet<String>,
    /// "connected" | "static" | "bgp".
    pub redistribute: BTreeSet<String>,
    /// 1..=8, capped at the probed ECMP width at commit.
    pub maximum_paths: u8,
    /// Per-interface knobs, stored under the protocol.
    pub interfaces: BTreeMap<String, OspfInterfaceIntent>,
}

impl Default for OspfIntent {
    fn default() -> Self {
        Self {
            router_id: None,
            areas: BTreeMap::new(),
            passive_interfaces: BTreeSet::new(),
            redistribute: BTreeSet::new(),
            maximum_paths: 4,
            interfaces: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OspfInterfaceIntent {
    pub cost: Option<u16>,
    pub hello_interval: Option<u16>,
    pub dead_interval: Option<u16>,
    pub priority: Option<u8>,
}

/// BGP process config (`routing { bgp { ... } }`), IPv4 unicast only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgpIntent {
    /// asplain, required.
    pub as_number: u32,
    pub router_id: Option<String>,
    /// Keyed by neighbor address text.
    pub neighbors: BTreeMap<String, BgpNeighborIntent>,
    pub networks: BTreeSet<String>,
    /// "connected" | "static" | "ospf".
    pub redistribute: BTreeSet<String>,
    pub maximum_paths: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BgpNeighborIntent {
    /// Required by commit.
    pub remote_as: Option<u32>,
    pub description: Option<String>,
    pub shutdown: bool,
    pub ebgp_multihop: Option<u8>,
    pub next_hop_self: bool,
}

/// One VRRP group (IPv4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VrrpIntent {
    /// Virtual addresses; at least one required by commit.
    pub addresses: BTreeSet<String>,
    /// 1..=254, default 100.
    pub priority: u8,
    /// Advertisement interval in seconds (1..=40), default 1.
    pub advertisement_interval: u8,
    /// Preempt is on by default; `no-preempt` turns it off.
    pub preempt: bool,
}

impl Default for VrrpIntent {
    fn default() -> Self {
        Self {
            addresses: BTreeSet::new(),
            priority: 100,
            advertisement_interval: 1,
            preempt: true,
        }
    }
}

/// One static route: the whole per-prefix state â€” an ECMP next-hop set
/// or a null route, plus the administrative distance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticRoute {
    /// Next hops; more than one = ECMP. Empty exactly when `drop`.
    pub next_hops: BTreeSet<String>,
    /// Null route (`<prefix> drop`): a kernel blackhole route.
    pub drop: bool,
    /// Administrative distance (rendered as the kernel metric).
    pub distance: u8,
}

impl Default for StaticRoute {
    fn default() -> Self {
        Self {
            next_hops: BTreeSet::new(),
            drop: false,
            distance: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SviIntent {
    /// Interface address in CIDR form; gives the VLAN a router
    /// interface (ASIC) and a kernel address on its bridge netdev.
    pub address: Option<String>,
    /// `mtu <bytes>` on the VLAN's bridge netdev. None = the default.
    pub mtu: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InterfaceIntent {
    /// None = leave the daemon default (up) untouched.
    pub admin_up: Option<bool>,
    pub description: Option<String>,
    /// Interface address in CIDR form; puts the port in L3 mode
    /// (router interface + routes in the ASIC, address on the port's
    /// hostif netdev).
    pub address: Option<String>,
    /// Explicit L2 switchport program; None = default (access, VLAN 1).
    pub switchport: Option<SwitchportIntent>,
    /// LAG membership (`channel-group <n> mode <...>`).
    pub channel_group: Option<ChannelGroup>,
    /// Per-member LACP tuning (`lacp { rate ...; port-priority ... }`).
    pub lacp: Option<PortLacpIntent>,
    /// Per-port spanning-tree config (`spanning-tree { ... }`).
    pub spanning_tree: Option<PortStpIntent>,
    /// Storm control levels, percent with two decimals, keyed by kind.
    pub storm_control: BTreeMap<StormKind, String>,
    /// ACL bindings (`access-group <name> <in|out>`).
    pub access_groups: AccessGroups,
    /// `port-security { ... }` (learn limit + violation action).
    pub port_security: Option<PortSecurityIntent>,
    /// `dot1x` marker: 802.1X authenticator on this port.
    pub dot1x: bool,
    /// `dhcp-snooping trust` / `arp-inspection trust` markers.
    pub dhcp_snooping_trust: bool,
    pub arp_inspection_trust: bool,
    /// `qos { ... }`: classification, scheduling, shaping, WRED.
    pub qos: Option<PortQosIntent>,
    /// `lldp disable`: LLDP runs on every port by default.
    pub lldp_disabled: bool,
    /// `sflow disable`: sampling runs on every port once a collector
    /// exists.
    pub sflow_disabled: bool,
    /// `speed <mbps>`: pinned line rate. None = `speed auto` or no
    /// leaf at all, which are the same thing to the ASIC.
    pub speed_mbps: Option<u32>,
    /// `duplex <full|half>`. None = `duplex auto` or no leaf.
    pub duplex: Option<Duplex>,
    /// `mtu <bytes>`. None = the platform default.
    pub mtu: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VlanIntent {
    pub description: Option<String>,
    /// `state suspend`: the VLAN exists but forwards nothing.
    pub suspended: bool,
}

/// A port's L2 switchport mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SwitchportMode {
    #[default]
    Access,
    Trunk,
    /// QinQ tunnel port: the S-VLAN is the access VLAN.
    Dot1qTunnel,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SwitchportIntent {
    pub mode: SwitchportMode,
    /// None = default VLAN (1).
    pub access_vlan: Option<u16>,
    /// Allowed tagged VLANs in trunk mode.
    pub trunk_vlans: Vec<u16>,
    /// None = default VLAN (1).
    pub native_vlan: Option<u16>,
}

/// `channel-group <n> mode <active|passive|on>` on a member port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelGroup {
    pub group: u16,
    pub mode: LacpMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LacpMode {
    Active,
    Passive,
    /// Static aggregation, no LACP.
    On,
}

impl LacpMode {
    pub fn word(self) -> &'static str {
        match self {
            LacpMode::Active => "active",
            LacpMode::Passive => "passive",
            LacpMode::On => "on",
        }
    }
}

/// Per-member LACP tuning.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PortLacpIntent {
    /// `rate fast` = 1s LACPDUs; default (normal) = 30s.
    pub rate_fast: bool,
    /// None = default (32768).
    pub port_priority: Option<u16>,
}

/// One `Port-Channel<n>` interface.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LagIntent {
    pub admin_up: Option<bool>,
    pub description: Option<String>,
    /// Explicit L2 switchport program; None = default (access, VLAN 1).
    pub switchport: Option<SwitchportIntent>,
    /// Minimum bundled members for the LAG to come up; None = default (0).
    pub min_links: Option<u8>,
    /// LACP fallback when no partner is heard.
    pub fallback: Option<LagFallback>,
    /// Fallback timeout in seconds; None = default (90).
    pub fallback_timeout: Option<u16>,
    pub spanning_tree: Option<PortStpIntent>,
    pub storm_control: BTreeMap<StormKind, String>,
    /// ACL bindings (`access-group <name> <in|out>`; syncd expands
    /// them to the member ports).
    pub access_groups: AccessGroups,
    /// `dhcp-snooping trust` / `arp-inspection trust` markers.
    pub dhcp_snooping_trust: bool,
    pub arp_inspection_trust: bool,
    /// `qos { ... }`: applies to every member port.
    pub qos: Option<PortQosIntent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LagFallback {
    /// The whole LAG forwards as a static bundle.
    Static,
    /// Members forward as individual ports.
    Individual,
}

/// `protocols { lacp { system-priority <n> } }`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LacpGlobalIntent {
    /// None = default (32768).
    pub system_priority: Option<u16>,
}

/// Global spanning-tree config. Absent leaves keep the defaults
/// (mstp, priority 32768, timers 2/20/15).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StpIntent {
    pub mode: StpMode,
    /// Bridge priority, multiple of 4096; None = default (32768).
    pub priority: Option<u16>,
    /// None = default (2).
    pub hello_time: Option<u8>,
    /// None = default (20).
    pub max_age: Option<u8>,
    /// None = default (15).
    pub forward_time: Option<u8>,
    pub mst_name: Option<String>,
    pub mst_revision: Option<u16>,
    /// MST instance -> mapped VLANs (sorted). Instance 0 is implicit
    /// (all unmapped VLANs).
    pub instances: BTreeMap<u8, Vec<u16>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StpMode {
    #[default]
    Mstp,
    Rstp,
    None,
}

/// Per-port spanning-tree config.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PortStpIntent {
    pub portfast: bool,
    pub bpduguard: bool,
    /// None = default (speed-derived).
    pub cost: Option<u32>,
    /// Multiple of 16; None = default (128).
    pub port_priority: Option<u8>,
}

/// Storm-control traffic class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StormKind {
    Broadcast,
    Multicast,
    UnknownUnicast,
}

impl StormKind {
    pub fn word(self) -> &'static str {
        match self {
            StormKind::Broadcast => "broadcast",
            StormKind::Multicast => "multicast",
            StormKind::UnknownUnicast => "unknown-unicast",
        }
    }
}

/// IGMP or MLD snooping (the families share one shape). Snooping is
/// globally enabled by default; `disable` is the off switch.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnoopingIntent {
    pub disabled: bool,
    /// None = default (2).
    pub robustness: Option<u8>,
    pub vlans: BTreeMap<u16, SnoopVlanIntent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnoopVlanIntent {
    pub disabled: bool,
    pub fast_leave: bool,
    /// Local querier enabled on this VLAN.
    pub querier: bool,
    /// Querier source address; None = derive from the SVI.
    pub querier_address: Option<String>,
    /// Static mrouter ports, sorted.
    pub mrouters: Vec<String>,
}

/// Global LLDP config (`services { lldp { ... } }`). LLDP runs by
/// default; `disable` is the off switch, exactly like snooping.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LldpIntent {
    pub disabled: bool,
    /// Seconds between advertisements; None = default (30).
    pub tx_interval: Option<u16>,
    /// TTL multiplier; None = default (4).
    pub hold_multiplier: Option<u8>,
}

/// NTP client config (`services { ntp { ... } }`). No servers = the
/// client is off (timesyncd stops).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NtpIntent {
    /// Servers in config order, deduplicated: IPv4/IPv6 literals or
    /// hostnames.
    pub servers: Vec<String>,
}

/// The management interface's address, without its prefix length —
/// what snmpd binds to and what `show snmp` names.
pub fn management_address(intents: &Intents) -> Option<String> {
    intents
        .management
        .values()
        .filter_map(|mgmt| mgmt.address.as_deref())
        .filter_map(|cidr| cidr.split('/').next())
        .map(str::to_string)
        .next()
}

/// The management interface's display name (the first configured one).
pub fn management_name(intents: &Intents) -> Option<String> {
    intents
        .management
        .iter()
        .find(|(_, mgmt)| mgmt.address.is_some())
        .map(|(name, _)| name.clone())
}

/// One sFlow collector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SflowCollector {
    pub address: String,
    /// None = the sFlow default (6343).
    pub port: Option<u16>,
}

/// sFlow config (`services { sflow { ... } }`). Sampling is off until
/// a collector exists — a sampler with nowhere to send costs CPU and
/// buys nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SflowIntent {
    /// In config order; at most [`MAX_SFLOW_COLLECTORS`].
    pub collectors: Vec<SflowCollector>,
    /// 1-in-N; None = default (16384).
    pub sample_rate: Option<u32>,
    /// Seconds; None = default (30).
    pub polling_interval: Option<u16>,
}

impl SflowIntent {
    /// Anything configured at all (a rate or a polling interval on
    /// their own still need a collector).
    pub fn is_set(&self) -> bool {
        *self != SflowIntent::default()
    }

    pub fn enabled(&self) -> bool {
        !self.collectors.is_empty()
    }

    pub fn rate(&self) -> u32 {
        self.sample_rate.unwrap_or(DEFAULT_SFLOW_SAMPLE_RATE)
    }

    pub fn polling(&self) -> u16 {
        self.polling_interval.unwrap_or(DEFAULT_SFLOW_POLLING)
    }
}

/// The default 1-in-N sampling rate.
pub const DEFAULT_SFLOW_SAMPLE_RATE: u32 = 16384;

/// The default counter-poll interval, in seconds.
pub const DEFAULT_SFLOW_POLLING: u16 = 30;

/// The most collectors a datagram is duplicated to.
pub const MAX_SFLOW_COLLECTORS: usize = 2;

/// The sampling-rate range, both ends inclusive and both powers of two.
pub const MIN_SFLOW_SAMPLE_RATE: u32 = 256;
pub const MAX_SFLOW_SAMPLE_RATE: u32 = 1_048_576;

/// The valid rate nearest below and above `rate`, so a rejection can
/// name what the operator probably meant.
pub fn nearest_sample_rates(rate: u32) -> (u32, u32) {
    let mut below = MIN_SFLOW_SAMPLE_RATE;
    let mut above = MAX_SFLOW_SAMPLE_RATE;
    let mut candidate = MIN_SFLOW_SAMPLE_RATE;
    while candidate <= MAX_SFLOW_SAMPLE_RATE {
        if candidate <= rate {
            below = candidate;
        }
        if candidate >= rate {
            above = candidate;
            break;
        }
        candidate *= 2;
    }
    (below, above)
}

/// One SNMP v3 USM user. Read-only `authPriv` only: SHA auth, AES
/// privacy, no write access (deferred, rejected at parse).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnmpUser {
    pub auth_password: String,
    pub priv_password: String,
}

/// One v2c read-only community.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnmpCommunityIntent {
    pub name: String,
    /// Source prefix the community answers on; None = anywhere.
    pub source: Option<String>,
}

/// SNMP agent config (`services { snmp { ... } }`). Absent block = the
/// agent is off; `enabled` records that the block existed at all, so a
/// bare `snmp { }` still starts a (community-less) agent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnmpIntent {
    pub enabled: bool,
    pub location: Option<String>,
    pub contact: Option<String>,
    /// v2c read-only communities, in config order — snmpd evaluates
    /// its `rocommunity` lines in order, so an operator putting a
    /// source-scoped name ahead of an open one means it.
    pub communities: Vec<SnmpCommunityIntent>,
    /// v3 USM users, keyed by name.
    pub users: BTreeMap<String, SnmpUser>,
}

/// SNMP community and USM user names: a letter, then letters, digits,
/// `_` or `-`, at most 32 characters.
pub fn valid_snmp_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The shortest v3 passphrase USM accepts.
pub const MIN_SNMP_PASSWORD: usize = 8;

/// The most NTP servers timesyncd is given (it walks the list in
/// order, so a longer one only slows failover).
pub const MAX_NTP_SERVERS: usize = 4;

/// `switching { mac-table { ... } }`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MacTableIntent {
    /// Seconds; 0 = no aging; None = default (300).
    pub aging_time: Option<u32>,
    /// Static entries keyed by (canonical MAC, VLAN id).
    pub statics: BTreeMap<(String, u16), FdbTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FdbTarget {
    Port(String),
    Drop,
}

/// One mirror session (`switching { mirror { session <n> { ... } } }`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MirrorIntent {
    /// Source port -> mirrored direction.
    pub sources: BTreeMap<String, MirrorDirection>,
    pub destination: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MirrorDirection {
    Rx,
    Tx,
    #[default]
    Both,
}

/// One named ACL (`security { acl { <family> <name> { ... } } }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclIntent {
    pub family: AclFamily,
    /// Ordered rules keyed by rule number.
    pub rules: BTreeMap<u32, AclRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclFamily {
    Ipv4,
    Ipv6,
    Mac,
}

impl AclFamily {
    pub fn word(self) -> &'static str {
        match self {
            AclFamily::Ipv4 => "ipv4",
            AclFamily::Ipv6 => "ipv6",
            AclFamily::Mac => "mac",
        }
    }
}

/// One ACL rule. Field applicability follows the family; the extractor
/// rejects out-of-family fields. `permit` is required by commit
/// (`permit;` or `deny;` marker leaf).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AclRule {
    pub permit: bool,
    /// IP protocol number (`tcp` = 6, `udp` = 17, `icmp` = 1).
    pub protocol: Option<u8>,
    /// Canonical prefixes; None = any.
    pub source: Option<String>,
    pub destination: Option<String>,
    /// Inclusive port ranges (low == high for one port).
    pub source_port: Option<(u16, u16)>,
    pub destination_port: Option<(u16, u16)>,
    pub dscp: Option<u8>,
    /// Trap+syslog on match, rate-limited via the CoPP acl-log class.
    pub log: bool,
    pub police: Option<AclPolice>,
    /// (canonical MAC, canonical and-mask) pairs.
    pub source_mac: Option<(String, String)>,
    pub destination_mac: Option<(String, String)>,
    pub ethertype: Option<u16>,
}

/// A rule policer: bits/sec (burst in bytes) or, with `pps`,
/// packets/sec (burst in packets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AclPolice {
    pub rate: u64,
    pub burst: u64,
    pub pps: bool,
}

/// A port's ACL bindings (`access-group <name> <in|out>`): one per
/// direction.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccessGroups {
    pub ingress: Option<String>,
    pub egress: Option<String>,
}

impl AccessGroups {
    pub fn is_empty(&self) -> bool {
        self.ingress.is_none() && self.egress.is_none()
    }
}

/// One CoPP class override; absent values keep the compiled default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CoppClassIntent {
    pub rate: Option<u32>,
    pub burst: Option<u32>,
}

/// Egress unicast queues per front-panel port. The Helix4 exposes
/// eight; the config grammar and the strict-priority contiguity rule
/// both key off it.
pub const QOS_QUEUE_COUNT: u8 = 8;

/// The compiled CoPP class names (`security copp class <name>`); the
/// full table with default rates lives in syncd.
pub const COPP_CLASS_NAMES: &[&str] = &[
    "bpdu", "lacp", "eapol", "igmp", "mld", "arp", "dhcp", "ospf", "bgp", "vrrp", "ip2me",
    "acl-log", "default",
];

/// `security { dot1x { ... } }`. Per-port enables live on the port
/// intents (`dot1x` marker).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dot1xIntent {
    /// RADIUS servers in config order (tried in order).
    pub radius_servers: Vec<RadiusServer>,
    /// Seconds; 0 = reauthentication off (the default).
    pub reauth_interval: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadiusServer {
    pub ip: String,
    /// Shared secret; required by commit when any port enables dot1x.
    pub key: Option<String>,
    /// Default 1812.
    pub port: u16,
    /// Seconds; default 5.
    pub timeout: u8,
    /// Default 3.
    pub retransmit: u8,
}

impl Default for RadiusServer {
    fn default() -> Self {
        Self {
            ip: String::new(),
            key: None,
            port: 1812,
            timeout: 5,
            retransmit: 3,
        }
    }
}

/// DHCP snooping + dynamic ARP inspection. Per-port trust flags live
/// on the port and LAG intents.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnoopSecIntent {
    pub dhcp_vlans: BTreeSet<u16>,
    pub arp_vlans: BTreeSet<u16>,
    /// DAI validation checks; empty = default (src-mac).
    pub validate: BTreeSet<ArpValidate>,
    /// Static bindings keyed by (canonical MAC, VLAN).
    pub static_bindings: BTreeMap<(String, u16), StaticBinding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArpValidate {
    SrcMac,
    DstMac,
    Ip,
}

impl ArpValidate {
    pub fn word(self) -> &'static str {
        match self {
            ArpValidate::SrcMac => "src-mac",
            ArpValidate::DstMac => "dst-mac",
            ArpValidate::Ip => "ip",
        }
    }
}

/// One static DHCP-snooping binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticBinding {
    /// IPv4 address text.
    pub address: String,
    pub interface: String,
}

/// `port-security { maximum <n>; violation <protect|shutdown> }` on a
/// port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortSecurityIntent {
    /// Default 1.
    pub maximum: u32,
    /// Violation action: true = shutdown (errdisable); default protect.
    pub shutdown: bool,
}

impl Default for PortSecurityIntent {
    fn default() -> Self {
        Self {
            maximum: 1,
            shutdown: false,
        }
    }
}

/// The four global QoS map tables. Empty tables mean the defaults:
/// unmapped DSCP/CoS values land in TC 0, and a TC with no rewrite entry
/// leaves the packet's markings alone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QosMapsIntent {
    /// DSCP (0..63) -> traffic class (0..7).
    pub dscp_to_tc: BTreeMap<u8, u8>,
    /// 802.1p CoS (0..7) -> traffic class.
    pub cos_to_tc: BTreeMap<u8, u8>,
    /// Traffic class -> DSCP rewrite.
    pub tc_to_dscp: BTreeMap<u8, u8>,
    /// Traffic class -> CoS rewrite.
    pub tc_to_cos: BTreeMap<u8, u8>,
}

impl QosMapsIntent {
    pub fn is_empty(&self) -> bool {
        self.dscp_to_tc.is_empty()
            && self.cos_to_tc.is_empty()
            && self.tc_to_dscp.is_empty()
            && self.tc_to_cos.is_empty()
    }
}

/// One named WRED/ECN profile. Both thresholds are required by commit
/// once a queue references the profile.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WredProfileIntent {
    /// KB.
    pub min_threshold: Option<u32>,
    pub max_threshold: Option<u32>,
    /// Percent at max-threshold; default 10.
    pub drop_probability: u32,
    /// Mark instead of drop for ECT traffic.
    pub ecn: bool,
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
}

/// A port's whole QoS program (`interfaces { <name> { qos { ... } } }`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PortQosIntent {
    pub trust: QosTrust,
    pub default_tc: u8,
    /// Port shaper in bits/sec.
    pub shape: Option<u64>,
    /// Queues carrying non-default config, keyed by queue index.
    pub queues: BTreeMap<u8, QueueQosIntent>,
}

/// One egress queue's config. Absent = the platform default (DWRR
/// weight 1, unshaped, no WRED).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueueQosIntent {
    /// `priority strict`.
    pub strict: bool,
    /// DWRR weight 1..127; None = the default 1.
    pub weight: Option<u8>,
    /// Queue shaper in bits/sec.
    pub shape: Option<u64>,
    pub wred_profile: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MgmtIntent {
    /// None = leave the netdev alone.
    pub admin_up: Option<bool>,
    /// Primary address in CIDR form; puts the interface in L3 mode.
    pub address: Option<String>,
    /// `mtu <bytes>` on the management netdev. None = leave it alone.
    pub mtu: Option<u32>,
}

/// `system { ssh { ... } }` â€” SSH is on exactly when the block exists.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SshIntent {
    pub enabled: bool,
    /// `authentication local`: password logins against the on-box user
    /// database (PAM).
    pub auth_local: bool,
}

/// The web console: each listener is on exactly when its `system { http }`
/// / `system { https }` block exists. hemlock-webd reads the running
/// config itself; mgmtd only starts/stops the unit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WebIntent {
    pub http: bool,
    pub https: bool,
}

impl WebIntent {
    pub fn enabled(&self) -> bool {
        self.http || self.https
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IntentError {
    #[error("interfaces: {0}")]
    BadInterfaceBlock(String),

    #[error("interface {name}: admin-state must be `enabled` or `disabled`, got {value:?}")]
    BadAdminState { name: String, value: String },

    #[error("interface {name}: duplicate interface block")]
    Duplicate { name: String },

    #[error("interface {name}: bad address: {reason}")]
    BadAddress { name: String, reason: String },

    #[error("interface {name}: {reason}")]
    BadLinkParam { name: String, reason: String },

    #[error("interface {name}: switchport: {reason}")]
    BadSwitchport { name: String, reason: String },

    #[error("interface {name}: address and switchport are mutually exclusive")]
    AddressSwitchportConflict { name: String },

    #[error("vlans: {0}")]
    BadVlanBlock(String),

    #[error("vlan {id}: {reason}")]
    BadVlan { id: String, reason: String },

    #[error("system ssh: {0}")]
    BadSsh(String),

    #[error("routing: {0}")]
    BadRouting(String),

    #[error("route {prefix}: {reason}")]
    BadRoute { prefix: String, reason: String },

    #[error("arp {ip}: {reason}")]
    BadArp { ip: String, reason: String },

    #[error("routing ospf: {0}")]
    BadOspf(String),

    #[error("routing bgp: {0}")]
    BadBgp(String),

    #[error("vrrp {interface} group {group}: {reason}")]
    BadVrrp {
        interface: String,
        group: String,
        reason: String,
    },

    #[error("interface {name}: {reason}")]
    BadLag { name: String, reason: String },

    #[error("interface {name}: channel-group: {reason}")]
    BadChannelGroup { name: String, reason: String },

    #[error("{member}: member of Port-Channel{group}; configure the Port-Channel")]
    MemberConfigConflict { member: String, group: u16 },

    #[error("interface {name}: address and channel-group are mutually exclusive")]
    AddressChannelGroupConflict { name: String },

    #[error("spanning-tree: {0}")]
    BadStp(String),

    #[error("interface {name}: spanning-tree: {reason}")]
    BadPortStp { name: String, reason: String },

    #[error("interface {name}: storm-control: {reason}")]
    BadStormControl { name: String, reason: String },

    #[error("{family}: {reason}")]
    BadSnooping {
        family: &'static str,
        reason: String,
    },

    #[error("mac-table: {0}")]
    BadMacTable(String),

    #[error("mirror: {0}")]
    BadMirrorBlock(String),

    #[error("mirror session {session}: {reason}")]
    BadMirror { session: u8, reason: String },

    #[error("protocols: {0}")]
    BadProtocols(String),

    #[error("switching: {0}")]
    BadSwitching(String),

    #[error("security: {0}")]
    BadSecurity(String),

    #[error("security acl {name}: {reason}")]
    BadAcl { name: String, reason: String },

    #[error("security acl {name} rule {rule}: {reason}")]
    BadAclRule {
        name: String,
        rule: String,
        reason: String,
    },

    #[error("interface {name}: access-group: {reason}")]
    BadAccessGroup { name: String, reason: String },

    #[error("{member}: member of Port-Channel{group}; bind on the Port-Channel")]
    AccessGroupOnMember { member: String, group: u16 },

    #[error("security copp: {0}")]
    BadCopp(String),

    #[error("security dot1x: {0}")]
    BadDot1x(String),

    #[error("interface {name}: port-security: {reason}")]
    BadPortSecurity { name: String, reason: String },

    #[error("security dhcp-snooping: {0}")]
    BadDhcpSnooping(String),

    #[error("security arp-inspection: {0}")]
    BadArpInspection(String),

    #[error("services: {0}")]
    BadServices(String),

    #[error("services lldp: {0}")]
    BadLldp(String),

    #[error("services ntp: {0}")]
    BadNtp(String),

    #[error("services snmp: {0}")]
    BadSnmp(String),

    #[error("services sflow: {0}")]
    BadSflow(String),

    #[error("{name}: dhcp-relay: {reason}")]
    BadDhcpRelay { name: String, reason: String },

    #[error("{name}: {feature} is a physical-port setting")]
    PortServiceOnNonPort { name: String, feature: &'static str },

    #[error("qos: {0}")]
    BadQos(String),

    #[error("qos map {table}: {reason}")]
    BadQosMap { table: String, reason: String },

    #[error("qos wred-profile {name}: {reason}")]
    BadWredProfile { name: String, reason: String },

    #[error("interface {name}: qos: {reason}")]
    BadPortQos { name: String, reason: String },

    #[error("{name} queue {queue}: {reason}")]
    BadQueueQos {
        name: String,
        queue: String,
        reason: String,
    },

    #[error("strict queues must be the highest-numbered queues")]
    StrictQueueOrder,
}

/// Extract every intent family from a config tree.
pub fn extract(tree: &ConfigTree) -> Result<Intents, IntentError> {
    let mut intents = Intents {
        ssh: ssh(tree)?,
        web: web(tree),
        vlans: vlans(tree)?,
        ..Intents::default()
    };
    routing(tree, &mut intents)?;
    protocols(tree, &mut intents)?;
    switching(tree, &mut intents)?;
    security(tree, &mut intents)?;
    services(tree, &mut intents)?;
    qos(tree, &mut intents)?;
    let Some((_, items)) = tree.block("interfaces") else {
        finish_validation(&mut intents)?;
        return Ok(intents);
    };

    for item in items {
        let Item::Block {
            name,
            keys,
            children,
        } = item
        else {
            continue;
        };
        enum Kind {
            Port,
            Management,
            Vlan,
            Lag,
        }
        let (kind, ifname) = match (name.as_str(), keys.as_slice()) {
            // Legacy keyed forms: `ethernet <name> { ... }`.
            ("ethernet", [key]) => (Kind::Port, key.clone()),
            ("management", [key]) => (Kind::Management, key.clone()),
            ("ethernet" | "management", _) => {
                return Err(IntentError::BadInterfaceBlock(format!(
                    "{name} block needs exactly one name key"
                )));
            }
            // Current form: the interface name is the block name.
            (n, []) if n.starts_with("Management") => (Kind::Management, name.clone()),
            (n, []) if n.starts_with("Ethernet") => (Kind::Port, name.clone()),
            (n, []) if n.starts_with("Vlan") => (Kind::Vlan, name.clone()),
            (n, []) if n.starts_with("Port-Channel") => (Kind::Lag, name.clone()),
            (n, _) => {
                return Err(IntentError::BadInterfaceBlock(format!(
                    "unrecognized interface block {n:?}"
                )));
            }
        };

        let admin_up = admin_state(children, &ifname)?;

        let address = match ConfigTree::leaf_value(children, "address") {
            Some(value) => {
                hemlock_common::net::parse_cidr(value).map_err(|reason| {
                    IntentError::BadAddress {
                        name: ifname.clone(),
                        reason,
                    }
                })?;
                Some(value.to_string())
            }
            None => None,
        };

        for (group, vrrp) in vrrp_groups(children, &ifname)? {
            if matches!(kind, Kind::Management | Kind::Lag) {
                return Err(IntentError::BadVrrp {
                    interface: ifname.clone(),
                    group: group.to_string(),
                    reason: "vrrp runs on L3 ports and SVIs only".into(),
                });
            }
            intents.vrrp.insert((ifname.clone(), group), vrrp);
        }

        match kind {
            Kind::Port => {
                let switchport = switchport(children, &ifname)?;
                if switchport.is_some() && address.is_some() {
                    return Err(IntentError::AddressSwitchportConflict { name: ifname });
                }
                let channel_group = channel_group(children, &ifname)?;
                if channel_group.is_some() && address.is_some() {
                    return Err(IntentError::AddressChannelGroupConflict { name: ifname });
                }
                let intent = InterfaceIntent {
                    admin_up,
                    description: ConfigTree::leaf_value(children, "description")
                        .map(str::to_string),
                    address,
                    switchport,
                    channel_group,
                    lacp: port_lacp(children, &ifname)?,
                    spanning_tree: port_stp(children, &ifname)?,
                    storm_control: storm_control(children, &ifname)?,
                    access_groups: access_groups(children, &ifname)?,
                    port_security: port_security(children, &ifname)?,
                    dot1x: ConfigTree::has_leaf(children, "dot1x"),
                    dhcp_snooping_trust: ConfigTree::has_phrase(children, "dhcp-snooping", "trust"),
                    arp_inspection_trust: ConfigTree::has_phrase(
                        children,
                        "arp-inspection",
                        "trust",
                    ),
                    qos: port_qos(children, &ifname)?,
                    lldp_disabled: port_lldp(children, &ifname)?,
                    sflow_disabled: port_service_disable(children, &ifname, "sflow")?,
                    speed_mbps: speed(children, &ifname)?,
                    duplex: duplex(children, &ifname)?,
                    mtu: mtu(children, &ifname)?,
                };
                no_dhcp_relay(children, &ifname)?;
                if intent.dot1x && intent.port_security.is_some() {
                    return Err(IntentError::BadPortSecurity {
                        name: ifname,
                        reason: "port-security and dot1x are mutually exclusive".into(),
                    });
                }
                if intents.ports.insert(ifname.clone(), intent).is_some() {
                    return Err(IntentError::Duplicate { name: ifname });
                }
            }
            Kind::Lag => {
                let group = ifname
                    .strip_prefix("Port-Channel")
                    .and_then(|d| d.parse::<u16>().ok())
                    .filter(|n| (1..=64).contains(n))
                    .ok_or_else(|| {
                        IntentError::BadInterfaceBlock(format!(
                            "bad port-channel name {ifname:?} (Port-Channel1..Port-Channel64)"
                        ))
                    })?;
                if address.is_some() {
                    return Err(IntentError::BadLag {
                        name: ifname,
                        reason: "port-channels are L2 interfaces (no address)".into(),
                    });
                }
                if ConfigTree::leaf_values(children, "channel-group").is_some() {
                    return Err(IntentError::BadLag {
                        name: ifname,
                        reason: "a port-channel cannot join a channel-group".into(),
                    });
                }
                if ConfigTree::has_leaf(children, "dot1x") {
                    return Err(IntentError::BadDot1x(format!(
                        "{ifname}: dot1x runs on physical ports only"
                    )));
                }
                no_port_services(children, &ifname)?;
                if ConfigTree::blocks_named(children, "port-security")
                    .next()
                    .is_some()
                {
                    return Err(IntentError::BadPortSecurity {
                        name: ifname,
                        reason: "port-security runs on physical ports only".into(),
                    });
                }
                no_link_pinning(children, &ifname, "port-channel interfaces")?;
                if ConfigTree::has_leaf(children, "mtu") {
                    return Err(IntentError::BadLinkParam {
                        name: ifname,
                        reason: "mtu follows the member ports; set it on those".into(),
                    });
                }
                no_dhcp_relay(children, &ifname)?;
                let intent = LagIntent {
                    admin_up,
                    description: ConfigTree::leaf_value(children, "description")
                        .map(str::to_string),
                    switchport: switchport(children, &ifname)?,
                    min_links: lag_min_links(children, &ifname)?,
                    fallback: lag_fallback(children, &ifname)?,
                    fallback_timeout: lag_fallback_timeout(children, &ifname)?,
                    spanning_tree: port_stp(children, &ifname)?,
                    storm_control: storm_control(children, &ifname)?,
                    access_groups: access_groups(children, &ifname)?,
                    dhcp_snooping_trust: ConfigTree::has_phrase(children, "dhcp-snooping", "trust"),
                    arp_inspection_trust: ConfigTree::has_phrase(
                        children,
                        "arp-inspection",
                        "trust",
                    ),
                    qos: port_qos(children, &ifname)?,
                };
                if intents.lags.insert(group, intent).is_some() {
                    return Err(IntentError::Duplicate { name: ifname });
                }
            }
            Kind::Management => {
                if ConfigTree::blocks_named(children, "qos").next().is_some() {
                    return Err(IntentError::BadPortQos {
                        name: ifname,
                        reason: "QoS is a front-panel concept (not supported on Management)".into(),
                    });
                }
                for (block, what) in [
                    ("switchport", "switchports"),
                    ("spanning-tree", "spanning-tree ports"),
                    ("storm-control", "storm-control ports"),
                    ("lacp", "LACP ports"),
                ] {
                    if ConfigTree::blocks_named(children, block).next().is_some() {
                        return Err(IntentError::BadSwitchport {
                            name: ifname,
                            reason: format!("management ports are not {what}"),
                        });
                    }
                }
                if ConfigTree::leaf_values(children, "channel-group").is_some() {
                    return Err(IntentError::BadChannelGroup {
                        name: ifname,
                        reason: "management ports cannot join a channel-group".into(),
                    });
                }
                if ConfigTree::blocks_named(children, "port-security")
                    .next()
                    .is_some()
                {
                    return Err(IntentError::BadPortSecurity {
                        name: ifname,
                        reason: "port-security is not supported on Management".into(),
                    });
                }
                if ConfigTree::has_leaf(children, "dot1x") {
                    return Err(IntentError::BadDot1x(format!(
                        "{ifname}: dot1x is not supported on Management"
                    )));
                }
                no_port_services(children, &ifname)?;
                if ConfigTree::has_leaf(children, "access-group") {
                    return Err(IntentError::BadAccessGroup {
                        name: ifname,
                        reason: "management ports take no ACL bindings".into(),
                    });
                }
                no_link_pinning(children, &ifname, "management ports")?;
                no_dhcp_relay(children, &ifname)?;
                let intent = MgmtIntent {
                    admin_up,
                    address,
                    mtu: mtu(children, &ifname)?,
                };
                if intents.management.insert(ifname.clone(), intent).is_some() {
                    return Err(IntentError::Duplicate { name: ifname });
                }
            }
            Kind::Vlan => {
                let id = ifname
                    .strip_prefix("Vlan")
                    .and_then(|d| d.parse::<u16>().ok())
                    .filter(|id| (1..=4094).contains(id))
                    .ok_or_else(|| {
                        IntentError::BadInterfaceBlock(format!(
                            "bad VLAN interface name {ifname:?} (Vlan1..Vlan4094)"
                        ))
                    })?;
                if ConfigTree::blocks_named(children, "switchport")
                    .next()
                    .is_some()
                {
                    return Err(IntentError::BadSwitchport {
                        name: ifname,
                        reason: "VLAN interfaces are not switchports".into(),
                    });
                }
                if ConfigTree::blocks_named(children, "qos").next().is_some() {
                    return Err(IntentError::BadPortQos {
                        name: ifname,
                        reason:
                            "QoS is a front-panel concept; classification happens at the physical port"
                                .into(),
                    });
                }
                // An SVI needs its VLAN to exist (the default VLAN 1
                // always does).
                if address.is_some() && id != 1 && !intents.vlans.contains_key(&id) {
                    return Err(IntentError::BadInterfaceBlock(format!(
                        "{ifname}: VLAN {id} is not defined (set vlans vlan {id})"
                    )));
                }
                no_link_pinning(children, &ifname, "VLAN interfaces")?;
                no_port_services(children, &ifname)?;
                let relay = dhcp_relay_servers(children, &ifname)?;
                if !relay.is_empty() {
                    intents.dhcp_relay.insert(id, relay);
                }
                let intent = SviIntent {
                    address,
                    mtu: mtu(children, &ifname)?,
                };
                if intents.svis.insert(ifname.clone(), intent).is_some() {
                    return Err(IntentError::Duplicate { name: ifname });
                }
            }
        }
    }
    finish_validation(&mut intents)?;
    Ok(intents)
}

/// `vrrp <group> { address ...; priority; advertisement-interval;
/// no-preempt }` blocks of one interface.
fn vrrp_groups(children: &[Item], ifname: &str) -> Result<Vec<(u8, VrrpIntent)>, IntentError> {
    let mut groups = Vec::new();
    for (keys, body) in ConfigTree::blocks_named(children, "vrrp") {
        let bad_group = |group: &str, reason: String| IntentError::BadVrrp {
            interface: ifname.to_string(),
            group: group.to_string(),
            reason,
        };
        let [group_text] = keys else {
            return Err(bad_group("?", "expected `vrrp <1-255>`".into()));
        };
        let group =
            parse_int::<u8>(group_text, 1..=255, "group").map_err(|e| bad_group(group_text, e))?;
        let bad = |reason: String| bad_group(group_text, reason);
        let mut vrrp = VrrpIntent::default();
        for item in body {
            let Item::Leaf { name, values } = item else {
                return Err(bad(format!("unrecognized block {:?}", item.name())));
            };
            match (name.as_str(), values.as_slice()) {
                ("address", [address]) => {
                    let vip: std::net::Ipv4Addr = address
                        .parse()
                        .map_err(|_| bad(format!("bad address {address:?} (IPv4)")))?;
                    vrrp.addresses.insert(vip.to_string());
                }
                ("priority", [priority]) => {
                    vrrp.priority = parse_int::<u8>(priority, 1..=254, "priority").map_err(&bad)?;
                }
                ("advertisement-interval", [interval]) => {
                    vrrp.advertisement_interval =
                        parse_int::<u8>(interval, 1..=40, "advertisement-interval")
                            .map_err(&bad)?;
                }
                ("no-preempt", []) => vrrp.preempt = false,
                _ => {
                    return Err(bad(format!("unrecognized statement {name:?}")));
                }
            }
        }
        groups.push((group, vrrp));
    }
    Ok(groups)
}

/// `qos { map { ... } wred-profile <name> { ... } }`.
fn qos(tree: &ConfigTree, intents: &mut Intents) -> Result<(), IntentError> {
    let Some((_, items)) = tree.block("qos") else {
        return Ok(());
    };
    for item in items {
        let Item::Block {
            name,
            keys,
            children,
        } = item
        else {
            return Err(IntentError::BadQos(format!(
                "unrecognized statement {:?}",
                item.name()
            )));
        };
        match (name.as_str(), keys.as_slice()) {
            ("map", []) => qos_maps(children, intents)?,
            ("wred-profile", [profile]) => {
                let intent = wred_profile(profile, children)?;
                if intents
                    .wred_profiles
                    .insert(profile.clone(), intent)
                    .is_some()
                {
                    return Err(IntentError::BadWredProfile {
                        name: profile.clone(),
                        reason: "duplicate profile".into(),
                    });
                }
            }
            ("wred-profile", _) => {
                return Err(IntentError::BadQos(
                    "wred-profile block needs exactly one name key".into(),
                ));
            }
            (other, _) => {
                return Err(IntentError::BadQos(format!("unrecognized block {other:?}")));
            }
        }
    }
    Ok(())
}

/// WRED/ECN profile name syntax: letter first, then letters/digits/`_`/
/// `-`, at most 32 characters — same shape as an ACL name.
pub fn valid_wred_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    name.len() <= 32 && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn wred_profile(name: &str, children: &[Item]) -> Result<WredProfileIntent, IntentError> {
    let bad = |reason: String| IntentError::BadWredProfile {
        name: name.to_string(),
        reason,
    };
    if !valid_wred_name(name) {
        return Err(bad(
            "bad name (letter first, then letters/digits/_/-, max 32)".into(),
        ));
    }
    let mut profile = WredProfileIntent {
        drop_probability: 10,
        ..WredProfileIntent::default()
    };
    for item in children {
        let Item::Leaf { name, values } = item else {
            return Err(bad(format!("unrecognized block {:?}", item.name())));
        };
        match (name.as_str(), values.as_slice()) {
            ("min-threshold", [value]) => {
                profile.min_threshold =
                    Some(parse_int::<u32>(value, 1..=4096, "min-threshold").map_err(&bad)?);
            }
            ("max-threshold", [value]) => {
                profile.max_threshold =
                    Some(parse_int::<u32>(value, 1..=4096, "max-threshold").map_err(&bad)?);
            }
            ("drop-probability", [value]) => {
                profile.drop_probability =
                    parse_int::<u32>(value, 1..=100, "drop-probability").map_err(&bad)?;
            }
            ("ecn", []) => profile.ecn = true,
            _ => return Err(bad(format!("unrecognized statement {name:?}"))),
        }
    }
    Ok(profile)
}

/// The four `map { <table> { ... } }` blocks. Each entry is a per-value
/// phrase leaf (`dscp 46 tc 5;`), so one mapping deletes on its own.
fn qos_maps(items: &[Item], intents: &mut Intents) -> Result<(), IntentError> {
    for item in items {
        let Item::Block {
            name,
            keys,
            children,
        } = item
        else {
            return Err(IntentError::BadQos(format!(
                "map: unrecognized statement {:?}",
                item.name()
            )));
        };
        if !keys.is_empty() {
            return Err(IntentError::BadQos(format!(
                "map: unrecognized block {name:?}"
            )));
        }
        // (key word, key max, value word, value max, destination)
        let (key_word, key_max, value_word, value_max) = match name.as_str() {
            "dscp-to-tc" => ("dscp", 63u8, "tc", 7u8),
            "cos-to-tc" => ("cos", 7, "tc", 7),
            "tc-to-dscp" => ("tc", 7, "dscp", 63),
            "tc-to-cos" => ("tc", 7, "cos", 7),
            other => {
                return Err(IntentError::BadQos(format!(
                    "map: unrecognized table {other:?}"
                )));
            }
        };
        let bad = |reason: String| IntentError::BadQosMap {
            table: name.clone(),
            reason,
        };
        let mut table = BTreeMap::new();
        for entry in children {
            let Item::Leaf { name: leaf, values } = entry else {
                return Err(bad(format!("unrecognized block {:?}", entry.name())));
            };
            let [key, keyword, value] = values.as_slice() else {
                return Err(bad(format!(
                    "expected `{key_word} <value> {value_word} <value>`"
                )));
            };
            if leaf != key_word || keyword != value_word {
                return Err(bad(format!(
                    "expected `{key_word} <value> {value_word} <value>`"
                )));
            }
            let key = parse_int::<u8>(key, 0..=key_max, key_word).map_err(&bad)?;
            let value = parse_int::<u8>(value, 0..=value_max, value_word).map_err(&bad)?;
            if table.insert(key, value).is_some() {
                return Err(bad(format!("duplicate {key_word} {key}")));
            }
        }
        let slot = match name.as_str() {
            "dscp-to-tc" => &mut intents.qos_maps.dscp_to_tc,
            "cos-to-tc" => &mut intents.qos_maps.cos_to_tc,
            "tc-to-dscp" => &mut intents.qos_maps.tc_to_dscp,
            _ => &mut intents.qos_maps.tc_to_cos,
        };
        if !slot.is_empty() {
            return Err(IntentError::BadQos(format!("duplicate map block {name:?}")));
        }
        *slot = table;
    }
    Ok(())
}

/// One interface's `qos { ... }` block.
fn port_qos(children: &[Item], ifname: &str) -> Result<Option<PortQosIntent>, IntentError> {
    let mut blocks = ConfigTree::blocks_named(children, "qos");
    let Some((keys, body)) = blocks.next() else {
        return Ok(None);
    };
    if blocks.next().is_some() || !keys.is_empty() {
        return Err(IntentError::BadPortQos {
            name: ifname.to_string(),
            reason: "duplicate qos block".into(),
        });
    }
    let bad = |reason: String| IntentError::BadPortQos {
        name: ifname.to_string(),
        reason,
    };
    let mut qos = PortQosIntent::default();
    for item in body {
        match item {
            Item::Leaf { name, values } => match (name.as_str(), values.as_slice()) {
                ("trust", [word]) => {
                    qos.trust = match word.as_str() {
                        "untrusted" => QosTrust::Untrusted,
                        "dscp" => QosTrust::Dscp,
                        "cos" => QosTrust::Cos,
                        other => {
                            return Err(bad(format!("bad trust {other:?} (dscp|cos|untrusted)")));
                        }
                    };
                }
                ("default-tc", [value]) => {
                    qos.default_tc = parse_int::<u8>(value, 0..=7, "default-tc").map_err(&bad)?;
                }
                ("shape", [keyword, rate]) if keyword == "rate" => {
                    qos.shape = Some(hemlock_common::net::parse_shape_rate(rate).map_err(&bad)?);
                }
                _ => return Err(bad(format!("unrecognized statement {name:?}"))),
            },
            Item::Block {
                name,
                keys,
                children,
            } => {
                if name != "queue" {
                    return Err(bad(format!("unrecognized block {name:?}")));
                }
                let [index_text] = keys.as_slice() else {
                    return Err(bad("queue block needs exactly one index key".into()));
                };
                let queue_bad = |reason: String| IntentError::BadQueueQos {
                    name: ifname.to_string(),
                    queue: index_text.clone(),
                    reason,
                };
                let index = parse_int::<u8>(index_text, 0..=7, "queue").map_err(&queue_bad)?;
                let queue = queue_qos(children, &queue_bad)?;
                if qos.queues.insert(index, queue).is_some() {
                    return Err(queue_bad("duplicate queue block".into()));
                }
            }
        }
    }
    // A queue left entirely at the defaults carries no config, so an
    // empty `queue <n> { }` diffs to nothing.
    qos.queues
        .retain(|_, queue| *queue != QueueQosIntent::default());
    Ok(Some(qos))
}

/// One `queue <0-7> { ... }` body.
fn queue_qos(
    children: &[Item],
    bad: &dyn Fn(String) -> IntentError,
) -> Result<QueueQosIntent, IntentError> {
    let mut queue = QueueQosIntent::default();
    for item in children {
        let Item::Leaf { name, values } = item else {
            return Err(bad(format!("unrecognized block {:?}", item.name())));
        };
        match (name.as_str(), values.as_slice()) {
            ("priority", [word]) if word == "strict" => queue.strict = true,
            ("priority", [other]) => {
                return Err(bad(format!("bad priority {other:?} (strict)")));
            }
            ("weight", [value]) => {
                queue.weight = Some(parse_int::<u8>(value, 1..=127, "weight").map_err(bad)?);
            }
            ("shape", [keyword, rate]) if keyword == "rate" => {
                queue.shape = Some(hemlock_common::net::parse_shape_rate(rate).map_err(bad)?);
            }
            ("wred-profile", [name]) => queue.wred_profile = Some(name.clone()),
            _ => return Err(bad(format!("unrecognized statement {name:?}"))),
        }
    }
    if queue.strict && queue.weight.is_some() {
        return Err(bad("strict and weight are mutually exclusive".into()));
    }
    Ok(queue)
}

/// Cross-family semantic checks that need the whole tree extracted:
/// channel-group consistency, mirror destination rules, and the
/// non-fatal commit notes.
/// An interface's configured address: `None` = no such interface,
/// `Some(None)` = exists but carries no address.
fn interface_address<'a>(intents: &'a Intents, name: &str) -> Option<Option<&'a String>> {
    intents
        .ports
        .get(name)
        .map(|p| p.address.as_ref())
        .or_else(|| intents.svis.get(name).map(|s| s.address.as_ref()))
        .or_else(|| intents.management.get(name).map(|m| m.address.as_ref()))
}

/// `access-group <name> <in|out>` leaves of one interface.
fn access_groups(children: &[Item], ifname: &str) -> Result<AccessGroups, IntentError> {
    let bad = |reason: String| IntentError::BadAccessGroup {
        name: ifname.to_string(),
        reason,
    };
    let mut groups = AccessGroups::default();
    for item in children {
        let Item::Leaf { name, values } = item else {
            continue;
        };
        if name != "access-group" {
            continue;
        }
        let [acl, direction] = values.as_slice() else {
            return Err(bad("expected `access-group <name> <in|out>`".into()));
        };
        let slot = match direction.as_str() {
            "in" => &mut groups.ingress,
            "out" => &mut groups.egress,
            other => {
                return Err(bad(format!(
                    "direction must be `in` or `out`, got {other:?}"
                )));
            }
        };
        if slot.is_some() {
            return Err(bad(format!("duplicate {direction} binding")));
        }
        *slot = Some(acl.clone());
    }
    Ok(groups)
}

/// `port-security { maximum <n>; violation <protect|shutdown> }`.
fn port_security(
    children: &[Item],
    ifname: &str,
) -> Result<Option<PortSecurityIntent>, IntentError> {
    let Some((_, body)) = ConfigTree::blocks_named(children, "port-security").next() else {
        return Ok(None);
    };
    let bad = |reason: String| IntentError::BadPortSecurity {
        name: ifname.to_string(),
        reason,
    };
    let mut intent = PortSecurityIntent::default();
    for item in body {
        let Item::Leaf { name, values } = item else {
            return Err(bad(format!("unrecognized block {:?}", item.name())));
        };
        match (name.as_str(), values.as_slice()) {
            ("maximum", [value]) => {
                intent.maximum = parse_int::<u32>(value, 1..=1024, "maximum").map_err(&bad)?;
            }
            ("violation", [value]) => {
                intent.shutdown = match value.as_str() {
                    "protect" => false,
                    "shutdown" => true,
                    other => {
                        return Err(bad(format!(
                            "violation must be `protect` or `shutdown`, got {other:?}"
                        )));
                    }
                };
            }
            _ => {
                return Err(bad(format!("unrecognized statement {name:?}")));
            }
        }
    }
    Ok(Some(intent))
}

/// The `security { ... }` block: ACLs, CoPP overrides, dot1x, DHCP
/// snooping + ARP inspection.
fn security(tree: &ConfigTree, intents: &mut Intents) -> Result<(), IntentError> {
    let Some((_, items)) = tree.block("security") else {
        return Ok(());
    };
    for item in items {
        let Item::Block {
            name,
            keys,
            children,
        } = item
        else {
            return Err(IntentError::BadSecurity(format!(
                "unrecognized statement {:?}",
                item.name()
            )));
        };
        if !keys.is_empty() {
            return Err(IntentError::BadSecurity(format!(
                "unrecognized block {name:?}"
            )));
        }
        match name.as_str() {
            "acl" => acls(children, intents)?,
            "copp" => copp(children, intents)?,
            "dot1x" => intents.dot1x = dot1x(children)?,
            "dhcp-snooping" => dhcp_snooping(children, intents)?,
            "arp-inspection" => arp_inspection(children, intents)?,
            other => {
                return Err(IntentError::BadSecurity(format!(
                    "unrecognized block {other:?}"
                )));
            }
        }
    }
    Ok(())
}

/// ACL name syntax: letter first, then letters/digits/`_`/`-`, at most
/// 32 characters.
fn valid_acl_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    name.len() <= 32 && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `acl { <ipv4|ipv6|mac> <name> { rule <n> { ... } } }`.
fn acls(items: &[Item], intents: &mut Intents) -> Result<(), IntentError> {
    for item in items {
        let Item::Block {
            name: family_word,
            keys,
            children,
        } = item
        else {
            return Err(IntentError::BadSecurity(format!(
                "acl: unrecognized statement {:?}",
                item.name()
            )));
        };
        let family = match family_word.as_str() {
            "ipv4" => AclFamily::Ipv4,
            "ipv6" => AclFamily::Ipv6,
            "mac" => AclFamily::Mac,
            other => {
                return Err(IntentError::BadSecurity(format!(
                    "acl: unrecognized family {other:?}"
                )));
            }
        };
        let [name] = keys.as_slice() else {
            return Err(IntentError::BadSecurity(format!(
                "acl {family_word} block needs exactly one name key"
            )));
        };
        if !valid_acl_name(name) {
            return Err(IntentError::BadAcl {
                name: name.clone(),
                reason: "bad name (letter first, then letters/digits/_/-, max 32)".into(),
            });
        }
        let intent = AclIntent {
            family,
            rules: acl_rules(name, family, children)?,
        };
        if intents.acls.insert(name.clone(), intent).is_some() {
            return Err(IntentError::BadAcl {
                name: name.clone(),
                reason: "duplicate ACL name (names are shared across families)".into(),
            });
        }
    }
    Ok(())
}

/// The `rule <n> { ... }` blocks of one ACL.
fn acl_rules(
    acl: &str,
    family: AclFamily,
    items: &[Item],
) -> Result<BTreeMap<u32, AclRule>, IntentError> {
    let mut rules = BTreeMap::new();
    for item in items {
        let Item::Block {
            name,
            keys,
            children,
        } = item
        else {
            return Err(IntentError::BadAcl {
                name: acl.to_string(),
                reason: format!("unrecognized statement {:?}", item.name()),
            });
        };
        if name != "rule" {
            return Err(IntentError::BadAcl {
                name: acl.to_string(),
                reason: format!("unrecognized block {name:?}"),
            });
        }
        let [number_text] = keys.as_slice() else {
            return Err(IntentError::BadAcl {
                name: acl.to_string(),
                reason: "rule block needs exactly one number key".into(),
            });
        };
        let bad = |reason: String| IntentError::BadAclRule {
            name: acl.to_string(),
            rule: number_text.clone(),
            reason,
        };
        let number = number_text
            .parse::<u32>()
            .ok()
            .filter(|n| *n >= 1 && !number_text.starts_with('0'))
            .ok_or_else(|| bad(format!("bad rule number {number_text:?} (1..4294967295)")))?;
        let rule = acl_rule(family, children, &bad)?;
        if rules.insert(number, rule).is_some() {
            return Err(bad("duplicate rule number".into()));
        }
    }
    Ok(rules)
}

/// One rule body, family-gated.
fn acl_rule(
    family: AclFamily,
    children: &[Item],
    bad: &dyn Fn(String) -> IntentError,
) -> Result<AclRule, IntentError> {
    let mut rule = AclRule::default();
    let mut action: Option<bool> = None;
    let ip_family = matches!(family, AclFamily::Ipv4 | AclFamily::Ipv6);
    for item in children {
        let Item::Leaf { name, values } = item else {
            return Err(bad(format!("unrecognized block {:?}", item.name())));
        };
        match (name.as_str(), values.as_slice()) {
            ("permit", []) | ("deny", []) => {
                if action.is_some() {
                    return Err(bad("both permit and deny".into()));
                }
                action = Some(name == "permit");
            }
            ("protocol", [value]) if ip_family => {
                rule.protocol = Some(match value.as_str() {
                    "tcp" => 6,
                    "udp" => 17,
                    "icmp" => 1,
                    other => other
                        .parse::<u8>()
                        .map_err(|_| bad(format!("bad protocol {other:?} (tcp|udp|icmp|0-255)")))?,
                });
            }
            ("source", [value]) | ("destination", [value]) if ip_family => {
                let slot = if name == "source" {
                    &mut rule.source
                } else {
                    &mut rule.destination
                };
                if value == "any" {
                    *slot = None;
                    continue;
                }
                let canonical =
                    hemlock_common::net::require_canonical_prefix(value).map_err(bad)?;
                let v6 = canonical.contains(':');
                if v6 != matches!(family, AclFamily::Ipv6) {
                    return Err(bad(format!(
                        "{canonical} does not match the ACL family ({})",
                        family.word()
                    )));
                }
                *slot = Some(canonical);
            }
            ("source-port", [value]) | ("destination-port", [value]) if ip_family => {
                let range = hemlock_common::net::parse_port_match(value).map_err(bad)?;
                if name == "source-port" {
                    rule.source_port = Some(range);
                } else {
                    rule.destination_port = Some(range);
                }
            }
            ("dscp", [value]) if ip_family => {
                rule.dscp = Some(parse_int::<u8>(value, 0..=63, "dscp").map_err(bad)?);
            }
            ("log", []) if ip_family => rule.log = true,
            ("police", [kw_rate, rate, kw_burst, burst])
                if ip_family && kw_rate == "rate" && kw_burst == "burst" =>
            {
                let (rate_value, pps) =
                    hemlock_common::net::parse_police_rate(rate).map_err(bad)?;
                let (burst_value, burst_pkts) =
                    hemlock_common::net::parse_police_burst(burst).map_err(bad)?;
                let burst_scaled = burst.to_ascii_lowercase().ends_with(['k', 'm', 'g']);
                if pps && burst_scaled {
                    return Err(bad("a pps rate takes its burst in packets".into()));
                }
                if !pps && burst_pkts {
                    return Err(bad("a bps rate takes its burst in bytes".into()));
                }
                rule.police = Some(AclPolice {
                    rate: rate_value,
                    burst: burst_value,
                    pps,
                });
            }
            ("source-mac", [value]) | ("destination-mac", [value]) if family == AclFamily::Mac => {
                let (mac_text, mask_text) = match value.split_once('/') {
                    Some((mac, mask)) => (mac, Some(mask)),
                    None => (value.as_str(), None),
                };
                let mac = hemlock_common::net::parse_mac(mac_text).map_err(bad)?;
                let mask = match mask_text {
                    Some(mask) => hemlock_common::net::parse_mac_mask(mask).map_err(bad)?,
                    None => "ff:ff:ff:ff:ff:ff".into(),
                };
                if name == "source-mac" {
                    rule.source_mac = Some((mac, mask));
                } else {
                    rule.destination_mac = Some((mac, mask));
                }
            }
            ("ethertype", [value]) if family == AclFamily::Mac => {
                rule.ethertype = Some(match value.as_str() {
                    "ipv4" => 0x0800,
                    "ipv6" => 0x86dd,
                    "arp" => 0x0806,
                    hex => hex
                        .strip_prefix("0x")
                        .and_then(|h| u16::from_str_radix(h, 16).ok())
                        .ok_or_else(|| {
                            bad(format!(
                                "bad ethertype {hex:?} (0x0000-0xffff|ipv4|ipv6|arp)"
                            ))
                        })?,
                });
            }
            _ => {
                return Err(bad(format!(
                    "unrecognized statement {name:?} for an {} rule",
                    family.word()
                )));
            }
        }
    }
    let Some(permit) = action else {
        return Err(bad("needs `permit` or `deny`".into()));
    };
    rule.permit = permit;
    if (rule.source_port.is_some() || rule.destination_port.is_some())
        && !matches!(rule.protocol, Some(6) | Some(17))
    {
        return Err(bad(
            "source-port/destination-port require protocol tcp or udp".into(),
        ));
    }
    Ok(rule)
}

/// `copp { class <name> { rate <pps>; burst <pkts> } }`.
fn copp(items: &[Item], intents: &mut Intents) -> Result<(), IntentError> {
    for item in items {
        let Item::Block {
            name,
            keys,
            children,
        } = item
        else {
            return Err(IntentError::BadCopp(format!(
                "unrecognized statement {:?}",
                item.name()
            )));
        };
        if name != "class" {
            return Err(IntentError::BadCopp(format!("unrecognized block {name:?}")));
        }
        let [class] = keys.as_slice() else {
            return Err(IntentError::BadCopp("class block needs a name key".into()));
        };
        if !COPP_CLASS_NAMES.contains(&class.as_str()) {
            return Err(IntentError::BadCopp(format!("unknown class {class:?}")));
        }
        let bad = |reason: String| IntentError::BadCopp(format!("class {class}: {reason}"));
        let mut intent = CoppClassIntent::default();
        for item in children {
            let Item::Leaf { name, values } = item else {
                return Err(bad(format!("unrecognized block {:?}", item.name())));
            };
            match (name.as_str(), values.as_slice()) {
                ("rate", [value]) => {
                    intent.rate =
                        Some(parse_int::<u32>(value, 1..=10_000_000, "rate").map_err(&bad)?);
                }
                ("burst", [value]) => {
                    intent.burst =
                        Some(parse_int::<u32>(value, 1..=1_000_000, "burst").map_err(&bad)?);
                }
                _ => return Err(bad(format!("unrecognized statement {name:?}"))),
            }
        }
        if intents.copp.insert(class.clone(), intent).is_some() {
            return Err(IntentError::BadCopp(format!("duplicate class {class:?}")));
        }
    }
    Ok(())
}

/// `dot1x { radius-server <ip> { ... }; reauth-interval <secs> }`.
fn dot1x(items: &[Item]) -> Result<Dot1xIntent, IntentError> {
    let bad = IntentError::BadDot1x;
    let mut intent = Dot1xIntent::default();
    for item in items {
        match item {
            Item::Block {
                name,
                keys,
                children,
            } if name == "radius-server" => {
                let [ip] = keys.as_slice() else {
                    return Err(bad("radius-server block needs an address key".into()));
                };
                let ip: std::net::IpAddr = ip
                    .parse()
                    .map_err(|_| bad(format!("bad radius-server address {ip:?}")))?;
                let mut server = RadiusServer {
                    ip: ip.to_string(),
                    ..RadiusServer::default()
                };
                let bad_server =
                    |reason: String| IntentError::BadDot1x(format!("radius-server {ip}: {reason}"));
                for item in children {
                    let Item::Leaf { name, values } = item else {
                        return Err(bad_server(format!("unrecognized block {:?}", item.name())));
                    };
                    match (name.as_str(), values.as_slice()) {
                        ("key", [key]) => server.key = Some(key.clone()),
                        ("port", [value]) => {
                            server.port =
                                parse_int::<u16>(value, 1..=65535, "port").map_err(&bad_server)?;
                        }
                        ("timeout", [value]) => {
                            server.timeout =
                                parse_int::<u8>(value, 1..=60, "timeout").map_err(&bad_server)?;
                        }
                        ("retransmit", [value]) => {
                            server.retransmit = parse_int::<u8>(value, 0..=10, "retransmit")
                                .map_err(&bad_server)?;
                        }
                        _ => {
                            return Err(bad_server(format!("unrecognized statement {name:?}")));
                        }
                    }
                }
                if intent.radius_servers.iter().any(|s| s.ip == server.ip) {
                    return Err(bad(format!("duplicate radius-server {ip}")));
                }
                intent.radius_servers.push(server);
            }
            Item::Leaf { name, values } if name == "reauth-interval" => {
                let [value] = values.as_slice() else {
                    return Err(bad("reauth-interval takes one value".into()));
                };
                let secs = parse_int::<u32>(value, 0..=86400, "reauth-interval").map_err(bad)?;
                if secs != 0 && secs < 60 {
                    return Err(bad(format!("bad reauth-interval {secs} (0|60-86400)")));
                }
                intent.reauth_interval = secs;
            }
            other => {
                return Err(bad(format!("unrecognized statement {:?}", other.name())));
            }
        }
    }
    Ok(intent)
}

/// `dhcp-snooping { vlan <id>; binding <mac> vlan <id> address <ip>
/// interface <port> }`.
fn dhcp_snooping(items: &[Item], intents: &mut Intents) -> Result<(), IntentError> {
    let bad = IntentError::BadDhcpSnooping;
    for item in items {
        let Item::Leaf { name, values } = item else {
            return Err(bad(format!("unrecognized block {:?}", item.name())));
        };
        match (name.as_str(), values.as_slice()) {
            ("vlan", [id]) => {
                let id = parse_vlan_id(id).map_err(bad)?;
                intents.snoop_sec.dhcp_vlans.insert(id);
            }
            ("binding", [mac, kw_vlan, vlan, kw_address, address, kw_interface, interface])
                if kw_vlan == "vlan" && kw_address == "address" && kw_interface == "interface" =>
            {
                let mac = hemlock_common::net::parse_unicast_mac(mac).map_err(bad)?;
                let vlan = parse_vlan_id(vlan).map_err(bad)?;
                let ip: std::net::Ipv4Addr = address
                    .parse()
                    .map_err(|_| bad(format!("bad binding address {address:?} (IPv4)")))?;
                let binding = StaticBinding {
                    address: ip.to_string(),
                    interface: interface.clone(),
                };
                if intents
                    .snoop_sec
                    .static_bindings
                    .insert((mac.clone(), vlan), binding)
                    .is_some()
                {
                    return Err(bad(format!("duplicate binding {mac} vlan {vlan}")));
                }
            }
            _ => {
                return Err(bad(format!("unrecognized statement {name:?}")));
            }
        }
    }
    Ok(())
}

/// `arp-inspection { vlan <id>; validate <src-mac|dst-mac|ip> }`.
fn arp_inspection(items: &[Item], intents: &mut Intents) -> Result<(), IntentError> {
    let bad = IntentError::BadArpInspection;
    for item in items {
        let Item::Leaf { name, values } = item else {
            return Err(bad(format!("unrecognized block {:?}", item.name())));
        };
        match (name.as_str(), values.as_slice()) {
            ("vlan", [id]) => {
                let id = parse_vlan_id(id).map_err(bad)?;
                intents.snoop_sec.arp_vlans.insert(id);
            }
            ("validate", [check]) => {
                let check = match check.as_str() {
                    "src-mac" => ArpValidate::SrcMac,
                    "dst-mac" => ArpValidate::DstMac,
                    "ip" => ArpValidate::Ip,
                    other => {
                        return Err(bad(format!("bad validate {other:?} (src-mac|dst-mac|ip)")));
                    }
                };
                intents.snoop_sec.validate.insert(check);
            }
            _ => {
                return Err(bad(format!("unrecognized statement {name:?}")));
            }
        }
    }
    Ok(())
}

fn finish_validation(intents: &mut Intents) -> Result<(), IntentError> {
    // A relay stamps the SVI's address into giaddr, and the server
    // replies to it. Without one the relay would forward requests
    // nothing could answer, so it is a commit error.
    for vlan in intents.dhcp_relay.keys() {
        let name = format!("Vlan{vlan}");
        let addressed = intents
            .svis
            .get(&name)
            .map(|svi| svi.address.is_some())
            .unwrap_or(false);
        if !addressed {
            return Err(IntentError::BadDhcpRelay {
                name,
                reason: "the interface must carry an address (the relay's giaddr)".into(),
            });
        }
    }

    // sFlow needs somewhere to send: a rate, a polling interval or a
    // per-port disable with no collector is config that samples
    // nothing, so it is an error rather than a silent no-op.
    let per_port_sflow = intents.ports.values().any(|port| port.sflow_disabled);
    if !intents.sflow.enabled() && (intents.sflow.is_set() || per_port_sflow) {
        return Err(IntentError::BadSflow(
            "at least one collector is required".into(),
        ));
    }

    // SNMP listens on the management port only, so snmpd needs an
    // address to bind. Without one the agent would either not start or
    // fall back to every interface — neither is what the config asked
    // for, so it is a commit error rather than a surprise.
    if intents.snmp.enabled && management_address(intents).is_none() {
        return Err(IntentError::BadSnmp(
            "the management interface must carry an address (the agent listens there only)".into(),
        ));
    }

    // ARP statics: the named interface must be L3 (carry an address in
    // this same config).
    for (ip, arp) in &intents.arp_statics {
        match interface_address(intents, &arp.interface) {
            Some(Some(_)) => {}
            Some(None) => {
                return Err(IntentError::BadArp {
                    ip: ip.clone(),
                    reason: format!("{} has no address (not L3)", arp.interface),
                });
            }
            None => {
                return Err(IntentError::BadArp {
                    ip: ip.clone(),
                    reason: format!("no such L3 interface {:?}", arp.interface),
                });
            }
        }
    }

    // OSPF interface knobs and passive-interfaces name L3 interfaces.
    if let Some(ospf) = &intents.ospf {
        for name in ospf.interfaces.keys().chain(ospf.passive_interfaces.iter()) {
            match interface_address(intents, name) {
                Some(Some(_)) => {}
                Some(None) => {
                    return Err(IntentError::BadOspf(format!(
                        "interface {name} has no address (not L3)"
                    )));
                }
                None => {
                    return Err(IntentError::BadOspf(format!(
                        "no such L3 interface {name:?}"
                    )));
                }
            }
        }
    }

    // VRRP: the parent must carry an address, group addresses are
    // required, and each VIP must fall inside one of the parent's
    // subnets â€” an off-subnet VIP on an access switch is a typo, so an
    // error, not a warning.
    for ((interface, group), vrrp) in &intents.vrrp {
        let bad = |reason: String| IntentError::BadVrrp {
            interface: interface.clone(),
            group: group.to_string(),
            reason,
        };
        let address = match interface_address(intents, interface) {
            Some(Some(address)) => address,
            _ => {
                return Err(bad("the interface must carry an address".into()));
            }
        };
        let Ok((addr, len)) = hemlock_common::net::parse_cidr(address) else {
            return Err(bad(format!("bad interface address {address:?}")));
        };
        if vrrp.addresses.is_empty() {
            return Err(bad("at least one address (VIP) is required".into()));
        }
        for vip in &vrrp.addresses {
            let Ok(vip_addr) = vip.parse::<std::net::IpAddr>() else {
                return Err(bad(format!("bad address {vip:?}")));
            };
            if !vip_addr.is_ipv4()
                || hemlock_common::net::network(vip_addr, len)
                    != hemlock_common::net::network(addr, len)
            {
                return Err(bad(format!(
                    "address {vip} is outside {interface}'s subnet {address}"
                )));
            }
        }
    }

    // Channel groups: members may not carry their own switchport
    // config, groups are capped at 8 members, and every member of a
    // group runs the same mode.
    let mut group_modes: BTreeMap<u16, (String, LacpMode)> = BTreeMap::new();
    let mut group_sizes: BTreeMap<u16, u32> = BTreeMap::new();
    for (name, port) in &intents.ports {
        let Some(cg) = &port.channel_group else {
            continue;
        };
        if port.switchport.is_some() {
            return Err(IntentError::MemberConfigConflict {
                member: name.clone(),
                group: cg.group,
            });
        }
        *group_sizes.entry(cg.group).or_default() += 1;
        match group_modes.get(&cg.group) {
            Some((first, mode)) if *mode != cg.mode => {
                return Err(IntentError::BadChannelGroup {
                    name: name.clone(),
                    reason: format!(
                        "mode {} does not match {} ({}) in channel-group {}",
                        cg.mode.word(),
                        first,
                        mode.word(),
                        cg.group
                    ),
                });
            }
            Some(_) => {}
            None => {
                group_modes.insert(cg.group, (name.clone(), cg.mode));
            }
        }
    }
    for (group, size) in &group_sizes {
        if *size > 8 {
            let member = intents
                .ports
                .iter()
                .find(|(_, p)| p.channel_group.map(|cg| cg.group) == Some(*group))
                .map(|(n, _)| n.clone())
                .unwrap_or_default();
            return Err(IntentError::BadChannelGroup {
                name: member,
                reason: format!("channel-group {group} has {size} members (max 8)"),
            });
        }
    }
    for group in intents.lags.keys() {
        if !group_sizes.contains_key(group) {
            intents
                .warnings
                .push(format!("Port-Channel{group} has no member ports"));
        }
    }

    // Mirror sessions: a destination forwards nothing, so it may not be
    // a source anywhere, a LAG member, or carry channel-group/address
    // config; a port is a source in at most one session per direction.
    let mut rx_sources: BTreeMap<&str, u8> = BTreeMap::new();
    let mut tx_sources: BTreeMap<&str, u8> = BTreeMap::new();
    for (session, mirror) in &intents.mirror {
        for (port, direction) in &mirror.sources {
            let claims: &mut [&mut BTreeMap<&str, u8>] = match direction {
                MirrorDirection::Rx => &mut [&mut rx_sources],
                MirrorDirection::Tx => &mut [&mut tx_sources],
                MirrorDirection::Both => &mut [&mut rx_sources, &mut tx_sources],
            };
            for map in claims {
                if let Some(other) = map.insert(port.as_str(), *session) {
                    if other != *session {
                        return Err(IntentError::BadMirror {
                            session: *session,
                            reason: format!(
                                "{port} is already a source in session {other} for that direction"
                            ),
                        });
                    }
                }
            }
        }
    }
    for (session, mirror) in &intents.mirror {
        let Some(dest) = &mirror.destination else {
            intents
                .warnings
                .push(format!("mirror session {session} has no destination"));
            continue;
        };
        if rx_sources.contains_key(dest.as_str()) || tx_sources.contains_key(dest.as_str()) {
            return Err(IntentError::BadMirror {
                session: *session,
                reason: format!("destination {dest} is a mirror source"),
            });
        }
        if let Some(port) = intents.ports.get(dest) {
            if let Some(cg) = &port.channel_group {
                return Err(IntentError::BadMirror {
                    session: *session,
                    reason: format!("destination {dest} is a member of Port-Channel{}", cg.group),
                });
            }
            if port.address.is_some() {
                return Err(IntentError::BadMirror {
                    session: *session,
                    reason: format!("destination {dest} carries an address"),
                });
            }
        }
    }

    // ACL bindings: the named ACL must exist, and a LAG member binds on
    // its Port-Channel, never directly.
    let mirror_destinations: BTreeSet<&String> = intents
        .mirror
        .values()
        .filter_map(|m| m.destination.as_ref())
        .collect();
    let mut all_bindings: Vec<(String, AccessGroups)> = Vec::new();
    for (name, port) in &intents.ports {
        if !port.access_groups.is_empty() {
            if let Some(cg) = &port.channel_group {
                return Err(IntentError::AccessGroupOnMember {
                    member: name.clone(),
                    group: cg.group,
                });
            }
            all_bindings.push((name.clone(), port.access_groups.clone()));
        }
    }
    for (group, lag) in &intents.lags {
        if !lag.access_groups.is_empty() {
            all_bindings.push((format!("Port-Channel{group}"), lag.access_groups.clone()));
        }
    }
    for (name, groups) in &all_bindings {
        for acl in [&groups.ingress, &groups.egress].into_iter().flatten() {
            if !intents.acls.contains_key(acl) {
                return Err(IntentError::BadAccessGroup {
                    name: name.clone(),
                    reason: format!("no such ACL {acl:?}"),
                });
            }
        }
    }

    // dot1x needs a keyed RADIUS server the moment any port enables it.
    let dot1x_ports: Vec<&String> = intents
        .ports
        .iter()
        .filter(|(_, p)| p.dot1x)
        .map(|(n, _)| n)
        .collect();
    if !dot1x_ports.is_empty() && !intents.dot1x.radius_servers.iter().any(|s| s.key.is_some()) {
        return Err(IntentError::BadDot1x(
            "radius-server with key is required".into(),
        ));
    }
    for name in &dot1x_ports {
        if let Some(port) = intents.ports.get(*name) {
            if let Some(cg) = &port.channel_group {
                return Err(IntentError::BadDot1x(format!(
                    "{name}: member of Port-Channel{}; dot1x runs on standalone ports",
                    cg.group
                )));
            }
        }
    }

    // QoS: strict/weight exclusivity and contiguity, shaper ordering,
    // WRED profile references, and the front-panel-only rule.
    let mut qos_targets: Vec<(String, &PortQosIntent)> = Vec::new();
    for (name, port) in &intents.ports {
        if let Some(qos) = &port.qos {
            if let Some(cg) = &port.channel_group {
                return Err(IntentError::MemberConfigConflict {
                    member: name.clone(),
                    group: cg.group,
                });
            }
            qos_targets.push((name.clone(), qos));
        }
    }
    for (group, lag) in &intents.lags {
        if let Some(qos) = &lag.qos {
            qos_targets.push((format!("Port-Channel{group}"), qos));
        }
    }
    let mut wred_references: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, qos) in &qos_targets {
        let mut strict: BTreeSet<u8> = BTreeSet::new();
        for (index, queue) in &qos.queues {
            if queue.strict && queue.weight.is_some() {
                return Err(IntentError::BadQueueQos {
                    name: name.clone(),
                    queue: index.to_string(),
                    reason: "strict and weight are mutually exclusive".into(),
                });
            }
            if queue.strict {
                strict.insert(*index);
            }
            // A queue shaper below the port shaper is the only useful
            // ordering: the other way round is silently dead config.
            if let (Some(queue_rate), Some(port_rate)) = (queue.shape, qos.shape) {
                if queue_rate > port_rate {
                    return Err(IntentError::BadQueueQos {
                        name: name.clone(),
                        queue: index.to_string(),
                        reason: format!(
                            "shaper {} exceeds the port shaper {}",
                            hemlock_common::net::format_shape_rate(queue_rate),
                            hemlock_common::net::format_shape_rate(port_rate)
                        ),
                    });
                }
            }
            if let Some(profile) = &queue.wred_profile {
                // References are gathered for every profile, defined or
                // not: an undefined one is reported once with the whole
                // list, so deleting a bound profile names every queue
                // holding it instead of one at a time.
                wred_references
                    .entry(profile.clone())
                    .or_default()
                    .push(format!("{name} (q{index})"));
            }
        }
        // The Helix4 scheduler tree can only express strict priority on
        // the top queues, so the strict set must run 7, 7-6, 7-6-5, ...
        if !strict.is_empty() {
            let top = QOS_QUEUE_COUNT - 1;
            let expected: BTreeSet<u8> = (top + 1 - strict.len() as u8..=top).collect();
            if strict != expected {
                return Err(IntentError::StrictQueueOrder);
            }
        }
    }
    // A reference to a profile this config does not define — a typo,
    // or a profile deleted while queues still bind it.
    for (profile, references) in &wred_references {
        if !intents.wred_profiles.contains_key(profile) {
            return Err(IntentError::BadWredProfile {
                name: profile.clone(),
                reason: format!("not defined; referenced by {}", references.join(", ")),
            });
        }
    }
    // A referenced profile needs both thresholds, in order.
    for (name, profile) in &intents.wred_profiles {
        let referenced = wred_references.contains_key(name);
        let bad = |reason: String| IntentError::BadWredProfile {
            name: name.clone(),
            reason,
        };
        match (profile.min_threshold, profile.max_threshold) {
            (Some(min), Some(max)) if min >= max => {
                return Err(bad(format!(
                    "min-threshold {min} must be below max-threshold {max}"
                )));
            }
            (Some(_), Some(_)) => {}
            _ if referenced => {
                return Err(bad(
                    "min-threshold and max-threshold are required when the profile is referenced"
                        .into(),
                ));
            }
            _ => {}
        }
    }

    // Port security: not on LAG members and not on mirror destinations.
    for (name, port) in &intents.ports {
        if port.port_security.is_none() {
            continue;
        }
        if let Some(cg) = &port.channel_group {
            return Err(IntentError::BadPortSecurity {
                name: name.clone(),
                reason: format!(
                    "member of Port-Channel{}; port-security and channel-group are mutually exclusive",
                    cg.group
                ),
            });
        }
        if mirror_destinations.contains(name) {
            return Err(IntentError::BadPortSecurity {
                name: name.clone(),
                reason: "mirror destinations take no port-security".into(),
            });
        }
    }

    // DAI leans on the snooping binding table: an inspected VLAN wants
    // DHCP snooping (or at least one static binding covering it).
    for vlan in &intents.snoop_sec.arp_vlans {
        let covered = intents.snoop_sec.dhcp_vlans.contains(vlan)
            || intents
                .snoop_sec
                .static_bindings
                .keys()
                .any(|(_, binding_vlan)| binding_vlan == vlan);
        if !covered {
            return Err(IntentError::BadArpInspection(format!(
                "vlan {vlan} needs dhcp-snooping vlan {vlan} (or a static binding) to validate against"
            )));
        }
    }

    // Which VLANs an L2 interface carries (its own switchport, or its
    // Port-Channel's for members).
    let carries_vlan = |intents: &Intents, name: &str, vlan: u16| -> bool {
        let switchport = intents
            .ports
            .get(name)
            .map(|p| match &p.channel_group {
                Some(cg) => intents
                    .lags
                    .get(&cg.group)
                    .map(|lag| lag.switchport.clone())
                    .unwrap_or(None),
                None => p.switchport.clone(),
            })
            .or_else(|| {
                name.strip_prefix("Port-Channel")
                    .and_then(|d| d.parse::<u16>().ok())
                    .and_then(|group| intents.lags.get(&group))
                    .map(|lag| lag.switchport.clone())
            });
        match switchport {
            None => false,
            Some(None) => vlan == 1,
            Some(Some(sp)) => match sp.mode {
                SwitchportMode::Access | SwitchportMode::Dot1qTunnel => {
                    sp.access_vlan.unwrap_or(1) == vlan
                }
                SwitchportMode::Trunk => {
                    sp.trunk_vlans.contains(&vlan) || sp.native_vlan.unwrap_or(1) == vlan
                }
            },
        }
    };

    // Static bindings name existing interfaces carrying their VLAN.
    for ((mac, vlan), binding) in &intents.snoop_sec.static_bindings {
        let exists = intents.ports.contains_key(&binding.interface)
            || binding
                .interface
                .strip_prefix("Port-Channel")
                .and_then(|d| d.parse::<u16>().ok())
                .map(|group| intents.lags.contains_key(&group))
                .unwrap_or(false);
        if !exists {
            return Err(IntentError::BadDhcpSnooping(format!(
                "binding {mac} vlan {vlan}: no such interface {:?}",
                binding.interface
            )));
        }
        if !carries_vlan(intents, &binding.interface, *vlan) {
            return Err(IntentError::BadDhcpSnooping(format!(
                "binding {mac} vlan {vlan}: {} does not carry VLAN {vlan}",
                binding.interface
            )));
        }
    }

    // Trust flags on interfaces that carry no snooped/inspected VLAN
    // are inert â€” a commit note, not an error.
    let mut trust_notes = Vec::new();
    {
        let snooped: Vec<u16> = intents.snoop_sec.dhcp_vlans.iter().copied().collect();
        let inspected: Vec<u16> = intents.snoop_sec.arp_vlans.iter().copied().collect();
        let mut check = |name: &str, feature: &str, trusted: bool, vlans: &[u16]| {
            if trusted && !vlans.iter().any(|vlan| carries_vlan(intents, name, *vlan)) {
                trust_notes.push(format!(
                    "{name}: {feature} trust has no effect (the interface carries no such VLAN)"
                ));
            }
        };
        for (name, port) in &intents.ports {
            check(name, "dhcp-snooping", port.dhcp_snooping_trust, &snooped);
            check(
                name,
                "arp-inspection",
                port.arp_inspection_trust,
                &inspected,
            );
        }
        for (group, lag) in &intents.lags {
            let name = format!("Port-Channel{group}");
            check(&name, "dhcp-snooping", lag.dhcp_snooping_trust, &snooped);
            check(
                &name,
                "arp-inspection",
                lag.arp_inspection_trust,
                &inspected,
            );
        }
    }
    intents.warnings.extend(trust_notes);

    Ok(())
}

/// `mtu <bytes>` on an interface block.
fn mtu(children: &[Item], ifname: &str) -> Result<Option<u32>, IntentError> {
    let Some(value) = ConfigTree::leaf_value(children, "mtu") else {
        return Ok(None);
    };
    let bytes: u32 = value.parse().map_err(|_| IntentError::BadLinkParam {
        name: ifname.to_string(),
        reason: format!("bad MTU {value:?} ({}..{})", link::MIN_MTU, link::MAX_MTU),
    })?;
    link::valid_mtu(bytes).map_err(|reason| IntentError::BadLinkParam {
        name: ifname.to_string(),
        reason,
    })?;
    Ok(Some(bytes))
}

/// `speed <auto|mbps>`. `auto` and an absent leaf are the same intent
/// â€” nothing is pinned â€” so both parse to `None`.
fn speed(children: &[Item], ifname: &str) -> Result<Option<u32>, IntentError> {
    let Some(value) = ConfigTree::leaf_value(children, "speed") else {
        return Ok(None);
    };
    if value.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    link::parse_speed(value)
        .map(Some)
        .ok_or_else(|| IntentError::BadLinkParam {
            name: ifname.to_string(),
            reason: format!("bad speed {value:?} (auto, or a rate in Mb/s such as 1000)"),
        })
}

/// `duplex <auto|full|half>`, with `auto` collapsing to "not forced".
/// Whether the pinned pair is one the *port* supports is syncd's call â€”
/// it owns the platform port table.
fn duplex(children: &[Item], ifname: &str) -> Result<Option<Duplex>, IntentError> {
    let Some(value) = ConfigTree::leaf_value(children, "duplex") else {
        return Ok(None);
    };
    if value.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    Duplex::parse(value)
        .map(Some)
        .ok_or_else(|| IntentError::BadLinkParam {
            name: ifname.to_string(),
            reason: format!("bad duplex {value:?} (auto, full or half)"),
        })
}

/// Reject `speed`/`duplex` on the interface kinds that have no PHY to
/// negotiate with (SVIs, port-channels, the management NIC).
fn no_link_pinning(children: &[Item], ifname: &str, what: &str) -> Result<(), IntentError> {
    for leaf in ["speed", "duplex"] {
        if ConfigTree::has_leaf(children, leaf) {
            return Err(IntentError::BadLinkParam {
                name: ifname.to_string(),
                reason: format!("{leaf} is not supported on {what}"),
            });
        }
    }
    Ok(())
}

/// Admin state of an interface: the `shutdown` / `no shutdown` marker
/// leaves, with the legacy `no-shutdown` and `admin-state
/// enabled|disabled` forms still accepted for configs persisted before
/// the format changes.
/// `lldp disable` on one physical port. LLDP is on by default, so the
/// only spelling is the off switch.
fn port_lldp(children: &[Item], ifname: &str) -> Result<bool, IntentError> {
    port_service_disable(children, ifname, "lldp")
}

/// `<feature> disable` on one physical port — the shape both LLDP and
/// sFlow use, since both run by default once the feature is on.
fn port_service_disable(
    children: &[Item],
    ifname: &str,
    feature: &'static str,
) -> Result<bool, IntentError> {
    let bad = || {
        let reason = format!("{ifname}: expected `{feature} disable`");
        match feature {
            "sflow" => IntentError::BadSflow(reason),
            _ => IntentError::BadLldp(reason),
        }
    };
    match ConfigTree::leaf_values(children, feature) {
        None => Ok(false),
        Some([word]) if word == "disable" => Ok(true),
        Some(_) => Err(bad()),
    }
}

/// The per-port service leaves are physical-port settings: SVIs,
/// Port-Channels and management ports carry none of them. (LLDP and
/// sFlow run below a LAG, so a Port-Channel *member* is fine — it is
/// the Port-Channel interface itself that has no wire.)
fn no_port_services(children: &[Item], ifname: &str) -> Result<(), IntentError> {
    for feature in ["lldp", "sflow"] {
        if ConfigTree::has_leaf(children, feature) {
            return Err(IntentError::PortServiceOnNonPort {
                name: ifname.to_string(),
                feature,
            });
        }
    }
    Ok(())
}

/// `dhcp-relay server <ipv4>` leaves on one SVI, in config order.
/// Repeated servers collapse; the relay walks the list in order, so a
/// duplicate would only slow failover.
fn dhcp_relay_servers(
    children: &[Item],
    ifname: &str,
) -> Result<Vec<std::net::Ipv4Addr>, IntentError> {
    let bad = |reason: String| IntentError::BadDhcpRelay {
        name: ifname.to_string(),
        reason,
    };
    let mut servers: Vec<std::net::Ipv4Addr> = Vec::new();
    for item in children {
        let Item::Leaf { name, values } = item else {
            continue;
        };
        if name != "dhcp-relay" {
            continue;
        }
        let [keyword, address] = values.as_slice() else {
            return Err(bad("expected `dhcp-relay server <ipv4>`".into()));
        };
        if keyword != "server" {
            return Err(bad(format!("expected `server`, got {keyword:?}")));
        }
        // IPv4 only: DHCPv6 relay is deferred, and a v6 address here
        // would be that by another name.
        let Ok(server) = address.parse::<std::net::Ipv4Addr>() else {
            return Err(bad(format!("bad server address {address:?}")));
        };
        if !servers.contains(&server) {
            servers.push(server);
        }
    }
    if servers.len() > MAX_DHCP_RELAY_SERVERS {
        return Err(bad(format!(
            "at most {MAX_DHCP_RELAY_SERVERS} servers ({} configured)",
            servers.len()
        )));
    }
    Ok(servers)
}

/// The most relay servers one SVI forwards to.
pub const MAX_DHCP_RELAY_SERVERS: usize = 4;

/// Reject `dhcp-relay` on anything that is not an SVI: a relay needs a
/// giaddr, which only a routed VLAN interface has.
fn no_dhcp_relay(children: &[Item], ifname: &str) -> Result<(), IntentError> {
    if ConfigTree::has_leaf(children, "dhcp-relay") {
        return Err(IntentError::BadDhcpRelay {
            name: ifname.to_string(),
            reason: "dhcp-relay is an SVI setting".into(),
        });
    }
    Ok(())
}

fn admin_state(children: &[Item], name: &str) -> Result<Option<bool>, IntentError> {
    let shutdown = ConfigTree::has_leaf(children, "shutdown");
    let no_shutdown = ConfigTree::has_phrase(children, "no", "shutdown")
        || ConfigTree::has_leaf(children, "no-shutdown");
    if shutdown && no_shutdown {
        return Err(IntentError::BadInterfaceBlock(format!(
            "{name}: both shutdown and no-shutdown"
        )));
    }
    if shutdown {
        return Ok(Some(false));
    }
    if no_shutdown {
        return Ok(Some(true));
    }
    match ConfigTree::leaf_value(children, "admin-state") {
        Some("enabled") => Ok(Some(true)),
        Some("disabled") => Ok(Some(false)),
        Some(other) => Err(IntentError::BadAdminState {
            name: name.to_string(),
            value: other.to_string(),
        }),
        None => Ok(None),
    }
}

/// An 802.1Q VLAN id.
fn parse_vlan_id(text: &str) -> Result<u16, String> {
    text.parse::<u16>()
        .ok()
        .filter(|id| (1..=4094).contains(id))
        .ok_or_else(|| format!("bad VLAN id {text:?} (1..4094)"))
}

/// The `switchport { ... }` block of a port, when present.
fn switchport(children: &[Item], name: &str) -> Result<Option<SwitchportIntent>, IntentError> {
    let Some((_, sp)) = ConfigTree::blocks_named(children, "switchport").next() else {
        return Ok(None);
    };
    let bad = |reason: String| IntentError::BadSwitchport {
        name: name.to_string(),
        reason,
    };
    let mode = match ConfigTree::leaf_value(sp, "mode") {
        Some("trunk") => SwitchportMode::Trunk,
        Some("dot1q-tunnel") => SwitchportMode::Dot1qTunnel,
        Some("access") | None => SwitchportMode::Access,
        Some(other) => {
            return Err(bad(format!(
                "mode must be `access`, `trunk` or `dot1q-tunnel`, got {other:?}"
            )))
        }
    };
    if mode == SwitchportMode::Dot1qTunnel
        && (ConfigTree::phrase_values(sp, "trunk", "vlans").is_some()
            || ConfigTree::phrase_values(sp, "native", "vlan").is_some()
            || ConfigTree::leaf_values(sp, "trunk-vlans").is_some()
            || ConfigTree::leaf_value(sp, "native-vlan").is_some())
    {
        return Err(bad(
            "dot1q-tunnel mode excludes trunk configuration".to_string()
        ));
    }
    // Phrase forms (`access vlan 10`), with the hyphenated legacy leaves
    // (`access-vlan 10`) still accepted.
    let vlan_leaf = |first: &str, second: &str, legacy: &str| -> Result<Option<u16>, IntentError> {
        let value = ConfigTree::phrase_values(sp, first, second)
            .and_then(<[String]>::first)
            .map(String::as_str)
            .or_else(|| ConfigTree::leaf_value(sp, legacy));
        match value {
            Some(value) => parse_vlan_id(value).map(Some).map_err(&bad),
            None => Ok(None),
        }
    };
    let mut trunk_vlans = Vec::new();
    let trunk_values = ConfigTree::phrase_values(sp, "trunk", "vlans")
        .or_else(|| ConfigTree::leaf_values(sp, "trunk-vlans"));
    if let Some(values) = trunk_values {
        // Lists render as `10, 20, 30`; each word may carry a trailing
        // comma, and hand-written `10 20 30` / `10,20,30` still parse.
        for value in values {
            for part in value.split(',').filter(|p| !p.is_empty()) {
                trunk_vlans.push(parse_vlan_id(part).map_err(&bad)?);
            }
        }
        trunk_vlans.sort_unstable();
        trunk_vlans.dedup();
    }
    Ok(Some(SwitchportIntent {
        mode,
        access_vlan: vlan_leaf("access", "vlan", "access-vlan")?,
        trunk_vlans,
        native_vlan: vlan_leaf("native", "vlan", "native-vlan")?,
    }))
}

/// `channel-group <n> mode <active|passive|on>` on a member port.
fn channel_group(children: &[Item], name: &str) -> Result<Option<ChannelGroup>, IntentError> {
    let Some(values) = ConfigTree::leaf_values(children, "channel-group") else {
        return Ok(None);
    };
    let bad = |reason: String| IntentError::BadChannelGroup {
        name: name.to_string(),
        reason,
    };
    let [group, keyword, mode] = values else {
        return Err(bad("expected `channel-group <1-64> mode <mode>`".into()));
    };
    if keyword != "mode" {
        return Err(bad(format!("expected `mode`, got {keyword:?}")));
    }
    let group = group
        .parse::<u16>()
        .ok()
        .filter(|n| (1..=64).contains(n))
        .ok_or_else(|| bad(format!("bad channel-group number {group:?} (1..64)")))?;
    let mode = match mode.as_str() {
        "active" => LacpMode::Active,
        "passive" => LacpMode::Passive,
        "on" => LacpMode::On,
        other => {
            return Err(bad(format!(
                "mode must be `active`, `passive` or `on`, got {other:?}"
            )))
        }
    };
    Ok(Some(ChannelGroup { group, mode }))
}

/// A bounded integer leaf value.
fn parse_int<T: std::str::FromStr + PartialOrd + Copy>(
    text: &str,
    range: std::ops::RangeInclusive<T>,
    what: &str,
) -> Result<T, String> {
    text.parse::<T>()
        .ok()
        .filter(|n| range.contains(n))
        .ok_or_else(|| format!("bad {what} {text:?}"))
}

/// A member port's `lacp { rate ...; port-priority ... }` block.
fn port_lacp(children: &[Item], name: &str) -> Result<Option<PortLacpIntent>, IntentError> {
    let Some((_, lacp)) = ConfigTree::blocks_named(children, "lacp").next() else {
        return Ok(None);
    };
    let bad = |reason: String| IntentError::BadChannelGroup {
        name: name.to_string(),
        reason: format!("lacp: {reason}"),
    };
    for keyword in ["fallback", "fallback-timeout"] {
        if ConfigTree::leaf_values(lacp, keyword).is_some() {
            return Err(bad(format!("{keyword} belongs on the Port-Channel")));
        }
    }
    let rate_fast = match ConfigTree::leaf_value(lacp, "rate") {
        Some("fast") => true,
        Some("normal") | None => false,
        Some(other) => {
            return Err(bad(format!(
                "rate must be `normal` or `fast`, got {other:?}"
            )))
        }
    };
    let port_priority = match ConfigTree::leaf_value(lacp, "port-priority") {
        Some(value) => Some(parse_int(value, 0u16..=65535, "port-priority").map_err(&bad)?),
        None => None,
    };
    Ok(Some(PortLacpIntent {
        rate_fast,
        port_priority,
    }))
}

/// A port-channel's `min-links <0-8>` leaf.
fn lag_min_links(children: &[Item], name: &str) -> Result<Option<u8>, IntentError> {
    match ConfigTree::leaf_value(children, "min-links") {
        Some(value) => parse_int(value, 0u8..=8, "min-links")
            .map(Some)
            .map_err(|reason| IntentError::BadLag {
                name: name.to_string(),
                reason,
            }),
        None => Ok(None),
    }
}

/// A port-channel's `lacp { fallback <static|individual> }` leaf.
fn lag_fallback(children: &[Item], name: &str) -> Result<Option<LagFallback>, IntentError> {
    let Some((_, lacp)) = ConfigTree::blocks_named(children, "lacp").next() else {
        return Ok(None);
    };
    let bad = |reason: String| IntentError::BadLag {
        name: name.to_string(),
        reason: format!("lacp: {reason}"),
    };
    for keyword in ["rate", "port-priority"] {
        if ConfigTree::leaf_values(lacp, keyword).is_some() {
            return Err(bad(format!("{keyword} belongs on the member ports")));
        }
    }
    match ConfigTree::leaf_value(lacp, "fallback") {
        Some("static") => Ok(Some(LagFallback::Static)),
        Some("individual") => Ok(Some(LagFallback::Individual)),
        Some(other) => Err(bad(format!(
            "fallback must be `static` or `individual`, got {other:?}"
        ))),
        None => Ok(None),
    }
}

/// A port-channel's `lacp { fallback-timeout <1-900> }` leaf.
fn lag_fallback_timeout(children: &[Item], name: &str) -> Result<Option<u16>, IntentError> {
    let Some((_, lacp)) = ConfigTree::blocks_named(children, "lacp").next() else {
        return Ok(None);
    };
    match ConfigTree::leaf_value(lacp, "fallback-timeout") {
        Some(value) => parse_int(value, 1u16..=900, "fallback-timeout")
            .map(Some)
            .map_err(|reason| IntentError::BadLag {
                name: name.to_string(),
                reason: format!("lacp: {reason}"),
            }),
        None => Ok(None),
    }
}

/// A port's `spanning-tree { ... }` block.
fn port_stp(children: &[Item], name: &str) -> Result<Option<PortStpIntent>, IntentError> {
    let Some((_, stp)) = ConfigTree::blocks_named(children, "spanning-tree").next() else {
        return Ok(None);
    };
    let bad = |reason: String| IntentError::BadPortStp {
        name: name.to_string(),
        reason,
    };
    let cost = match ConfigTree::leaf_value(stp, "cost") {
        Some(value) => Some(parse_int(value, 1u32..=200_000_000, "cost").map_err(&bad)?),
        None => None,
    };
    let port_priority = match ConfigTree::leaf_value(stp, "port-priority") {
        Some(value) => {
            let priority = parse_int(value, 0u8..=240, "port-priority").map_err(&bad)?;
            if priority % 16 != 0 {
                return Err(bad(format!(
                    "port-priority {priority} is not a multiple of 16"
                )));
            }
            Some(priority)
        }
        None => None,
    };
    Ok(Some(PortStpIntent {
        portfast: ConfigTree::has_leaf(stp, "portfast"),
        bpduguard: ConfigTree::has_leaf(stp, "bpduguard"),
        cost,
        port_priority,
    }))
}

pub use hemlock_common::net::{parse_storm_level, parse_unicast_mac};

/// A port's `storm-control { <kind> level <pct>; ... }` block.
fn storm_control(
    children: &[Item],
    name: &str,
) -> Result<BTreeMap<StormKind, String>, IntentError> {
    let mut out = BTreeMap::new();
    let Some((_, sc)) = ConfigTree::blocks_named(children, "storm-control").next() else {
        return Ok(out);
    };
    let bad = |reason: String| IntentError::BadStormControl {
        name: name.to_string(),
        reason,
    };
    for item in sc {
        let Item::Leaf { name: kind, values } = item else {
            return Err(bad(format!("unrecognized block {:?}", item.name())));
        };
        let kind = match kind.as_str() {
            "broadcast" => StormKind::Broadcast,
            "multicast" => StormKind::Multicast,
            "unknown-unicast" => StormKind::UnknownUnicast,
            other => return Err(bad(format!("unrecognized traffic class {other:?}"))),
        };
        let [keyword, level] = values.as_slice() else {
            return Err(bad(format!("{}: expected `level <pct>`", kind.word())));
        };
        if keyword != "level" {
            return Err(bad(format!("{}: expected `level`", kind.word())));
        }
        let level = parse_storm_level(level).map_err(&bad)?;
        if out.insert(kind, level).is_some() {
            return Err(bad(format!("duplicate {} level", kind.word())));
        }
    }
    Ok(out)
}

/// `vlans { vlan <id> { description ... } }`.
fn vlans(tree: &ConfigTree) -> Result<BTreeMap<u16, VlanIntent>, IntentError> {
    let mut out = BTreeMap::new();
    let Some((_, items)) = tree.block("vlans") else {
        return Ok(out);
    };
    for item in items {
        let Item::Block {
            name,
            keys,
            children,
        } = item
        else {
            return Err(IntentError::BadVlanBlock(format!(
                "unrecognized statement {:?}",
                item.name()
            )));
        };
        let (n, [key]) = (name.as_str(), keys.as_slice()) else {
            return Err(IntentError::BadVlanBlock(format!(
                "vlan block needs exactly one id key, got {name:?}"
            )));
        };
        if n != "vlan" {
            return Err(IntentError::BadVlanBlock(format!(
                "unrecognized block {n:?}"
            )));
        }
        let id = parse_vlan_id(key).map_err(|reason| IntentError::BadVlan {
            id: key.clone(),
            reason,
        })?;
        let suspended = match ConfigTree::leaf_value(children, "state") {
            Some("suspend") => true,
            Some("active") | None => false,
            Some(other) => {
                return Err(IntentError::BadVlan {
                    id: key.clone(),
                    reason: format!("state must be `active` or `suspend`, got {other:?}"),
                });
            }
        };
        if suspended && id == 1 {
            return Err(IntentError::BadVlan {
                id: key.clone(),
                reason: "the default VLAN cannot be suspended".into(),
            });
        }
        let intent = VlanIntent {
            description: ConfigTree::leaf_value(children, "description").map(str::to_string),
            suspended,
        };
        if out.insert(id, intent).is_some() {
            return Err(IntentError::BadVlan {
                id: key.clone(),
                reason: "duplicate vlan block".into(),
            });
        }
    }
    Ok(out)
}

fn ssh(tree: &ConfigTree) -> Result<SshIntent, IntentError> {
    let Some((_, system)) = tree.block("system") else {
        return Ok(SshIntent::default());
    };
    let Some((_, ssh)) = ConfigTree::blocks_named(system, "ssh").next() else {
        return Ok(SshIntent::default());
    };
    let mut intent = SshIntent {
        enabled: true,
        auth_local: false,
    };
    if let Some(value) = ConfigTree::leaf_value(ssh, "authentication") {
        match value {
            "local" => intent.auth_local = true,
            other => {
                return Err(IntentError::BadSsh(format!(
                    "authentication must be `local`, got {other:?}"
                )));
            }
        }
    }
    Ok(intent)
}

/// `system { http }` / `system { https }` â€” pure block presence.
fn web(tree: &ConfigTree) -> WebIntent {
    let Some((_, system)) = tree.block("system") else {
        return WebIntent::default();
    };
    let block = |name: &str| ConfigTree::blocks_named(system, name).next().is_some();
    WebIntent {
        http: block("http"),
        https: block("https"),
    }
}

/// `routing { static | arp }` (the FRR families join per suite).
/// Explicitly deferred families are rejected by name so the operator
/// sees "not supported" instead of a generic parse error.
fn routing(tree: &ConfigTree, intents: &mut Intents) -> Result<(), IntentError> {
    let Some((_, routing)) = tree.block("routing") else {
        return Ok(());
    };
    for item in routing {
        let (name, keys, children) = match item {
            Item::Block {
                name,
                keys,
                children,
            } => (name, keys, children.as_slice()),
            Item::Leaf { name, values } if name == "router-id" => {
                let [router_id] = values.as_slice() else {
                    return Err(IntentError::BadRouting(
                        "router-id takes one IPv4 address".into(),
                    ));
                };
                let router_id: std::net::Ipv4Addr = router_id
                    .parse()
                    .map_err(|_| IntentError::BadRouting(format!("bad router-id {router_id:?}")))?;
                intents.router_id = Some(router_id.to_string());
                continue;
            }
            _ => {
                return Err(IntentError::BadRouting(format!(
                    "unrecognized statement {:?}",
                    item.name()
                )));
            }
        };
        if !keys.is_empty() {
            return Err(IntentError::BadRouting(format!(
                "unrecognized block {name:?}"
            )));
        }
        match name.as_str() {
            "static" => intents.routes = static_routes(children)?,
            "arp" => intents.arp_statics = arp_statics(children)?,
            "ospf" => intents.ospf = Some(ospf_intent(children)?),
            "bgp" => intents.bgp = Some(bgp_intent(children)?),
            "vrf" | "ospfv3" | "pim" | "policy" | "route-map" | "prefix-list" => {
                return Err(IntentError::BadRouting(format!("{name} is not supported")));
            }
            _ => {
                return Err(IntentError::BadRouting(format!(
                    "unrecognized block {name:?}"
                )));
            }
        }
    }
    Ok(())
}

/// The canonical dotted form of an OSPF area id (dotted or integer).
fn canonical_area(text: &str) -> Result<String, String> {
    if let Ok(area) = text.parse::<std::net::Ipv4Addr>() {
        return Ok(area.to_string());
    }
    text.parse::<u32>()
        .map(|n| std::net::Ipv4Addr::from(n).to_string())
        .map_err(|_| format!("bad area {text:?} (dotted or 0..4294967295)"))
}

/// `ospf { router-id | area <id> { network ... } | passive-interface |
/// redistribute | maximum-paths | interface <name> { ... } }`.
fn ospf_intent(children: &[Item]) -> Result<OspfIntent, IntentError> {
    let bad = |reason: String| IntentError::BadOspf(reason);
    let mut ospf = OspfIntent::default();
    for item in children {
        match item {
            Item::Leaf { name, values } => match (name.as_str(), values.as_slice()) {
                ("router-id", [id]) => {
                    let id: std::net::Ipv4Addr = id
                        .parse()
                        .map_err(|_| bad(format!("bad router-id {id:?}")))?;
                    ospf.router_id = Some(id.to_string());
                }
                ("passive-interface", [interface]) => {
                    ospf.passive_interfaces.insert(interface.clone());
                }
                ("redistribute", [source]) => {
                    if !matches!(source.as_str(), "connected" | "static" | "bgp") {
                        return Err(bad(format!(
                            "redistribute {source:?} (connected|static|bgp)"
                        )));
                    }
                    ospf.redistribute.insert(source.clone());
                }
                ("maximum-paths", [paths]) => {
                    ospf.maximum_paths =
                        parse_int::<u8>(paths, 1..=8, "maximum-paths").map_err(&bad)?;
                }
                _ => {
                    return Err(bad(format!("unrecognized statement {name:?}")));
                }
            },
            Item::Block {
                name,
                keys,
                children,
            } => match (name.as_str(), keys.as_slice()) {
                ("area", [area]) => {
                    let area = canonical_area(area).map_err(&bad)?;
                    let networks = ospf.areas.entry(area.clone()).or_default();
                    for network in children {
                        let Item::Leaf { name, values } = network else {
                            return Err(bad(format!(
                                "area {area}: unrecognized block {:?}",
                                network.name()
                            )));
                        };
                        let ([prefix], "network") = (values.as_slice(), name.as_str()) else {
                            return Err(bad(format!("area {area}: expected `network <prefix>`")));
                        };
                        let prefix = hemlock_common::net::require_canonical_prefix(prefix)
                            .map_err(|e| bad(format!("area {area}: {e}")))?;
                        if prefix.contains(':') {
                            return Err(bad(format!(
                                "area {area}: {prefix} is IPv6 (OSPFv3 is not supported)"
                            )));
                        }
                        networks.insert(prefix);
                    }
                }
                ("interface", [interface]) => {
                    let mut knobs = OspfInterfaceIntent::default();
                    for knob in children {
                        let Item::Leaf { name, values } = knob else {
                            return Err(bad(format!(
                                "interface {interface}: unrecognized block {:?}",
                                knob.name()
                            )));
                        };
                        let [value] = values.as_slice() else {
                            return Err(bad(format!(
                                "interface {interface}: {name} takes one value"
                            )));
                        };
                        let knob_err = |e: String| bad(format!("interface {interface}: {e}"));
                        match name.as_str() {
                            "cost" => {
                                knobs.cost = Some(
                                    parse_int::<u16>(value, 1..=65535, "cost").map_err(knob_err)?,
                                );
                            }
                            "hello-interval" => {
                                knobs.hello_interval = Some(
                                    parse_int::<u16>(value, 1..=65535, "hello-interval")
                                        .map_err(knob_err)?,
                                );
                            }
                            "dead-interval" => {
                                knobs.dead_interval = Some(
                                    parse_int::<u16>(value, 1..=65535, "dead-interval")
                                        .map_err(knob_err)?,
                                );
                            }
                            "priority" => {
                                knobs.priority = Some(
                                    parse_int::<u8>(value, 0..=255, "priority")
                                        .map_err(knob_err)?,
                                );
                            }
                            other => {
                                return Err(bad(format!(
                                    "interface {interface}: unrecognized statement {other:?}"
                                )));
                            }
                        }
                    }
                    ospf.interfaces.insert(interface.clone(), knobs);
                }
                _ => {
                    return Err(bad(format!("unrecognized block {name:?}")));
                }
            },
        }
    }
    Ok(ospf)
}

/// `bgp { as | router-id | neighbor <ip> { ... } | network |
/// redistribute | maximum-paths }`.
fn bgp_intent(children: &[Item]) -> Result<BgpIntent, IntentError> {
    let bad = |reason: String| IntentError::BadBgp(reason);
    let mut bgp = BgpIntent {
        as_number: 0,
        router_id: None,
        neighbors: BTreeMap::new(),
        networks: BTreeSet::new(),
        redistribute: BTreeSet::new(),
        maximum_paths: 4,
    };
    for item in children {
        match item {
            Item::Leaf { name, values } => match (name.as_str(), values.as_slice()) {
                ("as", [as_number]) => {
                    bgp.as_number =
                        parse_int::<u32>(as_number, 1..=4294967295, "as").map_err(&bad)?;
                }
                ("router-id", [id]) => {
                    let id: std::net::Ipv4Addr = id
                        .parse()
                        .map_err(|_| bad(format!("bad router-id {id:?}")))?;
                    bgp.router_id = Some(id.to_string());
                }
                ("network", [prefix]) => {
                    let prefix =
                        hemlock_common::net::require_canonical_prefix(prefix).map_err(&bad)?;
                    if prefix.contains(':') {
                        return Err(bad(format!(
                            "{prefix} is IPv6 (the IPv6 address family is not supported)"
                        )));
                    }
                    bgp.networks.insert(prefix);
                }
                ("redistribute", [source]) => {
                    if !matches!(source.as_str(), "connected" | "static" | "ospf") {
                        return Err(bad(format!(
                            "redistribute {source:?} (connected|static|ospf)"
                        )));
                    }
                    bgp.redistribute.insert(source.clone());
                }
                ("maximum-paths", [paths]) => {
                    bgp.maximum_paths =
                        parse_int::<u8>(paths, 1..=8, "maximum-paths").map_err(&bad)?;
                }
                _ => {
                    return Err(bad(format!("unrecognized statement {name:?}")));
                }
            },
            Item::Block {
                name,
                keys,
                children,
            } => match (name.as_str(), keys.as_slice()) {
                ("neighbor", [address]) => {
                    let ip: std::net::IpAddr = address
                        .parse()
                        .map_err(|_| bad(format!("bad neighbor address {address:?}")))?;
                    if ip.is_ipv6() {
                        return Err(bad(format!(
                            "neighbor {ip} is IPv6 (the IPv6 address family is not supported)"
                        )));
                    }
                    let mut neighbor = BgpNeighborIntent::default();
                    for knob in children {
                        let Item::Leaf { name, values } = knob else {
                            return Err(bad(format!(
                                "neighbor {ip}: unrecognized block {:?}",
                                knob.name()
                            )));
                        };
                        let knob_err = |e: String| bad(format!("neighbor {ip}: {e}"));
                        match (name.as_str(), values.as_slice()) {
                            ("remote-as", [remote]) => {
                                neighbor.remote_as = Some(
                                    parse_int::<u32>(remote, 1..=4294967295, "remote-as")
                                        .map_err(knob_err)?,
                                );
                            }
                            ("description", [text]) => {
                                neighbor.description = Some(text.clone());
                            }
                            ("shutdown", []) => neighbor.shutdown = true,
                            ("ebgp-multihop", [ttl]) => {
                                neighbor.ebgp_multihop = Some(
                                    parse_int::<u8>(ttl, 1..=255, "ebgp-multihop")
                                        .map_err(knob_err)?,
                                );
                            }
                            ("next-hop-self", []) => neighbor.next_hop_self = true,
                            _ => {
                                return Err(bad(format!(
                                    "neighbor {ip}: unrecognized statement {name:?}"
                                )));
                            }
                        }
                    }
                    bgp.neighbors.insert(ip.to_string(), neighbor);
                }
                _ => {
                    return Err(bad(format!("unrecognized block {name:?}")));
                }
            },
        }
    }
    // `as` is required as soon as any other bgp leaf exists.
    if bgp.as_number == 0 {
        return Err(bad("as is required".into()));
    }
    for (ip, neighbor) in &bgp.neighbors {
        if neighbor.remote_as.is_none() {
            return Err(bad(format!("neighbor {ip}: remote-as is required")));
        }
    }
    Ok(bgp)
}

fn static_routes(children: &[Item]) -> Result<BTreeMap<String, StaticRoute>, IntentError> {
    let mut routes: BTreeMap<String, StaticRoute> = BTreeMap::new();
    // Explicit `distance` values seen per prefix â€” distance is
    // per-prefix, so explicit values on different lines must agree
    // (lines without one inherit rather than conflict).
    let mut explicit_distance: BTreeMap<String, u8> = BTreeMap::new();
    {
        for route in children {
            let Item::Leaf {
                name: prefix,
                values,
            } = route
            else {
                return Err(IntentError::BadRouting(format!(
                    "static: unrecognized block {:?}",
                    route.name()
                )));
            };
            let bad = |reason: String| IntentError::BadRoute {
                prefix: prefix.clone(),
                reason,
            };
            let canonical = hemlock_common::net::require_canonical_prefix(prefix).map_err(&bad)?;
            let entry = routes.entry(canonical.clone()).or_default();
            match values.as_slice() {
                [keyword] if keyword == "drop" => {
                    if !entry.next_hops.is_empty() {
                        return Err(bad("cannot mix drop with next hops".into()));
                    }
                    entry.drop = true;
                }
                [next_hop, rest @ ..] => {
                    hemlock_common::net::validate_next_hop(&canonical, next_hop).map_err(&bad)?;
                    if entry.drop {
                        return Err(bad("cannot mix drop with next hops".into()));
                    }
                    match rest {
                        [] => {}
                        [keyword, value] if keyword == "distance" => {
                            let distance =
                                parse_int::<u8>(value, 1..=255, "distance").map_err(&bad)?;
                            if let Some(prior) =
                                explicit_distance.insert(canonical.clone(), distance)
                            {
                                if prior != distance {
                                    return Err(bad(format!(
                                        "conflicting distances ({prior} and {distance}); \
                                         distance is per-prefix"
                                    )));
                                }
                            }
                            entry.distance = distance;
                        }
                        _ => {
                            return Err(bad(
                                "expected `<next-hop> [distance <1-255>]` or `drop`".into()
                            ))
                        }
                    }
                    // A repeated identical next hop merges (ECMP is a set).
                    entry.next_hops.insert(next_hop.clone());
                }
                [] => {
                    return Err(bad(
                        "expected `<next-hop> [distance <1-255>]` or `drop`".into()
                    ))
                }
            }
        }
    }
    Ok(routes)
}

/// `arp { <ip> interface <name> mac <mac> }` â€” one leaf per address.
fn arp_statics(children: &[Item]) -> Result<BTreeMap<String, ArpStatic>, IntentError> {
    let mut statics = BTreeMap::new();
    for item in children {
        let Item::Leaf { name: ip, values } = item else {
            return Err(IntentError::BadRouting(format!(
                "arp: unrecognized block {:?}",
                item.name()
            )));
        };
        let bad = |reason: String| IntentError::BadArp {
            ip: ip.clone(),
            reason,
        };
        let address: std::net::IpAddr =
            ip.parse().map_err(|_| bad(format!("bad address {ip:?}")))?;
        let [keyword_interface, interface, keyword_mac, mac] = values.as_slice() else {
            return Err(bad("expected `interface <name> mac <mac>`".into()));
        };
        if keyword_interface != "interface" || keyword_mac != "mac" {
            return Err(bad("expected `interface <name> mac <mac>`".into()));
        }
        let mac = hemlock_common::net::parse_unicast_mac(mac).map_err(&bad)?;
        statics.insert(
            address.to_string(),
            ArpStatic {
                interface: interface.clone(),
                mac,
            },
        );
    }
    Ok(statics)
}

/// `protocols { spanning-tree | igmp-snooping | mld-snooping | lacp }`.
fn protocols(tree: &ConfigTree, intents: &mut Intents) -> Result<(), IntentError> {
    let Some((_, items)) = tree.block("protocols") else {
        return Ok(());
    };
    for item in items {
        let Item::Block {
            name,
            keys,
            children,
        } = item
        else {
            return Err(IntentError::BadProtocols(format!(
                "unrecognized statement {:?}",
                item.name()
            )));
        };
        if !keys.is_empty() {
            return Err(IntentError::BadProtocols(format!(
                "unrecognized block {name:?}"
            )));
        }
        match name.as_str() {
            "spanning-tree" => intents.stp = stp(children)?,
            "igmp-snooping" => intents.igmp_snooping = snooping(children, "igmp-snooping")?,
            "mld-snooping" => intents.mld_snooping = snooping(children, "mld-snooping")?,
            "lacp" => intents.lacp = lacp_global(children)?,
            other => {
                return Err(IntentError::BadProtocols(format!(
                    "unrecognized block {other:?}"
                )));
            }
        }
    }
    Ok(())
}

/// `protocols { lacp { system-priority <n> } }`.
fn lacp_global(items: &[Item]) -> Result<LacpGlobalIntent, IntentError> {
    let bad = |reason: String| IntentError::BadProtocols(format!("lacp: {reason}"));
    let system_priority = match ConfigTree::leaf_value(items, "system-priority") {
        Some(value) => Some(parse_int(value, 0u16..=65535, "system-priority").map_err(bad)?),
        None => None,
    };
    Ok(LacpGlobalIntent { system_priority })
}

/// `protocols { spanning-tree { ... } }`.
fn stp(items: &[Item]) -> Result<StpIntent, IntentError> {
    let bad = IntentError::BadStp;
    let mode = match ConfigTree::leaf_value(items, "mode") {
        Some("mstp") | None => StpMode::Mstp,
        Some("rstp") => StpMode::Rstp,
        Some("none") => StpMode::None,
        Some("rapid-pvst") => {
            return Err(bad(
                "mode rapid-pvst is not supported (use mstp or rstp)".into()
            ));
        }
        Some(other) => {
            return Err(bad(format!(
                "mode must be `mstp`, `rstp` or `none`, got {other:?}"
            )));
        }
    };
    let priority = match ConfigTree::leaf_value(items, "priority") {
        Some(value) => {
            let priority = parse_int(value, 0u16..=61440, "priority").map_err(bad)?;
            if priority % 4096 != 0 {
                return Err(bad(format!(
                    "priority {priority} is not a multiple of 4096"
                )));
            }
            Some(priority)
        }
        None => None,
    };
    let timer = |leaf: &str, range: std::ops::RangeInclusive<u8>| -> Result<Option<u8>, _> {
        match ConfigTree::leaf_value(items, leaf) {
            Some(value) => parse_int(value, range, leaf).map(Some).map_err(bad),
            None => Ok(None),
        }
    };

    let mut intent = StpIntent {
        mode,
        priority,
        hello_time: timer("hello-time", 1..=10)?,
        max_age: timer("max-age", 6..=40)?,
        forward_time: timer("forward-time", 4..=30)?,
        ..StpIntent::default()
    };

    if let Some((_, mst)) = ConfigTree::blocks_named(items, "mst").next() {
        intent.mst_name = ConfigTree::leaf_value(mst, "name").map(str::to_string);
        intent.mst_revision = match ConfigTree::leaf_value(mst, "revision") {
            Some(value) => Some(parse_int(value, 0u16..=65535, "mst revision").map_err(bad)?),
            None => None,
        };
        let mut mapped: BTreeMap<u16, u8> = BTreeMap::new();
        for item in mst {
            let Item::Leaf { name, values } = item else {
                return Err(bad(format!("mst: unrecognized block {:?}", item.name())));
            };
            if name != "instance" {
                continue;
            }
            let [id, keyword, vlan_values @ ..] = values.as_slice() else {
                return Err(bad("mst: expected `instance <1-15> vlans <list>`".into()));
            };
            let id = parse_int(id, 1u8..=15, "mst instance").map_err(bad)?;
            if keyword != "vlans" || vlan_values.is_empty() {
                return Err(bad(format!("mst instance {id}: expected `vlans <list>`")));
            }
            let mut vlans = Vec::new();
            for value in vlan_values {
                let vlan = parse_vlan_id(value).map_err(bad)?;
                if let Some(other) = mapped.insert(vlan, id) {
                    return Err(bad(format!(
                        "vlan {vlan} is mapped to both mst instance {other} and {id}"
                    )));
                }
                vlans.push(vlan);
            }
            vlans.sort_unstable();
            vlans.dedup();
            if intent.instances.insert(id, vlans).is_some() {
                return Err(bad(format!("duplicate mst instance {id}")));
            }
        }
    }
    Ok(intent)
}

/// `protocols { igmp-snooping { ... } }` (and the mld mirror).
fn snooping(items: &[Item], family: &'static str) -> Result<SnoopingIntent, IntentError> {
    let bad = |reason: String| IntentError::BadSnooping { family, reason };
    let mut intent = SnoopingIntent {
        disabled: ConfigTree::has_leaf(items, "disable"),
        robustness: match ConfigTree::leaf_value(items, "robustness") {
            Some(value) => Some(parse_int(value, 1u8..=3, "robustness").map_err(&bad)?),
            None => None,
        },
        vlans: BTreeMap::new(),
    };
    for item in items {
        let (key, children): (&str, &[Item]) = match item {
            // `vlan 10 { ... }` â€” per-VLAN settings.
            Item::Block {
                name,
                keys,
                children,
            } if name == "vlan" => match keys.as_slice() {
                [key] => (key, children),
                _ => return Err(bad("vlan block needs exactly one id key".into())),
            },
            // `vlan 10` â€” the bare enabled form.
            Item::Leaf { name, values } if name == "vlan" => match values.as_slice() {
                [key] => (key, &[]),
                _ => return Err(bad("expected `vlan <id>`".into())),
            },
            _ => continue,
        };
        let id = parse_vlan_id(key).map_err(&bad)?;
        let vbad = |reason: String| IntentError::BadSnooping {
            family,
            reason: format!("vlan {id}: {reason}"),
        };
        let querier_values = ConfigTree::leaf_values(children, "querier");
        let (querier, querier_address) = match querier_values {
            None => (false, None),
            Some([]) => (true, None),
            Some([keyword, address]) if keyword == "address" => {
                if address.parse::<std::net::Ipv4Addr>().is_err() && family == "igmp-snooping" {
                    return Err(vbad(format!("bad querier address {address:?}")));
                }
                if address.parse::<std::net::Ipv6Addr>().is_err() && family == "mld-snooping" {
                    return Err(vbad(format!("bad querier address {address:?}")));
                }
                (true, Some(address.clone()))
            }
            Some(_) => return Err(vbad("expected `querier [address <ip>]`".into())),
        };
        let mut mrouters = Vec::new();
        for item in children {
            let Item::Leaf { name, values } = item else {
                continue;
            };
            if name != "mrouter" {
                continue;
            }
            let [keyword, port] = values.as_slice() else {
                return Err(vbad("expected `mrouter interface <port>`".into()));
            };
            if keyword != "interface" {
                return Err(vbad(format!("expected `interface`, got {keyword:?}")));
            }
            mrouters.push(port.clone());
        }
        mrouters.sort();
        mrouters.dedup();
        let vlan = SnoopVlanIntent {
            disabled: ConfigTree::has_leaf(children, "disable"),
            fast_leave: ConfigTree::has_leaf(children, "fast-leave"),
            querier,
            querier_address,
            mrouters,
        };
        if intent.vlans.insert(id, vlan).is_some() {
            return Err(bad(format!("duplicate vlan {id}")));
        }
    }
    Ok(intent)
}

/// `services { lldp { ... } }` — the network-services families.
fn services(tree: &ConfigTree, intents: &mut Intents) -> Result<(), IntentError> {
    let Some((_, items)) = tree.block("services") else {
        return Ok(());
    };
    for item in items {
        let Item::Block {
            name,
            keys,
            children,
        } = item
        else {
            return Err(IntentError::BadServices(format!(
                "unrecognized statement {:?}",
                item.name()
            )));
        };
        if !keys.is_empty() {
            return Err(IntentError::BadServices(format!(
                "unrecognized block {name:?}"
            )));
        }
        match name.as_str() {
            "lldp" => intents.lldp = lldp(children)?,
            "ntp" => intents.ntp = ntp(children)?,
            "snmp" => intents.snmp = snmp(children)?,
            "sflow" => intents.sflow = sflow(children)?,
            other => {
                return Err(IntentError::BadServices(format!(
                    "unrecognized block {other:?}"
                )));
            }
        }
    }
    Ok(())
}

/// `services { lldp { disable; tx-interval <s>; hold-multiplier <n> } }`.
fn lldp(items: &[Item]) -> Result<LldpIntent, IntentError> {
    let bad = IntentError::BadLldp;
    for item in items {
        let Item::Leaf { name, .. } = item else {
            return Err(bad(format!("unrecognized block {:?}", item.name())));
        };
        if !matches!(name.as_str(), "disable" | "tx-interval" | "hold-multiplier") {
            return Err(bad(format!("unrecognized statement {name:?}")));
        }
    }
    Ok(LldpIntent {
        disabled: ConfigTree::has_leaf(items, "disable"),
        tx_interval: match ConfigTree::leaf_value(items, "tx-interval") {
            Some(value) => Some(parse_int(value, 5u16..=300, "tx-interval").map_err(bad)?),
            None => None,
        },
        hold_multiplier: match ConfigTree::leaf_value(items, "hold-multiplier") {
            Some(value) => Some(parse_int(value, 2u8..=10, "hold-multiplier").map_err(bad)?),
            None => None,
        },
    })
}

/// One NTP server address: an IP literal, or a syntactically valid
/// hostname (resolution is timesyncd's problem, not the parser's).
pub fn valid_ntp_server(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// `services { ntp { server <host>; ... } }`.
fn ntp(items: &[Item]) -> Result<NtpIntent, IntentError> {
    let bad = IntentError::BadNtp;
    let mut servers: Vec<String> = Vec::new();
    for item in items {
        let Item::Leaf { name, values } = item else {
            return Err(bad(format!("unrecognized block {:?}", item.name())));
        };
        // Deferred by this suite; named rather than silently ignored.
        match name.as_str() {
            "server" => {}
            "listen" | "master" => {
                return Err(bad("NTP server mode is not supported".into()));
            }
            "authentication" | "key" => {
                return Err(bad("NTP authentication is not supported".into()));
            }
            other => return Err(bad(format!("unrecognized statement {other:?}"))),
        }
        let [host] = values.as_slice() else {
            return Err(bad("expected `server <host>`".into()));
        };
        if !valid_ntp_server(host) {
            return Err(bad(format!("bad server {host:?}")));
        }
        if !servers.iter().any(|existing| existing == host) {
            servers.push(host.clone());
        }
    }
    if servers.len() > MAX_NTP_SERVERS {
        return Err(bad(format!(
            "at most {MAX_NTP_SERVERS} servers ({} configured)",
            servers.len()
        )));
    }
    Ok(NtpIntent { servers })
}

/// `services { snmp { community ...; location ...; contact ...;
/// user ... } }`.
fn snmp(items: &[Item]) -> Result<SnmpIntent, IntentError> {
    let bad = IntentError::BadSnmp;
    let mut intent = SnmpIntent {
        enabled: true,
        ..SnmpIntent::default()
    };
    for item in items {
        let Item::Leaf { name, values } = item else {
            return Err(bad(format!("unrecognized block {:?}", item.name())));
        };
        match name.as_str() {
            "community" => {
                let (community, source) = match values.as_slice() {
                    [community] => (community, None),
                    [community, keyword, prefix] if keyword == "source" => {
                        let prefix = hemlock_common::net::canonical_prefix(prefix)
                            .map_err(|reason| bad(format!("community {community}: {reason}")))?;
                        (community, Some(prefix))
                    }
                    _ => return Err(bad("expected `community <name> [source <prefix>]`".into())),
                };
                if !valid_snmp_name(community) {
                    return Err(bad(format!("bad community name {community:?}")));
                }
                if intent.communities.iter().any(|c| c.name == *community) {
                    return Err(bad(format!("duplicate community {community}")));
                }
                intent.communities.push(SnmpCommunityIntent {
                    name: community.clone(),
                    source,
                });
            }
            "user" => {
                let [user, auth_keyword, auth_protocol, auth_password, priv_keyword, priv_protocol, priv_password] =
                    values.as_slice()
                else {
                    return Err(bad(
                        "expected `user <name> auth sha <pass> priv aes <pass>`".into(),
                    ));
                };
                if !valid_snmp_name(user) {
                    return Err(bad(format!("bad user name {user:?}")));
                }
                if auth_keyword != "auth" || priv_keyword != "priv" {
                    return Err(bad(
                        "expected `user <name> auth sha <pass> priv aes <pass>`".into(),
                    ));
                }
                // Read-only authPriv with fixed protocols: weaker ones
                // are refused rather than silently downgraded.
                if auth_protocol != "sha" {
                    return Err(bad(format!(
                        "user {user}: auth protocol {auth_protocol:?} is not supported (sha only)"
                    )));
                }
                if priv_protocol != "aes" {
                    return Err(bad(format!(
                        "user {user}: priv protocol {priv_protocol:?} is not supported (aes only)"
                    )));
                }
                for password in [auth_password, priv_password] {
                    if password.len() < MIN_SNMP_PASSWORD {
                        return Err(bad(format!(
                            "user {user}: passwords must be at least {MIN_SNMP_PASSWORD} characters"
                        )));
                    }
                }
                let entry = SnmpUser {
                    auth_password: auth_password.clone(),
                    priv_password: priv_password.clone(),
                };
                if intent.users.insert(user.clone(), entry).is_some() {
                    return Err(bad(format!("duplicate user {user}")));
                }
            }
            leaf @ ("location" | "contact") => {
                let [text] = values.as_slice() else {
                    return Err(bad(format!("expected `{leaf} <text>`")));
                };
                let slot = if leaf == "location" {
                    &mut intent.location
                } else {
                    &mut intent.contact
                };
                if slot.is_some() {
                    return Err(bad(format!("duplicate {leaf}")));
                }
                *slot = Some(text.clone());
            }
            // Deferred by this suite; named rather than ignored.
            "trap" | "trap2sink" | "informs" | "trapsink" => {
                return Err(bad("SNMP traps and informs are not supported".into()));
            }
            "rwcommunity" | "rwuser" => {
                return Err(bad("SNMP write access is not supported".into()));
            }
            other => return Err(bad(format!("unrecognized statement {other:?}"))),
        }
    }
    Ok(intent)
}

/// `services { sflow { collector ...; sample-rate ...;
/// polling-interval ... } }`.
fn sflow(items: &[Item]) -> Result<SflowIntent, IntentError> {
    let bad = IntentError::BadSflow;
    let mut intent = SflowIntent::default();
    for item in items {
        let Item::Leaf { name, values } = item else {
            return Err(bad(format!("unrecognized block {:?}", item.name())));
        };
        match name.as_str() {
            "collector" => {
                let (address, port) = match values.as_slice() {
                    [address] => (address, None),
                    [address, keyword, port] if keyword == "port" => {
                        let port = parse_int(port, 1u16..=65535, "collector port").map_err(bad)?;
                        (address, Some(port))
                    }
                    _ => return Err(bad("expected `collector <ip> [port <1-65535>]`".into())),
                };
                if address.parse::<std::net::IpAddr>().is_err() {
                    return Err(bad(format!("bad collector address {address:?}")));
                }
                if intent.collectors.iter().any(|c| c.address == *address) {
                    return Err(bad(format!("duplicate collector {address}")));
                }
                intent.collectors.push(SflowCollector {
                    address: address.clone(),
                    port,
                });
            }
            "sample-rate" => {
                let [value] = values.as_slice() else {
                    return Err(bad("expected `sample-rate <256-1048576>`".into()));
                };
                let rate = parse_int(
                    value,
                    MIN_SFLOW_SAMPLE_RATE..=MAX_SFLOW_SAMPLE_RATE,
                    "sample-rate",
                )
                .map_err(bad)?;
                // The ASIC's sampler divides by a power of two; naming
                // the neighbours turns a rejection into a correction.
                if !rate.is_power_of_two() {
                    let (below, above) = nearest_sample_rates(rate);
                    return Err(bad(format!(
                        "sample-rate {rate} is not a power of two (nearest: {below}, {above})"
                    )));
                }
                intent.sample_rate = Some(rate);
            }
            "polling-interval" => {
                let [value] = values.as_slice() else {
                    return Err(bad("expected `polling-interval <5-300>`".into()));
                };
                intent.polling_interval =
                    Some(parse_int(value, 5u16..=300, "polling-interval").map_err(bad)?);
            }
            // Deferred by this suite; named rather than ignored.
            "egress" | "egress-sample-rate" => {
                return Err(bad("sFlow egress sampling is not supported".into()));
            }
            other => return Err(bad(format!("unrecognized statement {other:?}"))),
        }
    }
    if intent.collectors.len() > MAX_SFLOW_COLLECTORS {
        return Err(bad(format!(
            "at most {MAX_SFLOW_COLLECTORS} collectors ({} configured)",
            intent.collectors.len()
        )));
    }
    Ok(intent)
}

/// `switching { mac-table { ... } mirror { ... } }`.
fn switching(tree: &ConfigTree, intents: &mut Intents) -> Result<(), IntentError> {
    let Some((_, items)) = tree.block("switching") else {
        return Ok(());
    };
    for item in items {
        let Item::Block {
            name,
            keys,
            children,
        } = item
        else {
            return Err(IntentError::BadSwitching(format!(
                "unrecognized statement {:?}",
                item.name()
            )));
        };
        if !keys.is_empty() {
            return Err(IntentError::BadSwitching(format!(
                "unrecognized block {name:?}"
            )));
        }
        match name.as_str() {
            "mac-table" => intents.mac_table = mac_table(children)?,
            "mirror" => intents.mirror = mirror(children)?,
            other => {
                return Err(IntentError::BadSwitching(format!(
                    "unrecognized block {other:?}"
                )));
            }
        }
    }
    Ok(())
}

/// `switching { mac-table { aging-time <s>; static <mac> vlan <id> ... } }`.
fn mac_table(items: &[Item]) -> Result<MacTableIntent, IntentError> {
    let bad = IntentError::BadMacTable;
    let mut intent = MacTableIntent::default();
    for item in items {
        let Item::Leaf { name, values } = item else {
            return Err(bad(format!("unrecognized block {:?}", item.name())));
        };
        match name.as_str() {
            "aging-time" => {
                let [value] = values.as_slice() else {
                    return Err(bad("expected `aging-time <seconds>`".into()));
                };
                let secs = parse_int(value, 0u32..=1_000_000, "aging-time").map_err(bad)?;
                if secs != 0 && secs < 10 {
                    return Err(bad(format!("bad aging-time {secs} (0 or 10..1000000)")));
                }
                intent.aging_time = Some(secs);
            }
            "static" => {
                let (mac, vlan, target) = match values.as_slice() {
                    [mac, kw, vlan, rest @ ..] if kw == "vlan" => {
                        let target = match rest {
                            [kw, port] if kw == "interface" => FdbTarget::Port(port.clone()),
                            [kw] if kw == "drop" => FdbTarget::Drop,
                            _ => {
                                return Err(bad(format!(
                                    "static {mac}: expected `interface <port>` or `drop`"
                                )));
                            }
                        };
                        (mac, vlan, target)
                    }
                    _ => {
                        return Err(bad(
                            "expected `static <mac> vlan <id> interface <port>|drop`".into(),
                        ));
                    }
                };
                let mac = parse_unicast_mac(mac).map_err(bad)?;
                let vlan = parse_vlan_id(vlan).map_err(bad)?;
                if intent.statics.insert((mac.clone(), vlan), target).is_some() {
                    return Err(bad(format!("duplicate static entry {mac} vlan {vlan}")));
                }
            }
            other => return Err(bad(format!("unrecognized statement {other:?}"))),
        }
    }
    Ok(intent)
}

/// `switching { mirror { session <n> { source ...; destination ... } } }`.
fn mirror(items: &[Item]) -> Result<BTreeMap<u8, MirrorIntent>, IntentError> {
    let mut out = BTreeMap::new();
    for item in items {
        let Item::Block {
            name,
            keys,
            children,
        } = item
        else {
            return Err(IntentError::BadMirrorBlock(format!(
                "unrecognized statement {:?}",
                item.name()
            )));
        };
        let (n, [key]) = (name.as_str(), keys.as_slice()) else {
            return Err(IntentError::BadMirrorBlock(format!(
                "session block needs exactly one id key, got {name:?}"
            )));
        };
        if n != "session" {
            return Err(IntentError::BadMirrorBlock(format!(
                "unrecognized block {n:?}"
            )));
        }
        let session = parse_int(key, 1u8..=4, "session").map_err(IntentError::BadMirrorBlock)?;
        let bad = |reason: String| IntentError::BadMirror { session, reason };
        let mut intent = MirrorIntent::default();
        for item in children {
            let Item::Leaf { name, values } = item else {
                return Err(bad(format!("unrecognized block {:?}", item.name())));
            };
            match name.as_str() {
                "source" => {
                    let (port, direction) = match values.as_slice() {
                        [port] => (port, MirrorDirection::Both),
                        [port, dir] => {
                            let direction = match dir.as_str() {
                                "rx" => MirrorDirection::Rx,
                                "tx" => MirrorDirection::Tx,
                                "both" => MirrorDirection::Both,
                                other => {
                                    return Err(bad(format!(
                                        "direction must be `rx`, `tx` or `both`, got {other:?}"
                                    )));
                                }
                            };
                            (port, direction)
                        }
                        _ => return Err(bad("expected `source <port> [rx|tx|both]`".into())),
                    };
                    if intent.sources.insert(port.clone(), direction).is_some() {
                        return Err(bad(format!("duplicate source {port}")));
                    }
                }
                "destination" => {
                    let [port] = values.as_slice() else {
                        return Err(bad("expected `destination <port>`".into()));
                    };
                    if intent.destination.replace(port.clone()).is_some() {
                        return Err(bad("duplicate destination".into()));
                    }
                }
                other => return Err(bad(format!("unrecognized statement {other:?}"))),
            }
        }
        if out.insert(session, intent).is_some() {
            return Err(IntentError::BadMirrorBlock(format!(
                "duplicate session {session}"
            )));
        }
    }
    Ok(out)
}

/// One change to push to syncd.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortChange {
    pub name: String,
    pub admin_up: Option<bool>,
    pub description: Option<String>,
    /// Pinned line rate in Mb/s; `Some(0)` = stop pinning (negotiate).
    pub speed_mbps: Option<u32>,
    /// `"auto"`, `"full"` or `"half"`.
    pub duplex: Option<String>,
    /// L2 MTU in bytes; `Some(0)` = back to the platform default.
    pub mtu: Option<u32>,
}

impl PortChange {
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(up) = self.admin_up {
            parts.push(format!(
                "admin-state {}",
                if up { "enabled" } else { "disabled" }
            ));
        }
        if let Some(desc) = &self.description {
            parts.push(format!("description {desc:?}"));
        }
        if let Some(speed) = self.speed_mbps {
            parts.push(match speed {
                0 => "speed auto".to_string(),
                mbps => format!("speed {}", link::format_speed(mbps)),
            });
        }
        if let Some(duplex) = &self.duplex {
            parts.push(format!("duplex {duplex}"));
        }
        if let Some(mtu) = self.mtu {
            parts.push(match mtu {
                0 => "mtu default".to_string(),
                bytes => format!("mtu {bytes}"),
            });
        }
        format!("{}: {}", self.name, parts.join(", "))
    }
}

/// Diff candidate intents against running intents.
///
/// An interface that disappears from the config reverts to defaults
/// (admin up, empty description).
pub fn diff(
    running: &BTreeMap<String, InterfaceIntent>,
    candidate: &BTreeMap<String, InterfaceIntent>,
) -> Vec<PortChange> {
    let mut changes = Vec::new();

    for (name, wanted) in candidate {
        let current = running.get(name);
        let admin_now = current.and_then(|c| c.admin_up);
        let desc_now = current.and_then(|c| c.description.clone());

        let admin_up = match (wanted.admin_up, admin_now) {
            (Some(w), Some(n)) if w == n => None,
            (Some(w), Some(_)) => Some(w),
            (Some(w), None) => Some(w),
            // Intent removed -> back to default (up).
            (None, Some(false)) => Some(true),
            (None, _) => None,
        };
        let description = match (&wanted.description, &desc_now) {
            (Some(w), Some(n)) if w == n => None,
            (Some(w), _) => Some(w.clone()),
            (None, Some(n)) if !n.is_empty() => Some(String::new()),
            (None, _) => None,
        };

        let (speed_mbps, duplex, mtu) = link_delta(wanted, current);

        if admin_up.is_some()
            || description.is_some()
            || speed_mbps.is_some()
            || duplex.is_some()
            || mtu.is_some()
        {
            changes.push(PortChange {
                name: name.clone(),
                admin_up,
                description,
                speed_mbps,
                duplex,
                mtu,
            });
        }
    }

    // Interfaces configured before but absent now: revert to defaults.
    for (name, had) in running {
        if candidate.contains_key(name) {
            continue;
        }
        let admin_up = matches!(had.admin_up, Some(false)).then_some(true);
        let description = had
            .description
            .as_ref()
            .filter(|d| !d.is_empty())
            .map(|_| String::new());
        let (speed_mbps, duplex, mtu) = link_delta(&InterfaceIntent::default(), Some(had));
        if admin_up.is_some()
            || description.is_some()
            || speed_mbps.is_some()
            || duplex.is_some()
            || mtu.is_some()
        {
            changes.push(PortChange {
                name: name.clone(),
                admin_up,
                description,
                speed_mbps,
                duplex,
                mtu,
            });
        }
    }

    changes
}

/// The speed/duplex/MTU fields of one port's [`PortChange`], each
/// present only when it actually moved. A pin that goes away is sent as
/// the "stop forcing" sentinel (`0` / `auto`) rather than omitted, so
/// syncd reprograms the port instead of leaving the old pin in place.
#[allow(clippy::type_complexity)]
fn link_delta(
    wanted: &InterfaceIntent,
    current: Option<&InterfaceIntent>,
) -> (Option<u32>, Option<String>, Option<u32>) {
    let speed_now = current.and_then(|c| c.speed_mbps);
    let duplex_now = current.and_then(|c| c.duplex);
    let mtu_now = current.and_then(|c| c.mtu);

    let speed_mbps =
        (wanted.speed_mbps != speed_now).then(|| wanted.speed_mbps.unwrap_or_default());
    let duplex = (wanted.duplex != duplex_now).then(|| {
        wanted
            .duplex
            .map(|d| d.as_str().to_string())
            .unwrap_or_else(|| "auto".to_string())
    });
    let mtu = (wanted.mtu != mtu_now).then(|| wanted.mtu.unwrap_or_default());
    (speed_mbps, duplex, mtu)
}

/// One kernel-netdev change for the OS applier (a management interface,
/// or the kernel side of a front-panel port's address).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetdevChange {
    pub name: String,
    pub admin_up: Option<bool>,
    pub set_address: Option<String>,
    /// Previous address to remove first â€” an address change is del +
    /// add, since `ip addr replace` only replaces an identical local
    /// address.
    pub del_address: Option<String>,
    /// Netdev MTU to program. A deleted `mtu` leaf sends the kind's
    /// boot default rather than nothing, so the netdev actually reverts.
    pub set_mtu: Option<u32>,
}

impl NetdevChange {
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(up) = self.admin_up {
            parts.push(format!(
                "admin-state {}",
                if up { "enabled" } else { "disabled" }
            ));
        }
        match (&self.set_address, &self.del_address) {
            (Some(new), _) => parts.push(format!("address {new}")),
            (None, Some(old)) => parts.push(format!("address {old} removed")),
            (None, None) => {}
        }
        if let Some(mtu) = self.set_mtu {
            parts.push(format!("mtu {mtu}"));
        }
        format!("{}: {}", self.name, parts.join(", "))
    }
}

/// One static-route change for the OS applier. Both sides travel
/// because the configured distance is the kernel metric, and the metric
/// is part of a kernel route's identity â€” a distance change must delete
/// the old route, not just replace at the new metric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteChange {
    pub prefix: String,
    /// The previously running route, if any.
    pub old: Option<StaticRoute>,
    /// The full wanted route (whole next-hop set); None = remove.
    pub new: Option<StaticRoute>,
}

impl RouteChange {
    pub fn describe(&self) -> String {
        match &self.new {
            Some(route) if route.drop => format!("route {} drop", self.prefix),
            Some(route) => {
                let hops = route
                    .next_hops
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                if route.distance == 1 {
                    format!("route {} via {hops}", self.prefix)
                } else {
                    format!(
                        "route {} via {hops} (distance {})",
                        self.prefix, route.distance
                    )
                }
            }
            None => format!("route {} removed", self.prefix),
        }
    }
}

/// One static ARP/ND entry change for the OS applier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArpChange {
    pub ip: String,
    /// The previously running entry (removed first when the interface
    /// changed).
    pub old: Option<ArpStatic>,
    /// The wanted entry; None = remove.
    pub new: Option<ArpStatic>,
}

impl ArpChange {
    pub fn describe(&self) -> String {
        match &self.new {
            Some(entry) => format!("arp {} is {} on {}", self.ip, entry.mac, entry.interface),
            None => format!("arp {} removed", self.ip),
        }
    }
}

/// One VRRP macvlan change for the OS applier. FRR's vrrpd requires a
/// macvlan per group carrying the virtual MAC, created on the group's
/// parent netdev *before* the FRR reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VrrpMacvlanChange {
    pub interface: String,
    pub group: u8,
    /// false = delete the macvlan.
    pub create: bool,
}

impl VrrpMacvlanChange {
    pub fn describe(&self) -> String {
        format!(
            "vrrp {} group {} macvlan {} {}",
            self.interface,
            self.group,
            vrrp_macvlan_name(&self.interface, self.group),
            if self.create { "created" } else { "removed" }
        )
    }
}

/// The macvlan netdev name for one (interface, group) â€” compact so the
/// worst case ("vrrp4-v4094-255") still fits IFNAMSIZ.
pub fn vrrp_macvlan_name(interface: &str, group: u8) -> String {
    let compact = if let Some(id) = interface.strip_prefix("Vlan") {
        format!("v{id}")
    } else if let Some(n) = interface.strip_prefix("Ethernet") {
        format!("e{n}")
    } else if let Some(n) = interface.strip_prefix("Port-Channel") {
        format!("p{n}")
    } else {
        interface
            .chars()
            .filter(char::is_ascii)
            .take(4)
            .collect::<String>()
            .to_lowercase()
    };
    format!("vrrp4-{compact}-{group}")
}

/// The IPv4 virtual router MAC of a VRRP group (RFC 5798).
pub fn vrrp_virtual_mac(group: u8) -> String {
    format!("00:00:5e:00:01:{group:02x}")
}

/// The OS-side delta of one commit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OsChanges {
    pub management: Vec<NetdevChange>,
    /// Front-panel address changes: the ASIC side goes to syncd
    /// (router interface + routes); the kernel side is the same
    /// `ip addr` treatment on the port's hostif netdev.
    pub ports: Vec<NetdevChange>,
    /// SVI address changes: the ASIC side goes to syncd (VLAN router
    /// interface + routes + kernel bridge); the kernel address lands on
    /// the bridge netdev the same way.
    pub svis: Vec<NetdevChange>,
    pub routes: Vec<RouteChange>,
    pub arp: Vec<ArpChange>,
    /// VRRP macvlan creates/deletes (group config changes ride the FRR
    /// render, not this).
    pub vrrp_macvlans: Vec<VrrpMacvlanChange>,
    /// The full wanted SSH state, present exactly when it changed.
    pub ssh: Option<SshIntent>,
    /// The full wanted web console state, present exactly when it changed.
    pub web: Option<WebIntent>,
    /// The full wanted NTP client state, present exactly when it changed.
    pub ntp: Option<NtpIntent>,
}

impl OsChanges {
    pub fn is_empty(&self) -> bool {
        self.management.is_empty()
            && self.ports.is_empty()
            && self.svis.is_empty()
            && self.routes.is_empty()
            && self.arp.is_empty()
            && self.vrrp_macvlans.is_empty()
            && self.ssh.is_none()
            && self.web.is_none()
            && self.ntp.is_none()
    }

    pub fn describe(&self) -> Vec<String> {
        let ssh = self.ssh.as_ref().map(|s| {
            if s.enabled {
                let auth = if s.auth_local {
                    " (authentication local)"
                } else {
                    ""
                };
                format!("ssh enabled{auth}")
            } else {
                "ssh disabled".into()
            }
        });
        let web = self.web.as_ref().map(|w| match (w.http, w.https) {
            (true, true) => "web ui enabled (http, https)".to_string(),
            (true, false) => "web ui enabled (http)".to_string(),
            (false, true) => "web ui enabled (https)".to_string(),
            (false, false) => "web ui disabled".to_string(),
        });
        let ntp = self.ntp.as_ref().map(|n| {
            if n.servers.is_empty() {
                "ntp disabled (no servers)".to_string()
            } else {
                format!("ntp servers: {}", n.servers.join(", "))
            }
        });
        self.ports
            .iter()
            .chain(&self.svis)
            .chain(&self.management)
            .map(NetdevChange::describe)
            .chain(self.routes.iter().map(RouteChange::describe))
            .chain(self.arp.iter().map(ArpChange::describe))
            .chain(self.vrrp_macvlans.iter().map(VrrpMacvlanChange::describe))
            .chain(ssh)
            .chain(web)
            .chain(ntp)
            .collect()
    }
}

/// Address delta of one interface (used for both management netdevs and
/// front-panel ports).
fn address_delta(
    wanted: Option<&String>,
    current: Option<&String>,
) -> (Option<String>, Option<String>) {
    match (wanted, current) {
        (Some(w), Some(n)) if w == n => (None, None),
        (Some(w), Some(n)) => (Some(w.clone()), Some(n.clone())),
        (Some(w), None) => (Some(w.clone()), None),
        (None, Some(n)) => (None, Some(n.clone())),
        (None, None) => (None, None),
    }
}

/// The netdev MTU to program, or None when nothing moved. A deleted
/// intent programs `default` explicitly â€” `ip link` has no "unset".
fn mtu_delta(wanted: Option<u32>, current: Option<u32>, default: u32) -> Option<u32> {
    (wanted != current).then(|| wanted.unwrap_or(default))
}

/// Diff the OS-side families, candidate against running.
pub fn diff_os(running: &Intents, candidate: &Intents) -> OsChanges {
    let mut changes = OsChanges::default();

    for (name, wanted) in &candidate.management {
        let current = running.management.get(name);
        let admin_now = current.and_then(|c| c.admin_up);

        let admin_up = match (wanted.admin_up, admin_now) {
            (Some(w), Some(n)) if w == n => None,
            (Some(w), _) => Some(w),
            // Intent removed -> back to default (up).
            (None, Some(false)) => Some(true),
            (None, _) => None,
        };
        let (set_address, del_address) = address_delta(
            wanted.address.as_ref(),
            current.and_then(|c| c.address.as_ref()),
        );

        let set_mtu = mtu_delta(wanted.mtu, current.and_then(|c| c.mtu), link::DEFAULT_MTU);

        if admin_up.is_some() || set_address.is_some() || del_address.is_some() || set_mtu.is_some()
        {
            changes.management.push(NetdevChange {
                name: name.clone(),
                admin_up,
                set_address,
                del_address,
                set_mtu,
            });
        }
    }
    for (name, had) in &running.management {
        if candidate.management.contains_key(name) {
            continue;
        }
        let admin_up = matches!(had.admin_up, Some(false)).then_some(true);
        let del_address = had.address.clone();
        let set_mtu = mtu_delta(None, had.mtu, link::DEFAULT_MTU);
        if admin_up.is_some() || del_address.is_some() || set_mtu.is_some() {
            changes.management.push(NetdevChange {
                name: name.clone(),
                admin_up,
                set_address: None,
                del_address,
                set_mtu,
            });
        }
    }

    // Front-panel port addresses (admin state stays with the syncd port
    // diff; only the address moves through here).
    for (name, wanted) in &candidate.ports {
        let current = running.ports.get(name);
        let (set_address, del_address) = address_delta(
            wanted.address.as_ref(),
            current.and_then(|c| c.address.as_ref()),
        );
        let set_mtu = mtu_delta(
            wanted.mtu,
            current.and_then(|c| c.mtu),
            link::DEFAULT_PORT_MTU,
        );
        if set_address.is_some() || del_address.is_some() || set_mtu.is_some() {
            changes.ports.push(NetdevChange {
                name: name.clone(),
                admin_up: None,
                set_address,
                del_address,
                set_mtu,
            });
        }
    }
    for (name, had) in &running.ports {
        if candidate.ports.contains_key(name) {
            continue;
        }
        let set_mtu = mtu_delta(None, had.mtu, link::DEFAULT_PORT_MTU);
        if had.address.is_some() || set_mtu.is_some() {
            changes.ports.push(NetdevChange {
                name: name.clone(),
                admin_up: None,
                set_address: None,
                del_address: had.address.clone(),
                set_mtu,
            });
        }
    }

    // SVI addresses, the same shape as ports.
    for (name, wanted) in &candidate.svis {
        let current = running.svis.get(name);
        let (set_address, del_address) = address_delta(
            wanted.address.as_ref(),
            current.and_then(|c| c.address.as_ref()),
        );
        let set_mtu = mtu_delta(wanted.mtu, current.and_then(|c| c.mtu), link::DEFAULT_MTU);
        if set_address.is_some() || del_address.is_some() || set_mtu.is_some() {
            changes.svis.push(NetdevChange {
                name: name.clone(),
                admin_up: None,
                set_address,
                del_address,
                set_mtu,
            });
        }
    }
    for (name, had) in &running.svis {
        if candidate.svis.contains_key(name) {
            continue;
        }
        let set_mtu = mtu_delta(None, had.mtu, link::DEFAULT_MTU);
        if had.address.is_some() || set_mtu.is_some() {
            changes.svis.push(NetdevChange {
                name: name.clone(),
                admin_up: None,
                set_address: None,
                del_address: had.address.clone(),
                set_mtu,
            });
        }
    }

    for (prefix, wanted) in &candidate.routes {
        let current = running.routes.get(prefix);
        if current != Some(wanted) {
            changes.routes.push(RouteChange {
                prefix: prefix.clone(),
                old: current.cloned(),
                new: Some(wanted.clone()),
            });
        }
    }
    for (prefix, had) in &running.routes {
        if !candidate.routes.contains_key(prefix) {
            changes.routes.push(RouteChange {
                prefix: prefix.clone(),
                old: Some(had.clone()),
                new: None,
            });
        }
    }

    for (ip, wanted) in &candidate.arp_statics {
        let current = running.arp_statics.get(ip);
        if current != Some(wanted) {
            changes.arp.push(ArpChange {
                ip: ip.clone(),
                old: current.cloned(),
                new: Some(wanted.clone()),
            });
        }
    }
    for (ip, had) in &running.arp_statics {
        if !candidate.arp_statics.contains_key(ip) {
            changes.arp.push(ArpChange {
                ip: ip.clone(),
                old: Some(had.clone()),
                new: None,
            });
        }
    }

    for (interface, group) in candidate.vrrp.keys() {
        if !running.vrrp.contains_key(&(interface.clone(), *group)) {
            changes.vrrp_macvlans.push(VrrpMacvlanChange {
                interface: interface.clone(),
                group: *group,
                create: true,
            });
        }
    }
    for (interface, group) in running.vrrp.keys() {
        if !candidate.vrrp.contains_key(&(interface.clone(), *group)) {
            changes.vrrp_macvlans.push(VrrpMacvlanChange {
                interface: interface.clone(),
                group: *group,
                create: false,
            });
        }
    }

    if running.ssh != candidate.ssh {
        changes.ssh = Some(candidate.ssh.clone());
    }
    if running.web != candidate.web {
        changes.web = Some(candidate.web.clone());
    }
    if running.ntp != candidate.ntp {
        changes.ntp = Some(candidate.ntp.clone());
    }
    changes
}

/// One VLAN change for syncd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VlanChange {
    pub id: u16,
    /// The full wanted VLAN state; None = remove the VLAN.
    pub ensure: Option<VlanIntent>,
}

impl VlanChange {
    pub fn describe(&self) -> String {
        match &self.ensure {
            Some(vlan) => {
                let mut text = match &vlan.description {
                    Some(name) if !name.is_empty() => format!("VLAN {} ({name})", self.id),
                    _ => format!("VLAN {}", self.id),
                };
                if vlan.suspended {
                    text.push_str(" suspended");
                }
                text
            }
            None => format!("VLAN {} removed", self.id),
        }
    }
}

/// Diff the VLAN table, candidate against running.
pub fn diff_vlans(running: &Intents, candidate: &Intents) -> Vec<VlanChange> {
    let mut changes = Vec::new();
    for (id, wanted) in &candidate.vlans {
        if running.vlans.get(id) != Some(wanted) {
            changes.push(VlanChange {
                id: *id,
                ensure: Some(wanted.clone()),
            });
        }
    }
    for id in running.vlans.keys() {
        if !candidate.vlans.contains_key(id) {
            changes.push(VlanChange {
                id: *id,
                ensure: None,
            });
        }
    }
    changes
}

/// One port's switchport change for syncd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchportChange {
    pub name: String,
    /// The full wanted program; None = back to default L2.
    pub set: Option<SwitchportIntent>,
}

impl SwitchportChange {
    pub fn describe(&self) -> String {
        match &self.set {
            None => format!("{}: switchport removed", self.name),
            Some(sp) if sp.mode == SwitchportMode::Trunk => {
                let vlans = sp
                    .trunk_vlans
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                let native = match sp.native_vlan {
                    Some(vlan) => format!(" native {vlan}"),
                    None => String::new(),
                };
                format!(
                    "{}: switchport trunk vlans {}{native}",
                    self.name,
                    if vlans.is_empty() { "-".into() } else { vlans },
                )
            }
            Some(sp) if sp.mode == SwitchportMode::Dot1qTunnel => format!(
                "{}: switchport dot1q-tunnel vlan {}",
                self.name,
                sp.access_vlan.unwrap_or(1)
            ),
            Some(sp) => format!(
                "{}: switchport access vlan {}",
                self.name,
                sp.access_vlan.unwrap_or(1)
            ),
        }
    }
}

/// Diff the switchport programs, candidate against running.
pub fn diff_switchports(running: &Intents, candidate: &Intents) -> Vec<SwitchportChange> {
    let mut changes = Vec::new();
    for (name, wanted) in &candidate.ports {
        let current = running.ports.get(name).and_then(|p| p.switchport.as_ref());
        match (&wanted.switchport, current) {
            (Some(w), Some(n)) if w == n => {}
            (Some(w), _) => changes.push(SwitchportChange {
                name: name.clone(),
                set: Some(w.clone()),
            }),
            (None, Some(_)) => changes.push(SwitchportChange {
                name: name.clone(),
                set: None,
            }),
            (None, None) => {}
        }
    }
    for (name, had) in &running.ports {
        if !candidate.ports.contains_key(name) && had.switchport.is_some() {
            changes.push(SwitchportChange {
                name: name.clone(),
                set: None,
            });
        }
    }
    changes
}

/// The full state of one port-channel: its own config plus its member
/// ports' channel-group modes and LACP tuning.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LagEnsure {
    pub lag: LagIntent,
    /// Member port -> (mode, per-member LACP tuning).
    pub members: BTreeMap<String, (LacpMode, PortLacpIntent)>,
}

/// One port-channel change for the LAG appliers (syncd objects + orch
/// LACP engine).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LagChange {
    pub group: u16,
    /// The full wanted state; None = remove the LAG.
    pub ensure: Option<LagEnsure>,
}

impl LagChange {
    pub fn describe(&self) -> String {
        match &self.ensure {
            Some(ensure) => {
                let members = ensure.members.keys().cloned().collect::<Vec<_>>().join(",");
                let mode = ensure
                    .members
                    .values()
                    .next()
                    .map(|(mode, _)| mode.word())
                    .unwrap_or("on");
                format!(
                    "Port-Channel{}: members {} mode {mode}",
                    self.group,
                    if members.is_empty() {
                        "-".into()
                    } else {
                        members
                    },
                )
            }
            None => format!("Port-Channel{} removed", self.group),
        }
    }
}

/// Assemble every channel group's full state: configured `Port-Channel`
/// blocks plus groups that exist only through member `channel-group`
/// leaves (commit materializes those).
pub fn lag_state(intents: &Intents) -> BTreeMap<u16, LagEnsure> {
    let mut out: BTreeMap<u16, LagEnsure> = intents
        .lags
        .iter()
        .map(|(group, lag)| {
            (
                *group,
                LagEnsure {
                    lag: lag.clone(),
                    members: BTreeMap::new(),
                },
            )
        })
        .collect();
    for (name, port) in &intents.ports {
        if let Some(cg) = &port.channel_group {
            out.entry(cg.group).or_default().members.insert(
                name.clone(),
                (cg.mode, port.lacp.clone().unwrap_or_default()),
            );
        }
    }
    out
}

/// Diff the port-channel family, candidate against running.
pub fn diff_lags(running: &Intents, candidate: &Intents) -> Vec<LagChange> {
    let now = lag_state(running);
    let want = lag_state(candidate);
    let mut changes = Vec::new();
    for (group, ensure) in &want {
        if now.get(group) != Some(ensure) {
            changes.push(LagChange {
                group: *group,
                ensure: Some(ensure.clone()),
            });
        }
    }
    for group in now.keys() {
        if !want.contains_key(group) {
            changes.push(LagChange {
                group: *group,
                ensure: None,
            });
        }
    }
    changes
}

/// The full spanning-tree state the orch engine consumes: global config
/// plus every interface's port-level config.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StpState {
    pub global: StpIntent,
    /// Interface name (Ethernet or Port-Channel) -> port config.
    pub ports: BTreeMap<String, PortStpIntent>,
}

/// Assemble the spanning-tree state from one intent set.
pub fn stp_state(intents: &Intents) -> StpState {
    let mut ports = BTreeMap::new();
    for (name, port) in &intents.ports {
        if let Some(stp) = &port.spanning_tree {
            ports.insert(name.clone(), stp.clone());
        }
    }
    for (group, lag) in &intents.lags {
        if let Some(stp) = &lag.spanning_tree {
            ports.insert(format!("Port-Channel{group}"), stp.clone());
        }
    }
    StpState {
        global: intents.stp.clone(),
        ports,
    }
}

/// Spanning-tree delta: the full wanted state exactly when it changed
/// (orch consumes whole states, not edits).
pub fn diff_stp(running: &Intents, candidate: &Intents) -> Option<StpState> {
    let now = stp_state(running);
    let want = stp_state(candidate);
    (now != want).then_some(want)
}

/// Snooping delta for one family (IGMP or MLD): the full wanted state
/// exactly when it changed.
pub fn diff_snooping(
    running: &SnoopingIntent,
    candidate: &SnoopingIntent,
) -> Option<SnoopingIntent> {
    (running != candidate).then(|| candidate.clone())
}

/// The full wanted LLDP state: the global block plus the ports that
/// carry `lldp disable`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LldpState {
    pub global: LldpIntent,
    /// Physical ports with LLDP turned off, sorted.
    pub disabled_ports: Vec<String>,
}

/// Assemble the LLDP state from one intent set.
pub fn lldp_state(intents: &Intents) -> LldpState {
    LldpState {
        global: intents.lldp.clone(),
        disabled_ports: intents
            .ports
            .iter()
            .filter(|(_, port)| port.lldp_disabled)
            .map(|(name, _)| name.clone())
            .collect(),
    }
}

/// LLDP delta: the full wanted state exactly when it changed (orch
/// consumes whole states, not edits).
pub fn diff_lldp(running: &Intents, candidate: &Intents) -> Option<LldpState> {
    let now = lldp_state(running);
    let want = lldp_state(candidate);
    (now != want).then_some(want)
}

/// The full wanted sFlow state: the global block plus the ports that
/// carry `sflow disable`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SflowState {
    pub global: SflowIntent,
    /// Physical ports with sampling turned off, sorted.
    pub disabled_ports: Vec<String>,
}

/// Assemble the sFlow state from one intent set.
pub fn sflow_state(intents: &Intents) -> SflowState {
    SflowState {
        global: intents.sflow.clone(),
        disabled_ports: intents
            .ports
            .iter()
            .filter(|(_, port)| port.sflow_disabled)
            .map(|(name, _)| name.clone())
            .collect(),
    }
}

/// sFlow delta: the full wanted state exactly when it changed.
pub fn diff_sflow(running: &Intents, candidate: &Intents) -> Option<SflowState> {
    let now = sflow_state(running);
    let want = sflow_state(candidate);
    (now != want).then_some(want)
}

/// The default MAC-table aging time (seconds).
pub const DEFAULT_FDB_AGING_SECS: u32 = 300;

/// MAC-table deltas for syncd.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MacTableChanges {
    /// Aging time to program (reverting to default sends 300).
    pub aging_time: Option<u32>,
    pub add: Vec<(String, u16, FdbTarget)>,
    pub remove: Vec<(String, u16)>,
}

impl MacTableChanges {
    pub fn is_empty(&self) -> bool {
        self.aging_time.is_none() && self.add.is_empty() && self.remove.is_empty()
    }

    pub fn describe(&self) -> Vec<String> {
        let aging = self
            .aging_time
            .map(|secs| format!("mac-table aging-time {secs}"));
        self.add
            .iter()
            .map(|(mac, vlan, target)| match target {
                FdbTarget::Port(port) => {
                    format!("mac-table static {mac} vlan {vlan} interface {port}")
                }
                FdbTarget::Drop => format!("mac-table static {mac} vlan {vlan} drop"),
            })
            .chain(
                self.remove
                    .iter()
                    .map(|(mac, vlan)| format!("mac-table static {mac} vlan {vlan} removed")),
            )
            .chain(aging)
            .collect()
    }
}

/// Diff the MAC-table family, candidate against running.
pub fn diff_mac_table(running: &Intents, candidate: &Intents) -> MacTableChanges {
    let mut changes = MacTableChanges::default();
    let now = running
        .mac_table
        .aging_time
        .unwrap_or(DEFAULT_FDB_AGING_SECS);
    let want = candidate
        .mac_table
        .aging_time
        .unwrap_or(DEFAULT_FDB_AGING_SECS);
    if now != want {
        changes.aging_time = Some(want);
    }
    for (key, target) in &candidate.mac_table.statics {
        if running.mac_table.statics.get(key) != Some(target) {
            changes.add.push((key.0.clone(), key.1, target.clone()));
        }
    }
    for key in running.mac_table.statics.keys() {
        if !candidate.mac_table.statics.contains_key(key) {
            changes.remove.push((key.0.clone(), key.1));
        }
    }
    changes
}

/// One storm-control change for syncd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StormChange {
    /// Interface name (Ethernet or Port-Channel).
    pub name: String,
    pub kind: StormKind,
    /// Percent level with two decimals; None = clear.
    pub level: Option<String>,
}

impl StormChange {
    pub fn describe(&self) -> String {
        match &self.level {
            Some(level) => format!(
                "{}: storm-control {} level {level}",
                self.name,
                self.kind.word()
            ),
            None => format!("{}: storm-control {} removed", self.name, self.kind.word()),
        }
    }
}

/// Every interface's storm-control map (Ethernet ports and LAGs).
fn storm_state(intents: &Intents) -> BTreeMap<String, &BTreeMap<StormKind, String>> {
    let mut out: BTreeMap<String, &BTreeMap<StormKind, String>> = BTreeMap::new();
    for (name, port) in &intents.ports {
        if !port.storm_control.is_empty() {
            out.insert(name.clone(), &port.storm_control);
        }
    }
    for (group, lag) in &intents.lags {
        if !lag.storm_control.is_empty() {
            out.insert(format!("Port-Channel{group}"), &lag.storm_control);
        }
    }
    out
}

/// Diff the storm-control family, candidate against running.
pub fn diff_storm_control(running: &Intents, candidate: &Intents) -> Vec<StormChange> {
    let now = storm_state(running);
    let want = storm_state(candidate);
    let mut changes = Vec::new();
    let empty = BTreeMap::new();
    for (name, levels) in &want {
        let current = now.get(name).copied().unwrap_or(&empty);
        for (kind, level) in *levels {
            if current.get(kind) != Some(level) {
                changes.push(StormChange {
                    name: name.clone(),
                    kind: *kind,
                    level: Some(level.clone()),
                });
            }
        }
        for kind in current.keys() {
            if !levels.contains_key(kind) {
                changes.push(StormChange {
                    name: name.clone(),
                    kind: *kind,
                    level: None,
                });
            }
        }
    }
    for (name, levels) in &now {
        if want.contains_key(name) {
            continue;
        }
        for kind in levels.keys() {
            changes.push(StormChange {
                name: name.clone(),
                kind: *kind,
                level: None,
            });
        }
    }
    changes
}

/// One mirror-session change for syncd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorChange {
    pub session: u8,
    /// The full wanted session; None = remove it.
    pub ensure: Option<MirrorIntent>,
}

impl MirrorChange {
    pub fn describe(&self) -> String {
        match &self.ensure {
            Some(mirror) => {
                let sources = mirror.sources.keys().cloned().collect::<Vec<_>>().join(",");
                format!(
                    "mirror session {}: sources {} -> {}",
                    self.session,
                    if sources.is_empty() {
                        "-".into()
                    } else {
                        sources
                    },
                    mirror.destination.as_deref().unwrap_or("-"),
                )
            }
            None => format!("mirror session {} removed", self.session),
        }
    }
}

/// Diff the mirror family, candidate against running.
pub fn diff_mirror(running: &Intents, candidate: &Intents) -> Vec<MirrorChange> {
    let mut changes = Vec::new();
    for (session, wanted) in &candidate.mirror {
        if running.mirror.get(session) != Some(wanted) {
            changes.push(MirrorChange {
                session: *session,
                ensure: Some(wanted.clone()),
            });
        }
    }
    for session in running.mirror.keys() {
        if !candidate.mirror.contains_key(session) {
            changes.push(MirrorChange {
                session: *session,
                ensure: None,
            });
        }
    }
    changes
}

// --- security-suite diffs --------------------------------------------

pub struct AclChange {
    pub name: String,
    /// The full wanted ACL; None = remove it.
    pub ensure: Option<AclIntent>,
}

impl AclChange {
    pub fn describe(&self) -> String {
        match &self.ensure {
            Some(acl) => format!(
                "security acl {} ({}, {} rule(s))",
                self.name,
                acl.family.word(),
                acl.rules.len()
            ),
            None => format!("security acl {} removed", self.name),
        }
    }
}

/// Diff the ACL definitions, candidate against running.
pub fn diff_acls(running: &Intents, candidate: &Intents) -> Vec<AclChange> {
    let mut changes = Vec::new();
    for (name, wanted) in &candidate.acls {
        if running.acls.get(name) != Some(wanted) {
            changes.push(AclChange {
                name: name.clone(),
                ensure: Some(wanted.clone()),
            });
        }
    }
    for name in running.acls.keys() {
        if !candidate.acls.contains_key(name) {
            changes.push(AclChange {
                name: name.clone(),
                ensure: None,
            });
        }
    }
    changes
}

/// Every ACL binding of a config: (interface, egress?) -> ACL name.
/// Port-Channel bindings ride under their display name (syncd expands
/// them to members).
pub fn acl_bindings(intents: &Intents) -> BTreeMap<(String, bool), String> {
    let mut bindings = BTreeMap::new();
    let mut add = |name: &str, groups: &AccessGroups| {
        if let Some(acl) = &groups.ingress {
            bindings.insert((name.to_string(), false), acl.clone());
        }
        if let Some(acl) = &groups.egress {
            bindings.insert((name.to_string(), true), acl.clone());
        }
    };
    for (name, port) in &intents.ports {
        add(name, &port.access_groups);
    }
    for (group, lag) in &intents.lags {
        add(&format!("Port-Channel{group}"), &lag.access_groups);
    }
    bindings
}

pub struct AclBindingChange {
    pub target: String,
    pub egress: bool,
    /// The ACL to bind; None = unbind.
    pub acl: Option<String>,
}

impl AclBindingChange {
    fn direction(&self) -> &'static str {
        if self.egress {
            "out"
        } else {
            "in"
        }
    }

    pub fn describe(&self) -> String {
        match &self.acl {
            Some(acl) => format!(
                "access-group {acl} applied to {} ({})",
                self.target,
                self.direction()
            ),
            None => format!(
                "access-group removed from {} ({})",
                self.target,
                self.direction()
            ),
        }
    }
}

/// Diff the ACL bindings, candidate against running.
pub fn diff_acl_bindings(running: &Intents, candidate: &Intents) -> Vec<AclBindingChange> {
    let running = acl_bindings(running);
    let wanted = acl_bindings(candidate);
    let mut changes = Vec::new();
    for ((target, egress), acl) in &wanted {
        if running.get(&(target.clone(), *egress)) != Some(acl) {
            changes.push(AclBindingChange {
                target: target.clone(),
                egress: *egress,
                acl: Some(acl.clone()),
            });
        }
    }
    for (target, egress) in running.keys() {
        if !wanted.contains_key(&(target.clone(), *egress)) {
            changes.push(AclBindingChange {
                target: target.clone(),
                egress: *egress,
                acl: None,
            });
        }
    }
    changes
}

pub struct CoppChange {
    pub class: String,
    /// The wanted override; None = back to compiled defaults.
    pub set: Option<CoppClassIntent>,
}

impl CoppChange {
    pub fn describe(&self) -> String {
        match &self.set {
            Some(intent) => format!(
                "copp class {}: rate {} burst {}",
                self.class,
                intent
                    .rate
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "default".into()),
                intent
                    .burst
                    .map(|b| b.to_string())
                    .unwrap_or_else(|| "default".into()),
            ),
            None => format!("copp class {} restored to defaults", self.class),
        }
    }
}

/// Diff the CoPP class overrides, candidate against running.
pub fn diff_copp(running: &Intents, candidate: &Intents) -> Vec<CoppChange> {
    let mut changes = Vec::new();
    for (class, wanted) in &candidate.copp {
        if running.copp.get(class) != Some(wanted) {
            changes.push(CoppChange {
                class: class.clone(),
                set: Some(wanted.clone()),
            });
        }
    }
    for class in running.copp.keys() {
        if !candidate.copp.contains_key(class) {
            changes.push(CoppChange {
                class: class.clone(),
                set: None,
            });
        }
    }
    changes
}

/// The per-port port-security programs of a config.
pub fn port_security_state(intents: &Intents) -> BTreeMap<String, PortSecurityIntent> {
    intents
        .ports
        .iter()
        .filter_map(|(name, port)| port.port_security.map(|ps| (name.clone(), ps)))
        .collect()
}

pub struct PortSecurityChange {
    pub port: String,
    /// The wanted program; None = unconfigure.
    pub set: Option<PortSecurityIntent>,
}

impl PortSecurityChange {
    pub fn describe(&self) -> String {
        match &self.set {
            Some(ps) => format!(
                "port-security on {}: maximum {} violation {}",
                self.port,
                ps.maximum,
                if ps.shutdown { "shutdown" } else { "protect" }
            ),
            None => format!("port-security removed from {}", self.port),
        }
    }
}

/// Diff the port-security programs, candidate against running.
pub fn diff_port_security(running: &Intents, candidate: &Intents) -> Vec<PortSecurityChange> {
    let running = port_security_state(running);
    let wanted = port_security_state(candidate);
    let mut changes = Vec::new();
    for (port, ps) in &wanted {
        if running.get(port) != Some(ps) {
            changes.push(PortSecurityChange {
                port: port.clone(),
                set: Some(*ps),
            });
        }
    }
    for port in running.keys() {
        if !wanted.contains_key(port) {
            changes.push(PortSecurityChange {
                port: port.clone(),
                set: None,
            });
        }
    }
    changes
}

/// The whole dot1x state orch consumes: the global intent plus the
/// enabled-port set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dot1xState {
    pub intent: Dot1xIntent,
    pub ports: BTreeSet<String>,
}

pub fn dot1x_state(intents: &Intents) -> Dot1xState {
    Dot1xState {
        intent: intents.dot1x.clone(),
        ports: intents
            .ports
            .iter()
            .filter(|(_, p)| p.dot1x)
            .map(|(name, _)| name.clone())
            .collect(),
    }
}

/// The whole dot1x state when it changed, else None.
pub fn diff_dot1x(running: &Intents, candidate: &Intents) -> Option<Dot1xState> {
    let wanted = dot1x_state(candidate);
    (dot1x_state(running) != wanted).then_some(wanted)
}

/// The whole snooping/DAI state orch consumes: the global intent plus
/// the trust sets.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnoopSecState {
    pub intent: SnoopSecIntent,
    pub dhcp_trusted: BTreeSet<String>,
    pub arp_trusted: BTreeSet<String>,
    /// The relay: per-VLAN (servers, giaddr). One engine, one push.
    pub relay: BTreeMap<u16, (Vec<std::net::Ipv4Addr>, String)>,
}

pub fn snoopsec_state(intents: &Intents) -> SnoopSecState {
    let mut state = SnoopSecState {
        intent: intents.snoop_sec.clone(),
        ..SnoopSecState::default()
    };
    for (name, port) in &intents.ports {
        if port.dhcp_snooping_trust {
            state.dhcp_trusted.insert(name.clone());
        }
        if port.arp_inspection_trust {
            state.arp_trusted.insert(name.clone());
        }
    }
    for (group, lag) in &intents.lags {
        let name = format!("Port-Channel{group}");
        if lag.dhcp_snooping_trust {
            state.dhcp_trusted.insert(name.clone());
        }
        if lag.arp_inspection_trust {
            state.arp_trusted.insert(name);
        }
    }
    for (vlan, servers) in &intents.dhcp_relay {
        // Validation guarantees the address; the strip keeps the
        // giaddr a bare host address, which is what a server sees.
        let giaddr = intents
            .svis
            .get(&format!("Vlan{vlan}"))
            .and_then(|svi| svi.address.as_deref())
            .and_then(|cidr| cidr.split('/').next())
            .unwrap_or_default()
            .to_string();
        state.relay.insert(*vlan, (servers.clone(), giaddr));
    }
    state
}

/// The whole snooping/DAI state when it changed, else None.
pub fn diff_snoopsec(running: &Intents, candidate: &Intents) -> Option<SnoopSecState> {
    let wanted = snoopsec_state(candidate);
    (snoopsec_state(running) != wanted).then_some(wanted)
}

/// The whole global-map state when it changed, else None. The push is
/// declarative (all four tables at once), so one Option carries it.
pub fn diff_qos_maps(running: &Intents, candidate: &Intents) -> Option<QosMapsIntent> {
    (running.qos_maps != candidate.qos_maps).then(|| candidate.qos_maps.clone())
}

pub struct WredProfileChange {
    pub name: String,
    /// The wanted profile; None = remove.
    pub ensure: Option<WredProfileIntent>,
}

impl WredProfileChange {
    pub fn describe(&self) -> String {
        match &self.ensure {
            Some(profile) => format!(
                "qos wred-profile {}: min {} max {} drop {}%{}",
                self.name,
                profile
                    .min_threshold
                    .map(|kb| format!("{kb}KB"))
                    .unwrap_or_else(|| "unset".into()),
                profile
                    .max_threshold
                    .map(|kb| format!("{kb}KB"))
                    .unwrap_or_else(|| "unset".into()),
                profile.drop_probability,
                if profile.ecn { " ecn" } else { "" },
            ),
            None => format!("qos wred-profile {} removed", self.name),
        }
    }
}

/// Diff the WRED profiles, candidate against running.
pub fn diff_wred_profiles(running: &Intents, candidate: &Intents) -> Vec<WredProfileChange> {
    let mut changes = Vec::new();
    for (name, profile) in &candidate.wred_profiles {
        if running.wred_profiles.get(name) != Some(profile) {
            changes.push(WredProfileChange {
                name: name.clone(),
                ensure: Some(profile.clone()),
            });
        }
    }
    for name in running.wred_profiles.keys() {
        if !candidate.wred_profiles.contains_key(name) {
            changes.push(WredProfileChange {
                name: name.clone(),
                ensure: None,
            });
        }
    }
    changes
}

/// The per-port QoS programs of a config, keyed by the display name the
/// operator configured (a port or a Port-Channel).
pub fn port_qos_state(intents: &Intents) -> BTreeMap<String, PortQosIntent> {
    let mut state: BTreeMap<String, PortQosIntent> = intents
        .ports
        .iter()
        .filter_map(|(name, port)| port.qos.clone().map(|qos| (name.clone(), qos)))
        .collect();
    for (group, lag) in &intents.lags {
        if let Some(qos) = &lag.qos {
            state.insert(format!("Port-Channel{group}"), qos.clone());
        }
    }
    state
}

pub struct PortQosChange {
    pub port: String,
    /// The wanted program; None = back to the platform defaults.
    pub set: Option<PortQosIntent>,
}

impl PortQosChange {
    pub fn describe(&self) -> String {
        match &self.set {
            Some(qos) => {
                let mut text = format!(
                    "qos on {}: trust {} default-tc {}",
                    self.port,
                    qos.trust.word(),
                    qos.default_tc
                );
                if let Some(rate) = qos.shape {
                    text.push_str(&format!(
                        " shaper {}",
                        hemlock_common::net::format_shape_rate(rate)
                    ));
                }
                if !qos.queues.is_empty() {
                    text.push_str(&format!(" ({} queue(s) configured)", qos.queues.len()));
                }
                text
            }
            None => format!("qos removed from {}", self.port),
        }
    }
}

/// Diff the per-port QoS programs, candidate against running.
pub fn diff_port_qos(running: &Intents, candidate: &Intents) -> Vec<PortQosChange> {
    let running = port_qos_state(running);
    let wanted = port_qos_state(candidate);
    let mut changes = Vec::new();
    for (port, qos) in &wanted {
        if running.get(port) != Some(qos) {
            changes.push(PortQosChange {
                port: port.clone(),
                set: Some(qos.clone()),
            });
        }
    }
    for port in running.keys() {
        if !wanted.contains_key(port) {
            changes.push(PortQosChange {
                port: port.clone(),
                set: None,
            });
        }
    }
    changes
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use hemlock_common::net::parse_mac;
    use hemlock_config::parse;

    fn intents_of(text: &str) -> Intents {
        extract(&parse(text).unwrap()).unwrap()
    }

    #[test]
    fn extracts_interface_intents() {
        let intents = intents_of(
            r#"
interfaces {
    ethernet Ethernet0 {
        description "uplink";
        admin-state disabled;
    }
    ethernet Ethernet1 {
        admin-state enabled;
    }
}
"#,
        );
        assert_eq!(intents.ports.len(), 2);
        assert_eq!(
            intents.ports["Ethernet0"],
            InterfaceIntent {
                admin_up: Some(false),
                description: Some("uplink".into()),
                ..InterfaceIntent::default()
            }
        );
        assert_eq!(intents.ports["Ethernet1"].admin_up, Some(true));
    }

    #[test]
    fn extracts_name_as_block_form_and_management() {
        let intents = intents_of(
            "interfaces {\n    Ethernet1 {\n        admin-state disabled\n        description uplink\n    }\n    Management1 {\n        admin-state enabled\n        address 10.42.10.9/24\n    }\n}\n",
        );
        assert_eq!(intents.ports.len(), 1);
        assert_eq!(
            intents.ports["Ethernet1"],
            InterfaceIntent {
                admin_up: Some(false),
                description: Some("uplink".into()),
                ..InterfaceIntent::default()
            }
        );
        assert_eq!(
            intents.management["Management1"],
            MgmtIntent {
                admin_up: Some(true),
                address: Some("10.42.10.9/24".into()),
                mtu: None,
            }
        );
    }

    #[test]
    fn legacy_and_current_forms_are_equivalent() {
        assert_eq!(
            intents_of("interfaces { ethernet Ethernet3 { admin-state disabled; } }"),
            intents_of("interfaces { Ethernet3 { admin-state disabled } }"),
        );
    }

    #[test]
    fn rejects_bad_admin_state() {
        let tree = parse("interfaces { ethernet Ethernet0 { admin-state banana; } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadAdminState { .. })
        ));
    }

    #[test]
    fn validates_addresses_on_any_interface() {
        let tree = parse("interfaces { Management1 { address banana } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadAddress { .. })
        ));
        let tree = parse("interfaces { Ethernet1 { address banana/24 } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadAddress { .. })
        ));
        // Front-panel ports take addresses (L3 mode).
        let intents = intents_of("interfaces { Ethernet1 { address 10.0.0.1/24 } }");
        assert_eq!(
            intents.ports["Ethernet1"].address.as_deref(),
            Some("10.0.0.1/24")
        );
    }

    #[test]
    fn extracts_svi_intents() {
        // Vlan1 always exists; other SVIs need their VLAN declared.
        let intents = intents_of("interfaces { Vlan1 { address 10.42.10.9/24 } }");
        assert_eq!(
            intents.svis["Vlan1"].address.as_deref(),
            Some("10.42.10.9/24")
        );
        let intents =
            intents_of("vlans { vlan 10 { } }\ninterfaces { Vlan10 { address 10.0.10.1/24 } }");
        assert_eq!(
            intents.svis["Vlan10"].address.as_deref(),
            Some("10.0.10.1/24")
        );
        // Empty Vlan blocks (as `show configuration` renders) are fine.
        let intents = intents_of("interfaces { Vlan1 { } }");
        assert_eq!(intents.svis["Vlan1"], SviIntent::default());

        // Undeclared VLAN behind an addressed SVI is an error.
        let tree = parse("interfaces { Vlan20 { address 10.0.20.1/24 } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadInterfaceBlock(_))
        ));
        // SVIs are not switchports.
        let tree = parse("interfaces { Vlan1 { switchport { } } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadSwitchport { .. })
        ));
        // Bad interface id.
        let tree = parse("interfaces { Vlan5000 { } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadInterfaceBlock(_))
        ));
    }

    #[test]
    fn svi_addresses_diff_like_ports() {
        let running = intents_of("interfaces { Vlan1 { address 10.42.10.9/24 } }");
        let candidate = intents_of("interfaces { Vlan1 { address 10.42.10.10/24 } }");
        let changes = diff_os(&running, &candidate);
        assert_eq!(
            changes.svis,
            vec![NetdevChange {
                name: "Vlan1".into(),
                admin_up: None,
                set_address: Some("10.42.10.10/24".into()),
                del_address: Some("10.42.10.9/24".into()),
                ..Default::default()
            }]
        );
        // Removal clears the address.
        let changes = diff_os(&running, &intents_of(""));
        assert_eq!(
            changes.svis,
            vec![NetdevChange {
                name: "Vlan1".into(),
                admin_up: None,
                set_address: None,
                del_address: Some("10.42.10.9/24".into()),
                ..Default::default()
            }]
        );
        // No change, no delta.
        assert!(diff_os(&running, &running).svis.is_empty());
    }

    #[test]
    fn parses_phrase_keywords() {
        // `no shutdown` and the switchport phrases, as the CLI now
        // writes them.
        let intents = intents_of(
            "interfaces {\n Ethernet1 {\n no shutdown\n switchport {\n mode trunk\n trunk vlans 10 20\n native vlan 5\n }\n }\n \
             Ethernet2 {\n shutdown\n switchport {\n mode access\n access vlan 30\n }\n }\n}\n\
             vlans {\n vlan 5 { }\n vlan 10 { }\n vlan 20 { }\n vlan 30 { }\n}",
        );
        let e1 = &intents.ports["Ethernet1"];
        assert_eq!(e1.admin_up, Some(true));
        let sp1 = e1.switchport.as_ref().unwrap();
        assert_eq!(sp1.mode, SwitchportMode::Trunk);
        assert_eq!(sp1.trunk_vlans, vec![10, 20]);
        assert_eq!(sp1.native_vlan, Some(5));
        let e2 = &intents.ports["Ethernet2"];
        assert_eq!(e2.admin_up, Some(false));
        assert_eq!(e2.switchport.as_ref().unwrap().access_vlan, Some(30));
    }

    #[test]
    fn extracts_ssh_intent() {
        assert_eq!(intents_of("").ssh, SshIntent::default());
        assert_eq!(
            intents_of("system { ssh { } }").ssh,
            SshIntent {
                enabled: true,
                auth_local: false
            }
        );
        assert_eq!(
            intents_of("system { ssh { authentication local } }").ssh,
            SshIntent {
                enabled: true,
                auth_local: true
            }
        );
        let tree = parse("system { ssh { authentication radius } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadSsh(_))));
    }

    #[test]
    fn extracts_web_intent() {
        assert_eq!(intents_of("").web, WebIntent::default());
        assert_eq!(
            intents_of("system { http { } }").web,
            WebIntent {
                http: true,
                https: false
            }
        );
        assert_eq!(
            intents_of("system { http { } https { } }").web,
            WebIntent {
                http: true,
                https: true
            }
        );
        assert!(intents_of("system { https { } }").web.enabled());
        assert!(!intents_of("system { ssh { } }").web.enabled());
    }

    #[test]
    fn diff_os_reports_web_deltas() {
        let running = intents_of("");
        let candidate = intents_of("system { http { } https { } }");
        let changes = diff_os(&running, &candidate);
        assert_eq!(
            changes.web,
            Some(WebIntent {
                http: true,
                https: true
            })
        );
        assert_eq!(
            changes.describe(),
            vec!["web ui enabled (http, https)".to_string()]
        );

        // Unchanged -> no delta; reverting -> disabled.
        assert!(diff_os(&candidate, &candidate).is_empty());
        let back = diff_os(&candidate, &running);
        assert_eq!(back.web, Some(WebIntent::default()));
        assert_eq!(back.describe(), vec!["web ui disabled".to_string()]);
    }

    /// Shorthand for the expected side of route assertions.
    fn static_route(next_hops: &[&str], drop: bool, distance: u8) -> StaticRoute {
        StaticRoute {
            next_hops: next_hops.iter().map(|h| h.to_string()).collect(),
            drop,
            distance,
        }
    }

    #[test]
    fn extracts_static_routes() {
        let intents = intents_of(
            "routing {\n    static {\n        0.0.0.0/0 10.42.10.1\n        10.99.0.0/16 10.9.9.0\n        10.99.0.0/16 10.42.10.7\n        192.0.2.0/24 drop\n        172.16.0.0/12 10.42.10.1 distance 250\n        2001:db8:99::/48 2001:db8:9::1\n    }\n}\n",
        );
        assert_eq!(intents.routes.len(), 5);
        assert_eq!(
            intents.routes["0.0.0.0/0"],
            static_route(&["10.42.10.1"], false, 1)
        );
        // Repeated prefix lines merge into one ECMP set.
        assert_eq!(
            intents.routes["10.99.0.0/16"],
            static_route(&["10.42.10.7", "10.9.9.0"], false, 1)
        );
        assert_eq!(intents.routes["192.0.2.0/24"], static_route(&[], true, 1));
        assert_eq!(
            intents.routes["172.16.0.0/12"],
            static_route(&["10.42.10.1"], false, 250)
        );
        assert_eq!(
            intents.routes["2001:db8:99::/48"],
            static_route(&["2001:db8:9::1"], false, 1)
        );

        // A repeated identical next hop is idempotent, and a line
        // without an explicit distance inherits the prefix's.
        let intents = intents_of(
            "routing { static { 10.0.0.0/8 10.1.1.1 distance 5\n10.0.0.0/8 10.1.1.1\n10.0.0.0/8 10.1.1.2 } }",
        );
        assert_eq!(
            intents.routes["10.0.0.0/8"],
            static_route(&["10.1.1.1", "10.1.1.2"], false, 5)
        );
    }

    #[test]
    fn rejects_bad_routes() {
        // Host bits set in the prefix name the canonical form.
        let tree = parse("routing { static { 10.42.10.9/24 10.42.10.1 } }").unwrap();
        let err = extract(&tree).unwrap_err();
        assert_eq!(
            err.to_string(),
            "route 10.42.10.9/24: host bits set; did you mean 10.42.10.0/24?"
        );
        // Next hop family mismatch.
        let tree = parse("routing { static { 0.0.0.0/0 2001:db8::1 } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadRoute { .. })));
        // A next hop must be a plain address, not a prefix.
        let tree = parse("routing { static { 0.0.0.0/0 10.42.10.1/24 } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadRoute { .. })));
        // drop and next hops are mutually exclusive, in either order.
        let tree = parse("routing { static { 10.0.0.0/8 10.1.1.1\n10.0.0.0/8 drop } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadRoute { .. })));
        let tree = parse("routing { static { 10.0.0.0/8 drop\n10.0.0.0/8 10.1.1.1 } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadRoute { .. })));
        // Distance is per-prefix: explicit values must agree.
        let tree = parse(
            "routing { static { 10.0.0.0/8 10.1.1.1 distance 5\n10.0.0.0/8 10.1.1.2 distance 9 } }",
        )
        .unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadRoute { .. })));
        // Distance range.
        let tree = parse("routing { static { 10.0.0.0/8 10.1.1.1 distance 0 } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadRoute { .. })));
        // Trailing junk after the next hop.
        let tree = parse("routing { static { 10.0.0.0/8 10.1.1.1 metric 5 } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadRoute { .. })));
        // Unknown routing sub-block.
        let tree = parse("routing { rip { } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadRouting(_))));
    }

    #[test]
    fn diff_only_reports_changes() {
        let running = intents_of(
            "interfaces { ethernet Ethernet0 { admin-state disabled; description \"a\"; } }",
        );
        let unchanged = diff(&running.ports, &running.ports);
        assert!(unchanged.is_empty());

        let candidate = intents_of(
            "interfaces { ethernet Ethernet0 { admin-state enabled; description \"a\"; } }",
        );
        let changes = diff(&running.ports, &candidate.ports);
        assert_eq!(
            changes,
            vec![PortChange {
                name: "Ethernet0".into(),
                admin_up: Some(true),
                description: None,
                ..Default::default()
            }]
        );
    }

    #[test]
    fn removed_interface_reverts_to_defaults() {
        let running = intents_of(
            "interfaces { ethernet Ethernet5 { admin-state disabled; description \"x\"; } }",
        );
        let candidate = intents_of("");
        let changes = diff(&running.ports, &candidate.ports);
        assert_eq!(
            changes,
            vec![PortChange {
                name: "Ethernet5".into(),
                admin_up: Some(true),
                description: Some(String::new()),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn diff_os_reports_address_route_and_ssh_deltas() {
        let running = intents_of("");
        let candidate = intents_of(
            "system { ssh { authentication local } }\ninterfaces { Management1 { address 10.42.10.9/24 } }\nrouting { static { 0.0.0.0/0 10.42.10.1 } }\n",
        );
        let changes = diff_os(&running, &candidate);
        assert_eq!(
            changes.management,
            vec![NetdevChange {
                name: "Management1".into(),
                admin_up: None,
                set_address: Some("10.42.10.9/24".into()),
                del_address: None,
                ..Default::default()
            }]
        );
        assert_eq!(
            changes.routes,
            vec![RouteChange {
                prefix: "0.0.0.0/0".into(),
                old: None,
                new: Some(static_route(&["10.42.10.1"], false, 1)),
            }]
        );
        assert_eq!(
            changes.ssh,
            Some(SshIntent {
                enabled: true,
                auth_local: true
            })
        );

        // Unchanged -> empty; reverting -> deletions + ssh disabled.
        assert!(diff_os(&candidate, &candidate).is_empty());
        let back = diff_os(&candidate, &running);
        assert_eq!(
            back.management,
            vec![NetdevChange {
                name: "Management1".into(),
                admin_up: None,
                set_address: None,
                del_address: Some("10.42.10.9/24".into()),
                ..Default::default()
            }]
        );
        assert_eq!(
            back.routes,
            vec![RouteChange {
                prefix: "0.0.0.0/0".into(),
                old: Some(static_route(&["10.42.10.1"], false, 1)),
                new: None,
            }]
        );
        assert_eq!(back.ssh, Some(SshIntent::default()));
    }

    #[test]
    fn extracts_and_validates_arp_statics() {
        let intents = intents_of(
            "vlans { vlan 99 { } } interfaces { Vlan99 { address 10.42.10.9/24 } }\nrouting { arp { 10.42.10.200 interface Vlan99 mac 00-50-56-BE-EF-99 } }",
        );
        assert_eq!(
            intents.arp_statics["10.42.10.200"],
            ArpStatic {
                interface: "Vlan99".into(),
                mac: "00:50:56:be:ef:99".into(),
            }
        );

        // The interface must exist and be L3 in the same config.
        let tree = parse("routing { arp { 10.42.10.200 interface Vlan99 mac 00:50:56:be:ef:99 } }")
            .unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadArp { .. })));
        let tree = parse(
            "interfaces { Ethernet1 { } }\nrouting { arp { 10.0.0.1 interface Ethernet1 mac 00:50:56:be:ef:99 } }",
        )
        .unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadArp { .. })));
        // Multicast MACs and bad addresses are rejected.
        let tree = parse(
            "vlans { vlan 99 { } } interfaces { Vlan99 { address 10.42.10.9/24 } }\nrouting { arp { 10.0.0.1 interface Vlan99 mac 01:00:5e:00:00:01 } }",
        )
        .unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadArp { .. })));
        let tree = parse(
            "vlans { vlan 99 { } } interfaces { Vlan99 { address 10.42.10.9/24 } }\nrouting { arp { banana interface Vlan99 mac 00:50:56:be:ef:99 } }",
        )
        .unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadArp { .. })));
        // Deferred families reject by name.
        let tree = parse("routing { vrf { } }").unwrap();
        let err = extract(&tree).unwrap_err();
        assert_eq!(err.to_string(), "routing: vrf is not supported");
    }

    #[test]
    fn extracts_and_validates_frr_families() {
        let intents = intents_of(
            "vlans { vlan 99 { } vlan 100 { } }\ninterfaces { Vlan99 { address 10.42.10.9/24 } Vlan100 { address 10.0.100.2/24\nvrrp 10 { address 10.0.100.1\npriority 200 } } }\nrouting { router-id 10.42.0.1\nospf { area 0 { network 10.42.10.0/24 }\npassive-interface Vlan100\nredistribute static }\nbgp { as 65000\nneighbor 10.42.10.1 { remote-as 65001\ndescription \"upstream\" }\nnetwork 10.42.0.0/16 } }",
        );
        assert_eq!(intents.router_id.as_deref(), Some("10.42.0.1"));
        let ospf = intents.ospf.unwrap();
        // Integer area ids canonicalize to dotted form.
        assert!(ospf.areas.contains_key("0.0.0.0"));
        assert_eq!(ospf.maximum_paths, 4);
        let bgp = intents.bgp.unwrap();
        assert_eq!(bgp.as_number, 65000);
        assert_eq!(
            bgp.neighbors["10.42.10.1"].description.as_deref(),
            Some("upstream")
        );
        let vrrp = &intents.vrrp[&("Vlan100".to_string(), 10)];
        assert_eq!(vrrp.priority, 200);
        assert!(vrrp.preempt);
        assert_eq!(vrrp.addresses.len(), 1);

        // BGP: `as` is required as soon as the block exists.
        let tree = parse("routing { bgp { network 10.0.0.0/8 } }").unwrap();
        let err = extract(&tree).unwrap_err();
        assert_eq!(err.to_string(), "routing bgp: as is required");
        // A neighbor needs remote-as by commit.
        let tree = parse("routing { bgp { as 65000\nneighbor 10.0.0.1 { } } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadBgp(_))));
        // The IPv6 address family is deferred.
        let tree = parse("routing { bgp { as 65000\nnetwork 2001:db8::/32 } }").unwrap();
        let err = extract(&tree).unwrap_err();
        assert!(err.to_string().contains("not supported"));
        // OSPF interface knobs must name an L3 interface.
        let tree = parse("routing { ospf { interface Vlan99 { cost 10 } } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadOspf(_))));
        // OSPF networks must be canonical.
        let tree = parse("routing { ospf { area 0 { network 10.42.10.9/24 } } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadOspf(_))));

        // VRRP: the VIP must sit inside the parent's subnet, the parent
        // must carry an address, and Management is rejected.
        let tree = parse(
            "vlans { vlan 100 { } }\ninterfaces { Vlan100 { address 10.0.100.2/24\nvrrp 10 { address 192.168.9.1 } } }",
        )
        .unwrap();
        let err = extract(&tree).unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");
        let tree = parse(
            "vlans { vlan 100 { } }\ninterfaces { Vlan100 { vrrp 10 { address 10.0.100.1 } } }",
        )
        .unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadVrrp { .. })));
        let tree = parse(
            "vlans { vlan 100 { } }\ninterfaces { Vlan100 { address 10.0.100.2/24\nvrrp 10 { } } }",
        )
        .unwrap();
        let err = extract(&tree).unwrap_err();
        assert!(err.to_string().contains("at least one address"), "{err}");
        let tree = parse(
            "interfaces { Management1 { address 192.168.0.2/24\nvrrp 10 { address 192.168.0.1 } } }",
        )
        .unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadVrrp { .. })));

        // The macvlan and virtual MAC derivations.
        assert_eq!(vrrp_macvlan_name("Vlan100", 10), "vrrp4-v100-10");
        assert_eq!(vrrp_macvlan_name("Ethernet48", 255), "vrrp4-e48-255");
        assert_eq!(vrrp_virtual_mac(10), "00:00:5e:00:01:0a");
    }

    #[test]
    fn routing_seed_round_trips_and_extracts() {
        // The spec's Part 1.1 seed: parse -> serialize -> parse is the
        // identity, and every family extracts.
        let text = "vlans {\n    vlan 99 {\n    }\n    vlan 100 {\n    }\n}\ninterfaces {\n    Vlan99 {\n        address 10.42.10.9/24\n    }\n    Vlan100 {\n        address 10.0.100.2/24\n        vrrp 10 {\n            address 10.0.100.1\n            priority 200\n            advertisement-interval 1\n        }\n    }\n    Ethernet48 {\n        address 10.9.9.1/31\n    }\n}\nrouting {\n    router-id 10.42.0.1\n    static {\n        0.0.0.0/0 10.42.10.1\n        10.99.0.0/16 10.9.9.0\n        10.99.0.0/16 10.42.10.7\n        192.0.2.0/24 drop\n        172.16.0.0/12 10.42.10.1 distance 250\n        2001:db8:99::/48 2001:db8:9::1\n    }\n    arp {\n        10.42.10.200 interface Vlan99 mac 00:50:56:be:ef:99\n    }\n    ospf {\n        area 0.0.0.0 {\n            network 10.42.10.0/24\n        }\n        passive-interface Vlan100\n        redistribute static\n        maximum-paths 4\n    }\n    bgp {\n        as 65000\n        neighbor 10.42.10.1 {\n            remote-as 65001\n            description upstream\n        }\n        network 10.42.0.0/16\n        redistribute connected\n        maximum-paths 4\n    }\n}\n";
        let tree = parse(text).unwrap();
        assert_eq!(parse(&tree.to_text()).unwrap(), tree);
        let intents = extract(&tree).unwrap();
        assert_eq!(intents.routes.len(), 5);
        assert_eq!(intents.arp_statics.len(), 1);
        assert!(intents.ospf.is_some());
        assert!(intents.bgp.is_some());
        assert_eq!(intents.vrrp.len(), 1);
        assert_eq!(intents.router_id.as_deref(), Some("10.42.0.1"));
    }

    #[test]
    fn diff_os_tracks_vrrp_macvlans() {
        let base = "vlans { vlan 100 { } }\ninterfaces { Vlan100 { address 10.0.100.2/24";
        let running = intents_of(&format!("{base} }} }}"));
        let candidate = intents_of(&format!("{base}\nvrrp 10 {{ address 10.0.100.1 }} }} }}"));
        let changes = diff_os(&running, &candidate);
        assert_eq!(
            changes.vrrp_macvlans,
            vec![VrrpMacvlanChange {
                interface: "Vlan100".into(),
                group: 10,
                create: true,
            }]
        );
        assert_eq!(
            changes.describe(),
            vec!["vrrp Vlan100 group 10 macvlan vrrp4-v100-10 created".to_string()]
        );
        let back = diff_os(&candidate, &running);
        assert!(!back.vrrp_macvlans[0].create);
        assert!(diff_os(&candidate, &candidate).is_empty());
    }

    #[test]
    fn diff_os_tracks_arp_statics() {
        let running = intents_of(
            "vlans { vlan 99 { } }
interfaces { Vlan99 { address 10.42.10.9/24 } }",
        );
        let candidate = intents_of(
            "vlans { vlan 99 { } } interfaces { Vlan99 { address 10.42.10.9/24 } }\nrouting { arp { 10.42.10.200 interface Vlan99 mac 00:50:56:be:ef:99 } }",
        );
        let changes = diff_os(&running, &candidate);
        assert_eq!(
            changes.arp,
            vec![ArpChange {
                ip: "10.42.10.200".into(),
                old: None,
                new: Some(ArpStatic {
                    interface: "Vlan99".into(),
                    mac: "00:50:56:be:ef:99".into(),
                }),
            }]
        );
        assert_eq!(
            changes.describe(),
            vec!["arp 10.42.10.200 is 00:50:56:be:ef:99 on Vlan99".to_string()]
        );
        let back = diff_os(&candidate, &running);
        assert_eq!(back.arp.len(), 1);
        assert!(back.arp[0].new.is_none());
        assert!(diff_os(&candidate, &candidate).is_empty());
    }

    #[test]
    fn diff_os_routes_track_ecmp_and_distance() {
        let running = intents_of("routing { static { 10.99.0.0/16 10.9.9.0 } }");
        let candidate = intents_of(
            "routing { static { 10.99.0.0/16 10.9.9.0\n10.99.0.0/16 10.42.10.7\n192.0.2.0/24 drop } }",
        );
        let changes = diff_os(&running, &candidate);
        assert_eq!(
            changes.routes,
            vec![
                RouteChange {
                    prefix: "10.99.0.0/16".into(),
                    old: Some(static_route(&["10.9.9.0"], false, 1)),
                    new: Some(static_route(&["10.42.10.7", "10.9.9.0"], false, 1)),
                },
                RouteChange {
                    prefix: "192.0.2.0/24".into(),
                    old: None,
                    new: Some(static_route(&[], true, 1)),
                },
            ]
        );
        assert_eq!(
            changes.describe(),
            vec![
                "route 10.99.0.0/16 via 10.42.10.7, 10.9.9.0".to_string(),
                "route 192.0.2.0/24 drop".to_string(),
            ]
        );

        // A distance change carries both sides so the applier can
        // delete the old-metric kernel route.
        let slower = intents_of("routing { static { 10.99.0.0/16 10.9.9.0 distance 250 } }");
        let changes = diff_os(&running, &slower);
        assert_eq!(
            changes.routes,
            vec![RouteChange {
                prefix: "10.99.0.0/16".into(),
                old: Some(static_route(&["10.9.9.0"], false, 1)),
                new: Some(static_route(&["10.9.9.0"], false, 250)),
            }]
        );
        assert_eq!(
            changes.describe(),
            vec!["route 10.99.0.0/16 via 10.9.9.0 (distance 250)".to_string()]
        );
        assert!(diff_os(&candidate, &candidate).is_empty());
    }

    #[test]
    fn diff_os_replaces_a_changed_address() {
        let running = intents_of("interfaces { Management1 { address 10.0.0.5/24 } }");
        let candidate = intents_of("interfaces { Management1 { address 10.42.10.9/24 } }");
        let changes = diff_os(&running, &candidate);
        assert_eq!(
            changes.management,
            vec![NetdevChange {
                name: "Management1".into(),
                admin_up: None,
                set_address: Some("10.42.10.9/24".into()),
                del_address: Some("10.0.0.5/24".into()),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn extracts_shutdown_markers() {
        let intents = intents_of(
            "interfaces { Ethernet1 { shutdown } Ethernet2 { no-shutdown } Ethernet3 { } }",
        );
        assert_eq!(intents.ports["Ethernet1"].admin_up, Some(false));
        assert_eq!(intents.ports["Ethernet2"].admin_up, Some(true));
        assert_eq!(intents.ports["Ethernet3"].admin_up, None);
        let tree = parse("interfaces { Ethernet1 { shutdown\nno-shutdown } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadInterfaceBlock(_))
        ));
    }

    #[test]
    fn extracts_vlans_and_switchports() {
        let intents = intents_of(
            r#"
vlans {
    vlan 10 {
        description "Management"
    }
    vlan 20 { }
}
interfaces {
    Ethernet1 {
        switchport {
            mode trunk
            trunk-vlans 10 20 30
            native-vlan 40
        }
    }
    Ethernet2 {
        switchport {
            mode access
            access-vlan 10
        }
    }
}
"#,
        );
        assert_eq!(intents.vlans.len(), 2);
        assert_eq!(
            intents.vlans[&10].description.as_deref(),
            Some("Management")
        );
        assert_eq!(intents.vlans[&20].description, None);
        assert_eq!(
            intents.ports["Ethernet1"].switchport,
            Some(SwitchportIntent {
                mode: SwitchportMode::Trunk,
                access_vlan: None,
                trunk_vlans: vec![10, 20, 30],
                native_vlan: Some(40),
            })
        );
        assert_eq!(
            intents.ports["Ethernet2"].switchport,
            Some(SwitchportIntent {
                mode: SwitchportMode::Access,
                access_vlan: Some(10),
                trunk_vlans: vec![],
                native_vlan: None,
            })
        );
    }

    #[test]
    fn rejects_bad_vlans_and_switchports() {
        let tree = parse("vlans { vlan 5000 { } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadVlan { .. })));
        let tree = parse("vlans { vlan 10 { } vlan 10 { } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadVlan { .. })));
        let tree = parse("interfaces { Ethernet1 { switchport { mode banana } } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadSwitchport { .. })
        ));
        // Address and switchport are mutually exclusive.
        let tree =
            parse("interfaces { Ethernet1 { address 10.0.0.1/24\nswitchport { } } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::AddressSwitchportConflict { .. })
        ));
        // Management ports are not switchports.
        let tree = parse("interfaces { Management1 { switchport { } } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadSwitchport { .. })
        ));
    }

    #[test]
    fn diff_vlans_and_switchports_report_changes() {
        let running = intents_of("");
        let candidate = intents_of(
            "vlans { vlan 10 { description \"Management\" } }\ninterfaces { Ethernet1 { switchport { mode access\naccess-vlan 10 } } }",
        );
        let vlans = diff_vlans(&running, &candidate);
        assert_eq!(
            vlans,
            vec![VlanChange {
                id: 10,
                ensure: Some(VlanIntent {
                    description: Some("Management".into()),
                    suspended: false,
                })
            }]
        );
        let switchports = diff_switchports(&running, &candidate);
        assert_eq!(switchports.len(), 1);
        assert_eq!(
            switchports[0].describe(),
            "Ethernet1: switchport access vlan 10"
        );

        // Unchanged -> empty; reverting -> removals.
        assert!(diff_vlans(&candidate, &candidate).is_empty());
        assert!(diff_switchports(&candidate, &candidate).is_empty());
        let back_vlans = diff_vlans(&candidate, &running);
        assert_eq!(
            back_vlans,
            vec![VlanChange {
                id: 10,
                ensure: None
            }]
        );
        let back_sp = diff_switchports(&candidate, &running);
        assert_eq!(back_sp[0].describe(), "Ethernet1: switchport removed");
    }

    /// The switching-suite seed config (spec Part 1.2, space-separated
    /// VLAN lists per the config lexer).
    fn seed() -> &'static str {
        r#"
interfaces {
    Ethernet1 {
        switchport {
            mode access
            access vlan 10
        }
        spanning-tree {
            portfast
            bpduguard
        }
        storm-control {
            broadcast level 10.00
            unknown-unicast level 5.00
        }
    }
    Ethernet49 {
        channel-group 1 mode active
        lacp {
            rate fast
            port-priority 32768
        }
    }
    Port-Channel1 {
        description "uplink to core"
        switchport {
            mode trunk
            trunk vlans 10 20 30 99
        }
        min-links 1
        lacp {
            fallback individual
            fallback-timeout 90
        }
    }
}
vlans {
    vlan 10 {
        description "LAN-USERS"
    }
    vlan 20 {
        description "VOICE"
        state suspend
    }
}
protocols {
    spanning-tree {
        mode mstp
        priority 32768
        hello-time 2
        max-age 20
        forward-time 15
        mst {
            name "QS-CORE"
            revision 3
            instance 1 vlans 10 20 30
            instance 2 vlans 99
        }
    }
    igmp-snooping {
        vlan 10 {
            fast-leave
            mrouter interface Port-Channel1
        }
        vlan 20 {
            querier address 10.0.20.1
        }
    }
    mld-snooping {
        vlan 10
    }
}
switching {
    mac-table {
        aging-time 300
        static 00:50:56:be:ef:01 vlan 10 interface Ethernet3
    }
    mirror {
        session 1 {
            source Ethernet1 both
            source Ethernet2 both
            destination Ethernet4
        }
    }
}
"#
    }

    #[test]
    fn seed_example_round_trips_and_extracts() {
        let tree = parse(seed()).unwrap();
        assert_eq!(parse(&tree.to_text()).unwrap(), tree);
        let intents = extract(&tree).unwrap();

        let e1 = &intents.ports["Ethernet1"];
        assert_eq!(
            e1.spanning_tree,
            Some(PortStpIntent {
                portfast: true,
                bpduguard: true,
                cost: None,
                port_priority: None,
            })
        );
        assert_eq!(e1.storm_control[&StormKind::Broadcast], "10.00");
        assert_eq!(e1.storm_control[&StormKind::UnknownUnicast], "5.00");

        let e49 = &intents.ports["Ethernet49"];
        assert_eq!(
            e49.channel_group,
            Some(ChannelGroup {
                group: 1,
                mode: LacpMode::Active
            })
        );
        assert_eq!(
            e49.lacp,
            Some(PortLacpIntent {
                rate_fast: true,
                port_priority: Some(32768),
            })
        );

        let po1 = &intents.lags[&1];
        assert_eq!(po1.description.as_deref(), Some("uplink to core"));
        assert_eq!(po1.min_links, Some(1));
        assert_eq!(po1.fallback, Some(LagFallback::Individual));
        assert_eq!(po1.fallback_timeout, Some(90));
        let sp = po1.switchport.as_ref().unwrap();
        assert_eq!(sp.mode, SwitchportMode::Trunk);
        assert_eq!(sp.trunk_vlans, [10, 20, 30, 99]);

        assert!(!intents.vlans[&10].suspended);
        assert!(intents.vlans[&20].suspended);

        assert_eq!(intents.stp.mode, StpMode::Mstp);
        assert_eq!(intents.stp.priority, Some(32768));
        assert_eq!(intents.stp.mst_name.as_deref(), Some("QS-CORE"));
        assert_eq!(intents.stp.mst_revision, Some(3));
        assert_eq!(intents.stp.instances[&1], [10, 20, 30]);
        assert_eq!(intents.stp.instances[&2], [99]);

        assert!(!intents.igmp_snooping.disabled);
        let v10 = &intents.igmp_snooping.vlans[&10];
        assert!(v10.fast_leave && !v10.querier);
        assert_eq!(v10.mrouters, ["Port-Channel1"]);
        let v20 = &intents.igmp_snooping.vlans[&20];
        assert!(v20.querier);
        assert_eq!(v20.querier_address.as_deref(), Some("10.0.20.1"));
        // The bare-leaf per-VLAN form is accepted too.
        assert_eq!(intents.mld_snooping.vlans[&10], SnoopVlanIntent::default());

        assert_eq!(intents.mac_table.aging_time, Some(300));
        assert_eq!(
            intents.mac_table.statics[&("00:50:56:be:ef:01".into(), 10)],
            FdbTarget::Port("Ethernet3".into())
        );

        let mirror = &intents.mirror[&1];
        assert_eq!(mirror.sources["Ethernet1"], MirrorDirection::Both);
        assert_eq!(mirror.sources["Ethernet2"], MirrorDirection::Both);
        assert_eq!(mirror.destination.as_deref(), Some("Ethernet4"));

        assert_eq!(
            lag_state(&intents)[&1].members.keys().collect::<Vec<_>>(),
            ["Ethernet49"]
        );
        assert!(intents.warnings.is_empty());
    }

    #[test]
    fn channel_group_semantics() {
        // Member ports carry no switchport of their own.
        let tree =
            parse("interfaces { Ethernet1 { channel-group 1 mode active\nswitchport { } } }")
                .unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::MemberConfigConflict { .. })
        ));
        // Address and channel-group are mutually exclusive.
        let tree =
            parse("interfaces { Ethernet1 { channel-group 1 mode active\naddress 10.0.0.1/24 } }")
                .unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::AddressChannelGroupConflict { .. })
        ));
        // All members of a group run the same mode.
        let tree = parse(
            "interfaces { Ethernet1 { channel-group 1 mode active }\nEthernet2 { channel-group 1 mode on } }",
        )
        .unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadChannelGroup { .. })
        ));
        // Max 8 members.
        let members: String = (1..=9)
            .map(|i| format!("Ethernet{i} {{ channel-group 1 mode active }}\n"))
            .collect();
        let tree = parse(&format!("interfaces {{ {members} }}")).unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadChannelGroup { .. })
        ));
        // Management ports stay out.
        let tree = parse("interfaces { Management1 { channel-group 1 mode active } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadChannelGroup { .. })
        ));
        // A Po with no members is a warning, not an error.
        let intents = intents_of("interfaces { Port-Channel2 { } }");
        assert_eq!(intents.warnings, ["Port-Channel2 has no member ports"]);
        // The exact member-conflict message from the spec.
        let tree =
            parse("interfaces { Ethernet49 { channel-group 1 mode active\nswitchport { } } }")
                .unwrap();
        assert_eq!(
            extract(&tree).unwrap_err().to_string(),
            "Ethernet49: member of Port-Channel1; configure the Port-Channel"
        );
    }

    #[test]
    fn rejects_bad_lag_blocks() {
        let tree = parse("interfaces { Port-Channel0 { } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadInterfaceBlock(_))
        ));
        let tree = parse("interfaces { Port-Channel65 { } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadInterfaceBlock(_))
        ));
        let tree = parse("interfaces { Port-Channel1 { address 10.0.0.1/24 } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadLag { .. })));
        let tree = parse("interfaces { Port-Channel1 { min-links 9 } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadLag { .. })));
        let tree = parse("interfaces { Port-Channel1 { lacp { fallback banana } } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadLag { .. })));
        let tree = parse("interfaces { Port-Channel1 { lacp { fallback-timeout 901 } } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadLag { .. })));
        // Member-side lacp leaves don't belong on the Po (and vice versa).
        let tree = parse("interfaces { Port-Channel1 { lacp { rate fast } } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadLag { .. })));
        let tree = parse("interfaces { Ethernet1 { lacp { fallback static } } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadChannelGroup { .. })
        ));
    }

    #[test]
    fn stp_validation() {
        // Priority must be a multiple of 4096.
        let tree = parse("protocols { spanning-tree { priority 4095 } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadStp(_))));
        // rapid-pvst is explicitly deferred.
        let tree = parse("protocols { spanning-tree { mode rapid-pvst } }").unwrap();
        assert_eq!(
            extract(&tree).unwrap_err().to_string(),
            "spanning-tree: mode rapid-pvst is not supported (use mstp or rstp)"
        );
        // MST instance VLAN sets must be disjoint.
        let tree = parse(
            "protocols { spanning-tree { mst { instance 1 vlans 10\ninstance 2 vlans 10 } } }",
        )
        .unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadStp(_))));
        // Instance ids are 1..15 (0 is implicit).
        let tree = parse("protocols { spanning-tree { mst { instance 0 vlans 10 } } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadStp(_))));
        let tree = parse("protocols { spanning-tree { mst { instance 16 vlans 10 } } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadStp(_))));
        // Timer ranges.
        let tree = parse("protocols { spanning-tree { hello-time 11 } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadStp(_))));
        let tree = parse("protocols { spanning-tree { max-age 5 } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadStp(_))));
        // Port-level: priority must be a multiple of 16.
        let tree =
            parse("interfaces { Ethernet1 { spanning-tree { port-priority 100 } } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadPortStp { .. })
        ));
        let intents = intents_of(
            "interfaces { Ethernet1 { spanning-tree { port-priority 112\ncost 20000 } } }",
        );
        let stp = intents.ports["Ethernet1"].spanning_tree.as_ref().unwrap();
        assert_eq!(stp.port_priority, Some(112));
        assert_eq!(stp.cost, Some(20000));
        // rstp and none parse.
        assert_eq!(
            intents_of("protocols { spanning-tree { mode rstp } }")
                .stp
                .mode,
            StpMode::Rstp
        );
        assert_eq!(
            intents_of("protocols { spanning-tree { mode none } }")
                .stp
                .mode,
            StpMode::None
        );
    }

    #[test]
    fn storm_levels_parse_and_canonicalize() {
        assert_eq!(parse_storm_level("10").unwrap(), "10.00");
        assert_eq!(parse_storm_level("10.5").unwrap(), "10.50");
        assert_eq!(parse_storm_level("0.01").unwrap(), "0.01");
        assert_eq!(parse_storm_level("100").unwrap(), "100.00");
        assert_eq!(parse_storm_level("100.00").unwrap(), "100.00");
        assert!(parse_storm_level("100.01").is_err());
        assert!(parse_storm_level("101").is_err());
        assert!(parse_storm_level("10.123").is_err());
        assert!(parse_storm_level("-1").is_err());
        assert!(parse_storm_level("banana").is_err());
        assert!(parse_storm_level("").is_err());

        let tree =
            parse("interfaces { Ethernet1 { storm-control { broadcast level 101 } } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadStormControl { .. })
        ));
        let tree = parse("interfaces { Ethernet1 { storm-control { banana level 10 } } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadStormControl { .. })
        ));
    }

    /// LLDP: defaults on, `disable` as the off switch, timers in
    /// range, and every rejection in the spec.
    /// The services suite's seed block (its LLDP half), round-tripped
    /// through the serializer and re-extracted.
    #[test]
    fn services_seed_round_trips_and_extracts() {
        let text = r#"
services {
    lldp {
        tx-interval 30;
        hold-multiplier 4;
    }
    ntp {
        server 10.42.0.5;
        server pool.ntp.org;
    }
    sflow {
        collector 10.42.0.20;
        collector 10.42.0.21 port 6344;
        sample-rate 16384;
        polling-interval 30;
    }
    snmp {
        community public;
        community netops source 10.42.0.0/16;
        location "rack 4, closet B";
        contact "cody@nightshade.systems";
        user monitor auth sha "authpass1" priv aes "privpass1";
    }
}
vlans {
    vlan 99 { }
}
interfaces {
    Management1 {
        address 10.42.0.9/24;
    }
    Ethernet3 {
        lldp disable;
    }
    Ethernet4 {
        sflow disable;
    }
    Vlan99 {
        address 10.42.10.9/24;
        dhcp-relay server 10.42.0.5;
        dhcp-relay server 10.42.0.6;
    }
}
"#;
        let tree = parse(text).unwrap();
        assert_eq!(parse(&tree.to_text()).unwrap(), tree);
        let intents = extract(&tree).unwrap();
        assert_eq!(
            intents.lldp,
            LldpIntent {
                disabled: false,
                tx_interval: Some(30),
                hold_multiplier: Some(4),
            }
        );
        assert_eq!(lldp_state(&intents).disabled_ports, ["Ethernet3"]);
        assert_eq!(intents.ntp.servers, ["10.42.0.5", "pool.ntp.org"]);
        // Communities keep config order; the user's passphrases survive
        // the round trip (redaction is a display-time concern).
        assert_eq!(
            intents
                .snmp
                .communities
                .iter()
                .map(|c| (c.name.as_str(), c.source.as_deref()))
                .collect::<Vec<_>>(),
            vec![("public", None), ("netops", Some("10.42.0.0/16"))]
        );
        assert_eq!(intents.snmp.location.as_deref(), Some("rack 4, closet B"));
        assert_eq!(
            intents
                .sflow
                .collectors
                .iter()
                .map(|c| (c.address.as_str(), c.port))
                .collect::<Vec<_>>(),
            vec![("10.42.0.20", None), ("10.42.0.21", Some(6344))]
        );
        assert_eq!(intents.sflow.rate(), 16384);
        assert_eq!(intents.sflow.polling(), 30);
        assert_eq!(sflow_state(&intents).disabled_ports, ["Ethernet4"]);
        assert_eq!(
            intents.dhcp_relay[&99],
            vec![
                std::net::Ipv4Addr::new(10, 42, 0, 5),
                std::net::Ipv4Addr::new(10, 42, 0, 6)
            ]
        );
        // The relay's giaddr is the SVI's address, stripped of its
        // prefix length.
        assert_eq!(
            snoopsec_state(&intents).relay[&99].1,
            "10.42.10.9".to_string()
        );
        assert_eq!(
            intents.snmp.users["monitor"],
            SnmpUser {
                auth_password: "authpass1".into(),
                priv_password: "privpass1".into(),
            }
        );
    }

    /// NTP: servers in order, deduplicated, capped at four, with the
    /// deferred spellings rejected rather than ignored.
    #[test]
    fn ntp_validation() {
        assert_eq!(intents_of("").ntp, NtpIntent::default());
        let intents = intents_of(
            "services { ntp { server 10.42.0.5
server pool.ntp.org } }",
        );
        assert_eq!(intents.ntp.servers, ["10.42.0.5", "pool.ntp.org"]);
        // IPv6 literals and trailing-dot FQDNs are servers too; a
        // repeat of one already listed collapses.
        let intents = intents_of(
            "services { ntp { server 2001:db8::1
server ntp.example.com.
server 2001:db8::1 } }",
        );
        assert_eq!(intents.ntp.servers, ["2001:db8::1", "ntp.example.com."]);

        for text in [
            "services { ntp { server } }",
            "services { ntp { server \"bad host\" } }",
            "services { ntp { server -leading.example.com } }",
            "services { ntp { server trailing-.example.com } }",
            "services { ntp { server a..b } }",
            "services { ntp { pool 10.0.0.1 } }",
            "services { ntp { server 1.1.1.1
server 2.2.2.2
server 3.3.3.3
server 4.4.4.4
server 5.5.5.5 } }",
        ] {
            let tree = parse(text).unwrap();
            assert!(
                matches!(extract(&tree), Err(IntentError::BadNtp(_))),
                "{text} should be rejected"
            );
        }
        // The deferred halves of NTP name themselves.
        let tree = parse("services { ntp { listen 10.0.0.1 } }").unwrap();
        assert_eq!(
            extract(&tree).unwrap_err().to_string(),
            "services ntp: NTP server mode is not supported"
        );
        let tree = parse("services { ntp { key 1 } }").unwrap();
        assert_eq!(
            extract(&tree).unwrap_err().to_string(),
            "services ntp: NTP authentication is not supported"
        );
    }

    /// NTP is an OS-side family: it rides the `diff_os` delta like ssh
    /// and the web console, and an emptied block is a real change (the
    /// client stops).
    #[test]
    fn ntp_diffs_through_the_os_changes() {
        let none = intents_of("");
        let two = intents_of(
            "services { ntp { server 10.42.0.5
server pool.ntp.org } }",
        );
        assert_eq!(diff_os(&none, &none).ntp, None);
        assert_eq!(
            diff_os(&none, &two).ntp.unwrap().servers,
            ["10.42.0.5", "pool.ntp.org"]
        );
        assert_eq!(diff_os(&two, &none).ntp, Some(NtpIntent::default()));
        assert_eq!(
            diff_os(&none, &two).describe(),
            ["ntp servers: 10.42.0.5, pool.ntp.org"]
        );
        assert_eq!(
            diff_os(&two, &none).describe(),
            ["ntp disabled (no servers)"]
        );
    }

    /// SNMP: names, passphrase length, fixed protocols, and every
    /// deferred spelling rejected rather than ignored.
    #[test]
    fn snmp_validation() {
        assert_eq!(intents_of("").snmp, SnmpIntent::default());
        assert!(!intents_of("").snmp.enabled);
        // A bare block still enables the agent.
        let mgmt = "interfaces { Management1 { address 10.42.0.9/24 } }\n";
        let intents = intents_of(&format!("{mgmt}services {{ snmp {{ }} }}"));
        assert!(intents.snmp.enabled);
        assert!(intents.snmp.communities.is_empty());

        for (text, reason) in [
            ("services { snmp { community 9bad } }", "bad community name"),
            ("services { snmp { community has-space bad } }", "expected"),
            (
                "services { snmp { community public source notaprefix } }",
                "community public",
            ),
            (
                "services { snmp { community a\ncommunity a } }",
                "duplicate community",
            ),
            (
                "services { snmp { user 9bad auth sha aaaaaaaa priv aes bbbbbbbb } }",
                "bad user name",
            ),
            (
                "services { snmp { user m auth md5 aaaaaaaa priv aes bbbbbbbb } }",
                "auth protocol",
            ),
            (
                "services { snmp { user m auth sha aaaaaaaa priv des bbbbbbbb } }",
                "priv protocol",
            ),
            (
                "services { snmp { user m auth sha short priv aes bbbbbbbb } }",
                "at least 8 characters",
            ),
            ("services { snmp { user m } }", "expected"),
            (
                "services { snmp { location a\nlocation b } }",
                "duplicate location",
            ),
            ("services { snmp { trap 10.0.0.1 } }", "traps and informs"),
            ("services { snmp { rwcommunity secret } }", "write access"),
            ("services { snmp { nonesuch } }", "unrecognized statement"),
        ] {
            let tree = parse(&format!("{mgmt}{text}")).unwrap();
            let message = extract(&tree).unwrap_err().to_string();
            assert!(
                matches!(extract(&tree), Err(IntentError::BadSnmp(_))),
                "{text} should be a BadSnmp"
            );
            assert!(
                message.contains(reason),
                "{text}: {message:?} should mention {reason:?}"
            );
        }

        // Names are letter-first, 32 characters at most.
        assert!(valid_snmp_name("a"));
        assert!(valid_snmp_name("net_ops-1"));
        assert!(valid_snmp_name("A2345678901234567890123456789012"));
        assert!(!valid_snmp_name("A23456789012345678901234567890123"));
        assert!(!valid_snmp_name("9bad"));
        assert!(!valid_snmp_name(""));
        assert!(!valid_snmp_name("has space"));
    }

    /// sFlow: collectors, the power-of-two rate, the collector cap,
    /// and the per-port rules.
    #[test]
    fn sflow_validation() {
        assert_eq!(intents_of("").sflow, SflowIntent::default());
        assert!(!intents_of("").sflow.enabled());

        let intents = intents_of(
            "services { sflow { collector 10.42.0.20\ncollector 10.42.0.21 port 6344\nsample-rate 4096\npolling-interval 60 } }",
        );
        assert!(intents.sflow.enabled());
        assert_eq!(intents.sflow.rate(), 4096);
        assert_eq!(intents.sflow.polling(), 60);
        // Defaults apply when the leaves are absent.
        let intents = intents_of("services { sflow { collector 10.42.0.20 } }");
        assert_eq!(intents.sflow.rate(), DEFAULT_SFLOW_SAMPLE_RATE);
        assert_eq!(intents.sflow.polling(), DEFAULT_SFLOW_POLLING);

        for text in [
            "services { sflow { collector notanip } }",
            "services { sflow { collector 10.0.0.1 port 0 } }",
            "services { sflow { collector 10.0.0.1\ncollector 10.0.0.1 } }",
            "services { sflow { collector 10.0.0.1\ncollector 10.0.0.2\ncollector 10.0.0.3 } }",
            "services { sflow { collector 10.0.0.1\npolling-interval 4 } }",
            "services { sflow { collector 10.0.0.1\npolling-interval 301 } }",
            "services { sflow { collector 10.0.0.1\nsample-rate 128 } }",
            "services { sflow { collector 10.0.0.1\nsample-rate 2097152 } }",
            "services { sflow { egress } }",
            "services { sflow { nonesuch } }",
        ] {
            let tree = parse(text).unwrap();
            assert!(
                matches!(extract(&tree), Err(IntentError::BadSflow(_))),
                "{text} should be rejected"
            );
        }

        // A non-power-of-two rate names the neighbours it sits between.
        let tree = parse("services { sflow { collector 10.0.0.1\nsample-rate 10000 } }").unwrap();
        assert_eq!(
            extract(&tree).unwrap_err().to_string(),
            "services sflow: sample-rate 10000 is not a power of two (nearest: 8192, 16384)"
        );
        assert_eq!(nearest_sample_rates(16384), (16384, 16384));
        assert_eq!(nearest_sample_rates(300), (256, 512));
        assert_eq!(nearest_sample_rates(1_000_000), (524_288, 1_048_576));

        // Anything sflow without a collector samples nothing.
        for text in [
            "services { sflow { sample-rate 4096 } }",
            "services { sflow { polling-interval 30 } }",
            "interfaces { Ethernet4 { sflow disable } }",
        ] {
            let tree = parse(text).unwrap();
            assert_eq!(
                extract(&tree).unwrap_err().to_string(),
                "services sflow: at least one collector is required",
                "{text} should demand a collector"
            );
        }

        // Per-port: physical ports only, Po members included.
        let intents = intents_of(
            "services { sflow { collector 10.0.0.1 } }\ninterfaces { Ethernet4 { sflow disable }\nEthernet5 { channel-group 1 mode active\nsflow disable } }",
        );
        assert!(intents.ports["Ethernet4"].sflow_disabled);
        assert!(intents.ports["Ethernet5"].sflow_disabled);
        let tree = parse("interfaces { Ethernet1 { sflow enable } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadSflow(_))));
        for name in ["Vlan99", "Port-Channel1", "Management1"] {
            let tree = parse(&format!(
                "vlans {{ vlan 99 {{ }} }}\ninterfaces {{ {name} {{ sflow disable }} }}"
            ))
            .unwrap();
            assert_eq!(
                extract(&tree).unwrap_err().to_string(),
                format!("{name}: sflow is a physical-port setting"),
                "{name} should reject a per-port sflow leaf"
            );
        }
    }

    /// The sFlow state is the global block plus the disabled ports, and
    /// it diffs as one whole.
    #[test]
    fn sflow_state_and_diff() {
        let running = intents_of("");
        assert_eq!(sflow_state(&running), SflowState::default());
        assert_eq!(diff_sflow(&running, &running), None);

        let candidate = intents_of(
            "services { sflow { collector 10.42.0.20 } }\ninterfaces { Ethernet4 { sflow disable }\nEthernet1 { } }",
        );
        let change = diff_sflow(&running, &candidate).unwrap();
        assert!(change.global.enabled());
        assert_eq!(change.disabled_ports, ["Ethernet4"]);
        // Removing the block reverts to "off".
        let change = diff_sflow(&candidate, &running).unwrap();
        assert_eq!(change, SflowState::default());
        assert!(!change.global.enabled());
    }

    /// DHCP relay: SVIs only, addressed by commit, IPv4 servers,
    /// capped at four.
    #[test]
    fn dhcp_relay_validation() {
        assert!(intents_of("").dhcp_relay.is_empty());

        let intents = intents_of(
            "vlans { vlan 99 { } }
interfaces { Vlan99 { address 10.42.10.9/24
dhcp-relay server 10.42.0.5
dhcp-relay server 10.42.0.6
dhcp-relay server 10.42.0.5 } }",
        );
        // Config order, duplicates collapsed.
        assert_eq!(
            intents.dhcp_relay[&99],
            vec![
                std::net::Ipv4Addr::new(10, 42, 0, 5),
                std::net::Ipv4Addr::new(10, 42, 0, 6)
            ]
        );

        // A relay needs a giaddr.
        let tree =
            parse("vlans { vlan 99 { } }\ninterfaces { Vlan99 { dhcp-relay server 10.42.0.5 } }")
                .unwrap();
        assert_eq!(
            extract(&tree).unwrap_err().to_string(),
            "Vlan99: dhcp-relay: the interface must carry an address (the relay's giaddr)"
        );

        // SVIs only.
        for name in ["Ethernet1", "Port-Channel1", "Management1"] {
            let tree = parse(&format!(
                "interfaces {{ {name} {{ dhcp-relay server 10.42.0.5 }} }}"
            ))
            .unwrap();
            assert_eq!(
                extract(&tree).unwrap_err().to_string(),
                format!("{name}: dhcp-relay: dhcp-relay is an SVI setting"),
                "{name} should reject dhcp-relay"
            );
        }

        let svi = |body: &str| {
            format!("vlans {{ vlan 99 {{ }} }}\ninterfaces {{ Vlan99 {{ address 10.42.10.9/24\n{body} }} }}")
        };
        for body in [
            "dhcp-relay server notanip",
            "dhcp-relay 10.42.0.5",
            "dhcp-relay server",
            // DHCPv6 relay is deferred, so a v6 server is not a server.
            "dhcp-relay server 2001:db8::5",
            "dhcp-relay server 10.0.0.1
dhcp-relay server 10.0.0.2
dhcp-relay server 10.0.0.3
dhcp-relay server 10.0.0.4
dhcp-relay server 10.0.0.5",
        ] {
            let tree = parse(&svi(body)).unwrap();
            assert!(
                matches!(extract(&tree), Err(IntentError::BadDhcpRelay { .. })),
                "{body} should be rejected"
            );
        }
    }

    /// The relay rides the snooping engine's whole-state push, so a
    /// relay change is a snoopsec change.
    #[test]
    fn dhcp_relay_diffs_with_the_snooping_state() {
        let running = intents_of("");
        let candidate = intents_of(
            "vlans { vlan 99 { } }
interfaces { Vlan99 { address 10.42.10.9/24
dhcp-relay server 10.42.0.5 } }",
        );
        let change = diff_snoopsec(&running, &candidate).unwrap();
        assert_eq!(
            change.relay[&99],
            (
                vec![std::net::Ipv4Addr::new(10, 42, 0, 5)],
                "10.42.10.9".to_string()
            )
        );
        // Removing the relay is a change back to nothing.
        let change = diff_snoopsec(&candidate, &running).unwrap();
        assert!(change.relay.is_empty());
        assert_eq!(diff_snoopsec(&candidate, &candidate), None);
    }

    #[test]
    fn lldp_validation() {
        // Absent block = defaults, LLDP running.
        assert_eq!(intents_of("").lldp, LldpIntent::default());
        assert!(!intents_of("").lldp.disabled);

        let intents = intents_of(
            "services { lldp { tx-interval 45
hold-multiplier 3 } }",
        );
        assert_eq!(
            intents.lldp,
            LldpIntent {
                disabled: false,
                tx_interval: Some(45),
                hold_multiplier: Some(3),
            }
        );
        assert!(intents_of("services { lldp { disable } }").lldp.disabled);

        for text in [
            "services { lldp { tx-interval 4 } }",
            "services { lldp { tx-interval 301 } }",
            "services { lldp { hold-multiplier 1 } }",
            "services { lldp { hold-multiplier 11 } }",
            "services { lldp { tx-interval banana } }",
        ] {
            let tree = parse(text).unwrap();
            assert!(
                matches!(extract(&tree), Err(IntentError::BadLldp(_))),
                "{text} should be rejected"
            );
        }
        // Unknown statements inside the block and unknown services.
        let tree = parse("services { lldp { med } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadLldp(_))));
        let tree = parse("services { ptp { } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadServices(_))));

        // Per-port: `lldp disable` on physical ports only, Po members
        // included (LLDP runs below the LAG).
        let intents = intents_of(
            "interfaces { Ethernet3 { lldp disable }
Ethernet4 { channel-group 1 mode active
lldp disable } }",
        );
        assert!(intents.ports["Ethernet3"].lldp_disabled);
        assert!(intents.ports["Ethernet4"].lldp_disabled);
        assert!(!intents_of("interfaces { Ethernet1 { } }").ports["Ethernet1"].lldp_disabled);
        let tree = parse("interfaces { Ethernet1 { lldp enable } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadLldp(_))));
        for name in ["Vlan99", "Port-Channel1", "Management1"] {
            let tree = parse(&format!(
                "vlans {{ vlan 99 {{ }} }}
interfaces {{ {name} {{ lldp disable }} }}"
            ))
            .unwrap();
            assert!(
                matches!(
                    extract(&tree),
                    Err(IntentError::PortServiceOnNonPort {
                        feature: "lldp",
                        ..
                    })
                ),
                "{name} should reject a per-port lldp leaf"
            );
        }
        assert_eq!(
            IntentError::PortServiceOnNonPort {
                name: "Vlan99".into(),
                feature: "lldp",
            }
            .to_string(),
            "Vlan99: lldp is a physical-port setting"
        );
    }

    /// The LLDP state is the global block plus the disabled ports, and
    /// it diffs as one whole (orch consumes whole states).
    #[test]
    fn lldp_state_and_diff() {
        let running = intents_of("");
        assert_eq!(lldp_state(&running), LldpState::default());
        assert_eq!(diff_lldp(&running, &running), None);

        let candidate = intents_of(
            "services { lldp { tx-interval 15 } }
interfaces { Ethernet3 { lldp disable }
Ethernet1 { } }",
        );
        let change = diff_lldp(&running, &candidate).unwrap();
        assert_eq!(change.global.tx_interval, Some(15));
        assert_eq!(change.disabled_ports, ["Ethernet3"]);
        // Removing the whole block reverts to the engine defaults.
        let change = diff_lldp(&candidate, &running).unwrap();
        assert_eq!(change, LldpState::default());
    }

    #[test]
    fn snooping_validation() {
        // Global disable and robustness.
        let intents = intents_of("protocols { igmp-snooping { disable\nrobustness 3 } }");
        assert!(intents.igmp_snooping.disabled);
        assert_eq!(intents.igmp_snooping.robustness, Some(3));
        let tree = parse("protocols { igmp-snooping { robustness 4 } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadSnooping { .. })
        ));
        // Querier addresses must match the family.
        let tree =
            parse("protocols { igmp-snooping { vlan 10 { querier address banana } } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadSnooping { .. })
        ));
        let tree =
            parse("protocols { mld-snooping { vlan 10 { querier address 10.0.0.1 } } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadSnooping { .. })
        ));
        let intents =
            intents_of("protocols { mld-snooping { vlan 10 { querier address fe80::1 } } }");
        assert_eq!(
            intents.mld_snooping.vlans[&10].querier_address.as_deref(),
            Some("fe80::1")
        );
        // Bare querier is a local querier with a derived address.
        let intents = intents_of("protocols { igmp-snooping { vlan 10 { querier } } }");
        assert!(intents.igmp_snooping.vlans[&10].querier);
        assert_eq!(intents.igmp_snooping.vlans[&10].querier_address, None);
        // Per-VLAN disable; multiple mrouters sorted + deduplicated.
        let intents = intents_of(
            "protocols { igmp-snooping { vlan 10 { disable\nmrouter interface Ethernet2\nmrouter interface Ethernet1\nmrouter interface Ethernet2 } } }",
        );
        let v10 = &intents.igmp_snooping.vlans[&10];
        assert!(v10.disabled);
        assert_eq!(v10.mrouters, ["Ethernet1", "Ethernet2"]);
        // Unknown protocols block.
        let tree = parse("protocols { ospf { } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadProtocols(_))));
    }

    #[test]
    fn mac_table_validation() {
        // Aging: 0 = no aging; 1..9 invalid.
        assert_eq!(
            intents_of("switching { mac-table { aging-time 0 } }")
                .mac_table
                .aging_time,
            Some(0)
        );
        let tree = parse("switching { mac-table { aging-time 5 } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadMacTable(_))));
        let tree = parse("switching { mac-table { aging-time 1000001 } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadMacTable(_))));
        // Statics normalize their MACs; drop targets parse.
        let intents = intents_of("switching { mac-table { static 0050.56BE.EF01 vlan 10 drop } }");
        assert_eq!(
            intents.mac_table.statics[&("00:50:56:be:ef:01".into(), 10)],
            FdbTarget::Drop
        );
        // Multicast MACs are rejected.
        let tree = parse(
            "switching { mac-table { static 01:00:5e:00:00:01 vlan 10 interface Ethernet1 } }",
        )
        .unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadMacTable(_))));
        // Duplicate (mac, vlan) keys are rejected.
        let tree = parse(
            "switching { mac-table { static 00:50:56:be:ef:01 vlan 10 drop\nstatic 00:50:56:be:ef:01 vlan 10 interface Ethernet1 } }",
        )
        .unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadMacTable(_))));
        // Unknown switching block.
        let tree = parse("switching { banana { } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadSwitching(_))));
    }

    #[test]
    fn mac_parsing_normalizes() {
        assert_eq!(parse_mac("00:50:56:BE:EF:01").unwrap(), "00:50:56:be:ef:01");
        assert_eq!(parse_mac("0050.56be.ef01").unwrap(), "00:50:56:be:ef:01");
        assert_eq!(parse_mac("00-50-56-be-ef-01").unwrap(), "00:50:56:be:ef:01");
        assert!(parse_mac("00:50:56:be:ef").is_err());
        assert!(parse_mac("zz:50:56:be:ef:01").is_err());
        assert!(parse_unicast_mac("01:00:5e:00:00:01").is_err());
        assert!(parse_unicast_mac("00:50:56:be:ef:01").is_ok());
    }

    #[test]
    fn mirror_validation() {
        // Session ids are 1..4.
        let tree = parse("switching { mirror { session 5 { } } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadMirrorBlock(_))
        ));
        // A destination may not be a source anywhere.
        let tree = parse(
            "switching { mirror { session 1 { source Ethernet1\ndestination Ethernet2 }\nsession 2 { source Ethernet2\ndestination Ethernet3 } } }",
        )
        .unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadMirror { .. })));
        // A port sources at most one session per direction...
        let tree = parse(
            "switching { mirror { session 1 { source Ethernet1 rx\ndestination Ethernet3 }\nsession 2 { source Ethernet1 both\ndestination Ethernet4 } } }",
        )
        .unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadMirror { .. })));
        // ...but rx in one and tx in another is fine.
        let intents = intents_of(
            "switching { mirror { session 1 { source Ethernet1 rx\ndestination Ethernet3 }\nsession 2 { source Ethernet1 tx\ndestination Ethernet4 } } }",
        );
        assert_eq!(intents.mirror.len(), 2);
        // The destination carries no channel-group or address.
        let tree = parse(
            "interfaces { Ethernet4 { channel-group 1 mode on } }\nswitching { mirror { session 1 { source Ethernet1\ndestination Ethernet4 } } }",
        )
        .unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadMirror { .. })));
        let tree = parse(
            "interfaces { Ethernet4 { address 10.0.0.1/24 } }\nswitching { mirror { session 1 { source Ethernet1\ndestination Ethernet4 } } }",
        )
        .unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadMirror { .. })));
        // A destination-less session is a warning.
        let intents = intents_of("switching { mirror { session 1 { source Ethernet1 } } }");
        assert_eq!(intents.warnings, ["mirror session 1 has no destination"]);
    }

    #[test]
    fn vlan_state_suspend() {
        assert!(intents_of("vlans { vlan 20 { state suspend } }").vlans[&20].suspended);
        assert!(!intents_of("vlans { vlan 20 { state active } }").vlans[&20].suspended);
        let tree = parse("vlans { vlan 1 { state suspend } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadVlan { .. })));
        let tree = parse("vlans { vlan 20 { state banana } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadVlan { .. })));
    }

    #[test]
    fn dot1q_tunnel_mode() {
        let intents = intents_of(
            "interfaces { Ethernet1 { switchport { mode dot1q-tunnel\naccess vlan 100 } } }",
        );
        let sp = intents.ports["Ethernet1"].switchport.as_ref().unwrap();
        assert_eq!(sp.mode, SwitchportMode::Dot1qTunnel);
        assert_eq!(sp.access_vlan, Some(100));
        // Trunk leaves are excluded under dot1q-tunnel.
        let tree =
            parse("interfaces { Ethernet1 { switchport { mode dot1q-tunnel\ntrunk vlans 10 } } }")
                .unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadSwitchport { .. })
        ));
    }

    #[test]
    fn lacp_global_extracts() {
        assert_eq!(
            intents_of("protocols { lacp { system-priority 100 } }")
                .lacp
                .system_priority,
            Some(100)
        );
        let tree = parse("protocols { lacp { system-priority 65536 } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadProtocols(_))));
    }

    #[test]
    fn diff_lags_reports_membership_and_config_changes() {
        let running = intents_of("");
        let candidate = intents_of(
            "interfaces { Ethernet49 { channel-group 1 mode active }\nPort-Channel1 { min-links 1 } }",
        );
        let changes = diff_lags(&running, &candidate);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].group, 1);
        let ensure = changes[0].ensure.as_ref().unwrap();
        assert_eq!(ensure.lag.min_links, Some(1));
        assert_eq!(ensure.members.keys().collect::<Vec<_>>(), ["Ethernet49"]);
        assert_eq!(
            changes[0].describe(),
            "Port-Channel1: members Ethernet49 mode active"
        );

        // Unchanged -> empty; reverting -> removal.
        assert!(diff_lags(&candidate, &candidate).is_empty());
        let back = diff_lags(&candidate, &running);
        assert_eq!(
            back,
            vec![LagChange {
                group: 1,
                ensure: None
            }]
        );
        assert_eq!(back[0].describe(), "Port-Channel1 removed");

        // A member-only group materializes the Po.
        let member_only = intents_of("interfaces { Ethernet49 { channel-group 2 mode on } }");
        let changes = diff_lags(&running, &member_only);
        assert_eq!(changes[0].group, 2);

        // A member config change (rate) is a diff, not delete+recreate.
        let tuned = intents_of(
            "interfaces { Ethernet49 { channel-group 1 mode active\nlacp { rate fast } }\nPort-Channel1 { min-links 1 } }",
        );
        let changes = diff_lags(&candidate, &tuned);
        assert_eq!(changes.len(), 1);
        assert!(changes[0].ensure.is_some());
    }

    #[test]
    fn diff_stp_pushes_whole_state_on_change() {
        let running = intents_of("");
        assert!(diff_stp(&running, &running).is_none());
        let candidate = intents_of(
            "protocols { spanning-tree { priority 4096 } }\ninterfaces { Ethernet1 { spanning-tree { portfast } } }",
        );
        let state = diff_stp(&running, &candidate).unwrap();
        assert_eq!(state.global.priority, Some(4096));
        assert!(state.ports["Ethernet1"].portfast);
        // Removal reverts to the default state.
        let back = diff_stp(&candidate, &running).unwrap();
        assert_eq!(back, StpState::default());
        // A Po's port config joins under its display name.
        let with_po = intents_of("interfaces { Port-Channel1 { spanning-tree { cost 10000 } } }");
        let state = diff_stp(&running, &with_po).unwrap();
        assert_eq!(state.ports["Port-Channel1"].cost, Some(10000));
    }

    #[test]
    fn diff_snooping_pushes_whole_state_on_change() {
        let running = intents_of("");
        let candidate = intents_of("protocols { igmp-snooping { vlan 10 { fast-leave } } }");
        assert!(diff_snooping(&running.igmp_snooping, &running.igmp_snooping).is_none());
        let state = diff_snooping(&running.igmp_snooping, &candidate.igmp_snooping).unwrap();
        assert!(state.vlans[&10].fast_leave);
        assert_eq!(
            diff_snooping(&candidate.igmp_snooping, &running.igmp_snooping),
            Some(SnoopingIntent::default())
        );
    }

    #[test]
    fn diff_mac_table_reports_minimal_deltas() {
        let running = intents_of(
            "switching { mac-table { aging-time 600\nstatic 00:50:56:be:ef:01 vlan 10 interface Ethernet3 } }",
        );
        assert!(diff_mac_table(&running, &running).is_empty());

        // Editing one entry produces one add (replace), not delete+add.
        let candidate = intents_of(
            "switching { mac-table { aging-time 600\nstatic 00:50:56:be:ef:01 vlan 10 interface Ethernet4 } }",
        );
        let changes = diff_mac_table(&running, &candidate);
        assert_eq!(
            changes.add,
            [(
                "00:50:56:be:ef:01".to_string(),
                10,
                FdbTarget::Port("Ethernet4".into())
            )]
        );
        assert!(changes.remove.is_empty() && changes.aging_time.is_none());

        // Removal reverts aging to the default and deletes the static.
        let back = diff_mac_table(&running, &intents_of(""));
        assert_eq!(back.aging_time, Some(DEFAULT_FDB_AGING_SECS));
        assert_eq!(back.remove, [("00:50:56:be:ef:01".to_string(), 10)]);
    }

    #[test]
    fn diff_storm_control_per_port_and_kind() {
        let running = intents_of(
            "interfaces { Ethernet1 { storm-control { broadcast level 10.00\nunknown-unicast level 5.00 } } }",
        );
        assert!(diff_storm_control(&running, &running).is_empty());
        let candidate = intents_of(
            "interfaces { Ethernet1 { storm-control { broadcast level 20.00 } }\nPort-Channel1 { storm-control { broadcast level 10.00 } } }",
        );
        let mut changes = diff_storm_control(&running, &candidate);
        changes.sort_by_key(|a| (a.name.clone(), a.kind));
        assert_eq!(
            changes,
            vec![
                StormChange {
                    name: "Ethernet1".into(),
                    kind: StormKind::Broadcast,
                    level: Some("20.00".into()),
                },
                StormChange {
                    name: "Ethernet1".into(),
                    kind: StormKind::UnknownUnicast,
                    level: None,
                },
                StormChange {
                    name: "Port-Channel1".into(),
                    kind: StormKind::Broadcast,
                    level: Some("10.00".into()),
                },
            ]
        );
        // Removing the port block clears everything.
        let back = diff_storm_control(&running, &intents_of(""));
        assert_eq!(back.len(), 2);
        assert!(back.iter().all(|c| c.level.is_none()));
    }

    #[test]
    fn diff_mirror_reports_session_changes() {
        let running = intents_of(
            "switching { mirror { session 1 { source Ethernet1 both\ndestination Ethernet4 } } }",
        );
        assert!(diff_mirror(&running, &running).is_empty());
        let candidate = intents_of(
            "switching { mirror { session 1 { source Ethernet1 rx\ndestination Ethernet4 } } }",
        );
        let changes = diff_mirror(&running, &candidate);
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0].ensure.as_ref().unwrap().sources["Ethernet1"],
            MirrorDirection::Rx
        );
        let back = diff_mirror(&running, &intents_of(""));
        assert_eq!(
            back,
            vec![MirrorChange {
                session: 1,
                ensure: None
            }]
        );
    }

    #[test]
    fn diff_os_tracks_port_addresses() {
        let running = intents_of("interfaces { Ethernet49 { admin-state enabled } }");
        let candidate =
            intents_of("interfaces { Ethernet49 { admin-state enabled\naddress 10.42.10.9/24 } }");
        let changes = diff_os(&running, &candidate);
        assert!(changes.management.is_empty());
        assert_eq!(
            changes.ports,
            vec![NetdevChange {
                name: "Ethernet49".into(),
                admin_up: None,
                set_address: Some("10.42.10.9/24".into()),
                del_address: None,
                ..Default::default()
            }]
        );
        // Port block removed entirely -> address torn down.
        let gone = diff_os(&candidate, &intents_of(""));
        assert_eq!(
            gone.ports,
            vec![NetdevChange {
                name: "Ethernet49".into(),
                admin_up: None,
                set_address: None,
                del_address: Some("10.42.10.9/24".into()),
                ..Default::default()
            }]
        );
    }

    /// The security-suite seed from the spec's Part 1.1.
    fn security_seed() -> &'static str {
        r#"
vlans {
    vlan 10 {
    }
    vlan 20 {
    }
}
security {
    acl {
        ipv4 EDGE-IN {
            rule 10 {
                permit
                protocol tcp
                source 10.0.0.0/8
                destination 10.42.0.0/16
                destination-port 443
            }
            rule 20 {
                permit
                protocol udp
                destination-port 67-68
            }
            rule 30 {
                deny
                source 192.0.2.0/24
                log
            }
            rule 40 {
                permit
                police rate 10m burst 256k
            }
        }
        ipv6 MGMT6-IN {
            rule 10 {
                permit
                protocol tcp
                source 2001:db8:9::/48
                destination-port 22
            }
            rule 20 {
                deny
                log
            }
        }
        mac IOT-MAC {
            rule 10 {
                permit
                source-mac 00:1c:73:00:00:00/ff:ff:ff:00:00:00
            }
            rule 20 {
                deny
            }
        }
    }
    copp {
        class bpdu {
            rate 512
            burst 128
        }
        class arp {
            rate 2000
            burst 500
        }
    }
    dot1x {
        radius-server 10.42.0.5 {
            key "s3cret"
        }
        reauth-interval 3600
    }
    dhcp-snooping {
        vlan 10
        vlan 20
    }
    arp-inspection {
        vlan 10
    }
}
interfaces {
    Ethernet1 {
        access-group EDGE-IN in
        switchport {
            mode trunk
            trunk vlans 10 20
        }
    }
    Ethernet5 {
        port-security {
            maximum 4
            violation shutdown
        }
    }
    Ethernet10 {
        dot1x
    }
    Port-Channel1 {
        switchport {
            mode trunk
            trunk vlans 10 20
        }
        dhcp-snooping trust
        arp-inspection trust
    }
}
"#
    }

    /// The Part 1.1 seed: the golden fixture for the QoS family.
    fn qos_seed() -> &'static str {
        r#"
qos {
    map {
        dscp-to-tc {
            dscp 46 tc 5
            dscp 26 tc 3
            dscp 8 tc 1
        }
        cos-to-tc {
            cos 5 tc 5
            cos 3 tc 3
        }
        tc-to-dscp {
            tc 5 dscp 46
            tc 3 dscp 26
        }
        tc-to-cos {
            tc 5 cos 5
            tc 3 cos 3
        }
    }
    wred-profile BULK {
        min-threshold 64
        max-threshold 256
        drop-probability 10
        ecn
    }
}
interfaces {
    Ethernet1 {
        qos {
            trust dscp
            default-tc 1
            queue 7 {
                priority strict
            }
            queue 5 {
                weight 40
                shape rate 100m
            }
            queue 3 {
                weight 30
                wred-profile BULK
            }
        }
    }
    Port-Channel1 {
        qos {
            trust dscp
            shape rate 800m
        }
    }
}
"#
    }

    #[test]
    fn qos_seed_round_trips_and_extracts() {
        let tree = parse(qos_seed()).unwrap();
        assert_eq!(parse(&tree.to_text()).unwrap(), tree);
        let intents = extract(&tree).unwrap();

        // Global maps.
        assert_eq!(
            intents.qos_maps.dscp_to_tc,
            BTreeMap::from([(46, 5), (26, 3), (8, 1)])
        );
        assert_eq!(intents.qos_maps.cos_to_tc, BTreeMap::from([(5, 5), (3, 3)]));
        assert_eq!(
            intents.qos_maps.tc_to_dscp,
            BTreeMap::from([(5, 46), (3, 26)])
        );
        assert_eq!(intents.qos_maps.tc_to_cos, BTreeMap::from([(5, 5), (3, 3)]));

        // The WRED profile.
        assert_eq!(
            intents.wred_profiles["BULK"],
            WredProfileIntent {
                min_threshold: Some(64),
                max_threshold: Some(256),
                drop_probability: 10,
                ecn: true,
            }
        );

        // Per-port classification, scheduling, shaping.
        let qos = intents.ports["Ethernet1"].qos.as_ref().unwrap();
        assert_eq!(qos.trust, QosTrust::Dscp);
        assert_eq!(qos.default_tc, 1);
        assert_eq!(qos.shape, None);
        assert_eq!(
            qos.queues.keys().copied().collect::<Vec<_>>(),
            vec![3, 5, 7]
        );
        assert!(qos.queues[&7].strict);
        assert_eq!(qos.queues[&5].weight, Some(40));
        assert_eq!(qos.queues[&5].shape, Some(100_000_000));
        assert_eq!(qos.queues[&3].wred_profile.as_deref(), Some("BULK"));

        // A Port-Channel program applies to every member.
        let po = intents.lags[&1].qos.as_ref().unwrap();
        assert_eq!(po.trust, QosTrust::Dscp);
        assert_eq!(po.shape, Some(800_000_000));
        assert_eq!(
            port_qos_state(&intents).keys().collect::<Vec<_>>(),
            vec!["Ethernet1", "Port-Channel1"]
        );
    }

    #[test]
    fn qos_queue_validation() {
        let bad = |text: &str| extract(&parse(text).unwrap()).unwrap_err();

        // Strict and weight on the same queue.
        assert_eq!(
            bad("interfaces { Ethernet1 { qos { queue 5 { priority strict\nweight 40 } } } }")
                .to_string(),
            "Ethernet1 queue 5: strict and weight are mutually exclusive"
        );
        // Strict queues must be the top ones.
        assert_eq!(
            bad("interfaces { Ethernet1 { qos { queue 5 { priority strict } } } }").to_string(),
            "strict queues must be the highest-numbered queues"
        );
        assert_eq!(
            bad("interfaces { Ethernet1 { qos { queue 7 { priority strict }\nqueue 5 { priority strict } } } }")
                .to_string(),
            "strict queues must be the highest-numbered queues"
        );
        // ... and a contiguous run from the top is fine.
        extract(
            &parse(
                "interfaces { Ethernet1 { qos { queue 7 { priority strict }\nqueue 6 { priority strict } } } }",
            )
            .unwrap(),
        )
        .unwrap();

        // A queue shaper above the port shaper is dead config.
        assert_eq!(
            bad(
                "interfaces { Ethernet1 { qos { shape rate 100m\nqueue 3 { shape rate 200m } } } }"
            )
            .to_string(),
            "Ethernet1 queue 3: shaper 200m exceeds the port shaper 100m"
        );
        // Rates below the shaper granularity floor.
        assert!(bad("interfaces { Ethernet1 { qos { shape rate 32k } } }")
            .to_string()
            .contains("below the 64k shaper granularity floor"));
    }

    #[test]
    fn qos_wred_validation() {
        let bad = |text: &str| extract(&parse(text).unwrap()).unwrap_err();

        // A dangling profile reference names every queue holding it,
        // so deleting a bound profile is one round trip, not N.
        assert_eq!(
            bad("interfaces { Ethernet1 { qos { queue 3 { wred-profile GONE } } } }").to_string(),
            "qos wred-profile GONE: not defined; referenced by Ethernet1 (q3)"
        );
        assert_eq!(
            bad("interfaces { Ethernet1 { qos { queue 3 { wred-profile BULK } } }
Ethernet2 { qos { queue 5 { wred-profile BULK } } }
Port-Channel1 { qos { queue 7 { wred-profile BULK } } } }")
                .to_string(),
            "qos wred-profile BULK: not defined; referenced by Ethernet1 (q3), Ethernet2 (q5), Port-Channel1 (q7)"
        );
        // Thresholds must be ordered...
        assert_eq!(
            bad("qos { wred-profile BULK { min-threshold 256\nmax-threshold 64 } }").to_string(),
            "qos wred-profile BULK: min-threshold 256 must be below max-threshold 64"
        );
        // ... and both are required once a queue references it.
        assert_eq!(
            bad("qos { wred-profile BULK { min-threshold 64 } }\ninterfaces { Ethernet1 { qos { queue 3 { wred-profile BULK } } } }")
                .to_string(),
            "qos wred-profile BULK: min-threshold and max-threshold are required when the profile is referenced"
        );
        // An unreferenced half-configured profile is only a draft.
        extract(&parse("qos { wred-profile BULK { min-threshold 64 } }").unwrap()).unwrap();
    }

    #[test]
    fn qos_is_a_front_panel_concept() {
        let bad = |text: &str| extract(&parse(text).unwrap()).unwrap_err();

        // LAG members configure on the Port-Channel.
        assert_eq!(
            bad("interfaces { Ethernet49 { channel-group 1 mode active\nqos { trust dscp } } }")
                .to_string(),
            "Ethernet49: member of Port-Channel1; configure the Port-Channel"
        );
        // SVIs and Management carry no QoS.
        assert!(
            bad("vlans { vlan 10 { } }\ninterfaces { Vlan10 { qos { trust dscp } } }")
                .to_string()
                .contains("QoS is a front-panel concept")
        );
        assert!(bad("interfaces { Management1 { qos { trust dscp } } }")
            .to_string()
            .contains("not supported on Management"));
    }

    #[test]
    fn qos_map_validation() {
        let bad = |text: &str| extract(&parse(text).unwrap()).unwrap_err();
        assert!(bad("qos { map { dscp-to-tc { dscp 64 tc 5 } } }")
            .to_string()
            .starts_with("qos map dscp-to-tc:"));
        assert!(bad("qos { map { dscp-to-tc { dscp 46 tc 9 } } }")
            .to_string()
            .contains("bad tc"));
        assert!(
            bad("qos { map { dscp-to-tc { dscp 46 tc 5 }\ndscp-to-tc { dscp 8 tc 1 } } }")
                .to_string()
                .contains("duplicate map block")
        );
        assert!(bad("qos { map { banana { dscp 46 tc 5 } } }")
            .to_string()
            .contains("unrecognized table"));
        // The value words are fixed per table.
        assert!(bad("qos { map { tc-to-dscp { dscp 46 tc 5 } } }")
            .to_string()
            .contains("expected `tc <value> dscp <value>`"));
    }

    #[test]
    fn qos_diffs_report_minimal_deltas() {
        let running = intents_of(qos_seed());
        // No change at all.
        assert!(diff_qos_maps(&running, &running).is_none());
        assert!(diff_wred_profiles(&running, &running).is_empty());
        assert!(diff_port_qos(&running, &running).is_empty());

        // One map value edited: the whole (declarative) map state ships.
        let edited = intents_of(&qos_seed().replace("dscp 46 tc 5", "dscp 46 tc 6"));
        let maps = diff_qos_maps(&running, &edited).unwrap();
        assert_eq!(maps.dscp_to_tc[&46], 6);
        assert!(diff_port_qos(&running, &edited).is_empty());

        // A profile threshold change is one ensure; a removed profile
        // is one remove.
        let edited = intents_of(&qos_seed().replace("max-threshold 256", "max-threshold 512"));
        let changes = diff_wred_profiles(&running, &edited);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].name, "BULK");
        assert_eq!(changes[0].ensure.as_ref().unwrap().max_threshold, Some(512));

        // Dropping the port's qos block clears the port.
        let cleared = intents_of(&qos_seed().replace(
            "        qos {\n            trust dscp\n            default-tc 1\n",
            "        qos {\n",
        ));
        let changes = diff_port_qos(&running, &cleared);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].port, "Ethernet1");
        let set = changes[0].set.as_ref().unwrap();
        assert_eq!(set.trust, QosTrust::Untrusted);
        assert_eq!(set.default_tc, 0);

        // Removing the block entirely reverts to the platform defaults.
        let none = intents_of("qos { }\ninterfaces { Ethernet1 { } }");
        let changes = diff_port_qos(&running, &none);
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().all(|change| change.set.is_none()));
    }

    #[test]
    fn security_seed_round_trips_and_extracts() {
        let tree = parse(security_seed()).unwrap();
        assert_eq!(parse(&tree.to_text()).unwrap(), tree);
        let intents = extract(&tree).unwrap();

        // The three families share one namespace.
        assert_eq!(intents.acls.len(), 3);
        let edge = &intents.acls["EDGE-IN"];
        assert_eq!(edge.family, AclFamily::Ipv4);
        assert_eq!(
            edge.rules.keys().copied().collect::<Vec<_>>(),
            vec![10, 20, 30, 40]
        );
        assert_eq!(edge.rules[&10].protocol, Some(6));
        assert_eq!(edge.rules[&10].source.as_deref(), Some("10.0.0.0/8"));
        assert_eq!(edge.rules[&10].destination_port, Some((443, 443)));
        assert_eq!(edge.rules[&20].destination_port, Some((67, 68)));
        assert!(edge.rules[&30].log && !edge.rules[&30].permit);
        assert_eq!(
            edge.rules[&40].police,
            Some(AclPolice {
                rate: 10_000_000,
                burst: 256_000,
                pps: false
            })
        );
        assert_eq!(intents.acls["MGMT6-IN"].family, AclFamily::Ipv6);
        let iot = &intents.acls["IOT-MAC"];
        assert_eq!(iot.family, AclFamily::Mac);
        assert_eq!(
            iot.rules[&10].source_mac,
            Some(("00:1c:73:00:00:00".into(), "ff:ff:ff:00:00:00".into()))
        );

        // CoPP overrides, dot1x, snooping/DAI.
        assert_eq!(intents.copp.len(), 2);
        assert_eq!(intents.copp["bpdu"].rate, Some(512));
        assert_eq!(intents.copp["arp"].burst, Some(500));
        let radius = &intents.dot1x.radius_servers;
        assert_eq!(radius.len(), 1);
        assert_eq!(radius[0].key.as_deref(), Some("s3cret"));
        assert_eq!(
            (radius[0].port, radius[0].timeout, radius[0].retransmit),
            (1812, 5, 3)
        );
        assert_eq!(intents.dot1x.reauth_interval, 3600);
        assert_eq!(
            intents
                .snoop_sec
                .dhcp_vlans
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![10, 20]
        );
        assert!(intents.snoop_sec.arp_vlans.contains(&10));

        // Per-port folds.
        assert_eq!(
            intents.ports["Ethernet1"].access_groups.ingress.as_deref(),
            Some("EDGE-IN")
        );
        let ps = intents.ports["Ethernet5"].port_security.unwrap();
        assert_eq!((ps.maximum, ps.shutdown), (4, true));
        assert!(intents.ports["Ethernet10"].dot1x);
        assert!(intents.lags[&1].dhcp_snooping_trust);
        assert!(intents.lags[&1].arp_inspection_trust);

        // The Port-Channel carries the snooped VLANs, so no trust
        // notes; the only warning is the memberless port-channel.
        assert_eq!(
            intents.warnings,
            vec!["Port-Channel1 has no member ports".to_string()]
        );
    }

    #[test]
    fn acl_validation() {
        // A rule needs permit or deny by commit.
        let tree = parse("security { acl { ipv4 A { rule 10 { protocol tcp } } } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadAclRule { .. })
        ));
        // L4 ports require tcp or udp.
        let tree =
            parse("security { acl { ipv4 A { rule 10 { permit\ndestination-port 443 } } } }")
                .unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadAclRule { .. })
        ));
        // Prefixes must be canonical, in the ACL's family.
        let tree =
            parse("security { acl { ipv4 A { rule 10 { permit\nsource 10.0.0.1/8 } } } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadAclRule { .. })
        ));
        let tree =
            parse("security { acl { ipv4 A { rule 10 { permit\nsource 2001:db8::/32 } } } }")
                .unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadAclRule { .. })
        ));
        // MAC fields don't ride IP families and vice versa.
        let tree = parse(
            "security { acl { ipv4 A { rule 10 { permit\nsource-mac 00:00:5e:00:00:01 } } } }",
        )
        .unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadAclRule { .. })
        ));
        let tree = parse("security { acl { mac A { rule 10 { permit\ndscp 10 } } } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadAclRule { .. })
        ));
        // A pps rate takes its burst in packets.
        let tree = parse(
            "security { acl { ipv4 A { rule 10 { permit\npolice rate 2000pps burst 256k } } } }",
        )
        .unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadAclRule { .. })
        ));
        // Names: letter first; unique across families.
        let tree = parse("security { acl { ipv4 9BAD { } } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadAcl { .. })));
        let tree = parse("security { acl { ipv4 DUP { } mac DUP { } } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadAcl { .. })));
        // Unknown security sub-block.
        let tree = parse("security { banana { } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadSecurity(_))));
    }

    #[test]
    fn security_cross_validation() {
        // A binding must name an existing ACL.
        let tree = parse("interfaces { Ethernet1 { access-group NOPE in } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadAccessGroup { .. })
        ));
        // A LAG member binds on the Port-Channel.
        let tree = parse(
            "security { acl { ipv4 A { } } }\ninterfaces { Port-Channel1 { }\nEthernet1 { channel-group 1 mode active\naccess-group A in } }",
        )
        .unwrap();
        assert_eq!(
            extract(&tree).unwrap_err().to_string(),
            "Ethernet1: member of Port-Channel1; bind on the Port-Channel"
        );
        // dot1x needs a keyed RADIUS server once a port enables it.
        let tree = parse("interfaces { Ethernet1 { dot1x } }").unwrap();
        assert_eq!(
            extract(&tree).unwrap_err().to_string(),
            "security dot1x: radius-server with key is required"
        );
        // dot1x and port-security are mutually exclusive on a port.
        let tree = parse(
            "security { dot1x { radius-server 10.0.0.1 { key x } } }\ninterfaces { Ethernet1 { dot1x\nport-security { } } }",
        )
        .unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadPortSecurity { .. })
        ));
        // Port-security refuses LAG members and Management.
        let tree = parse(
            "interfaces { Port-Channel1 { }\nEthernet1 { channel-group 1 mode active\nport-security { } } }",
        )
        .unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadPortSecurity { .. })
        ));
        let tree = parse("interfaces { Management1 { port-security { } } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadPortSecurity { .. })
        ));
        // DAI leans on DHCP snooping (or a covering static binding).
        let tree = parse("security { arp-inspection { vlan 10 } }").unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadArpInspection(_))
        ));
        let covered = parse(
            "vlans { vlan 10 { } }\nsecurity { arp-inspection { vlan 10 }\ndhcp-snooping { binding 00:50:56:be:ef:99 vlan 10 address 10.0.10.50 interface Ethernet1 } }\ninterfaces { Ethernet1 { switchport { mode access\naccess vlan 10 } } }",
        )
        .unwrap();
        assert!(extract(&covered).is_ok());
        // A static binding needs an interface carrying its VLAN.
        let tree = parse(
            "vlans { vlan 10 { } }\nsecurity { dhcp-snooping { vlan 10\nbinding 00:50:56:be:ef:99 vlan 10 address 10.0.10.50 interface Ethernet7 } }",
        )
        .unwrap();
        assert!(matches!(
            extract(&tree),
            Err(IntentError::BadDhcpSnooping(_))
        ));
        // Trust on an interface with no snooped VLAN is a note.
        let tree = parse(
            "vlans { vlan 10 { } }\nsecurity { dhcp-snooping { vlan 10 } }\ninterfaces { Ethernet1 { dhcp-snooping trust } }",
        )
        .unwrap();
        let intents = extract(&tree).unwrap();
        assert!(intents
            .warnings
            .iter()
            .any(|w| w.contains("trust has no effect")));
        // CoPP classes are the fixed set.
        let tree = parse("security { copp { class banana { rate 1 } } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadCopp(_))));
    }

    #[test]
    fn security_diffs_report_minimal_deltas() {
        let running = intents_of(security_seed());
        let mut candidate = intents_of(security_seed());

        // Nothing changed: no deltas.
        assert!(diff_acls(&running, &candidate).is_empty());
        assert!(diff_acl_bindings(&running, &candidate).is_empty());
        assert!(diff_copp(&running, &candidate).is_empty());
        assert!(diff_port_security(&running, &candidate).is_empty());
        assert!(diff_dot1x(&running, &candidate).is_none());
        assert!(diff_snoopsec(&running, &candidate).is_none());

        // Edit one rule: only that ACL re-ensures.
        candidate
            .acls
            .get_mut("EDGE-IN")
            .unwrap()
            .rules
            .get_mut(&20)
            .unwrap()
            .destination_port = Some((68, 69));
        let changes = diff_acls(&running, &candidate);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].name, "EDGE-IN");
        assert!(changes[0].ensure.is_some());

        // Drop a binding and add another: one unbind, one bind.
        candidate.ports.get_mut("Ethernet1").unwrap().access_groups = AccessGroups::default();
        candidate.lags.get_mut(&1).unwrap().access_groups.egress = Some("MGMT6-IN".into());
        let changes = diff_acl_bindings(&running, &candidate);
        assert_eq!(changes.len(), 2);
        assert!(changes
            .iter()
            .any(|c| c.target == "Ethernet1" && !c.egress && c.acl.is_none()));
        assert!(changes.iter().any(|c| c.target == "Port-Channel1"
            && c.egress
            && c.acl.as_deref() == Some("MGMT6-IN")));

        // A dropped CoPP override restores the compiled default.
        candidate.copp.remove("bpdu");
        let changes = diff_copp(&running, &candidate);
        assert_eq!(changes.len(), 1);
        assert!(changes[0].set.is_none());

        // Port-security and the whole-state families.
        candidate.ports.get_mut("Ethernet5").unwrap().port_security = None;
        let changes = diff_port_security(&running, &candidate);
        assert_eq!(changes.len(), 1);
        assert!(changes[0].set.is_none());
        candidate.dot1x.reauth_interval = 7200;
        assert!(diff_dot1x(&running, &candidate).is_some());
        candidate.snoop_sec.arp_vlans.clear();
        let snoopsec = diff_snoopsec(&running, &candidate).unwrap();
        assert!(snoopsec.intent.arp_vlans.is_empty());
        assert!(snoopsec.dhcp_trusted.contains("Port-Channel1"));
    }

    #[test]
    fn link_params_parse_on_the_kinds_that_have_a_phy() {
        let intents = intents_of(
            "interfaces { Ethernet1 { speed 1000
duplex full
mtu 9216 } }",
        );
        let eth = &intents.ports["Ethernet1"];
        assert_eq!(eth.speed_mbps, Some(1000));
        assert_eq!(eth.duplex, Some(Duplex::Full));
        assert_eq!(eth.mtu, Some(9216));

        // `auto` is the same intent as no leaf at all: nothing pinned.
        let intents = intents_of(
            "interfaces { Ethernet1 { speed auto
duplex auto } }",
        );
        assert_eq!(intents.ports["Ethernet1"].speed_mbps, None);
        assert_eq!(intents.ports["Ethernet1"].duplex, None);

        // Unit suffixes are accepted alongside bare megabits.
        let intents = intents_of("interfaces { Ethernet1 { speed 10G } }");
        assert_eq!(intents.ports["Ethernet1"].speed_mbps, Some(10_000));
    }

    #[test]
    fn mtu_lands_on_management_and_svis_but_pinning_does_not() {
        let intents = intents_of("interfaces { Management1 { mtu 1500 } }");
        assert_eq!(intents.management["Management1"].mtu, Some(1500));

        let intents = intents_of(
            "vlans { vlan 10 { } }
interfaces { Vlan10 { mtu 9216 } }",
        );
        assert_eq!(intents.svis["Vlan10"].mtu, Some(9216));

        // No PHY to negotiate with on an SVI, a port-channel, or the
        // management NIC.
        for text in [
            "interfaces { Vlan1 { speed 1000 } }",
            "interfaces { Vlan1 { duplex full } }",
            "interfaces { Management1 { speed 1000 } }",
            "interfaces { Port-Channel1 { duplex half } }",
            // A port-channel's MTU follows its members.
            "interfaces { Port-Channel1 { mtu 9216 } }",
        ] {
            assert!(
                extract(&parse(text).unwrap()).is_err(),
                "expected {text:?} to be rejected"
            );
        }
    }

    #[test]
    fn rejects_out_of_range_link_params() {
        for text in [
            "interfaces { Ethernet1 { mtu 67 } }",
            "interfaces { Ethernet1 { mtu 9217 } }",
            "interfaces { Ethernet1 { mtu jumbo } }",
            "interfaces { Ethernet1 { speed fast } }",
            "interfaces { Ethernet1 { speed 0 } }",
            "interfaces { Ethernet1 { duplex quarter } }",
        ] {
            assert!(
                extract(&parse(text).unwrap()).is_err(),
                "expected {text:?} to be rejected"
            );
        }
    }

    #[test]
    fn link_diffs_send_sentinels_when_a_pin_goes_away() {
        let running = intents_of(
            "interfaces { Ethernet1 { speed 100
duplex half
mtu 9216 } }",
        );
        let candidate = intents_of("interfaces { Ethernet1 { speed 1000 } }");
        assert_eq!(
            diff(&running.ports, &candidate.ports),
            vec![PortChange {
                name: "Ethernet1".into(),
                speed_mbps: Some(1000),
                // Both the duplex force and the MTU were dropped, so
                // syncd is told to stop forcing rather than left with
                // the old pin.
                duplex: Some("auto".into()),
                mtu: Some(0),
                ..Default::default()
            }]
        );

        // Deleting the interface reverts every pin.
        assert_eq!(
            diff(&running.ports, &BTreeMap::new()),
            vec![PortChange {
                name: "Ethernet1".into(),
                speed_mbps: Some(0),
                duplex: Some("auto".into()),
                mtu: Some(0),
                ..Default::default()
            }]
        );

        // No move, no delta.
        assert!(diff(&running.ports, &running.ports).is_empty());
    }

    #[test]
    fn netdev_mtu_reverts_to_the_kind_default() {
        let running = intents_of(
            "vlans { vlan 10 { } }
interfaces { Vlan10 { address 10.0.10.1/24
mtu 9216 } }",
        );
        let candidate = intents_of(
            "vlans { vlan 10 { } }
interfaces { Vlan10 { address 10.0.10.1/24 } }",
        );
        assert_eq!(
            diff_os(&running, &candidate).svis,
            vec![NetdevChange {
                name: "Vlan10".into(),
                set_mtu: Some(link::DEFAULT_MTU),
                ..Default::default()
            }]
        );

        // Front-panel ports revert to the KNET default instead.
        let running = intents_of("interfaces { Ethernet1 { mtu 9216 } }");
        assert_eq!(
            diff_os(&running, &intents_of("")).ports,
            vec![NetdevChange {
                name: "Ethernet1".into(),
                set_mtu: Some(link::DEFAULT_PORT_MTU),
                ..Default::default()
            }]
        );
    }
}
