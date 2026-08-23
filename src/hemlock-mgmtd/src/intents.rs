//! Config intents: the typed slices of the config tree mgmtd knows how
//! to apply. ASIC ports (`interfaces { Ethernet1 { ... } }`) are pushed
//! to syncd; the OS-side families — management addressing (`interfaces
//! { Management1 { address ... } }`), static routes (`routing { static
//! { <prefix> <next-hop> } }`) and the SSH service (`system { ssh {
//! ... } }`) — go through the OS applier (`osapply`). The legacy
//! `ethernet <name>` keyed form is still accepted for configs persisted
//! before the format change.
//!
//! Each family stays a pure function from config tree to typed intent,
//! diffed against the running tree and pushed to the owning applier.

use std::collections::BTreeMap;

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
    /// Static routes: canonical prefix -> next hop.
    pub routes: BTreeMap<String, String>,
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
    /// Non-fatal commit notes (empty port-channels, MTU hints).
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SviIntent {
    /// Interface address in CIDR form; gives the VLAN a router
    /// interface (ASIC) and a kernel address on its bridge netdev.
    pub address: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MgmtIntent {
    /// None = leave the netdev alone.
    pub admin_up: Option<bool>,
    /// Primary address in CIDR form; puts the interface in L3 mode.
    pub address: Option<String>,
}

/// `system { ssh { ... } }` — SSH is on exactly when the block exists.
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
}

/// Extract every intent family from a config tree.
pub fn extract(tree: &ConfigTree) -> Result<Intents, IntentError> {
    let mut intents = Intents {
        ssh: ssh(tree)?,
        web: web(tree),
        routes: routes(tree)?,
        vlans: vlans(tree)?,
        ..Intents::default()
    };
    protocols(tree, &mut intents)?;
    switching(tree, &mut intents)?;
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
                };
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
                };
                if intents.lags.insert(group, intent).is_some() {
                    return Err(IntentError::Duplicate { name: ifname });
                }
            }
            Kind::Management => {
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
                let intent = MgmtIntent { admin_up, address };
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
                // An SVI needs its VLAN to exist (the default VLAN 1
                // always does).
                if address.is_some() && id != 1 && !intents.vlans.contains_key(&id) {
                    return Err(IntentError::BadInterfaceBlock(format!(
                        "{ifname}: VLAN {id} is not defined (set vlans vlan {id})"
                    )));
                }
                let intent = SviIntent { address };
                if intents.svis.insert(ifname.clone(), intent).is_some() {
                    return Err(IntentError::Duplicate { name: ifname });
                }
            }
        }
    }
    finish_validation(&mut intents)?;
    Ok(intents)
}

/// Cross-family semantic checks that need the whole tree extracted:
/// channel-group consistency, mirror destination rules, and the
/// non-fatal commit notes.
fn finish_validation(intents: &mut Intents) -> Result<(), IntentError> {
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

    Ok(())
}

/// Admin state of an interface: the `shutdown` / `no shutdown` marker
/// leaves, with the legacy `no-shutdown` and `admin-state
/// enabled|disabled` forms still accepted for configs persisted before
/// the format changes.
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

/// `system { http }` / `system { https }` — pure block presence.
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

fn routes(tree: &ConfigTree) -> Result<BTreeMap<String, String>, IntentError> {
    let mut routes = BTreeMap::new();
    let Some((_, routing)) = tree.block("routing") else {
        return Ok(routes);
    };
    for item in routing {
        let Item::Block {
            name,
            keys,
            children,
        } = item
        else {
            return Err(IntentError::BadRouting(format!(
                "unrecognized statement {:?}",
                item.name()
            )));
        };
        if name != "static" || !keys.is_empty() {
            return Err(IntentError::BadRouting(format!(
                "unrecognized block {name:?}"
            )));
        }
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
            let [next_hop] = values.as_slice() else {
                return Err(IntentError::BadRoute {
                    prefix: prefix.clone(),
                    reason: "expected exactly one next-hop".into(),
                });
            };
            let canonical =
                hemlock_common::net::validate_route(prefix, next_hop).map_err(|reason| {
                    IntentError::BadRoute {
                        prefix: prefix.clone(),
                        reason,
                    }
                })?;
            if canonical != *prefix {
                return Err(IntentError::BadRoute {
                    prefix: prefix.clone(),
                    reason: format!("host bits set (use {canonical})"),
                });
            }
            if routes.insert(canonical, next_hop.clone()).is_some() {
                return Err(IntentError::BadRoute {
                    prefix: prefix.clone(),
                    reason: "duplicate route".into(),
                });
            }
        }
    }
    Ok(routes)
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
            // `vlan 10 { ... }` — per-VLAN settings.
            Item::Block {
                name,
                keys,
                children,
            } if name == "vlan" => match keys.as_slice() {
                [key] => (key, children),
                _ => return Err(bad("vlan block needs exactly one id key".into())),
            },
            // `vlan 10` — the bare enabled form.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortChange {
    pub name: String,
    pub admin_up: Option<bool>,
    pub description: Option<String>,
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

        if admin_up.is_some() || description.is_some() {
            changes.push(PortChange {
                name: name.clone(),
                admin_up,
                description,
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
        if admin_up.is_some() || description.is_some() {
            changes.push(PortChange {
                name: name.clone(),
                admin_up,
                description,
            });
        }
    }

    changes
}

/// One kernel-netdev change for the OS applier (a management interface,
/// or the kernel side of a front-panel port's address).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetdevChange {
    pub name: String,
    pub admin_up: Option<bool>,
    pub set_address: Option<String>,
    /// Previous address to remove first — an address change is del +
    /// add, since `ip addr replace` only replaces an identical local
    /// address.
    pub del_address: Option<String>,
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
        format!("{}: {}", self.name, parts.join(", "))
    }
}

