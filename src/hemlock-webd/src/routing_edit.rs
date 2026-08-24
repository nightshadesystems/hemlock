//! Config edits for the routing pages: static routes (ECMP-aware).
//!
//! Same discipline as `edit.rs` / `switching_edit.rs`: the builders
//! write exactly the leaves hemlockctl writes — one `routing { static {
//! <prefix> <next-hop> [distance <n>] } }` leaf per next hop, or a
//! single `<prefix> drop` leaf — and the result goes through mgmtd's
//! normal SetCandidate + Commit path, so validation, the rollback ring,
//! and `show configuration` behave as if the change came from the CLI.

use hemlock_config::{ConfigTree, Item};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct StaticRouteSet {
    pub prefix: String,
    /// The whole wanted next-hop set for the prefix — existing lines
    /// are replaced wholesale, so an edit modal round-trips cleanly.
    #[serde(default)]
    pub next_hops: Vec<String>,
    /// Null route; mutually exclusive with next hops.
    #[serde(default)]
    pub drop: bool,
    /// Administrative distance; absent = 1.
    #[serde(default)]
    pub distance: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct StaticRouteDelete {
    pub prefix: String,
    /// Delete just this next hop; absent = the whole prefix.
    #[serde(default)]
    pub next_hop: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct StaticRouteEdit {
    #[serde(default)]
    pub set: Vec<StaticRouteSet>,
    #[serde(default)]
    pub delete: Vec<StaticRouteDelete>,
}

pub fn apply_static_route_edit(
    tree: &mut ConfigTree,
    edit: &StaticRouteEdit,
) -> Result<(), String> {
    if edit.set.is_empty() && edit.delete.is_empty() {
        return Err("nothing to change".into());
    }

    // Validate everything first; the tree stays untouched on rejection.
    let mut sets: Vec<(String, Vec<Vec<String>>)> = Vec::new();
    for set in &edit.set {
        let prefix = hemlock_common::net::require_canonical_prefix(&set.prefix)
            .map_err(|e| format!("{}: {e}", set.prefix))?;
        if set.drop && !set.next_hops.is_empty() {
            return Err(format!(
                "{prefix}: drop and next hops are mutually exclusive"
            ));
        }
        if !set.drop && set.next_hops.is_empty() {
            return Err(format!(
                "{prefix}: at least one next hop (or drop) is required"
            ));
        }
        let distance = match set.distance {
            None => 1,
            Some(d) if (1..=255).contains(&d) => d,
            Some(d) => return Err(format!("{prefix}: bad distance {d} (1..255)")),
        };
        let lines = if set.drop {
            vec![vec!["drop".to_string()]]
        } else {
            let mut lines = Vec::new();
            for next_hop in &set.next_hops {
                hemlock_common::net::validate_next_hop(&prefix, next_hop)
                    .map_err(|e| format!("{prefix}: {e}"))?;
                let mut values = vec![next_hop.clone()];
                if distance != 1 {
                    values.extend(["distance".to_string(), distance.to_string()]);
                }
                if !lines.contains(&values) {
                    lines.push(values);
                }
            }
            lines
        };
        sets.push((prefix, lines));
    }
    let mut deletes: Vec<(String, Option<String>)> = Vec::new();
    for delete in &edit.delete {
        let prefix = hemlock_common::net::require_canonical_prefix(&delete.prefix)
            .map_err(|e| format!("{}: {e}", delete.prefix))?;
        deletes.push((prefix, delete.next_hop.clone()));
    }

    // Mutate.
    {
        let routing = tree.block_mut("routing");
        let routes = ConfigTree::ensure_block(routing, "static", &[]);
        for (prefix, lines) in sets {
            routes.retain(|item| !matches!(item, Item::Leaf { name, .. } if *name == prefix));
            for values in lines {
                routes.push(Item::Leaf {
                    name: prefix.clone(),
                    values,
                });
            }
        }
        for (prefix, next_hop) in deletes {
            routes.retain(|item| match item {
                Item::Leaf { name, values } if *name == prefix => match &next_hop {
                    Some(hop) => values.first() != Some(hop),
                    None => false,
                },
                _ => true,
            });
        }
        if routes.is_empty() {
            ConfigTree::remove_block(routing, "static", &[]);
        }
    }
    if tree
        .block("routing")
        .is_some_and(|(_, children)| children.is_empty())
    {
        ConfigTree::remove_block(&mut tree.items, "routing", &[]);
    }
    Ok(())
}

// ------------------------------------------------------------- ARP / ND

#[derive(Debug, Deserialize)]
pub struct ArpSet {
    pub ip: String,
    pub interface: String,
    pub mac: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct ArpEdit {
    #[serde(default)]
    pub set: Vec<ArpSet>,
    /// Addresses whose static entries go away.
    #[serde(default)]
    pub delete: Vec<String>,
}

pub fn apply_arp_edit(tree: &mut ConfigTree, edit: &ArpEdit) -> Result<(), String> {
    if edit.set.is_empty() && edit.delete.is_empty() {
        return Err("nothing to change".into());
    }
    // Validate everything first; the tree stays untouched on rejection.
    let mut sets: Vec<(String, String, String)> = Vec::new();
    for set in &edit.set {
        let ip: std::net::IpAddr = set
            .ip
            .parse()
            .map_err(|_| format!("bad IP address {:?}", set.ip))?;
        if set.interface.is_empty() {
            return Err(format!("{ip}: an interface is required"));
        }
        let mac =
            hemlock_common::net::parse_unicast_mac(&set.mac).map_err(|e| format!("{ip}: {e}"))?;
        sets.push((ip.to_string(), set.interface.clone(), mac));
    }
    let mut deletes = Vec::new();
    for ip in &edit.delete {
        let ip: std::net::IpAddr = ip.parse().map_err(|_| format!("bad IP address {ip:?}"))?;
        deletes.push(ip.to_string());
    }

    {
        let routing = tree.block_mut("routing");
        let entries = ConfigTree::ensure_block(routing, "arp", &[]);
        for (ip, interface, mac) in sets {
            ConfigTree::set_leaf(
                entries,
                &ip,
                vec!["interface".into(), interface, "mac".into(), mac],
            );
        }
        for ip in deletes {
            ConfigTree::remove_leaf(entries, &ip);
        }
        if entries.is_empty() {
            ConfigTree::remove_block(routing, "arp", &[]);
        }
    }
    if tree
        .block("routing")
        .is_some_and(|(_, children)| children.is_empty())
    {
        ConfigTree::remove_block(&mut tree.items, "routing", &[]);
    }
    Ok(())
}

fn push_leaf(items: &mut Vec<Item>, name: &str, values: Vec<String>) {
    items.push(Item::Leaf {
        name: name.to_string(),
        values,
    });
}

fn block_children_mut<'a>(items: &'a mut [Item], name: &str) -> Option<&'a mut Vec<Item>> {
    items.iter_mut().find_map(|item| match item {
        Item::Block {
            name: n, children, ..
        } if n == name => Some(children),
        _ => None,
    })
}

// ------------------------------------------------------------------ OSPF

#[derive(Debug, Deserialize)]
pub struct OspfAreaSet {
    pub id: String,
    pub networks: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct OspfInterfaceSet {
    pub interface: String,
    #[serde(default)]
    pub cost: Option<u16>,
    #[serde(default)]
    pub hello_interval: Option<u16>,
    #[serde(default)]
    pub dead_interval: Option<u16>,
    #[serde(default)]
    pub priority: Option<u8>,
}

/// Whole-section replaces: an absent field leaves the section alone,
/// a present one replaces it (empty = clear), so the edit modal
/// round-trips what the GET returned.
#[derive(Debug, Default, Deserialize)]
pub struct OspfEdit {
    /// Remove the whole `ospf` block.
    #[serde(default)]
    pub delete: bool,
    /// Some("") clears.
    #[serde(default)]
    pub router_id: Option<String>,
    #[serde(default)]
    pub maximum_paths: Option<u8>,
    #[serde(default)]
    pub areas: Option<Vec<OspfAreaSet>>,
    #[serde(default)]
    pub passive_interfaces: Option<Vec<String>>,
    #[serde(default)]
    pub redistribute: Option<Vec<String>>,
    #[serde(default)]
    pub interfaces: Option<Vec<OspfInterfaceSet>>,
}

/// The canonical dotted form of an OSPF area id.
fn canonical_area(text: &str) -> Result<String, String> {
    if let Ok(area) = text.parse::<std::net::Ipv4Addr>() {
        return Ok(area.to_string());
    }
    text.parse::<u32>()
        .map(|n| std::net::Ipv4Addr::from(n).to_string())
        .map_err(|_| format!("bad area {text:?} (dotted or 0..4294967295)"))
}

fn v4_prefix(prefix: &str, family: &str) -> Result<String, String> {
    let canonical = hemlock_common::net::require_canonical_prefix(prefix)
        .map_err(|e| format!("{prefix}: {e}"))?;
    if canonical.contains(':') {
        return Err(format!("{canonical}: {family} is IPv4-only"));
    }
    Ok(canonical)
}

pub fn apply_ospf_edit(tree: &mut ConfigTree, edit: &OspfEdit) -> Result<(), String> {
    if edit.delete {
        let routing = tree.block_mut("routing");
        ConfigTree::remove_block(routing, "ospf", &[]);
        prune_routing(tree);
        return Ok(());
    }
    // Validate everything first.
    if let Some(id) = edit.router_id.as_deref() {
        if !id.is_empty() && id.parse::<std::net::Ipv4Addr>().is_err() {
            return Err(format!("bad router-id {id:?}"));
        }
    }
    if let Some(paths) = edit.maximum_paths {
        if !(1..=8).contains(&paths) {
            return Err(format!("bad maximum-paths {paths} (1..8)"));
        }
    }
    let mut areas = Vec::new();
    if let Some(sets) = &edit.areas {
        for area in sets {
            let id = canonical_area(&area.id)?;
            let mut networks = Vec::new();
            for network in &area.networks {
                networks.push(v4_prefix(network, "ospf")?);
            }
            areas.push((id, networks));
        }
    }
    if let Some(sources) = &edit.redistribute {
        for source in sources {
            if !matches!(source.as_str(), "connected" | "static" | "bgp") {
                return Err(format!("redistribute {source:?} (connected|static|bgp)"));
            }
        }
    }
    if let Some(interfaces) = &edit.interfaces {
        for iface in interfaces {
            if iface.interface.is_empty() {
                return Err("an interface name is required".into());
            }
            for (knob, value, low) in [
                ("cost", iface.cost, 1u16),
                ("hello-interval", iface.hello_interval, 1),
                ("dead-interval", iface.dead_interval, 1),
            ] {
                if let Some(value) = value {
                    if value < low {
                        return Err(format!("bad {knob} {value}"));
                    }
                }
            }
        }
    }

    // Mutate.
    {
        let routing = tree.block_mut("routing");
        let ospf = ConfigTree::ensure_block(routing, "ospf", &[]);
        if let Some(id) = edit.router_id.as_deref() {
            if id.is_empty() {
                ConfigTree::remove_leaf(ospf, "router-id");
            } else {
                ConfigTree::set_leaf(ospf, "router-id", vec![id.to_string()]);
            }
        }
        if let Some(paths) = edit.maximum_paths {
            ConfigTree::set_leaf(ospf, "maximum-paths", vec![paths.to_string()]);
        }
        if edit.areas.is_some() {
            ospf.retain(|item| !matches!(item, Item::Block { name, .. } if name == "area"));
            for (id, networks) in areas {
                let body = ConfigTree::ensure_block(ospf, "area", &[&id]);
                for network in networks {
                    push_leaf(body, "network", vec![network]);
                }
            }
        }
        if let Some(interfaces) = &edit.passive_interfaces {
            ConfigTree::remove_leaf(ospf, "passive-interface");
            for interface in interfaces {
                push_leaf(ospf, "passive-interface", vec![interface.clone()]);
            }
        }
        if let Some(sources) = &edit.redistribute {
            ConfigTree::remove_leaf(ospf, "redistribute");
            for source in sources {
                push_leaf(ospf, "redistribute", vec![source.clone()]);
            }
        }
        if let Some(interfaces) = &edit.interfaces {
            ospf.retain(|item| !matches!(item, Item::Block { name, .. } if name == "interface"));
            for iface in interfaces {
                let body = ConfigTree::ensure_block(ospf, "interface", &[&iface.interface]);
                if let Some(cost) = iface.cost {
                    ConfigTree::set_leaf(body, "cost", vec![cost.to_string()]);
                }
                if let Some(hello) = iface.hello_interval {
                    ConfigTree::set_leaf(body, "hello-interval", vec![hello.to_string()]);
                }
                if let Some(dead) = iface.dead_interval {
                    ConfigTree::set_leaf(body, "dead-interval", vec![dead.to_string()]);
                }
                if let Some(priority) = iface.priority {
                    ConfigTree::set_leaf(body, "priority", vec![priority.to_string()]);
                }
            }
            ospf.retain(|item| {
                !matches!(item, Item::Block { name, children, .. }
                    if name == "interface" && children.is_empty())
            });
        }
    }
    prune_routing(tree);
    Ok(())
}

// ------------------------------------------------------------------- BGP

#[derive(Debug, Deserialize)]
pub struct BgpNeighborSet {
    pub ip: String,
    #[serde(default)]
    pub remote_as: Option<u32>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub shutdown: bool,
    #[serde(default)]
    pub ebgp_multihop: Option<u8>,
    #[serde(default)]
    pub next_hop_self: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct BgpEdit {
    /// Remove the whole `bgp` block.
    #[serde(default)]
    pub delete: bool,
    #[serde(default)]
    pub as_number: Option<u32>,
    /// Some("") clears.
    #[serde(default)]
    pub router_id: Option<String>,
    #[serde(default)]
    pub maximum_paths: Option<u8>,
    #[serde(default)]
    pub networks: Option<Vec<String>>,
    #[serde(default)]
    pub redistribute: Option<Vec<String>>,
    /// Each set replaces the neighbor's whole block.
    #[serde(default)]
    pub set_neighbors: Vec<BgpNeighborSet>,
    #[serde(default)]
    pub delete_neighbors: Vec<String>,
}

pub fn apply_bgp_edit(tree: &mut ConfigTree, edit: &BgpEdit) -> Result<(), String> {
    if edit.delete {
        let routing = tree.block_mut("routing");
        ConfigTree::remove_block(routing, "bgp", &[]);
        prune_routing(tree);
        return Ok(());
    }
    if let Some(0) = edit.as_number {
        return Err("bad as 0 (1..4294967295)".into());
    }
    if let Some(id) = edit.router_id.as_deref() {
        if !id.is_empty() && id.parse::<std::net::Ipv4Addr>().is_err() {
            return Err(format!("bad router-id {id:?}"));
        }
    }
    if let Some(paths) = edit.maximum_paths {
        if !(1..=8).contains(&paths) {
            return Err(format!("bad maximum-paths {paths} (1..8)"));
        }
    }
    let mut networks = Vec::new();
    if let Some(sets) = &edit.networks {
        for network in sets {
            networks.push(v4_prefix(network, "the BGP IPv4 address family")?);
        }
    }
    if let Some(sources) = &edit.redistribute {
        for source in sources {
            if !matches!(source.as_str(), "connected" | "static" | "ospf") {
                return Err(format!("redistribute {source:?} (connected|static|ospf)"));
            }
        }
    }
    let mut set_neighbors = Vec::new();
    for neighbor in &edit.set_neighbors {
        let ip: std::net::IpAddr = neighbor
            .ip
            .parse()
            .map_err(|_| format!("bad neighbor address {:?}", neighbor.ip))?;
        if ip.is_ipv6() {
            return Err(format!(
                "neighbor {ip}: the BGP IPv6 address family is not supported"
            ));
        }
        if neighbor.remote_as.is_none_or(|remote| remote == 0) {
            return Err(format!("neighbor {ip}: remote-as is required"));
        }
        set_neighbors.push((ip.to_string(), neighbor));
    }
    let mut delete_neighbors = Vec::new();
    for ip in &edit.delete_neighbors {
        let ip: std::net::IpAddr = ip
            .parse()
            .map_err(|_| format!("bad neighbor address {ip:?}"))?;
        delete_neighbors.push(ip.to_string());
    }

    {
        let routing = tree.block_mut("routing");
        let bgp = ConfigTree::ensure_block(routing, "bgp", &[]);
        if let Some(as_number) = edit.as_number {
            ConfigTree::set_leaf(bgp, "as", vec![as_number.to_string()]);
        }
        if let Some(id) = edit.router_id.as_deref() {
            if id.is_empty() {
                ConfigTree::remove_leaf(bgp, "router-id");
            } else {
                ConfigTree::set_leaf(bgp, "router-id", vec![id.to_string()]);
            }
        }
        if let Some(paths) = edit.maximum_paths {
            ConfigTree::set_leaf(bgp, "maximum-paths", vec![paths.to_string()]);
        }
        if edit.networks.is_some() {
            ConfigTree::remove_leaf(bgp, "network");
            for network in networks {
                push_leaf(bgp, "network", vec![network]);
            }
        }
        if let Some(sources) = &edit.redistribute {
            ConfigTree::remove_leaf(bgp, "redistribute");
            for source in sources {
                push_leaf(bgp, "redistribute", vec![source.clone()]);
            }
        }
        for (ip, neighbor) in set_neighbors {
            ConfigTree::remove_block(bgp, "neighbor", &[&ip]);
            let body = ConfigTree::ensure_block(bgp, "neighbor", &[&ip]);
            if let Some(remote) = neighbor.remote_as {
                ConfigTree::set_leaf(body, "remote-as", vec![remote.to_string()]);
            }
            if let Some(description) = neighbor.description.as_deref() {
                if !description.is_empty() {
                    ConfigTree::set_leaf(body, "description", vec![description.to_string()]);
                }
            }
            if neighbor.shutdown {
                ConfigTree::set_leaf(body, "shutdown", vec![]);
            }
            if let Some(ttl) = neighbor.ebgp_multihop {
                if ttl > 0 {
                    ConfigTree::set_leaf(body, "ebgp-multihop", vec![ttl.to_string()]);
                }
            }
            if neighbor.next_hop_self {
                ConfigTree::set_leaf(body, "next-hop-self", vec![]);
            }
        }
        for ip in delete_neighbors {
            ConfigTree::remove_block(bgp, "neighbor", &[&ip]);
        }
    }
    prune_routing(tree);
    Ok(())
}

// ------------------------------------------------------------------ VRRP

#[derive(Debug, Deserialize)]
pub struct VrrpSet {
    pub interface: String,
    pub group: u8,
    pub addresses: Vec<String>,
    #[serde(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    pub advertisement_interval: Option<u8>,
    /// Preempt defaults on; false writes `no-preempt`.
    #[serde(default = "preempt_default")]
    pub preempt: bool,
}

fn preempt_default() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct VrrpDelete {
    pub interface: String,
    pub group: u8,
}

#[derive(Debug, Default, Deserialize)]
pub struct VrrpEdit {
    /// Each set replaces the group's whole block.
    #[serde(default)]
    pub set: Vec<VrrpSet>,
    #[serde(default)]
    pub delete: Vec<VrrpDelete>,
}

pub fn apply_vrrp_edit(tree: &mut ConfigTree, edit: &VrrpEdit) -> Result<(), String> {
    if edit.set.is_empty() && edit.delete.is_empty() {
        return Err("nothing to change".into());
    }
    let mut sets = Vec::new();
    for set in &edit.set {
        if set.interface.is_empty() {
            return Err("an interface name is required".into());
        }
        if set.group == 0 {
            return Err("bad group 0 (1..255)".into());
        }
        if set.addresses.is_empty() {
            return Err(format!(
                "{} group {}: at least one address (VIP) is required",
                set.interface, set.group
            ));
        }
        let mut addresses = Vec::new();
        for address in &set.addresses {
            let vip: std::net::Ipv4Addr = address
                .parse()
                .map_err(|_| format!("bad address {address:?} (IPv4)"))?;
            addresses.push(vip.to_string());
        }
        if let Some(priority) = set.priority {
            if !(1..=254).contains(&priority) {
                return Err(format!("bad priority {priority} (1..254)"));
            }
        }
        if let Some(interval) = set.advertisement_interval {
            if !(1..=40).contains(&interval) {
                return Err(format!("bad advertisement-interval {interval} (1..40)"));
            }
        }
        sets.push((set, addresses));
    }

    let interfaces = tree.block_mut("interfaces");
    for (set, addresses) in sets {
        let Some(iface) = block_children_mut(interfaces, &set.interface) else {
            return Err(format!("{}: not an editable interface", set.interface));
        };
        let group = set.group.to_string();
        ConfigTree::remove_block(iface, "vrrp", &[&group]);
        let body = ConfigTree::ensure_block(iface, "vrrp", &[&group]);
        for address in addresses {
            push_leaf(body, "address", vec![address]);
        }
        if let Some(priority) = set.priority {
            ConfigTree::set_leaf(body, "priority", vec![priority.to_string()]);
        }
        if let Some(interval) = set.advertisement_interval {
            ConfigTree::set_leaf(body, "advertisement-interval", vec![interval.to_string()]);
        }
        if !set.preempt {
            ConfigTree::set_leaf(body, "no-preempt", vec![]);
        }
    }
    for delete in &edit.delete {
        if let Some(iface) = block_children_mut(interfaces, &delete.interface) {
            ConfigTree::remove_block(iface, "vrrp", &[&delete.group.to_string()]);
        }
    }
    Ok(())
}

/// Drop `routing` sub-blocks a section edit emptied, then the husk.
fn prune_routing(tree: &mut ConfigTree) {
    let routing = tree.block_mut("routing");
    routing.retain(|item| {
        !matches!(item, Item::Block { name, children, .. }
            if matches!(name.as_str(), "ospf" | "bgp") && children.is_empty())
    });
    if tree
        .block("routing")
        .is_some_and(|(_, children)| children.is_empty())
    {
        ConfigTree::remove_block(&mut tree.items, "routing", &[]);
    }
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
    fn arp_create_update_delete_and_rejections() {
        let mut t = tree("vlans { vlan 99 { } }\ninterfaces { Vlan99 { address 10.42.10.9/24 } }");
        apply_arp_edit(
            &mut t,
            &ArpEdit {
                set: vec![ArpSet {
                    ip: "10.42.10.200".into(),
                    interface: "Vlan99".into(),
                    mac: "00-50-56-BE-EF-99".into(),
                }],
                delete: vec![],
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("10.42.10.200 interface Vlan99 mac 00:50:56:be:ef:99"));
        assert_eq!(hemlock_config::parse(&text).unwrap(), t);

        apply_arp_edit(
            &mut t,
            &ArpEdit {
                set: vec![],
                delete: vec!["10.42.10.200".into()],
            },
        )
        .unwrap();
        assert!(!t.to_text().contains("arp"));

        let err = apply_arp_edit(&mut t, &ArpEdit::default()).unwrap_err();
        assert!(err.contains("nothing to change"));
        let err = apply_arp_edit(
            &mut t,
            &ArpEdit {
                set: vec![ArpSet {
                    ip: "banana".into(),
                    interface: "Vlan99".into(),
                    mac: "00:50:56:be:ef:99".into(),
                }],
                delete: vec![],
            },
        )
        .unwrap_err();
        assert!(err.contains("bad IP address"));
        let err = apply_arp_edit(
            &mut t,
            &ArpEdit {
                set: vec![ArpSet {
                    ip: "10.0.0.1".into(),
                    interface: "Vlan99".into(),
                    mac: "01:00:5e:00:00:01".into(),
                }],
                delete: vec![],
            },
        )
        .unwrap_err();
        assert!(err.contains("not a unicast MAC"));
    }

    #[test]
    fn static_route_create_update_delete() {
        let mut t = tree("");

        // ECMP set with default distance.
        apply_static_route_edit(
            &mut t,
            &StaticRouteEdit {
                set: vec![StaticRouteSet {
                    prefix: "10.99.0.0/16".into(),
                    next_hops: vec!["10.9.9.0".into(), "10.42.10.7".into()],
                    drop: false,
                    distance: None,
                }],
                delete: vec![],
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("10.99.0.0/16 10.9.9.0"));
        assert!(text.contains("10.99.0.0/16 10.42.10.7"));
        assert_eq!(hemlock_config::parse(&text).unwrap(), t);

        // Replace wholesale with a drop route; add a distance route.
        apply_static_route_edit(
            &mut t,
            &StaticRouteEdit {
                set: vec![
                    StaticRouteSet {
                        prefix: "10.99.0.0/16".into(),
                        next_hops: vec![],
                        drop: true,
                        distance: None,
                    },
                    StaticRouteSet {
                        prefix: "172.16.0.0/12".into(),
                        next_hops: vec!["10.42.10.1".into()],
                        drop: false,
                        distance: Some(250),
                    },
                ],
                delete: vec![],
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("10.99.0.0/16 drop"));
        assert!(!text.contains("10.9.9.0"));
        assert!(text.contains("172.16.0.0/12 10.42.10.1 distance 250"));

        // Delete one next hop of an ECMP set, then a whole prefix; the
        // emptied blocks disappear.
        let mut t = tree(
            "routing {\n    static {\n        10.99.0.0/16 10.9.9.0\n        10.99.0.0/16 10.42.10.7\n        192.0.2.0/24 drop\n    }\n}\n",
        );
        apply_static_route_edit(
            &mut t,
            &StaticRouteEdit {
                set: vec![],
                delete: vec![StaticRouteDelete {
                    prefix: "10.99.0.0/16".into(),
                    next_hop: Some("10.9.9.0".into()),
                }],
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(!text.contains("10.9.9.0"));
        assert!(text.contains("10.99.0.0/16 10.42.10.7"));
        apply_static_route_edit(
            &mut t,
            &StaticRouteEdit {
                set: vec![],
                delete: vec![
                    StaticRouteDelete {
                        prefix: "10.99.0.0/16".into(),
                        next_hop: None,
                    },
                    StaticRouteDelete {
                        prefix: "192.0.2.0/24".into(),
                        next_hop: None,
                    },
                ],
            },
        )
        .unwrap();
        assert_eq!(t.to_text(), "");
    }

    #[test]
    fn rejects_bad_static_route_edits() {
        let set =
            |prefix: &str, hops: &[&str], drop: bool, distance: Option<u16>| StaticRouteEdit {
                set: vec![StaticRouteSet {
                    prefix: prefix.into(),
                    next_hops: hops.iter().map(|h| h.to_string()).collect(),
                    drop,
                    distance,
                }],
                delete: vec![],
            };
        let mut t = tree("");
        let err = apply_static_route_edit(&mut t, &StaticRouteEdit::default()).unwrap_err();
        assert!(err.contains("nothing to change"));
        let err = apply_static_route_edit(&mut t, &set("10.99.1.0/16", &["10.0.0.1"], false, None))
            .unwrap_err();
        assert!(err.contains("host bits set; did you mean 10.99.0.0/16?"));
        let err =
            apply_static_route_edit(&mut t, &set("10.99.0.0/16", &[], false, None)).unwrap_err();
        assert!(err.contains("at least one next hop"));
        let err = apply_static_route_edit(&mut t, &set("10.99.0.0/16", &["10.0.0.1"], true, None))
            .unwrap_err();
        assert!(err.contains("mutually exclusive"));
        let err =
            apply_static_route_edit(&mut t, &set("10.99.0.0/16", &["2001:db8::1"], false, None))
                .unwrap_err();
        assert!(err.contains("does not match the address family"));
        let err =
            apply_static_route_edit(&mut t, &set("10.99.0.0/16", &["10.0.0.1"], false, Some(0)))
                .unwrap_err();
        assert!(err.contains("bad distance 0 (1..255)"));
        // Nothing landed in the tree.
        assert_eq!(t.to_text(), "");
    }

    #[test]
    fn ospf_bgp_vrrp_builders_write_cli_shaped_text() {
        let mut t = tree(
            "vlans { vlan 99 { } vlan 100 { } } interfaces { Vlan99 { address 10.42.10.9/24 } Vlan100 { address 10.0.100.2/24 } }",
        );
        apply_ospf_edit(
            &mut t,
            &OspfEdit {
                router_id: Some("10.42.0.1".into()),
                maximum_paths: Some(4),
                areas: Some(vec![OspfAreaSet {
                    id: "0".into(),
                    networks: vec!["10.42.10.0/24".into()],
                }]),
                passive_interfaces: Some(vec!["Vlan100".into()]),
                redistribute: Some(vec!["static".into()]),
                ..OspfEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("area 0.0.0.0"), "{text}");
        assert!(text.contains("network 10.42.10.0/24"));
        assert!(text.contains("passive-interface Vlan100"));
        assert_eq!(hemlock_config::parse(&text).unwrap(), t);

        apply_bgp_edit(
            &mut t,
            &BgpEdit {
                as_number: Some(65000),
                networks: Some(vec!["10.42.0.0/16".into()]),
                set_neighbors: vec![BgpNeighborSet {
                    ip: "10.42.10.1".into(),
                    remote_as: Some(65001),
                    description: Some("upstream".into()),
                    shutdown: false,
                    ebgp_multihop: None,
                    next_hop_self: true,
                }],
                ..BgpEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("as 65000"));
        assert!(text.contains("remote-as 65001"));
        assert!(text.contains("next-hop-self"));

        apply_vrrp_edit(
            &mut t,
            &VrrpEdit {
                set: vec![VrrpSet {
                    interface: "Vlan100".into(),
                    group: 10,
                    addresses: vec!["10.0.100.1".into()],
                    priority: Some(200),
                    advertisement_interval: None,
                    preempt: false,
                }],
                delete: vec![],
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("vrrp 10"));
        assert!(text.contains("no-preempt"));
        assert_eq!(hemlock_config::parse(&text).unwrap(), t);

        // Deletes prune the emptied blocks.
        apply_vrrp_edit(
            &mut t,
            &VrrpEdit {
                set: vec![],
                delete: vec![VrrpDelete {
                    interface: "Vlan100".into(),
                    group: 10,
                }],
            },
        )
        .unwrap();
        apply_ospf_edit(
            &mut t,
            &OspfEdit {
                delete: true,
                ..OspfEdit::default()
            },
        )
        .unwrap();
        apply_bgp_edit(
            &mut t,
            &BgpEdit {
                delete: true,
                ..BgpEdit::default()
            },
        )
        .unwrap();
        assert!(!t.to_text().contains("routing"), "{}", t.to_text());

        // Rejections.
        let err = apply_bgp_edit(
            &mut t,
            &BgpEdit {
                set_neighbors: vec![BgpNeighborSet {
                    ip: "10.0.0.1".into(),
                    remote_as: None,
                    description: None,
                    shutdown: false,
                    ebgp_multihop: None,
                    next_hop_self: false,
                }],
                ..BgpEdit::default()
            },
        )
        .unwrap_err();
        assert!(err.contains("remote-as is required"));
        let err = apply_vrrp_edit(
            &mut t,
            &VrrpEdit {
                set: vec![VrrpSet {
                    interface: "Vlan100".into(),
                    group: 10,
                    addresses: vec![],
                    priority: None,
                    advertisement_interval: None,
                    preempt: true,
                }],
                delete: vec![],
            },
        )
        .unwrap_err();
        assert!(err.contains("at least one address"));
    }
}
