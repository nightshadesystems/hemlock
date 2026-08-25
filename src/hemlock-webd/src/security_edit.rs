//! Config edits for the security-suite pages: ACLs and their port
//! bindings, CoPP, port security, 802.1X, and DHCP snooping / ARP
//! inspection.
//!
//! Same discipline as `switching_edit.rs`: the builders write exactly
//! the leaves and phrases hemlockctl writes, based on the running
//! config, and the result goes through mgmtd's normal SetCandidate +
//! Commit path — so validation (including every security-suite
//! `IntentError`), the rollback ring, and `show configuration` behave
//! as if the change came from the CLI. Every builder validates its
//! whole edit before touching the tree.

use hemlock_config::{ConfigTree, Item};
use serde::Deserialize;

const ACL_FAMILIES: &[&str] = &["ipv4", "ipv6", "mac"];

const COPP_CLASSES: &[&str] = &[
    "bpdu", "lacp", "lldp", "eapol", "igmp", "mld", "arp", "dhcp", "ospf", "bgp", "vrrp", "ip2me",
    "acl-log", "default",
];

fn valid_vlan(id: u16) -> Result<(), String> {
    if (1..=4094).contains(&id) {
        Ok(())
    } else {
        Err(format!("bad VLAN id {id} (1..4094)"))
    }
}

/// ACL name syntax (letter first, then letters/digits/_/-, max 32) —
/// mirror of the CLI's prompt check; mgmtd re-validates.
fn valid_acl_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "bad ACL name {name:?} (letter first, then letters/digits/_/-, max 32)"
        ))
    }
}

fn valid_direction(direction: &str) -> Result<(), String> {
    if matches!(direction, "in" | "out") {
        Ok(())
    } else {
        Err(format!(
            "direction must be \"in\" or \"out\" (got {direction:?})"
        ))
    }
}

fn valid_binding_port(name: &str) -> Result<(), String> {
    if name.starts_with("Ethernet") || name.starts_with("Port-Channel") {
        Ok(())
    } else {
        Err(format!("{name}: not a bindable port"))
    }
}

fn block_children_mut<'a>(items: &'a mut [Item], name: &str) -> Option<&'a mut Vec<Item>> {
    items.iter_mut().find_map(|item| match item {
        Item::Block {
            name: n, children, ..
        } if n == name => Some(children),
        _ => None,
    })
}

fn remove_block_if_empty(tree: &mut ConfigTree, name: &str) {
    if tree
        .block(name)
        .is_some_and(|(_, children)| children.is_empty())
    {
        ConfigTree::remove_block(&mut tree.items, name, &[]);
    }
}

fn push_leaf(items: &mut Vec<Item>, name: &str, values: Vec<String>) {
    items.push(Item::Leaf {
        name: name.to_string(),
        values,
    });
}

fn leaf(name: &str, values: Vec<String>) -> Item {
    Item::Leaf {
        name: name.to_string(),
        values,
    }
}

/// Remove an emptied `security { <sub> { ... } }` chain bottom-up.
fn prune_security(tree: &mut ConfigTree) {
    let security = tree.block_mut("security");
    security.retain(|item| !matches!(item, Item::Block { children, .. } if children.is_empty()));
    remove_block_if_empty(tree, "security");
}

/// `Some("")` counts as absent everywhere: the pages send empty inputs
/// for fields the operator left blank.
fn nonempty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|s| !s.is_empty())
}

// ------------------------------------------------------------------ ACLs

