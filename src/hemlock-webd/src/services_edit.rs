//! Config edits for the services-suite pages: LLDP, NTP, SNMP,
//! sFlow, and the DHCP relay and server.
//!
//! Same discipline as `edit.rs`: the builders write exactly the leaves
//! hemlockctl writes, based on the running config, and the result goes
//! through mgmtd's normal SetCandidate + Commit path — so validation
//! (including every services-suite `IntentError`), the rollback ring,
//! and `show configuration` behave as if the change came from the CLI.

use hemlock_config::{ConfigTree, Item};
use serde::Deserialize;

fn block_children_mut<'a>(items: &'a mut [Item], name: &str) -> Option<&'a mut Vec<Item>> {
    items.iter_mut().find_map(|item| match item {
        Item::Block {
            name: n, children, ..
        } if n == name => Some(children),
        _ => None,
    })
}

/// Drop every leaf named `name` whose leading values match `prefix`.
fn remove_leaf_matching(items: &mut Vec<Item>, name: &str, prefix: &[&str]) {
    items.retain(|item| match item {
        Item::Leaf { name: n, values } if n == name => {
            !(values.len() >= prefix.len()
                && values.iter().zip(prefix).all(|(value, want)| value == want))
        }
        _ => true,
    });
}

fn push_leaf(items: &mut Vec<Item>, name: &str, values: Vec<String>) {
    items.push(Item::Leaf {
        name: name.to_string(),
        values,
    });
}

fn remove_block_if_empty(tree: &mut ConfigTree, name: &str) {
    if tree
        .block(name)
        .is_some_and(|(_, children)| children.is_empty())
    {
        ConfigTree::remove_block(&mut tree.items, name, &[]);
    }
}

/// Mutable children of one interface block, creating it if absent.
fn interface_mut<'a>(tree: &'a mut ConfigTree, port: &str) -> &'a mut Vec<Item> {
    let interfaces = tree.block_mut("interfaces");
    ConfigTree::ensure_block(interfaces, port, &[])
}

// ------------------------------------------------------------------ LLDP

#[derive(Debug, Default, Deserialize)]
pub struct LldpEdit {
    /// The global off switch; None leaves it alone.
    #[serde(default)]
    pub disabled: Option<bool>,
    /// 0 clears (back to the default 30).
    #[serde(default)]
    pub tx_interval: Option<u16>,
    /// 0 clears (back to the default 4).
    #[serde(default)]
    pub hold_multiplier: Option<u8>,
    /// When present, replaces the set of ports carrying
    /// `lldp disable` — every other port's leaf is removed.
    #[serde(default)]
    pub disabled_ports: Option<Vec<String>>,
}

