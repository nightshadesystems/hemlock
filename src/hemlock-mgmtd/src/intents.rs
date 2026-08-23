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
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VlanIntent {
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SwitchportIntent {
    /// `mode trunk`; false = access.
    pub trunk: bool,
    /// None = default VLAN (1).
    pub access_vlan: Option<u16>,
    /// Allowed tagged VLANs in trunk mode.
    pub trunk_vlans: Vec<u16>,
    /// None = default VLAN (1).
    pub native_vlan: Option<u16>,
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
    let Some((_, items)) = tree.block("interfaces") else {
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
                let intent = InterfaceIntent {
                    admin_up,
                    description: ConfigTree::leaf_value(children, "description")
                        .map(str::to_string),
                    address,
                    switchport,
                };
                if intents.ports.insert(ifname.clone(), intent).is_some() {
                    return Err(IntentError::Duplicate { name: ifname });
                }
            }
            Kind::Management => {
                if ConfigTree::blocks_named(children, "switchport")
                    .next()
                    .is_some()
                {
                    return Err(IntentError::BadSwitchport {
                        name: ifname,
                        reason: "management ports are not switchports".into(),
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
    Ok(intents)
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
    let trunk = match ConfigTree::leaf_value(sp, "mode") {
        Some("trunk") => true,
        Some("access") | None => false,
        Some(other) => {
            return Err(bad(format!(
                "mode must be `access` or `trunk`, got {other:?}"
            )))
        }
    };
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
        for value in values {
            trunk_vlans.push(parse_vlan_id(value).map_err(&bad)?);
        }
        trunk_vlans.sort_unstable();
        trunk_vlans.dedup();
    }
    Ok(Some(SwitchportIntent {
        trunk,
        access_vlan: vlan_leaf("access", "vlan", "access-vlan")?,
        trunk_vlans,
        native_vlan: vlan_leaf("native", "vlan", "native-vlan")?,
    }))
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
        let intent = VlanIntent {
            description: ConfigTree::leaf_value(children, "description").map(str::to_string),
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
    /// Display name to ensure; None = remove the VLAN.
    pub ensure: Option<String>,
}

impl VlanChange {
    pub fn describe(&self) -> String {
        match &self.ensure {
            Some(name) if !name.is_empty() => format!("vlan {} ({name})", self.id),
            Some(_) => format!("vlan {}", self.id),
            None => format!("vlan {} removed", self.id),
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
                ensure: Some(wanted.description.clone().unwrap_or_default()),
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
            Some(sp) if sp.trunk => {
                let vlans = sp
                    .trunk_vlans
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    "{}: switchport trunk vlans {} native {}",
                    self.name,
                    if vlans.is_empty() { "-".into() } else { vlans },
                    sp.native_vlan.unwrap_or(1)
                )
            }
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
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
                address: None,
                switchport: None,
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
                address: None,
                switchport: None,
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
        assert!(sp1.trunk);
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
                trunk: true,
                access_vlan: None,
                trunk_vlans: vec![10, 20, 30],
                native_vlan: Some(40),
            })
        );
        assert_eq!(
            intents.ports["Ethernet2"].switchport,
            Some(SwitchportIntent {
                trunk: false,
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
                ensure: Some("Management".into())
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