#[derive(Debug, Default, Deserialize)]
pub struct RuleSet {
    pub number: u32,
    /// "permit" | "deny".
    pub action: String,
    /// tcp|udp|icmp|<0-255> (ipv4/ipv6 only).
    #[serde(default)]
    pub protocol: Option<String>,
    /// Canonical prefix; empty/"any" = any.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub destination: Option<String>,
    /// "443" or "67-68".
    #[serde(default)]
    pub source_port: Option<String>,
    #[serde(default)]
    pub destination_port: Option<String>,
    #[serde(default)]
    pub dscp: Option<String>,
    #[serde(default)]
    pub log: bool,
    /// Suffixed forms as the CLI takes them ("10m", "2000pps").
    #[serde(default)]
    pub police_rate: Option<String>,
    /// "256k" or "64pkts".
    #[serde(default)]
    pub police_burst: Option<String>,
    /// mac family only: <mac>[/<mask>].
    #[serde(default)]
    pub source_mac: Option<String>,
    #[serde(default)]
    pub destination_mac: Option<String>,
    /// 0xHHHH | ipv4 | ipv6 | arp (mac family only).
    #[serde(default)]
    pub ethertype: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AclSet {
    /// "ipv4" | "ipv6" | "mac".
    pub family: String,
    pub name: String,
    /// Replaces the ACL's rule set wholesale.
    #[serde(default)]
    pub rules: Vec<RuleSet>,
}

#[derive(Debug, Deserialize)]
pub struct AclDelete {
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct AclEdit {
    #[serde(default)]
    pub set: Vec<AclSet>,
    #[serde(default)]
    pub delete: Vec<AclDelete>,
}

/// Validate + canonicalize one rule into its config-tree leaves, in the
/// order the CLI writes them.
fn build_rule(family: &str, rule: &RuleSet) -> Result<(String, Vec<Item>), String> {
    let context = |message: String| format!("rule {}: {message}", rule.number);
    if rule.number == 0 {
        return Err(format!("bad rule number {} (1..4294967295)", rule.number));
    }
    if !matches!(rule.action.as_str(), "permit" | "deny") {
        return Err(context(format!(
            "action must be \"permit\" or \"deny\" (got {:?})",
            rule.action
        )));
    }
    let mac_family = family == "mac";
    if mac_family {
        for (field, present) in [
            ("protocol", rule.protocol.is_some()),
            ("source", nonempty(&rule.source).is_some()),
            ("destination", nonempty(&rule.destination).is_some()),
            ("source-port", nonempty(&rule.source_port).is_some()),
            (
                "destination-port",
                nonempty(&rule.destination_port).is_some(),
            ),
            ("dscp", nonempty(&rule.dscp).is_some()),
        ] {
            if present {
                return Err(context(format!("{field} does not apply to a mac ACL")));
            }
        }
    } else {
        for (field, present) in [
            ("source-mac", nonempty(&rule.source_mac).is_some()),
            ("destination-mac", nonempty(&rule.destination_mac).is_some()),
            ("ethertype", nonempty(&rule.ethertype).is_some()),
        ] {
            if present {
                return Err(context(format!("{field} only applies to a mac ACL")));
            }
        }
    }

    let mut items = vec![leaf(&rule.action, vec![])];
    if let Some(protocol) = nonempty(&rule.protocol) {
        let canonical = match protocol {
            "tcp" | "udp" | "icmp" => protocol.to_string(),
            other => other
                .parse::<u8>()
                .map_err(|_| context(format!("bad protocol {other:?} (tcp|udp|icmp|0-255)")))?
                .to_string(),
        };
        items.push(leaf("protocol", vec![canonical]));
    }
    for (slot, value) in [("source", &rule.source), ("destination", &rule.destination)] {
        let Some(value) = nonempty(value) else {
            continue;
        };
        if value == "any" {
            continue;
        }
        let canonical = hemlock_common::net::require_canonical_prefix(value).map_err(&context)?;
        if canonical.contains(':') != (family == "ipv6") {
            return Err(context(format!(
                "{canonical} does not match the ACL family ({family})"
            )));
        }
        items.push(leaf(slot, vec![canonical]));
    }
    for (slot, value) in [
        ("source-port", &rule.source_port),
        ("destination-port", &rule.destination_port),
    ] {
        let Some(value) = nonempty(value) else {
            continue;
        };
        hemlock_common::net::parse_port_match(value).map_err(&context)?;
        items.push(leaf(slot, vec![value.to_string()]));
    }
    if let Some(dscp) = nonempty(&rule.dscp) {
        let dscp = dscp
            .parse::<u8>()
            .ok()
            .filter(|d| *d <= 63)
            .ok_or_else(|| context(format!("bad dscp {dscp:?} (0-63)")))?;
        items.push(leaf("dscp", vec![dscp.to_string()]));
    }
    if rule.log {
        items.push(leaf("log", vec![]));
    }
    match (nonempty(&rule.police_rate), nonempty(&rule.police_burst)) {
        (Some(rate), Some(burst)) => {
            let (_, pps) = hemlock_common::net::parse_police_rate(rate).map_err(&context)?;
            let (_, burst_pkts) =
                hemlock_common::net::parse_police_burst(burst).map_err(&context)?;
            let scaled = burst.to_ascii_lowercase().ends_with(['k', 'm', 'g']);
            if pps && scaled {
                return Err(context("a pps rate takes its burst in packets".into()));
            }
            if !pps && burst_pkts {
                return Err(context("a bps rate takes its burst in bytes".into()));
            }
            items.push(leaf(
                "police",
                vec![
                    "rate".into(),
                    rate.to_string(),
                    "burst".into(),
                    burst.to_string(),
                ],
            ));
        }
        (None, None) => {}
        _ => return Err(context("police wants both rate and burst".into())),
    }
    for (slot, value) in [
        ("source-mac", &rule.source_mac),
        ("destination-mac", &rule.destination_mac),
    ] {
        let Some(value) = nonempty(value) else {
            continue;
        };
        let canonical = match value.split_once('/') {
            Some((mac, mask)) => format!(
                "{}/{}",
                hemlock_common::net::parse_mac(mac).map_err(&context)?,
                hemlock_common::net::parse_mac_mask(mask).map_err(&context)?
            ),
            None => hemlock_common::net::parse_mac(value).map_err(&context)?,
        };
        items.push(leaf(slot, vec![canonical]));
    }
    if let Some(ethertype) = nonempty(&rule.ethertype) {
        let canonical = match ethertype {
            "ipv4" | "ipv6" | "arp" => ethertype.to_string(),
            hex => {
                hex.strip_prefix("0x")
                    .and_then(|h| u16::from_str_radix(h, 16).ok())
                    .ok_or_else(|| {
                        context(format!(
                            "bad ethertype {hex:?} (0x0000-0xffff|ipv4|ipv6|arp)"
                        ))
                    })?;
                hex.to_string()
            }
        };
        items.push(leaf("ethertype", vec![canonical]));
    }
    Ok((rule.number.to_string(), items))
}

pub fn apply_acl_edit(tree: &mut ConfigTree, edit: &AclEdit) -> Result<(), String> {
    if edit.set.is_empty() && edit.delete.is_empty() {
        return Err("nothing to change".into());
    }
    // Validate + canonicalize everything up front; the tree is only
    // touched once the whole edit is known-good.
    #[allow(clippy::type_complexity)]
    let mut prepared: Vec<(&AclSet, Vec<(String, Vec<Item>)>)> = Vec::new();
    for set in &edit.set {
        if !ACL_FAMILIES.contains(&set.family.as_str()) {
            return Err(format!("bad ACL family {:?} (ipv4|ipv6|mac)", set.family));
        }
        valid_acl_name(&set.name)?;
        let mut rules = Vec::new();
        for rule in &set.rules {
            let built = build_rule(&set.family, rule).map_err(|e| format!("{}: {e}", set.name))?;
            if rules.iter().any(|(number, _)| *number == built.0) {
                return Err(format!("{}: duplicate rule number {}", set.name, built.0));
            }
            rules.push(built);
        }
        rules.sort_by_key(|(number, _)| number.parse::<u32>().unwrap_or(0));
        prepared.push((set, rules));
    }
    for delete in &edit.delete {
        valid_acl_name(&delete.name)?;
    }

    for (set, rules) in prepared {
        let security = tree.block_mut("security");
        let acl = ConfigTree::ensure_block(security, "acl", &[]);
        // The name is switch-wide: a family change moves the ACL.
        for family in ACL_FAMILIES {
            if *family != set.family {
                ConfigTree::remove_block(acl, family, &[&set.name]);
            }
        }
        let block = ConfigTree::ensure_block(acl, &set.family, &[&set.name]);
        block.retain(|item| !matches!(item, Item::Block { name, .. } if name == "rule"));
        for (number, items) in rules {
            let rule = ConfigTree::ensure_block(block, "rule", &[&number]);
            *rule = items;
        }
    }
    for delete in &edit.delete {
        if let Some(security) = block_children_mut(&mut tree.items, "security") {
            if let Some(acl) = block_children_mut(security, "acl") {
                for family in ACL_FAMILIES {
                    ConfigTree::remove_block(acl, family, &[&delete.name]);
                }
                if acl.is_empty() {
                    ConfigTree::remove_block(security, "acl", &[]);
                }
            }
        }
    }
    remove_block_if_empty(tree, "security");
    Ok(())
}

// --------------------------------------------------------- ACL bindings

#[derive(Debug, Deserialize)]
pub struct AclBindingSet {
    pub interface: String,
    pub acl: String,
    /// "in" | "out".
    pub direction: String,
}

#[derive(Debug, Deserialize)]
pub struct AclBindingDelete {
    pub interface: String,
    /// "in" | "out".
    pub direction: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct AclBindingEdit {
    #[serde(default)]
    pub set: Vec<AclBindingSet>,
    #[serde(default)]
    pub delete: Vec<AclBindingDelete>,
}

fn remove_binding_leaf(eth: &mut Vec<Item>, direction: &str) {
    eth.retain(|item| {
        !matches!(item, Item::Leaf { name, values }
            if name == "access-group"
                && values.get(1).map(String::as_str) == Some(direction))
    });
}

pub fn apply_acl_binding_edit(tree: &mut ConfigTree, edit: &AclBindingEdit) -> Result<(), String> {
    if edit.set.is_empty() && edit.delete.is_empty() {
        return Err("nothing to change".into());
    }
    for set in &edit.set {
        valid_binding_port(&set.interface)?;
        valid_acl_name(&set.acl)?;
        valid_direction(&set.direction)?;
    }
    for delete in &edit.delete {
        valid_binding_port(&delete.interface)?;
        valid_direction(&delete.direction)?;
    }

    for set in &edit.set {
        let interfaces = tree.block_mut("interfaces");
        let eth = ConfigTree::ensure_block(interfaces, &set.interface, &[]);
        // One binding per direction: replace any previous one.
        remove_binding_leaf(eth, &set.direction);
        push_leaf(
            eth,
            "access-group",
            vec![set.acl.clone(), set.direction.clone()],
        );
    }
    for delete in &edit.delete {
        let interfaces = tree.block_mut("interfaces");
        if let Some(eth) = block_children_mut(interfaces, &delete.interface) {
            remove_binding_leaf(eth, &delete.direction);
        }
    }
    remove_block_if_empty(tree, "interfaces");
    Ok(())
}

// ------------------------------------------------------------------ CoPP

#[derive(Debug, Deserialize)]
pub struct CoppSet {
    pub class: String,
    /// Present values are written; absent ones revert to the compiled
    /// default (the leaf is removed).
    #[serde(default)]
    pub rate: Option<u32>,
    #[serde(default)]
    pub burst: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
pub struct CoppEdit {
    #[serde(default)]
    pub set: Vec<CoppSet>,
    /// Class names whose override blocks are removed entirely.
    #[serde(default)]
    pub delete: Vec<String>,
}

pub fn apply_copp_edit(tree: &mut ConfigTree, edit: &CoppEdit) -> Result<(), String> {
    if edit.set.is_empty() && edit.delete.is_empty() {
        return Err("nothing to change".into());
    }
    for set in &edit.set {
        if !COPP_CLASSES.contains(&set.class.as_str()) {
            return Err(format!("unknown CoPP class {:?}", set.class));
        }
        if let Some(rate) = set.rate {
            if !(1..=10_000_000).contains(&rate) {
                return Err(format!("bad rate {rate} (1..10000000)"));
            }
        }
        if let Some(burst) = set.burst {
            if !(1..=1_000_000).contains(&burst) {
                return Err(format!("bad burst {burst} (1..1000000)"));
            }
        }
    }
    for class in &edit.delete {
        if !COPP_CLASSES.contains(&class.as_str()) {
            return Err(format!("unknown CoPP class {class:?}"));
        }
    }

    for set in &edit.set {
        let security = tree.block_mut("security");
        let copp = ConfigTree::ensure_block(security, "copp", &[]);
        let block = ConfigTree::ensure_block(copp, "class", &[&set.class]);
        match set.rate {
            Some(rate) => ConfigTree::set_leaf(block, "rate", vec![rate.to_string()]),
            None => ConfigTree::remove_leaf(block, "rate"),
        }
        match set.burst {
            Some(burst) => ConfigTree::set_leaf(block, "burst", vec![burst.to_string()]),
            None => ConfigTree::remove_leaf(block, "burst"),
        }
    }
    for class in &edit.delete {
        if let Some(security) = block_children_mut(&mut tree.items, "security") {
            if let Some(copp) = block_children_mut(security, "copp") {
                ConfigTree::remove_block(copp, "class", &[class]);
                if copp.is_empty() {
                    ConfigTree::remove_block(security, "copp", &[]);
                }
            }
        }
    }
    remove_block_if_empty(tree, "security");
    Ok(())
}

// --------------------------------------------------------- port security

#[derive(Debug, Deserialize)]
pub struct PortSecuritySet {
    pub interface: String,
    /// Present values are written; absent ones revert to the defaults
    /// (maximum 1, protect) by removing the leaf.
    #[serde(default)]
    pub maximum: Option<u32>,
    /// "protect" | "shutdown".
    #[serde(default)]
    pub violation: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PortSecurityEdit {
    #[serde(default)]
    pub set: Vec<PortSecuritySet>,
    /// Ports whose `port-security` blocks are removed (disable).
    #[serde(default)]
    pub delete: Vec<String>,
}

pub fn apply_port_security_edit(
    tree: &mut ConfigTree,
    edit: &PortSecurityEdit,
) -> Result<(), String> {
    if edit.set.is_empty() && edit.delete.is_empty() {
        return Err("nothing to change".into());
    }
    for set in &edit.set {
        if !set.interface.starts_with("Ethernet") {
            return Err(format!("{}: not a port-security port", set.interface));
        }
        if let Some(maximum) = set.maximum {
            if !(1..=1024).contains(&maximum) {
                return Err(format!("bad maximum {maximum} (1..1024)"));
            }
        }
        if let Some(violation) = &set.violation {
            if !matches!(violation.as_str(), "protect" | "shutdown") {
                return Err(format!(
                    "bad violation action {violation:?} (protect|shutdown)"
                ));
            }
        }
    }
    for port in &edit.delete {
        if !port.starts_with("Ethernet") {
            return Err(format!("{port}: not a port-security port"));
        }
    }

    for set in &edit.set {
        let interfaces = tree.block_mut("interfaces");
        let eth = ConfigTree::ensure_block(interfaces, &set.interface, &[]);
        let ps = ConfigTree::ensure_block(eth, "port-security", &[]);
        match set.maximum {
            Some(maximum) => ConfigTree::set_leaf(ps, "maximum", vec![maximum.to_string()]),
            None => ConfigTree::remove_leaf(ps, "maximum"),
        }
        match &set.violation {
            Some(violation) => ConfigTree::set_leaf(ps, "violation", vec![violation.clone()]),
            None => ConfigTree::remove_leaf(ps, "violation"),
        }
    }
    for port in &edit.delete {
        let interfaces = tree.block_mut("interfaces");
        if let Some(eth) = block_children_mut(interfaces, port) {
            ConfigTree::remove_block(eth, "port-security", &[]);
        }
    }
    remove_block_if_empty(tree, "interfaces");
    Ok(())
}

// ---------------------------------------------------------------- 802.1X

#[derive(Debug, Deserialize)]
pub struct RadiusServerSet {
    pub ip: String,
    /// Write-only: absent (or empty) keeps any existing `key` leaf.
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub port: Option<u32>,
    #[serde(default)]
    pub timeout: Option<u32>,
    #[serde(default)]
    pub retransmit: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Dot1xEdit {
    #[serde(default)]
    pub servers_set: Vec<RadiusServerSet>,
    #[serde(default)]
    pub servers_delete: Vec<String>,
    /// 0 = reauthentication off; otherwise 60..86400.
    #[serde(default)]
    pub reauth_interval: Option<u32>,
    /// Revert to the default (no `reauth-interval` leaf).
    #[serde(default)]
    pub clear_reauth: bool,
    /// Physical ports gaining / losing the `dot1x` marker.
    #[serde(default)]
    pub ports_enable: Vec<String>,
    #[serde(default)]
    pub ports_disable: Vec<String>,
}

pub fn apply_dot1x_edit(tree: &mut ConfigTree, edit: &Dot1xEdit) -> Result<(), String> {
    if edit.servers_set.is_empty()
        && edit.servers_delete.is_empty()
        && edit.reauth_interval.is_none()
        && !edit.clear_reauth
        && edit.ports_enable.is_empty()
        && edit.ports_disable.is_empty()
    {
        return Err("nothing to change".into());
    }
    let mut servers = Vec::new();
    for set in &edit.servers_set {
        let ip: std::net::IpAddr = set
            .ip
            .parse()
            .map_err(|_| format!("bad radius-server address {:?}", set.ip))?;
        if let Some(port) = set.port {
            if !(1..=65535).contains(&port) {
                return Err(format!("bad port {port} (1..65535)"));
            }
        }
        if let Some(timeout) = set.timeout {
            if !(1..=60).contains(&timeout) {
                return Err(format!("bad timeout {timeout} (1..60)"));
            }
        }
        if let Some(retransmit) = set.retransmit {
            if retransmit > 10 {
                return Err(format!("bad retransmit {retransmit} (0..10)"));
            }
        }
        servers.push((ip.to_string(), set));
    }
    let mut deletes = Vec::new();
    for ip in &edit.servers_delete {
        let ip: std::net::IpAddr = ip
            .parse()
            .map_err(|_| format!("bad radius-server address {ip:?}"))?;
        deletes.push(ip.to_string());
    }
    if let Some(secs) = edit.reauth_interval {
        if secs != 0 && !(60..=86400).contains(&secs) {
            return Err(format!("bad reauth-interval {secs} (0|60-86400)"));
        }
    }
    for port in edit.ports_enable.iter().chain(edit.ports_disable.iter()) {
        if !port.starts_with("Ethernet") {
            return Err(format!("{port}: not an 802.1X-capable port"));
        }
    }

    if !servers.is_empty() || edit.reauth_interval.is_some() {
        let security = tree.block_mut("security");
        let dot1x = ConfigTree::ensure_block(security, "dot1x", &[]);
        for (ip, set) in &servers {
            let server = ConfigTree::ensure_block(dot1x, "radius-server", &[ip]);
            if let Some(key) = nonempty(&set.key) {
                ConfigTree::set_leaf(server, "key", vec![key.to_string()]);
            }
            match set.port {
                Some(port) => ConfigTree::set_leaf(server, "port", vec![port.to_string()]),
                None => ConfigTree::remove_leaf(server, "port"),
            }
            match set.timeout {
                Some(timeout) => ConfigTree::set_leaf(server, "timeout", vec![timeout.to_string()]),
                None => ConfigTree::remove_leaf(server, "timeout"),
            }
            match set.retransmit {
                Some(retransmit) => {
                    ConfigTree::set_leaf(server, "retransmit", vec![retransmit.to_string()])
                }
                None => ConfigTree::remove_leaf(server, "retransmit"),
            }
        }
        if let Some(secs) = edit.reauth_interval {
            ConfigTree::set_leaf(dot1x, "reauth-interval", vec![secs.to_string()]);
        }
    }
    if !deletes.is_empty() || edit.clear_reauth {
        if let Some(security) = block_children_mut(&mut tree.items, "security") {
            if let Some(dot1x) = block_children_mut(security, "dot1x") {
                for ip in &deletes {
                    ConfigTree::remove_block(dot1x, "radius-server", &[ip]);
                }
                if edit.clear_reauth {
                    ConfigTree::remove_leaf(dot1x, "reauth-interval");
                }
            }
        }
        prune_security(tree);
    }
    for port in &edit.ports_enable {
        let interfaces = tree.block_mut("interfaces");
        let eth = ConfigTree::ensure_block(interfaces, port, &[]);
        ConfigTree::set_leaf(eth, "dot1x", vec![]);
    }
    for port in &edit.ports_disable {
        let interfaces = tree.block_mut("interfaces");
        if let Some(eth) = block_children_mut(interfaces, port) {
            ConfigTree::remove_leaf(eth, "dot1x");
        }
    }
    remove_block_if_empty(tree, "interfaces");
    Ok(())
}

// ------------------------------------- DHCP snooping + ARP inspection

#[derive(Debug, Deserialize)]
pub struct TrustSet {
    pub interface: String,
    /// "dhcp-snooping" | "arp-inspection".
    pub feature: String,
    pub trusted: bool,
}

#[derive(Debug, Deserialize)]
pub struct BindingSet {
    pub mac: String,
    pub vlan: u16,
    /// IPv4.
    pub address: String,
    pub interface: String,
}

#[derive(Debug, Deserialize)]
pub struct BindingKey {
    pub mac: String,
    pub vlan: u16,
}

#[derive(Debug, Default, Deserialize)]
pub struct SnoopingSecEdit {
    /// When present, replaces the `dhcp-snooping` VLAN list wholesale.
    #[serde(default)]
    pub dhcp_vlans: Option<Vec<u16>>,
    /// When present, replaces the `arp-inspection` VLAN list wholesale.
    #[serde(default)]
    pub arp_vlans: Option<Vec<u16>>,
    /// When present, replaces the DAI validate set (src-mac|dst-mac|ip).
    #[serde(default)]
    pub validate: Option<Vec<String>>,
    #[serde(default)]
    pub trust_set: Vec<TrustSet>,
    #[serde(default)]
    pub bindings_set: Vec<BindingSet>,
    #[serde(default)]
    pub bindings_delete: Vec<BindingKey>,
}

pub fn apply_snooping_sec_edit(
    tree: &mut ConfigTree,
    edit: &SnoopingSecEdit,
) -> Result<(), String> {
    if edit.dhcp_vlans.is_none()
        && edit.arp_vlans.is_none()
        && edit.validate.is_none()
        && edit.trust_set.is_empty()
        && edit.bindings_set.is_empty()
        && edit.bindings_delete.is_empty()
    {
        return Err("nothing to change".into());
    }
    for vlan in edit
        .dhcp_vlans
        .iter()
        .flatten()
        .chain(edit.arp_vlans.iter().flatten())
    {
        valid_vlan(*vlan)?;
    }
    if let Some(validate) = &edit.validate {
        for check in validate {
            if !matches!(check.as_str(), "src-mac" | "dst-mac" | "ip") {
                return Err(format!("bad validate check {check:?} (src-mac|dst-mac|ip)"));
            }
        }
    }
    for trust in &edit.trust_set {
        valid_binding_port(&trust.interface)?;
        if !matches!(trust.feature.as_str(), "dhcp-snooping" | "arp-inspection") {
            return Err(format!(
                "bad trust feature {:?} (dhcp-snooping|arp-inspection)",
                trust.feature
            ));
        }
    }
    let mut binding_sets = Vec::new();
    for set in &edit.bindings_set {
        valid_vlan(set.vlan)?;
        valid_binding_port(&set.interface)?;
        let mac = hemlock_common::net::parse_unicast_mac(&set.mac)?;
        let ip: std::net::Ipv4Addr = set
            .address
            .parse()
            .map_err(|_| format!("bad binding address {:?} (IPv4)", set.address))?;
        binding_sets.push((mac, ip.to_string(), set));
    }
    let mut binding_deletes = Vec::new();
    for key in &edit.bindings_delete {
        valid_vlan(key.vlan)?;
        binding_deletes.push((hemlock_common::net::parse_mac(&key.mac)?, key.vlan));
    }

    let sorted_vlans = |vlans: &[u16]| {
        let mut sorted = vlans.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        sorted
    };
    if let Some(vlans) = &edit.dhcp_vlans {
        let security = tree.block_mut("security");
        let block = ConfigTree::ensure_block(security, "dhcp-snooping", &[]);
        ConfigTree::remove_leaf(block, "vlan");
        for vlan in sorted_vlans(vlans) {
            push_leaf(block, "vlan", vec![vlan.to_string()]);
        }
    }
    if edit.arp_vlans.is_some() || edit.validate.is_some() {
        let security = tree.block_mut("security");
        let block = ConfigTree::ensure_block(security, "arp-inspection", &[]);
        if let Some(vlans) = &edit.arp_vlans {
            ConfigTree::remove_leaf(block, "vlan");
            for vlan in sorted_vlans(vlans) {
                push_leaf(block, "vlan", vec![vlan.to_string()]);
            }
        }
        if let Some(validate) = &edit.validate {
            ConfigTree::remove_leaf(block, "validate");
            for check in validate {
                push_leaf(block, "validate", vec![check.clone()]);
            }
        }
    }
    for (mac, ip, set) in &binding_sets {
        let security = tree.block_mut("security");
        let block = ConfigTree::ensure_block(security, "dhcp-snooping", &[]);
        let vlan = set.vlan.to_string();
        // One binding per (mac, vlan): replace it.
        block.retain(|item| {
            !matches!(item, Item::Leaf { name, values }
                if name == "binding"
                    && values.first() == Some(mac)
                    && values.get(2) == Some(&vlan))
        });
        push_leaf(
            block,
            "binding",
            vec![
                mac.clone(),
                "vlan".into(),
                vlan,
                "address".into(),
                ip.clone(),
                "interface".into(),
                set.interface.clone(),
            ],
        );
    }
    for (mac, vlan) in &binding_deletes {
        if let Some(security) = block_children_mut(&mut tree.items, "security") {
            if let Some(block) = block_children_mut(security, "dhcp-snooping") {
                let vlan = vlan.to_string();
                block.retain(|item| {
                    !matches!(item, Item::Leaf { name, values }
                        if name == "binding"
                            && values.first() == Some(mac)
                            && values.get(2) == Some(&vlan))
                });
            }
        }
    }
    for trust in &edit.trust_set {
        let interfaces = tree.block_mut("interfaces");
        if trust.trusted {
            let eth = ConfigTree::ensure_block(interfaces, &trust.interface, &[]);
            ConfigTree::set_phrase(eth, &trust.feature, "trust", vec![]);
        } else if let Some(eth) = block_children_mut(interfaces, &trust.interface) {
            ConfigTree::remove_leaf(eth, &trust.feature);
        }
    }
    if tree.block("security").is_some() {
        prune_security(tree);
    }
    remove_block_if_empty(tree, "interfaces");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tree(text: &str) -> ConfigTree {
        let mut tree = hemlock_config::parse(text).unwrap();
        tree.normalize_interfaces();
        tree
    }

    #[test]
    fn acl_edit_writes_and_deletes() {
        let mut t = tree("");
        apply_acl_edit(
            &mut t,
            &AclEdit {
                set: vec![AclSet {
                    family: "ipv4".into(),
                    name: "EDGE-IN".into(),
                    rules: vec![
                        RuleSet {
                            number: 20,
                            action: "deny".into(),
                            log: true,
                            ..RuleSet::default()
                        },
                        RuleSet {
                            number: 10,
                            action: "permit".into(),
                            protocol: Some("tcp".into()),
                            source: Some("10.0.0.0/8".into()),
                            destination_port: Some("443".into()),
                            police_rate: Some("10m".into()),
                            police_burst: Some("256k".into()),
                            ..RuleSet::default()
                        },
                    ],
                }],
                delete: vec![],
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("ipv4 EDGE-IN"));
        assert!(text.contains("rule 10"));
        assert!(text.contains("protocol tcp"));
        assert!(text.contains("source 10.0.0.0/8"));
        assert!(text.contains("destination-port 443"));
        assert!(text.contains("police rate 10m burst 256k"));
        assert!(text.contains("rule 20"));
        // Round-trips.
        assert_eq!(hemlock_config::parse(&text).unwrap(), t);

        // Wholesale replace drops rule 20.
        apply_acl_edit(
            &mut t,
            &AclEdit {
                set: vec![AclSet {
                    family: "ipv4".into(),
                    name: "EDGE-IN".into(),
                    rules: vec![RuleSet {
                        number: 10,
                        action: "permit".into(),
                        ..RuleSet::default()
                    }],
                }],
                delete: vec![],
            },
        )
        .unwrap();
        assert!(!t.to_text().contains("rule 20"));

        apply_acl_edit(
            &mut t,
            &AclEdit {
                set: vec![],
                delete: vec![AclDelete {
                    name: "EDGE-IN".into(),
                }],
            },
        )
        .unwrap();
        assert_eq!(t.to_text(), "");
        // A mac field on an ipv4 ACL is rejected.
        assert!(apply_acl_edit(
            &mut t,
            &AclEdit {
                set: vec![AclSet {
                    family: "ipv4".into(),
                    name: "X".into(),
                    rules: vec![RuleSet {
                        number: 10,
                        action: "permit".into(),
                        ethertype: Some("arp".into()),
                        ..RuleSet::default()
                    }],
                }],
                delete: vec![],
            }
        )
        .is_err());
    }

    #[test]
    fn binding_and_copp_edits() {
        let mut t = tree("");
        apply_acl_binding_edit(
            &mut t,
            &AclBindingEdit {
                set: vec![AclBindingSet {
                    interface: "Ethernet1".into(),
                    acl: "EDGE-IN".into(),
                    direction: "in".into(),
                }],
                delete: vec![],
            },
        )
        .unwrap();
        assert!(t.to_text().contains("access-group EDGE-IN in"));
        apply_acl_binding_edit(
            &mut t,
            &AclBindingEdit {
                set: vec![],
                delete: vec![AclBindingDelete {
                    interface: "Ethernet1".into(),
                    direction: "in".into(),
                }],
            },
        )
        .unwrap();
        // The binding leaf goes; the (now empty) interface node is fine
        // — the CLI leaves those too.
        assert!(!t.to_text().contains("access-group"));

        apply_copp_edit(
            &mut t,
            &CoppEdit {
                set: vec![CoppSet {
                    class: "bpdu".into(),
                    rate: Some(512),
                    burst: Some(128),
                }],
                delete: vec![],
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("class bpdu"));
        assert!(text.contains("rate 512"));
        apply_copp_edit(
            &mut t,
            &CoppEdit {
                set: vec![],
                delete: vec!["bpdu".into()],
            },
        )
        .unwrap();
        assert!(!t.to_text().contains("copp"));
        assert!(!t.to_text().contains("security"));
        assert!(apply_copp_edit(
            &mut t,
            &CoppEdit {
                set: vec![CoppSet {
                    class: "nonsense".into(),
                    rate: None,
                    burst: None,
                }],
                delete: vec![],
            }
        )
        .is_err());
    }

    #[test]
    fn dot1x_edit_keeps_existing_key() {
        let mut t = tree("");
        apply_dot1x_edit(
            &mut t,
            &Dot1xEdit {
                servers_set: vec![RadiusServerSet {
                    ip: "10.42.0.5".into(),
                    key: Some("s3cret".into()),
                    port: Some(1812),
                    timeout: Some(5),
                    retransmit: Some(3),
                }],
                reauth_interval: Some(3600),
                ports_enable: vec!["Ethernet10".into()],
                ..Dot1xEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("radius-server 10.42.0.5"));
        assert!(text.contains("key s3cret"));
        assert!(text.contains("reauth-interval 3600"));
        assert!(text.contains("dot1x"));

        // Absent key keeps the leaf; port removal reverts to default.
        apply_dot1x_edit(
            &mut t,
            &Dot1xEdit {
                servers_set: vec![RadiusServerSet {
                    ip: "10.42.0.5".into(),
                    key: None,
                    port: None,
                    timeout: Some(5),
                    retransmit: Some(3),
                }],
                ..Dot1xEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("key s3cret"));
        assert!(!text.contains("port 1812"));

        apply_dot1x_edit(
            &mut t,
            &Dot1xEdit {
                servers_delete: vec!["10.42.0.5".into()],
                clear_reauth: true,
                ports_disable: vec!["Ethernet10".into()],
                ..Dot1xEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(!text.contains("security"));
        assert!(!text.contains("dot1x"));
    }

    #[test]
    fn snooping_sec_edit_is_declarative() {
        let mut t = tree("");
        apply_snooping_sec_edit(
            &mut t,
            &SnoopingSecEdit {
                dhcp_vlans: Some(vec![20, 10]),
                arp_vlans: Some(vec![10]),
                validate: Some(vec!["src-mac".into(), "ip".into()]),
                trust_set: vec![TrustSet {
                    interface: "Port-Channel1".into(),
                    feature: "dhcp-snooping".into(),
                    trusted: true,
                }],
                bindings_set: vec![BindingSet {
                    mac: "0050.56BE.EF99".into(),
                    vlan: 20,
                    address: "10.0.20.50".into(),
                    interface: "Ethernet7".into(),
                }],
                bindings_delete: vec![],
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("dhcp-snooping"));
        assert!(text.contains("validate src-mac"));
        assert!(text.contains("validate ip"));
        assert!(text.contains("dhcp-snooping trust"));
        assert!(text
            .contains("binding 00:50:56:be:ef:99 vlan 20 address 10.0.20.50 interface Ethernet7"));
        // Round-trips.
        assert_eq!(hemlock_config::parse(&text).unwrap(), t);

        apply_snooping_sec_edit(
            &mut t,
            &SnoopingSecEdit {
                dhcp_vlans: Some(vec![]),
                arp_vlans: Some(vec![]),
                validate: Some(vec![]),
                trust_set: vec![TrustSet {
                    interface: "Port-Channel1".into(),
                    feature: "dhcp-snooping".into(),
                    trusted: false,
                }],
                bindings_set: vec![],
                bindings_delete: vec![BindingKey {
                    mac: "00:50:56:be:ef:99".into(),
                    vlan: 20,
                }],
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(!text.contains("security"));
        assert!(!text.contains("trust"));
    }
}