pub fn apply_lldp_edit(tree: &mut ConfigTree, edit: &LldpEdit) -> Result<(), String> {
    if let Some(interval) = edit.tx_interval {
        if interval != 0 && !(5..=300).contains(&interval) {
            return Err(format!("bad tx-interval {interval} (5..300)"));
        }
    }
    if let Some(multiplier) = edit.hold_multiplier {
        if multiplier != 0 && !(2..=10).contains(&multiplier) {
            return Err(format!("bad hold-multiplier {multiplier} (2..10)"));
        }
    }
    if let Some(ports) = &edit.disabled_ports {
        for port in ports {
            // LLDP is a physical-port setting: the same rule the CLI
            // and the intent extractor enforce.
            if !port.starts_with("Ethernet") {
                return Err(format!("{port}: lldp is a physical-port setting"));
            }
        }
    }

    let services = tree.block_mut("services");
    let block = ConfigTree::ensure_block(services, "lldp", &[]);
    match edit.disabled {
        Some(true) => ConfigTree::set_leaf(block, "disable", vec![]),
        Some(false) => ConfigTree::remove_leaf(block, "disable"),
        None => {}
    }
    for (leaf, value) in [
        ("tx-interval", edit.tx_interval.map(u32::from)),
        ("hold-multiplier", edit.hold_multiplier.map(u32::from)),
    ] {
        match value {
            Some(0) => ConfigTree::remove_leaf(block, leaf),
            Some(n) => ConfigTree::set_leaf(block, leaf, vec![n.to_string()]),
            None => {}
        }
    }
    if block.is_empty() {
        ConfigTree::remove_block(services, "lldp", &[]);
    }
    remove_block_if_empty(tree, "services");

    // The per-port disables are a whole set: ports that dropped out of
    // it lose their leaf.
    if let Some(wanted) = &edit.disabled_ports {
        let existing: Vec<String> = tree
            .block("interfaces")
            .map(|(_, items)| {
                items
                    .iter()
                    .filter_map(|item| match item {
                        Item::Block { name, children, .. }
                            if ConfigTree::has_leaf(children, "lldp") =>
                        {
                            Some(name.clone())
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        for port in existing.iter().filter(|port| !wanted.contains(port)) {
            let interfaces = tree.block_mut("interfaces");
            if let Some(children) = block_children_mut(interfaces, port) {
                ConfigTree::remove_leaf(children, "lldp");
                // An interface node that held nothing but the disable
                // goes with it (an empty node configures nothing).
                if children.is_empty() {
                    ConfigTree::remove_block(interfaces, port, &[]);
                }
            }
        }
        for port in wanted {
            let children = interface_mut(tree, port);
            ConfigTree::set_leaf(children, "lldp", vec!["disable".into()]);
        }
        remove_block_if_empty(tree, "interfaces");
    }
    Ok(())
}

// ------------------------------------------------------------------- NTP

/// The most NTP servers the config accepts — the same cap the CLI and
/// the intent extractor enforce.
const MAX_NTP_SERVERS: usize = 4;

/// NTP server syntax: an IP literal or a syntactically valid hostname
/// (resolution is timesyncd's problem). mgmtd re-validates.
fn valid_ntp_server(host: &str) -> bool {
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

#[derive(Debug, Default, Deserialize)]
pub struct NtpEdit {
    /// The whole wanted server list, in order; an empty list turns the
    /// client off (mgmtd stops timesyncd).
    #[serde(default)]
    pub servers: Vec<String>,
}

pub fn apply_ntp_edit(tree: &mut ConfigTree, edit: &NtpEdit) -> Result<(), String> {
    let mut wanted: Vec<&String> = Vec::new();
    for server in &edit.servers {
        if !valid_ntp_server(server) {
            return Err(format!("bad ntp server {server:?}"));
        }
        // The page sends a list; a duplicate in it is a UI slip, not a
        // second server.
        if !wanted.contains(&server) {
            wanted.push(server);
        }
    }
    if wanted.len() > MAX_NTP_SERVERS {
        return Err(format!(
            "at most {MAX_NTP_SERVERS} ntp servers ({} given)",
            wanted.len()
        ));
    }

    let services = tree.block_mut("services");
    if wanted.is_empty() {
        ConfigTree::remove_block(services, "ntp", &[]);
        remove_block_if_empty(tree, "services");
        return Ok(());
    }
    let block = ConfigTree::ensure_block(services, "ntp", &[]);
    ConfigTree::remove_leaf(block, "server");
    for server in wanted {
        push_leaf(block, "server", vec![server.clone()]);
    }
    Ok(())
}

// ------------------------------------------------------------------ SNMP

/// The shortest v3 passphrase USM accepts — the same floor the CLI and
/// the intent extractor enforce.
const MIN_SNMP_PASSWORD: usize = 8;

/// SNMP community and USM user names: a letter, then letters, digits,
/// `_` or `-`, at most 32 characters. mgmtd re-validates.
fn valid_snmp_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[derive(Debug, Deserialize)]
pub struct SnmpCommunitySet {
    pub name: String,
    /// "" = answers anywhere.
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Deserialize)]
pub struct SnmpUserSet {
    pub name: String,
    /// Write-only: the page never reads a passphrase back, so an
    /// absent one on an edit keeps whatever is configured.
    #[serde(default)]
    pub auth_password: Option<String>,
    #[serde(default)]
    pub priv_password: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SnmpEdit {
    /// False removes the whole `snmp` block (the agent stops).
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub contact: Option<String>,
    /// When present, replaces the whole community list, in order.
    #[serde(default)]
    pub communities: Option<Vec<SnmpCommunitySet>>,
    /// Users to add or update.
    #[serde(default)]
    pub users_set: Vec<SnmpUserSet>,
    #[serde(default)]
    pub users_delete: Vec<String>,
}

/// The `user <name> auth sha <pass> priv aes <pass>` leaf a name
/// already has, so an edit that omits a passphrase keeps it.
fn existing_user_passwords(block: &[Item], name: &str) -> Option<(String, String)> {
    block.iter().find_map(|item| match item {
        Item::Leaf { name: leaf, values }
            if leaf == "user" && values.len() == 7 && values[0] == name =>
        {
            Some((values[3].clone(), values[6].clone()))
        }
        _ => None,
    })
}

pub fn apply_snmp_edit(tree: &mut ConfigTree, edit: &SnmpEdit) -> Result<(), String> {
    if edit.enabled == Some(false) {
        let services = tree.block_mut("services");
        ConfigTree::remove_block(services, "snmp", &[]);
        remove_block_if_empty(tree, "services");
        return Ok(());
    }
    if let Some(communities) = &edit.communities {
        let mut seen: Vec<&str> = Vec::new();
        for community in communities {
            if !valid_snmp_name(&community.name) {
                return Err(format!("bad community name {:?}", community.name));
            }
            if seen.contains(&community.name.as_str()) {
                return Err(format!("duplicate community {}", community.name));
            }
            seen.push(&community.name);
            if !community.source.is_empty() {
                hemlock_common::net::canonical_prefix(&community.source)
                    .map_err(|reason| format!("community {}: {reason}", community.name))?;
            }
        }
    }
    for user in &edit.users_set {
        if !valid_snmp_name(&user.name) {
            return Err(format!("bad user name {:?}", user.name));
        }
        for password in [&user.auth_password, &user.priv_password]
            .into_iter()
            .flatten()
        {
            if password.len() < MIN_SNMP_PASSWORD {
                return Err(format!(
                    "user {}: passwords must be at least {MIN_SNMP_PASSWORD} characters",
                    user.name
                ));
            }
        }
    }

    let services = tree.block_mut("services");
    let block = ConfigTree::ensure_block(services, "snmp", &[]);
    for (leaf, value) in [("location", &edit.location), ("contact", &edit.contact)] {
        match value.as_deref() {
            // An empty string clears the leaf; absent leaves it alone.
            Some("") => ConfigTree::remove_leaf(block, leaf),
            Some(text) => ConfigTree::set_leaf(block, leaf, vec![text.to_string()]),
            None => {}
        }
    }
    if let Some(communities) = &edit.communities {
        ConfigTree::remove_leaf(block, "community");
        for community in communities {
            let mut values = vec![community.name.clone()];
            if !community.source.is_empty() {
                let prefix = hemlock_common::net::canonical_prefix(&community.source)
                    .unwrap_or_else(|_| community.source.clone());
                values.push("source".into());
                values.push(prefix);
            }
            push_leaf(block, "community", values);
        }
    }
    for user in &edit.users_set {
        let existing = existing_user_passwords(block, &user.name);
        let (auth, priv_) = match (&user.auth_password, &user.priv_password, existing) {
            (Some(auth), Some(priv_), _) => (auth.clone(), priv_.clone()),
            // A partial edit keeps whichever passphrase it omits.
            (auth, priv_, Some((old_auth, old_priv))) => (
                auth.clone().unwrap_or(old_auth),
                priv_.clone().unwrap_or(old_priv),
            ),
            _ => {
                return Err(format!(
                    "user {}: both passphrases are required for a new user",
                    user.name
                ))
            }
        };
        remove_leaf_matching(block, "user", &[&user.name]);
        push_leaf(
            block,
            "user",
            vec![
                user.name.clone(),
                "auth".into(),
                "sha".into(),
                auth,
                "priv".into(),
                "aes".into(),
                priv_,
            ],
        );
    }
    for name in &edit.users_delete {
        remove_leaf_matching(block, "user", &[name]);
    }
    Ok(())
}

// ----------------------------------------------------------------- sFlow

/// The most collectors, and the sampling-rate range — the same limits
/// the CLI and the intent extractor enforce.
const MAX_SFLOW_COLLECTORS: usize = 2;
const MIN_SFLOW_SAMPLE_RATE: u32 = 256;
const MAX_SFLOW_SAMPLE_RATE: u32 = 1_048_576;

#[derive(Debug, Deserialize)]
pub struct SflowCollectorSet {
    pub address: String,
    /// 0 = the sFlow default (6343).
    #[serde(default)]
    pub port: u16,
}

#[derive(Debug, Default, Deserialize)]
pub struct SflowEdit {
    /// When present, replaces the whole collector list, in order. An
    /// empty list turns sampling off.
    #[serde(default)]
    pub collectors: Option<Vec<SflowCollectorSet>>,
    /// 0 clears (back to the default 16384).
    #[serde(default)]
    pub sample_rate: Option<u32>,
    /// 0 clears (back to the default 30).
    #[serde(default)]
    pub polling_interval: Option<u16>,
    /// When present, replaces the set of ports carrying
    /// `sflow disable`.
    #[serde(default)]
    pub disabled_ports: Option<Vec<String>>,
}

pub fn apply_sflow_edit(tree: &mut ConfigTree, edit: &SflowEdit) -> Result<(), String> {
    if let Some(collectors) = &edit.collectors {
        if collectors.len() > MAX_SFLOW_COLLECTORS {
            return Err(format!(
                "at most {MAX_SFLOW_COLLECTORS} collectors ({} given)",
                collectors.len()
            ));
        }
        let mut seen: Vec<&str> = Vec::new();
        for collector in collectors {
            if collector.address.parse::<std::net::IpAddr>().is_err() {
                return Err(format!("bad collector address {:?}", collector.address));
            }
            if seen.contains(&collector.address.as_str()) {
                return Err(format!("duplicate collector {}", collector.address));
            }
            seen.push(&collector.address);
        }
    }
    if let Some(rate) = edit.sample_rate {
        if rate != 0
            && (!(MIN_SFLOW_SAMPLE_RATE..=MAX_SFLOW_SAMPLE_RATE).contains(&rate)
                || !rate.is_power_of_two())
        {
            return Err(format!(
                "bad sample-rate {rate} ({MIN_SFLOW_SAMPLE_RATE}..{MAX_SFLOW_SAMPLE_RATE}, a power of two)"
            ));
        }
    }
    if let Some(interval) = edit.polling_interval {
        if interval != 0 && !(5..=300).contains(&interval) {
            return Err(format!("bad polling-interval {interval} (5..300)"));
        }
    }
    if let Some(ports) = &edit.disabled_ports {
        for port in ports {
            if !port.starts_with("Ethernet") {
                return Err(format!("{port}: sflow is a physical-port setting"));
            }
        }
    }

    let services = tree.block_mut("services");
    let block = ConfigTree::ensure_block(services, "sflow", &[]);
    if let Some(collectors) = &edit.collectors {
        ConfigTree::remove_leaf(block, "collector");
        for collector in collectors {
            let mut values = vec![collector.address.clone()];
            if collector.port != 0 {
                values.push("port".into());
                values.push(collector.port.to_string());
            }
            push_leaf(block, "collector", values);
        }
    }
    for (leaf, value) in [
        ("sample-rate", edit.sample_rate),
        ("polling-interval", edit.polling_interval.map(u32::from)),
    ] {
        match value {
            Some(0) => ConfigTree::remove_leaf(block, leaf),
            Some(n) => ConfigTree::set_leaf(block, leaf, vec![n.to_string()]),
            None => {}
        }
    }
    if block.is_empty() {
        ConfigTree::remove_block(services, "sflow", &[]);
    }
    remove_block_if_empty(tree, "services");

    // The per-port disables are a whole set, like LLDP's.
    if let Some(wanted) = &edit.disabled_ports {
        let existing: Vec<String> = tree
            .block("interfaces")
            .map(|(_, items)| {
                items
                    .iter()
                    .filter_map(|item| match item {
                        Item::Block { name, children, .. }
                            if ConfigTree::has_leaf(children, "sflow") =>
                        {
                            Some(name.clone())
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        for port in existing.iter().filter(|port| !wanted.contains(port)) {
            let interfaces = tree.block_mut("interfaces");
            if let Some(children) = block_children_mut(interfaces, port) {
                ConfigTree::remove_leaf(children, "sflow");
                if children.is_empty() {
                    ConfigTree::remove_block(interfaces, port, &[]);
                }
            }
        }
        for port in wanted {
            let children = interface_mut(tree, port);
            ConfigTree::set_leaf(children, "sflow", vec!["disable".into()]);
        }
        remove_block_if_empty(tree, "interfaces");
    }
    Ok(())
}

// ------------------------------------------------------------ DHCP relay

/// The most relay servers one SVI forwards to — the same cap the CLI
/// and the intent extractor enforce.
const MAX_DHCP_RELAY_SERVERS: usize = 4;

#[derive(Debug, Deserialize)]
pub struct DhcpRelayVlanSet {
    pub vlan: u16,
    /// The whole wanted server list, in order; empty removes the relay
    /// from that SVI.
    #[serde(default)]
    pub servers: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DhcpRelayEdit {
    #[serde(default)]
    pub set: Vec<DhcpRelayVlanSet>,
    /// VLANs to stop relaying on.
    #[serde(default)]
    pub delete: Vec<u16>,
}

pub fn apply_dhcp_relay_edit(tree: &mut ConfigTree, edit: &DhcpRelayEdit) -> Result<(), String> {
    for set in &edit.set {
        valid_vlan(set.vlan)?;
        if set.servers.len() > MAX_DHCP_RELAY_SERVERS {
            return Err(format!(
                "vlan {}: at most {MAX_DHCP_RELAY_SERVERS} servers ({} given)",
                set.vlan,
                set.servers.len()
            ));
        }
        for server in &set.servers {
            // IPv4 only: DHCPv6 relay is deferred.
            if server.parse::<std::net::Ipv4Addr>().is_err() {
                return Err(format!("vlan {}: bad server address {server:?}", set.vlan));
            }
        }
    }
    for vlan in &edit.delete {
        valid_vlan(*vlan)?;
    }

    for set in &edit.set {
        let svi = interface_mut(tree, &format!("Vlan{}", set.vlan));
        ConfigTree::remove_leaf(svi, "dhcp-relay");
        // A duplicate in the list is a UI slip, not a second server.
        let mut seen: Vec<&String> = Vec::new();
        for server in &set.servers {
            if seen.contains(&server) {
                continue;
            }
            seen.push(server);
            push_leaf(svi, "dhcp-relay", vec!["server".into(), server.clone()]);
        }
    }
    for vlan in &edit.delete {
        let name = format!("Vlan{vlan}");
        let interfaces = tree.block_mut("interfaces");
        if let Some(children) = block_children_mut(interfaces, &name) {
            ConfigTree::remove_leaf(children, "dhcp-relay");
            // An SVI that held nothing but the relay goes with it.
            if children.is_empty() {
                ConfigTree::remove_block(interfaces, &name, &[]);
            }
        }
    }
    remove_block_if_empty(tree, "interfaces");
    Ok(())
}

/// A VLAN id the config language accepts.
fn valid_vlan(id: u16) -> Result<(), String> {
    if (1..=4094).contains(&id) {
        Ok(())
    } else {
        Err(format!("bad VLAN id {id} (1..4094)"))
    }
}

// ----------------------------------------------------------- DHCP server

/// The most DNS servers a pool hands out — the same cap the CLI and the
/// intent extractor enforce.
const MAX_POOL_DNS_SERVERS: usize = 3;

/// Pool names follow the ACL/WRED convention. mgmtd re-validates.
fn valid_pool_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[derive(Debug, Deserialize)]
pub struct DhcpReservationSet {
    pub mac: String,
    pub address: String,
}

#[derive(Debug, Deserialize)]
pub struct DhcpPoolSet {
    pub name: String,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub range_start: Option<String>,
    #[serde(default)]
    pub range_end: Option<String>,
    #[serde(default)]
    pub default_gateway: Option<String>,
    /// When present, replaces the whole DNS list.
    #[serde(default)]
    pub dns_servers: Option<Vec<String>>,
    /// 0 clears (back to the default 86400).
    #[serde(default)]
    pub lease_time: Option<u32>,
    /// An empty string clears the leaf.
    #[serde(default)]
    pub domain_name: Option<String>,
    /// When present, replaces the whole reservation set.
    #[serde(default)]
    pub reservations: Option<Vec<DhcpReservationSet>>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DhcpServerEdit {
    #[serde(default)]
    pub set: Vec<DhcpPoolSet>,
    /// Pools to remove entirely.
    #[serde(default)]
    pub delete: Vec<String>,
}

fn ipv4(what: &str, text: &str) -> Result<String, String> {
    match text.parse::<std::net::Ipv4Addr>() {
        Ok(address) => Ok(address.to_string()),
        // DHCPv6 pools are deferred, so a v6 address is not a typo.
        Err(_) if text.parse::<std::net::Ipv6Addr>().is_ok() => {
            Err("DHCPv6 pools are not supported".into())
        }
        Err(_) => Err(format!("bad {what} {text:?}")),
    }
}

pub fn apply_dhcp_server_edit(tree: &mut ConfigTree, edit: &DhcpServerEdit) -> Result<(), String> {
    for set in &edit.set {
        if !valid_pool_name(&set.name) {
            return Err(format!("bad pool name {:?}", set.name));
        }
        if let Some(network) = &set.network {
            let canonical = hemlock_common::net::require_canonical_prefix(network)?;
            if canonical.contains(':') {
                return Err("DHCPv6 pools are not supported".into());
            }
        }
        for (what, value) in [
            ("range start", &set.range_start),
            ("range end", &set.range_end),
            ("default-gateway", &set.default_gateway),
        ] {
            if let Some(value) = value {
                ipv4(what, value)?;
            }
        }
        if let Some(servers) = &set.dns_servers {
            if servers.len() > MAX_POOL_DNS_SERVERS {
                return Err(format!(
                    "pool {}: at most {MAX_POOL_DNS_SERVERS} dns-servers ({} given)",
                    set.name,
                    servers.len()
                ));
            }
            for server in servers {
                ipv4("dns-server", server)?;
            }
        }
        if let Some(lease) = set.lease_time {
            if lease != 0 && !(300..=2_592_000).contains(&lease) {
                return Err(format!("bad lease-time {lease} (300..2592000)"));
            }
        }
        if let Some(reservations) = &set.reservations {
            for reservation in reservations {
                hemlock_common::net::parse_unicast_mac(&reservation.mac)?;
                ipv4("reservation address", &reservation.address)?;
            }
        }
        // A range is two halves of one leaf: half of it configures
        // nothing the intent extractor would accept.
        if set.range_start.is_some() != set.range_end.is_some() {
            return Err(format!(
                "pool {}: a range needs both a start and an end",
                set.name
            ));
        }
    }

    for set in &edit.set {
        let services = tree.block_mut("services");
        let server = ConfigTree::ensure_block(services, "dhcp-server", &[]);
        let pool = ConfigTree::ensure_block(server, "pool", &[&set.name]);
        if let Some(network) = &set.network {
            let canonical = hemlock_common::net::require_canonical_prefix(network)
                .unwrap_or_else(|_| network.clone());
            ConfigTree::set_leaf(pool, "network", vec![canonical]);
        }
        if let (Some(start), Some(end)) = (&set.range_start, &set.range_end) {
            ConfigTree::set_leaf(pool, "range", vec![start.clone(), end.clone()]);
        }
        if let Some(gateway) = &set.default_gateway {
            ConfigTree::set_leaf(pool, "default-gateway", vec![gateway.clone()]);
        }
        if let Some(servers) = &set.dns_servers {
            ConfigTree::remove_leaf(pool, "dns-server");
            let mut seen: Vec<&String> = Vec::new();
            for server in servers {
                if seen.contains(&server) {
                    continue;
                }
                seen.push(server);
                push_leaf(pool, "dns-server", vec![server.clone()]);
            }
        }
        match set.lease_time {
            Some(0) => ConfigTree::remove_leaf(pool, "lease-time"),
            Some(lease) => ConfigTree::set_leaf(pool, "lease-time", vec![lease.to_string()]),
            None => {}
        }
        match set.domain_name.as_deref() {
            Some("") => ConfigTree::remove_leaf(pool, "domain-name"),
            Some(domain) => {
                ConfigTree::set_leaf(pool, "domain-name", vec![domain.to_string()]);
            }
            None => {}
        }
        if let Some(reservations) = &set.reservations {
            ConfigTree::remove_leaf(pool, "reservation");
            let mut seen: Vec<String> = Vec::new();
            for reservation in reservations {
                let mac = hemlock_common::net::parse_unicast_mac(&reservation.mac)
                    .unwrap_or_else(|_| reservation.mac.clone());
                if seen.contains(&mac) {
                    continue;
                }
                seen.push(mac.clone());
                push_leaf(
                    pool,
                    "reservation",
                    vec![mac, "address".into(), reservation.address.clone()],
                );
            }
        }
    }

    for name in &edit.delete {
        let services = tree.block_mut("services");
        if let Some(server) = block_children_mut(services, "dhcp-server") {
            ConfigTree::remove_block(server, "pool", &[name]);
            if server.is_empty() {
                ConfigTree::remove_block(services, "dhcp-server", &[]);
            }
        }
    }
    remove_block_if_empty(tree, "services");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tree(text: &str) -> ConfigTree {
        hemlock_config::parse(text).unwrap()
    }

    #[test]
    fn lldp_edit_mirrors_cli_shapes() {
        let mut t = tree("");
        apply_lldp_edit(
            &mut t,
            &LldpEdit {
                tx_interval: Some(15),
                hold_multiplier: Some(3),
                disabled_ports: Some(vec!["Ethernet3".into()]),
                ..LldpEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("tx-interval 15"));
        assert!(text.contains("hold-multiplier 3"));
        assert!(text.contains("lldp disable"));

        // 0 clears a timer; an empty port set removes every leaf, and
        // an emptied block takes `services` with it.
        apply_lldp_edit(
            &mut t,
            &LldpEdit {
                tx_interval: Some(0),
                hold_multiplier: Some(0),
                disabled_ports: Some(Vec::new()),
                ..LldpEdit::default()
            },
        )
        .unwrap();
        assert_eq!(t.to_text().trim(), "");
    }

    #[test]
    fn lldp_edit_rejects_what_the_cli_rejects() {
        let mut t = tree("");
        for edit in [
            LldpEdit {
                tx_interval: Some(4),
                ..LldpEdit::default()
            },
            LldpEdit {
                hold_multiplier: Some(11),
                ..LldpEdit::default()
            },
            LldpEdit {
                disabled_ports: Some(vec!["Vlan99".into()]),
                ..LldpEdit::default()
            },
        ] {
            assert!(apply_lldp_edit(&mut t, &edit).is_err());
        }
        assert_eq!(
            apply_lldp_edit(
                &mut t,
                &LldpEdit {
                    disabled_ports: Some(vec!["Vlan99".into()]),
                    ..LldpEdit::default()
                }
            ),
            Err("Vlan99: lldp is a physical-port setting".into())
        );
    }

    #[test]
    fn ntp_edit_replaces_the_whole_server_list() {
        let mut t = tree("");
        apply_ntp_edit(
            &mut t,
            &NtpEdit {
                servers: vec!["10.42.0.5".into(), "pool.ntp.org".into()],
            },
        )
        .unwrap();
        assert!(t.to_text().contains("server 10.42.0.5"));
        assert!(t.to_text().contains("server pool.ntp.org"));
        // A replacement list wins outright — no merge, no leftovers.
        apply_ntp_edit(
            &mut t,
            &NtpEdit {
                servers: vec!["2001:db8::1".into()],
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("server 2001:db8::1"));
        assert!(!text.contains("10.42.0.5"));
        // Emptying it removes the block, and `services` with it.
        apply_ntp_edit(&mut t, &NtpEdit::default()).unwrap();
        assert_eq!(t.to_text().trim(), "");
    }

    #[test]
    fn ntp_edit_rejects_what_the_cli_rejects() {
        let mut t = tree("");
        assert!(apply_ntp_edit(
            &mut t,
            &NtpEdit {
                servers: vec!["not a host".into()],
            }
        )
        .is_err());
        assert!(apply_ntp_edit(
            &mut t,
            &NtpEdit {
                servers: (1..=5).map(|n| format!("10.0.0.{n}")).collect(),
            }
        )
        .is_err());
        // A duplicate collapses rather than eating a slot.
        apply_ntp_edit(
            &mut t,
            &NtpEdit {
                servers: vec!["10.0.0.1".into(), "10.0.0.1".into()],
            },
        )
        .unwrap();
        assert_eq!(t.to_text().matches("server 10.0.0.1").count(), 1);
    }

    #[test]
    fn snmp_edit_mirrors_cli_shapes() {
        let mut t = tree("");
        apply_snmp_edit(
            &mut t,
            &SnmpEdit {
                location: Some("rack 4, closet B".into()),
                contact: Some("cody@nightshade.systems".into()),
                communities: Some(vec![
                    SnmpCommunitySet {
                        name: "public".into(),
                        source: String::new(),
                    },
                    SnmpCommunitySet {
                        name: "netops".into(),
                        source: "10.42.0.0/16".into(),
                    },
                ]),
                users_set: vec![SnmpUserSet {
                    name: "monitor".into(),
                    auth_password: Some("authpass1".into()),
                    priv_password: Some("privpass1".into()),
                }],
                ..SnmpEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("community public"));
        assert!(text.contains("community netops source 10.42.0.0/16"));
        assert!(text.contains("user monitor auth sha authpass1 priv aes privpass1"));
        // Community order is the list's order, not alphabetical.
        assert!(text.find("community public").unwrap() < text.find("community netops").unwrap());

        // Disabling drops the whole block, and `services` with it.
        apply_snmp_edit(
            &mut t,
            &SnmpEdit {
                enabled: Some(false),
                ..SnmpEdit::default()
            },
        )
        .unwrap();
        assert_eq!(t.to_text().trim(), "");
    }

    /// Passphrases are write-only in the UI, so an edit that omits one
    /// must keep what is configured rather than blanking it.
    #[test]
    fn snmp_user_edit_keeps_omitted_passphrases() {
        let mut t = tree("");
        apply_snmp_edit(
            &mut t,
            &SnmpEdit {
                users_set: vec![SnmpUserSet {
                    name: "monitor".into(),
                    auth_password: Some("authpass1".into()),
                    priv_password: Some("privpass1".into()),
                }],
                ..SnmpEdit::default()
            },
        )
        .unwrap();
        apply_snmp_edit(
            &mut t,
            &SnmpEdit {
                users_set: vec![SnmpUserSet {
                    name: "monitor".into(),
                    auth_password: Some("newauthpass".into()),
                    priv_password: None,
                }],
                ..SnmpEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("user monitor auth sha newauthpass priv aes privpass1"));
        assert_eq!(text.matches("user monitor").count(), 1);

        // A brand-new user cannot be half-specified.
        assert!(apply_snmp_edit(
            &mut t,
            &SnmpEdit {
                users_set: vec![SnmpUserSet {
                    name: "fresh".into(),
                    auth_password: Some("authpass1".into()),
                    priv_password: None,
                }],
                ..SnmpEdit::default()
            }
        )
        .is_err());

        apply_snmp_edit(
            &mut t,
            &SnmpEdit {
                users_delete: vec!["monitor".into()],
                ..SnmpEdit::default()
            },
        )
        .unwrap();
        assert!(!t.to_text().contains("user monitor"));
    }

    #[test]
    fn snmp_edit_rejects_what_the_cli_rejects() {
        let mut t = tree("");
        let bad = [
            SnmpEdit {
                communities: Some(vec![SnmpCommunitySet {
                    name: "9bad".into(),
                    source: String::new(),
                }]),
                ..SnmpEdit::default()
            },
            SnmpEdit {
                communities: Some(vec![SnmpCommunitySet {
                    name: "public".into(),
                    source: "notaprefix".into(),
                }]),
                ..SnmpEdit::default()
            },
            SnmpEdit {
                users_set: vec![SnmpUserSet {
                    name: "monitor".into(),
                    auth_password: Some("short".into()),
                    priv_password: Some("privpass1".into()),
                }],
                ..SnmpEdit::default()
            },
        ];
        for edit in bad {
            assert!(apply_snmp_edit(&mut t, &edit).is_err());
        }
        // A duplicate in one list is a UI slip, not two communities.
        assert!(apply_snmp_edit(
            &mut t,
            &SnmpEdit {
                communities: Some(vec![
                    SnmpCommunitySet {
                        name: "public".into(),
                        source: String::new(),
                    },
                    SnmpCommunitySet {
                        name: "public".into(),
                        source: "10.0.0.0/8".into(),
                    },
                ]),
                ..SnmpEdit::default()
            }
        )
        .is_err());
    }

    #[test]
    fn sflow_edit_mirrors_cli_shapes() {
        let mut t = tree("");
        apply_sflow_edit(
            &mut t,
            &SflowEdit {
                collectors: Some(vec![
                    SflowCollectorSet {
                        address: "10.42.0.20".into(),
                        port: 0,
                    },
                    SflowCollectorSet {
                        address: "10.42.0.21".into(),
                        port: 6344,
                    },
                ]),
                sample_rate: Some(4096),
                polling_interval: Some(60),
                disabled_ports: Some(vec!["Ethernet4".into()]),
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("collector 10.42.0.20"));
        assert!(text.contains("collector 10.42.0.21 port 6344"));
        assert!(text.contains("sample-rate 4096"));
        assert!(text.contains("polling-interval 60"));
        assert!(text.contains("sflow disable"));
        // Collector order is the list's order.
        assert!(
            text.find("collector 10.42.0.20").unwrap() < text.find("collector 10.42.0.21").unwrap()
        );

        // Clearing everything removes the block and `services` with it.
        apply_sflow_edit(
            &mut t,
            &SflowEdit {
                collectors: Some(Vec::new()),
                sample_rate: Some(0),
                polling_interval: Some(0),
                disabled_ports: Some(Vec::new()),
            },
        )
        .unwrap();
        assert_eq!(t.to_text().trim(), "");
    }

    #[test]
    fn sflow_edit_rejects_what_the_cli_rejects() {
        let mut t = tree("");
        let bad = [
            SflowEdit {
                collectors: Some(vec![SflowCollectorSet {
                    address: "notanip".into(),
                    port: 0,
                }]),
                ..SflowEdit::default()
            },
            SflowEdit {
                collectors: Some(
                    (1..=3)
                        .map(|n| SflowCollectorSet {
                            address: format!("10.0.0.{n}"),
                            port: 0,
                        })
                        .collect(),
                ),
                ..SflowEdit::default()
            },
            // Not a power of two, and out of range.
            SflowEdit {
                sample_rate: Some(10_000),
                ..SflowEdit::default()
            },
            SflowEdit {
                sample_rate: Some(128),
                ..SflowEdit::default()
            },
            SflowEdit {
                polling_interval: Some(4),
                ..SflowEdit::default()
            },
            SflowEdit {
                disabled_ports: Some(vec!["Vlan99".into()]),
                ..SflowEdit::default()
            },
        ];
        for edit in bad {
            assert!(apply_sflow_edit(&mut t, &edit).is_err());
        }
        assert_eq!(
            apply_sflow_edit(
                &mut t,
                &SflowEdit {
                    disabled_ports: Some(vec!["Port-Channel1".into()]),
                    ..SflowEdit::default()
                }
            ),
            Err("Port-Channel1: sflow is a physical-port setting".into())
        );
    }

    #[test]
    fn dhcp_relay_edit_replaces_the_whole_server_list() {
        let mut t = tree("interfaces { Vlan99 { address 10.42.10.9/24 } }");
        apply_dhcp_relay_edit(
            &mut t,
            &DhcpRelayEdit {
                set: vec![DhcpRelayVlanSet {
                    vlan: 99,
                    servers: vec!["10.42.0.5".into(), "10.42.0.6".into()],
                }],
                ..DhcpRelayEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("dhcp-relay server 10.42.0.5"));
        assert!(text.contains("dhcp-relay server 10.42.0.6"));
        // Order is the list's, because the relay walks it in order.
        assert!(text.find("10.42.0.5").unwrap() < text.find("10.42.0.6").unwrap());
        // The SVI's own config is untouched.
        assert!(text.contains("address 10.42.10.9/24"));

        // A replacement list wins outright.
        apply_dhcp_relay_edit(
            &mut t,
            &DhcpRelayEdit {
                set: vec![DhcpRelayVlanSet {
                    vlan: 99,
                    servers: vec!["10.42.0.7".into()],
                }],
                ..DhcpRelayEdit::default()
            },
        )
        .unwrap();
        assert!(!t.to_text().contains("10.42.0.5"));

        apply_dhcp_relay_edit(
            &mut t,
            &DhcpRelayEdit {
                delete: vec![99],
                ..DhcpRelayEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(!text.contains("dhcp-relay"));
        // The SVI survives, because it still carries its address.
        assert!(text.contains("address 10.42.10.9/24"));
    }

    #[test]
    fn dhcp_relay_edit_rejects_what_the_cli_rejects() {
        let mut t = tree("");
        for edit in [
            DhcpRelayEdit {
                set: vec![DhcpRelayVlanSet {
                    vlan: 99,
                    servers: vec!["notanip".into()],
                }],
                ..DhcpRelayEdit::default()
            },
            // DHCPv6 relay is deferred, so a v6 server is not a server.
            DhcpRelayEdit {
                set: vec![DhcpRelayVlanSet {
                    vlan: 99,
                    servers: vec!["2001:db8::5".into()],
                }],
                ..DhcpRelayEdit::default()
            },
            DhcpRelayEdit {
                set: vec![DhcpRelayVlanSet {
                    vlan: 99,
                    servers: (1..=5).map(|n| format!("10.0.0.{n}")).collect(),
                }],
                ..DhcpRelayEdit::default()
            },
            DhcpRelayEdit {
                set: vec![DhcpRelayVlanSet {
                    vlan: 5000,
                    servers: vec!["10.0.0.1".into()],
                }],
                ..DhcpRelayEdit::default()
            },
        ] {
            assert!(apply_dhcp_relay_edit(&mut t, &edit).is_err());
        }
        // A duplicate collapses rather than eating a slot.
        apply_dhcp_relay_edit(
            &mut t,
            &DhcpRelayEdit {
                set: vec![DhcpRelayVlanSet {
                    vlan: 99,
                    servers: vec!["10.0.0.1".into(), "10.0.0.1".into()],
                }],
                ..DhcpRelayEdit::default()
            },
        )
        .unwrap();
        assert_eq!(t.to_text().matches("dhcp-relay server 10.0.0.1").count(), 1);
    }

    #[test]
    fn dhcp_server_edit_writes_a_whole_pool() {
        let mut t = tree("");
        apply_dhcp_server_edit(
            &mut t,
            &DhcpServerEdit {
                set: vec![DhcpPoolSet {
                    name: "LAN-USERS".into(),
                    network: Some("10.0.10.0/24".into()),
                    range_start: Some("10.0.10.100".into()),
                    range_end: Some("10.0.10.200".into()),
                    default_gateway: Some("10.0.10.1".into()),
                    dns_servers: Some(vec!["10.42.0.5".into(), "10.42.0.6".into()]),
                    lease_time: Some(86400),
                    domain_name: None,
                    reservations: Some(vec![DhcpReservationSet {
                        mac: "00:1C:73:0C:AA:01".into(),
                        address: "10.0.10.50".into(),
                    }]),
                }],
                ..DhcpServerEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("network 10.0.10.0/24"));
        assert!(text.contains("range 10.0.10.100 10.0.10.200"));
        assert!(text.contains("default-gateway 10.0.10.1"));
        assert!(text.contains("dns-server 10.42.0.5"));
        assert!(text.contains("lease-time 86400"));
        // The MAC is canonicalized on the way in.
        assert!(text.contains("reservation 00:1c:73:0c:aa:01 address 10.0.10.50"));

        // A replacement list wins outright.
        apply_dhcp_server_edit(
            &mut t,
            &DhcpServerEdit {
                set: vec![DhcpPoolSet {
                    name: "LAN-USERS".into(),
                    network: None,
                    range_start: None,
                    range_end: None,
                    default_gateway: None,
                    dns_servers: Some(vec!["10.42.0.9".into()]),
                    lease_time: Some(0),
                    domain_name: None,
                    reservations: Some(Vec::new()),
                }],
                ..DhcpServerEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("dns-server 10.42.0.9"));
        assert!(!text.contains("10.42.0.5"));
        assert!(!text.contains("lease-time"), "0 clears the leaf");
        assert!(!text.contains("reservation"));
        // ...and the untouched leaves survive.
        assert!(text.contains("network 10.0.10.0/24"));

        apply_dhcp_server_edit(
            &mut t,
            &DhcpServerEdit {
                delete: vec!["LAN-USERS".into()],
                ..DhcpServerEdit::default()
            },
        )
        .unwrap();
        assert_eq!(t.to_text().trim(), "");
    }

    #[test]
    fn dhcp_server_edit_rejects_what_the_cli_rejects() {
        let mut t = tree("");
        let pool = |edit: DhcpPoolSet| DhcpServerEdit {
            set: vec![edit],
            ..DhcpServerEdit::default()
        };
        let base = || DhcpPoolSet {
            name: "P".into(),
            network: None,
            range_start: None,
            range_end: None,
            default_gateway: None,
            dns_servers: None,
            lease_time: None,
            domain_name: None,
            reservations: None,
        };
        for edit in [
            pool(DhcpPoolSet {
                name: "9bad".into(),
                ..base()
            }),
            pool(DhcpPoolSet {
                network: Some("10.0.10.5/24".into()),
                ..base()
            }),
            pool(DhcpPoolSet {
                network: Some("2001:db8::/64".into()),
                ..base()
            }),
            // Half a range configures nothing.
            pool(DhcpPoolSet {
                range_start: Some("10.0.10.100".into()),
                ..base()
            }),
            pool(DhcpPoolSet {
                lease_time: Some(299),
                ..base()
            }),
            pool(DhcpPoolSet {
                dns_servers: Some((1..=4).map(|n| format!("10.0.0.{n}")).collect()),
                ..base()
            }),
            pool(DhcpPoolSet {
                reservations: Some(vec![DhcpReservationSet {
                    mac: "ff:ff:ff:ff:ff:ff".into(),
                    address: "10.0.10.5".into(),
                }]),
                ..base()
            }),
        ] {
            assert!(apply_dhcp_server_edit(&mut t, &edit).is_err());
        }
        assert_eq!(t.to_text().trim(), "", "a rejected edit writes nothing");
    }

    /// The global disable is independent of the per-port set.
    #[test]
    fn global_disable_round_trips() {
        let mut t = tree("");
        apply_lldp_edit(
            &mut t,
            &LldpEdit {
                disabled: Some(true),
                ..LldpEdit::default()
            },
        )
        .unwrap();
        assert!(t.to_text().contains("disable"));
        apply_lldp_edit(
            &mut t,
            &LldpEdit {
                disabled: Some(false),
                ..LldpEdit::default()
            },
        )
        .unwrap();
        assert_eq!(t.to_text().trim(), "");
    }
}