/// One static-route change for the OS applier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteChange {
    pub prefix: String,
    /// None = remove the route.
    pub next_hop: Option<String>,
}

impl RouteChange {
    pub fn describe(&self) -> String {
        match &self.next_hop {
            Some(next_hop) => format!("route {} via {next_hop}", self.prefix),
            None => format!("route {} removed", self.prefix),
        }
    }
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
    /// The full wanted SSH state, present exactly when it changed.
    pub ssh: Option<SshIntent>,
    /// The full wanted web console state, present exactly when it changed.
    pub web: Option<WebIntent>,
}

impl OsChanges {
    pub fn is_empty(&self) -> bool {
        self.management.is_empty()
            && self.ports.is_empty()
            && self.svis.is_empty()
            && self.routes.is_empty()
            && self.ssh.is_none()
            && self.web.is_none()
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
        self.ports
            .iter()
            .chain(&self.svis)
            .chain(&self.management)
            .map(NetdevChange::describe)
            .chain(self.routes.iter().map(RouteChange::describe))
            .chain(ssh)
            .chain(web)
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

        if admin_up.is_some() || set_address.is_some() || del_address.is_some() {
            changes.management.push(NetdevChange {
                name: name.clone(),
                admin_up,
                set_address,
                del_address,
            });
        }
    }
    for (name, had) in &running.management {
        if candidate.management.contains_key(name) {
            continue;
        }
        let admin_up = matches!(had.admin_up, Some(false)).then_some(true);
        let del_address = had.address.clone();
        if admin_up.is_some() || del_address.is_some() {
            changes.management.push(NetdevChange {
                name: name.clone(),
                admin_up,
                set_address: None,
                del_address,
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
        if set_address.is_some() || del_address.is_some() {
            changes.ports.push(NetdevChange {
                name: name.clone(),
                admin_up: None,
                set_address,
                del_address,
            });
        }
    }
    for (name, had) in &running.ports {
        if candidate.ports.contains_key(name) {
            continue;
        }
        if let Some(old) = &had.address {
            changes.ports.push(NetdevChange {
                name: name.clone(),
                admin_up: None,
                set_address: None,
                del_address: Some(old.clone()),
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
        if set_address.is_some() || del_address.is_some() {
            changes.svis.push(NetdevChange {
                name: name.clone(),
                admin_up: None,
                set_address,
                del_address,
            });
        }
    }
    for (name, had) in &running.svis {
        if candidate.svis.contains_key(name) {
            continue;
        }
        if let Some(old) = &had.address {
            changes.svis.push(NetdevChange {
                name: name.clone(),
                admin_up: None,
                set_address: None,
                del_address: Some(old.clone()),
            });
        }
    }

    for (prefix, next_hop) in &candidate.routes {
        if running.routes.get(prefix) != Some(next_hop) {
            changes.routes.push(RouteChange {
                prefix: prefix.clone(),
                next_hop: Some(next_hop.clone()),
            });
        }
    }
    for prefix in running.routes.keys() {
        if !candidate.routes.contains_key(prefix) {
            changes.routes.push(RouteChange {
                prefix: prefix.clone(),
                next_hop: None,
            });
        }
    }

    if running.ssh != candidate.ssh {
        changes.ssh = Some(candidate.ssh.clone());
    }
    if running.web != candidate.web {
        changes.web = Some(candidate.web.clone());
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
                address: Some("10.42.10.9/24".into())
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

    #[test]
    fn extracts_static_routes() {
        let intents = intents_of(
            "routing {\n    static {\n        0.0.0.0/0 10.42.10.1\n        10.99.0.0/16 10.42.10.2\n    }\n}\n",
        );
        assert_eq!(intents.routes.len(), 2);
        assert_eq!(intents.routes["0.0.0.0/0"], "10.42.10.1");
        assert_eq!(intents.routes["10.99.0.0/16"], "10.42.10.2");
    }

    #[test]
    fn rejects_bad_routes() {
        // Host bits set in the prefix.
        let tree = parse("routing { static { 10.42.10.9/24 10.42.10.1 } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadRoute { .. })));
        // Next hop family mismatch.
        let tree = parse("routing { static { 0.0.0.0/0 2001:db8::1 } }").unwrap();
        assert!(matches!(extract(&tree), Err(IntentError::BadRoute { .. })));
        // Unknown routing sub-block.
        let tree = parse("routing { ospf { } }").unwrap();
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
            }]
        );
        assert_eq!(
            changes.routes,
            vec![RouteChange {
                prefix: "0.0.0.0/0".into(),
                next_hop: Some("10.42.10.1".into()),
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
            }]
        );
        assert_eq!(
            back.routes,
            vec![RouteChange {
                prefix: "0.0.0.0/0".into(),
                next_hop: None,
            }]
        );
        assert_eq!(back.ssh, Some(SshIntent::default()));
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
            }]
        );
    }
}
