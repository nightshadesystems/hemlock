//! Config edits for the switching-suite pages: port-channels,
//! spanning tree, MAC table, snooping, storm control, and mirroring.
//!
//! Same discipline as `edit.rs`: the builders write exactly the leaves
//! and phrases hemlockctl writes, based on the running config, and the
//! result goes through mgmtd's normal SetCandidate + Commit path — so
//! validation (including every switching-suite `IntentError`), the
//! rollback ring, and `show configuration` behave as if the change came
//! from the CLI.

use hemlock_config::{ConfigTree, Item};
use serde::Deserialize;

fn valid_vlan(id: u16) -> Result<(), String> {
    if (1..=4094).contains(&id) {
        Ok(())
    } else {
        Err(format!("bad VLAN id {id} (1..4094)"))
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

fn remove_leaf_matching(items: &mut Vec<Item>, name: &str, prefix: &[&str]) {
    items.retain(|item| match item {
        Item::Leaf { name: n, values } if n == name => {
            !(values.len() >= prefix.len()
                && values.iter().zip(prefix).all(|(value, want)| value == want))
        }
        _ => true,
    });
}

// ------------------------------------------------------------------ LAGs

#[derive(Debug, Deserialize)]
pub struct LagMemberSpec {
    pub port: String,
    /// "active" | "passive" | "on".
    pub mode: String,
    #[serde(default)]
    pub rate_fast: bool,
    #[serde(default)]
    pub port_priority: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct LagSet {
    pub group: u16,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub min_links: Option<u8>,
    /// "static" | "individual" | "" (clears).
    #[serde(default)]
    pub fallback: Option<String>,
    #[serde(default)]
    pub fallback_timeout: Option<u16>,
    /// "access" | "trunk".
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub access_vlan: Option<u16>,
    #[serde(default)]
    pub trunk_vlans: Option<Vec<u16>>,
    #[serde(default)]
    pub native_vlan: Option<u16>,
    /// When present, replaces the member set (ports currently in the
    /// group but absent here lose their channel-group).
    #[serde(default)]
    pub members: Option<Vec<LagMemberSpec>>,
}

#[derive(Debug, Default, Deserialize)]
pub struct LagEdit {
    #[serde(default)]
    pub set: Vec<LagSet>,
    #[serde(default)]
    pub delete: Vec<u16>,
}

/// Ports whose `channel-group` names `group`, from the current tree.
fn members_of(tree: &ConfigTree, group: u16) -> Vec<String> {
    let Some((_, interfaces)) = tree.block("interfaces") else {
        return Vec::new();
    };
    interfaces
        .iter()
        .filter_map(|item| match item {
            Item::Block { name, children, .. } => {
                let values = ConfigTree::leaf_values(children, "channel-group")?;
                (values.first().map(String::as_str) == Some(group.to_string().as_str()))
                    .then(|| name.clone())
            }
            _ => None,
        })
        .collect()
}

pub fn apply_lag_edit(tree: &mut ConfigTree, edit: &LagEdit) -> Result<(), String> {
    if edit.set.is_empty() && edit.delete.is_empty() {
        return Err("nothing to change".into());
    }
    for set in &edit.set {
        if !(1..=64).contains(&set.group) {
            return Err(format!("bad channel-group number {} (1..64)", set.group));
        }
        if let Some(members) = &set.members {
            if members.len() > 8 {
                return Err(format!(
                    "channel-group {} has {} members (max 8)",
                    set.group,
                    members.len()
                ));
            }
            for member in members {
                if !member.port.starts_with("Ethernet") {
                    return Err(format!("{}: not a LAG-capable port", member.port));
                }
                if !matches!(member.mode.as_str(), "active" | "passive" | "on") {
                    return Err(format!("bad channel-group mode {:?}", member.mode));
                }
            }
        }
        if let Some(fallback) = &set.fallback {
            if !matches!(fallback.as_str(), "static" | "individual" | "") {
                return Err(format!("bad fallback mode {fallback:?}"));
            }
        }
        if let Some(min_links) = set.min_links {
            if min_links > 8 {
                return Err(format!("bad min-links {min_links} (0..8)"));
            }
        }
        if let Some(timeout) = set.fallback_timeout {
            if !(1..=900).contains(&timeout) {
                return Err(format!("bad fallback-timeout {timeout} (1..900)"));
            }
        }
        for vlan in set
            .access_vlan
            .iter()
            .chain(set.native_vlan.iter())
            .chain(set.trunk_vlans.iter().flatten())
        {
            valid_vlan(*vlan)?;
        }
    }
    for group in &edit.delete {
        if !(1..=64).contains(group) {
            return Err(format!("bad channel-group number {group} (1..64)"));
        }
    }

    for set in &edit.set {
        let name = format!("Port-Channel{}", set.group);
        // Member reconcile first (it walks sibling port blocks).
        if let Some(members) = &set.members {
            let current = members_of(tree, set.group);
            let interfaces = tree.block_mut("interfaces");
            for port in &current {
                if !members.iter().any(|m| m.port == *port) {
                    if let Some(eth) = block_children_mut(interfaces, port) {
                        ConfigTree::remove_leaf(eth, "channel-group");
                        ConfigTree::remove_block(eth, "lacp", &[]);
                    }
                }
            }
            for member in members {
                let eth = ConfigTree::ensure_block(interfaces, &member.port, &[]);
                ConfigTree::set_leaf(
                    eth,
                    "channel-group",
                    vec![set.group.to_string(), "mode".into(), member.mode.clone()],
                );
                let wants_lacp = member.rate_fast || member.port_priority.is_some();
                if wants_lacp {
                    let lacp = ConfigTree::ensure_block(eth, "lacp", &[]);
                    if member.rate_fast {
                        ConfigTree::set_leaf(lacp, "rate", vec!["fast".into()]);
                    } else {
                        ConfigTree::remove_leaf(lacp, "rate");
                    }
                    match member.port_priority {
                        Some(priority) => ConfigTree::set_leaf(
                            lacp,
                            "port-priority",
                            vec![priority.to_string()],
                        ),
                        None => ConfigTree::remove_leaf(lacp, "port-priority"),
                    }
                } else {
                    ConfigTree::remove_block(eth, "lacp", &[]);
                }
            }
        }

        let interfaces = tree.block_mut("interfaces");
        let po = ConfigTree::ensure_block(interfaces, &name, &[]);
        if let Some(description) = &set.description {
            if description.is_empty() {
                ConfigTree::remove_leaf(po, "description");
            } else {
                ConfigTree::set_leaf(po, "description", vec![description.clone()]);
            }
        }
        match set.min_links {
            Some(0) => ConfigTree::remove_leaf(po, "min-links"),
            Some(n) => ConfigTree::set_leaf(po, "min-links", vec![n.to_string()]),
            None => {}
        }
        if let Some(fallback) = &set.fallback {
            if fallback.is_empty() {
                if let Some(lacp) = block_children_mut(po, "lacp") {
                    ConfigTree::remove_leaf(lacp, "fallback");
                    ConfigTree::remove_leaf(lacp, "fallback-timeout");
                }
            } else {
                let lacp = ConfigTree::ensure_block(po, "lacp", &[]);
                ConfigTree::set_leaf(lacp, "fallback", vec![fallback.clone()]);
                if let Some(timeout) = set.fallback_timeout {
                    ConfigTree::set_leaf(lacp, "fallback-timeout", vec![timeout.to_string()]);
                }
            }
        }
        if let Some(mode) = &set.mode {
            let trunk = mode == "trunk";
            let sp = ConfigTree::ensure_block(po, "switchport", &[]);
            ConfigTree::set_leaf(sp, "mode", vec![mode.clone()]);
            if trunk {
                ConfigTree::remove_leaf(sp, "access");
            } else {
                ConfigTree::remove_leaf(sp, "trunk");
                ConfigTree::remove_leaf(sp, "native");
            }
        }
        if let Some(vlan) = set.access_vlan {
            if let Some(sp) = block_children_mut(po, "switchport") {
                ConfigTree::set_phrase(sp, "access", "vlan", vec![vlan.to_string()]);
            }
        }
        if let Some(vlans) = &set.trunk_vlans {
            if let Some(sp) = block_children_mut(po, "switchport") {
                let mut sorted = vlans.clone();
                sorted.sort_unstable();
                sorted.dedup();
                if sorted.is_empty() {
                    ConfigTree::remove_leaf(sp, "trunk");
                } else {
                    ConfigTree::set_phrase(
                        sp,
                        "trunk",
                        "vlans",
                        sorted.iter().map(u16::to_string).collect(),
                    );
                }
            }
        }
        if let Some(vlan) = set.native_vlan {
            if let Some(sp) = block_children_mut(po, "switchport") {
                ConfigTree::set_phrase(sp, "native", "vlan", vec![vlan.to_string()]);
            }
        }
    }
    for group in &edit.delete {
        let members = members_of(tree, *group);
        let interfaces = tree.block_mut("interfaces");
        for port in members {
            if let Some(eth) = block_children_mut(interfaces, &port) {
                ConfigTree::remove_leaf(eth, "channel-group");
                ConfigTree::remove_block(eth, "lacp", &[]);
            }
        }
        ConfigTree::remove_block(interfaces, &format!("Port-Channel{group}"), &[]);
    }
    remove_block_if_empty(tree, "interfaces");
    Ok(())
}

// --------------------------------------------------------- spanning tree

#[derive(Debug, Deserialize)]
pub struct StpPortEdit {
    pub name: String,
    #[serde(default)]
    pub portfast: Option<bool>,
    #[serde(default)]
    pub bpduguard: Option<bool>,
    /// 0 clears (back to the speed-derived default).
    #[serde(default)]
    pub cost: Option<u32>,
    /// 0 clears (back to 128).
    #[serde(default)]
    pub port_priority: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct MstInstanceEdit {
    pub instance: u8,
    pub vlans: Vec<u16>,
}

#[derive(Debug, Default, Deserialize)]
pub struct StpEdit {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub priority: Option<u16>,
    #[serde(default)]
    pub hello_time: Option<u8>,
    #[serde(default)]
    pub max_age: Option<u8>,
    #[serde(default)]
    pub forward_time: Option<u8>,
    #[serde(default)]
    pub mst_name: Option<String>,
    #[serde(default)]
    pub mst_revision: Option<u16>,
    /// When present, replaces the whole instance mapping.
    #[serde(default)]
    pub instances: Option<Vec<MstInstanceEdit>>,
    #[serde(default)]
    pub ports: Vec<StpPortEdit>,
}

pub fn apply_stp_edit(tree: &mut ConfigTree, edit: &StpEdit) -> Result<(), String> {
    if let Some(mode) = &edit.mode {
        if !matches!(mode.as_str(), "mstp" | "rstp" | "none") {
            return Err(format!("bad spanning-tree mode {mode:?}"));
        }
    }
    if let Some(priority) = edit.priority {
        if priority > 61440 || priority % 4096 != 0 {
            return Err(format!(
                "bad priority {priority} (0..61440, multiple of 4096)"
            ));
        }
    }
    if let Some(instances) = &edit.instances {
        for map in instances {
            if !(1..=15).contains(&map.instance) {
                return Err(format!("bad mst instance {} (1..15)", map.instance));
            }
            for vlan in &map.vlans {
                valid_vlan(*vlan)?;
            }
        }
    }
    for port in &edit.ports {
        if let Some(priority) = port.port_priority {
            if priority > 240 || priority % 16 != 0 {
                return Err(format!(
                    "bad port-priority {priority} (0..240, multiple of 16)"
                ));
            }
        }
        if let Some(cost) = port.cost {
            if cost > 200_000_000 {
                return Err(format!("bad cost {cost} (1..200000000)"));
            }
        }
    }

    let has_global = edit.mode.is_some()
        || edit.priority.is_some()
        || edit.hello_time.is_some()
        || edit.max_age.is_some()
        || edit.forward_time.is_some()
        || edit.mst_name.is_some()
        || edit.mst_revision.is_some()
        || edit.instances.is_some();
    if has_global {
        let protocols = tree.block_mut("protocols");
        let stp = ConfigTree::ensure_block(protocols, "spanning-tree", &[]);
        if let Some(mode) = &edit.mode {
            ConfigTree::set_leaf(stp, "mode", vec![mode.clone()]);
        }
        let numeric = |stp: &mut Vec<Item>, leaf: &str, value: Option<u32>| {
            if let Some(value) = value {
                ConfigTree::set_leaf(stp, leaf, vec![value.to_string()]);
            }
        };
        numeric(stp, "priority", edit.priority.map(u32::from));
        numeric(stp, "hello-time", edit.hello_time.map(u32::from));
        numeric(stp, "max-age", edit.max_age.map(u32::from));
        numeric(stp, "forward-time", edit.forward_time.map(u32::from));
        let wants_mst = edit.mst_name.is_some()
            || edit.mst_revision.is_some()
            || edit.instances.is_some();
        if wants_mst {
            let mst = ConfigTree::ensure_block(stp, "mst", &[]);
            if let Some(name) = &edit.mst_name {
                if name.is_empty() {
                    ConfigTree::remove_leaf(mst, "name");
                } else {
                    ConfigTree::set_leaf(mst, "name", vec![name.clone()]);
                }
            }
            if let Some(revision) = edit.mst_revision {
                ConfigTree::set_leaf(mst, "revision", vec![revision.to_string()]);
            }
            if let Some(instances) = &edit.instances {
                ConfigTree::remove_leaf(mst, "instance");
                for map in instances {
                    let mut sorted = map.vlans.clone();
                    sorted.sort_unstable();
                    sorted.dedup();
                    let mut values = vec![map.instance.to_string(), "vlans".to_string()];
                    values.extend(sorted.iter().map(u16::to_string));
                    push_leaf(mst, "instance", values);
                }
            }
            if mst.is_empty() {
                ConfigTree::remove_block(stp, "mst", &[]);
            }
        }
    }

    for port in &edit.ports {
        if !(port.name.starts_with("Ethernet") || port.name.starts_with("Port-Channel")) {
            return Err(format!("{}: not a spanning-tree port", port.name));
        }
        let interfaces = tree.block_mut("interfaces");
        let eth = ConfigTree::ensure_block(interfaces, &port.name, &[]);
        let stp = ConfigTree::ensure_block(eth, "spanning-tree", &[]);
        if let Some(portfast) = port.portfast {
            if portfast {
                ConfigTree::set_leaf(stp, "portfast", vec![]);
            } else {
                ConfigTree::remove_leaf(stp, "portfast");
            }
        }
        if let Some(bpduguard) = port.bpduguard {
            if bpduguard {
                ConfigTree::set_leaf(stp, "bpduguard", vec![]);
            } else {
                ConfigTree::remove_leaf(stp, "bpduguard");
            }
        }
        match port.cost {
            Some(0) => ConfigTree::remove_leaf(stp, "cost"),
            Some(cost) => ConfigTree::set_leaf(stp, "cost", vec![cost.to_string()]),
            None => {}
        }
        match port.port_priority {
            Some(0) => ConfigTree::remove_leaf(stp, "port-priority"),
            Some(priority) => {
                ConfigTree::set_leaf(stp, "port-priority", vec![priority.to_string()])
            }
            None => {}
        }
        if stp.is_empty() {
            ConfigTree::remove_block(eth, "spanning-tree", &[]);
        }
    }
    Ok(())
}

// ------------------------------------------------------------- MAC table

#[derive(Debug, Deserialize)]
pub struct StaticFdbSet {
    pub mac: String,
    pub vlan: u16,
    /// Absent with `drop` set = drop entry.
    #[serde(default)]
    pub port: Option<String>,
    #[serde(default)]
    pub drop: bool,
}

#[derive(Debug, Deserialize)]
pub struct StaticFdbKey {
    pub mac: String,
    pub vlan: u16,
}

#[derive(Debug, Default, Deserialize)]
pub struct MacTableEdit {
    /// Some(0 or 10..1000000) sets; `clear_aging` reverts to default.
    #[serde(default)]
    pub aging_time: Option<u32>,
    #[serde(default)]
    pub clear_aging: bool,
    #[serde(default)]
    pub set: Vec<StaticFdbSet>,
    #[serde(default)]
    pub delete: Vec<StaticFdbKey>,
}

pub fn apply_mac_table_edit(tree: &mut ConfigTree, edit: &MacTableEdit) -> Result<(), String> {
    if edit.aging_time.is_none() && !edit.clear_aging && edit.set.is_empty() && edit.delete.is_empty()
    {
        return Err("nothing to change".into());
    }
    if let Some(secs) = edit.aging_time {
        if secs != 0 && !(10..=1_000_000).contains(&secs) {
            return Err(format!("bad aging-time {secs} (0 or 10..1000000)"));
        }
    }
    let mut sets = Vec::new();
    for set in &edit.set {
        valid_vlan(set.vlan)?;
        let mac = hemlock_common::net::parse_unicast_mac(&set.mac)?;
        if !set.drop && set.port.is_none() {
            return Err(format!("static {mac}: a port or drop is required"));
        }
        sets.push((mac, set));
    }
    let mut deletes = Vec::new();
    for key in &edit.delete {
        valid_vlan(key.vlan)?;
        deletes.push((hemlock_common::net::parse_mac(&key.mac)?, key.vlan));
    }

    let switching = tree.block_mut("switching");
    let table = ConfigTree::ensure_block(switching, "mac-table", &[]);
    if let Some(secs) = edit.aging_time {
        ConfigTree::set_leaf(table, "aging-time", vec![secs.to_string()]);
    }
    if edit.clear_aging {
        ConfigTree::remove_leaf(table, "aging-time");
    }
    for (mac, set) in sets {
        let vlan = set.vlan.to_string();
        remove_leaf_matching(table, "static", &[&mac, "vlan", &vlan]);
        let values = if set.drop {
            vec![mac, "vlan".into(), vlan, "drop".into()]
        } else {
            vec![
                mac,
                "vlan".into(),
                vlan,
                "interface".into(),
                set.port.clone().unwrap_or_default(),
            ]
        };
        push_leaf(table, "static", values);
    }
    for (mac, vlan) in deletes {
        remove_leaf_matching(table, "static", &[&mac, "vlan", &vlan.to_string()]);
    }
    if table.is_empty() {
        ConfigTree::remove_block(switching, "mac-table", &[]);
    }
    remove_block_if_empty(tree, "switching");
    Ok(())
}

// -------------------------------------------------------------- snooping

#[derive(Debug, Deserialize)]
pub struct SnoopingVlanSet {
    pub vlan: u16,
    #[serde(default)]
    pub disabled: Option<bool>,
    #[serde(default)]
    pub fast_leave: Option<bool>,
    #[serde(default)]
    pub querier: Option<bool>,
    /// "" derives the address.
    #[serde(default)]
    pub querier_address: Option<String>,
    /// When present, replaces the static mrouter set.
    #[serde(default)]
    pub mrouters: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SnoopingEdit {
    /// "igmp" | "mld".
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub disabled: Option<bool>,
    /// 0 clears (back to 2).
    #[serde(default)]
    pub robustness: Option<u8>,
    #[serde(default)]
    pub set: Vec<SnoopingVlanSet>,
    #[serde(default)]
    pub delete: Vec<u16>,
}

pub fn apply_snooping_edit(tree: &mut ConfigTree, edit: &SnoopingEdit) -> Result<(), String> {
    let family = match edit.family.as_str() {
        "igmp" => "igmp-snooping",
        "mld" => "mld-snooping",
        other => return Err(format!("bad snooping family {other:?}")),
    };
    if let Some(robustness) = edit.robustness {
        if robustness > 3 {
            return Err(format!("bad robustness {robustness} (1..3)"));
        }
    }
    for set in &edit.set {
        valid_vlan(set.vlan)?;
        if let Some(address) = &set.querier_address {
            if !address.is_empty() {
                let ok = if edit.family == "igmp" {
                    address.parse::<std::net::Ipv4Addr>().is_ok()
                } else {
                    address.parse::<std::net::Ipv6Addr>().is_ok()
                };
                if !ok {
                    return Err(format!("bad querier address {address:?}"));
                }
            }
        }
    }
    for vlan in &edit.delete {
        valid_vlan(*vlan)?;
    }

    let protocols = tree.block_mut("protocols");
    let block = ConfigTree::ensure_block(protocols, family, &[]);
    match edit.disabled {
        Some(true) => ConfigTree::set_leaf(block, "disable", vec![]),
        Some(false) => ConfigTree::remove_leaf(block, "disable"),
        None => {}
    }
    match edit.robustness {
        Some(0) => ConfigTree::remove_leaf(block, "robustness"),
        Some(n) => ConfigTree::set_leaf(block, "robustness", vec![n.to_string()]),
        None => {}
    }
    for set in &edit.set {
        let id = set.vlan.to_string();
        // Upgrade the bare `vlan <id>` leaf form, then edit the block.
        remove_leaf_matching(block, "vlan", &[&id]);
        let vlan = ConfigTree::ensure_block(block, "vlan", &[&id]);
        match set.disabled {
            Some(true) => ConfigTree::set_leaf(vlan, "disable", vec![]),
            Some(false) => ConfigTree::remove_leaf(vlan, "disable"),
            None => {}
        }
        match set.fast_leave {
            Some(true) => ConfigTree::set_leaf(vlan, "fast-leave", vec![]),
            Some(false) => ConfigTree::remove_leaf(vlan, "fast-leave"),
            None => {}
        }
        match set.querier {
            Some(true) => {
                let values = match &set.querier_address {
                    Some(address) if !address.is_empty() => {
                        vec!["address".to_string(), address.clone()]
                    }
                    _ => vec![],
                };
                ConfigTree::set_leaf(vlan, "querier", values);
            }
            Some(false) => ConfigTree::remove_leaf(vlan, "querier"),
            None => {
                if let Some(address) = &set.querier_address {
                    if !address.is_empty() {
                        ConfigTree::set_leaf(
                            vlan,
                            "querier",
                            vec!["address".to_string(), address.clone()],
                        );
                    }
                }
            }
        }
        if let Some(mrouters) = &set.mrouters {
            ConfigTree::remove_leaf(vlan, "mrouter");
            for port in mrouters {
                push_leaf(vlan, "mrouter", vec!["interface".into(), port.clone()]);
            }
        }
    }
    for vlan in &edit.delete {
        let id = vlan.to_string();
        remove_leaf_matching(block, "vlan", &[&id]);
        ConfigTree::remove_block(block, "vlan", &[&id]);
    }
    if block.is_empty() {
        ConfigTree::remove_block(protocols, family, &[]);
    }
    remove_block_if_empty(tree, "protocols");
    Ok(())
}

// --------------------------------------------------------- storm control

#[derive(Debug, Deserialize)]
pub struct StormSet {
    pub name: String,
    /// "broadcast" | "multicast" | "unknown-unicast".
    pub kind: String,
    /// Percent; empty clears.
    pub level: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct StormEdit {
    #[serde(default)]
    pub set: Vec<StormSet>,
}

pub fn apply_storm_edit(tree: &mut ConfigTree, edit: &StormEdit) -> Result<(), String> {
    if edit.set.is_empty() {
        return Err("nothing to change".into());
    }
    for set in &edit.set {
        if !(set.name.starts_with("Ethernet") || set.name.starts_with("Port-Channel")) {
            return Err(format!("{}: not a storm-control port", set.name));
        }
        if !matches!(set.kind.as_str(), "broadcast" | "multicast" | "unknown-unicast") {
            return Err(format!("bad traffic class {:?}", set.kind));
        }
        if !set.level.is_empty() {
            hemlock_common::net::parse_storm_level(&set.level)?;
        }
    }
    for set in &edit.set {
        let interfaces = tree.block_mut("interfaces");
        let eth = ConfigTree::ensure_block(interfaces, &set.name, &[]);
        if set.level.is_empty() {
            if let Some(sc) = block_children_mut(eth, "storm-control") {
                ConfigTree::remove_leaf(sc, &set.kind);
                if sc.is_empty() {
                    ConfigTree::remove_block(eth, "storm-control", &[]);
                }
            }
        } else {
            let level = hemlock_common::net::parse_storm_level(&set.level)?;
            let sc = ConfigTree::ensure_block(eth, "storm-control", &[]);
            ConfigTree::set_leaf(sc, &set.kind, vec!["level".into(), level]);
        }
    }
    Ok(())
}

// -------------------------------------------------------------- mirroring

#[derive(Debug, Deserialize)]
pub struct MirrorSourceSpec {
    pub port: String,
    /// "rx" | "tx" | "both".
    #[serde(default)]
    pub direction: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MirrorSet {
    pub session: u8,
    #[serde(default)]
    pub destination: Option<String>,
    /// When present, replaces the source set.
    #[serde(default)]
    pub sources: Option<Vec<MirrorSourceSpec>>,
}

#[derive(Debug, Default, Deserialize)]
pub struct MirrorEdit {
    #[serde(default)]
    pub set: Vec<MirrorSet>,
    #[serde(default)]
    pub delete: Vec<u8>,
}

pub fn apply_mirror_edit(tree: &mut ConfigTree, edit: &MirrorEdit) -> Result<(), String> {
    if edit.set.is_empty() && edit.delete.is_empty() {
        return Err("nothing to change".into());
    }
    for set in &edit.set {
        if !(1..=4).contains(&set.session) {
            return Err(format!("bad mirror session {} (1..4)", set.session));
        }
        if let Some(sources) = &set.sources {
            for source in sources {
                if let Some(direction) = &source.direction {
                    if !matches!(direction.as_str(), "rx" | "tx" | "both") {
                        return Err(format!("bad direction {direction:?}"));
                    }
                }
            }
        }
    }
    for session in &edit.delete {
        if !(1..=4).contains(session) {
            return Err(format!("bad mirror session {session} (1..4)"));
        }
    }

    for set in &edit.set {
        let id = set.session.to_string();
        let switching = tree.block_mut("switching");
        let mirror = ConfigTree::ensure_block(switching, "mirror", &[]);
        let block = ConfigTree::ensure_block(mirror, "session", &[&id]);
        if let Some(destination) = &set.destination {
            if destination.is_empty() {
                ConfigTree::remove_leaf(block, "destination");
            } else {
                ConfigTree::set_leaf(block, "destination", vec![destination.clone()]);
            }
        }
        if let Some(sources) = &set.sources {
            ConfigTree::remove_leaf(block, "source");
            for source in sources {
                push_leaf(
                    block,
                    "source",
                    vec![
                        source.port.clone(),
                        source.direction.clone().unwrap_or_else(|| "both".into()),
                    ],
                );
            }
        }
    }
    for session in &edit.delete {
        let id = session.to_string();
        let switching = tree.block_mut("switching");
        if let Some(mirror) = block_children_mut(switching, "mirror") {
            ConfigTree::remove_block(mirror, "session", &[&id]);
            if mirror.is_empty() {
                ConfigTree::remove_block(switching, "mirror", &[]);
            }
        }
    }
    remove_block_if_empty(tree, "switching");
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
    fn lag_edit_reconciles_members() {
        let mut t = tree("interfaces { Ethernet49 { channel-group 1 mode active } }");
        apply_lag_edit(
            &mut t,
            &LagEdit {
                set: vec![LagSet {
                    group: 1,
                    description: Some("uplink".into()),
                    min_links: Some(1),
                    fallback: Some("individual".into()),
                    fallback_timeout: Some(90),
                    mode: Some("trunk".into()),
                    access_vlan: None,
                    trunk_vlans: Some(vec![20, 10]),
                    native_vlan: None,
                    members: Some(vec![
                        LagMemberSpec {
                            port: "Ethernet50".into(),
                            mode: "active".into(),
                            rate_fast: true,
                            port_priority: None,
                        },
                        LagMemberSpec {
                            port: "Ethernet51".into(),
                            mode: "active".into(),
                            rate_fast: false,
                            port_priority: Some(100),
                        },
                    ]),
                }],
                delete: vec![],
            },
        )
        .unwrap();
        let text = t.to_text();
        // Et49 left the group; Et50/51 joined with their tuning.
        assert!(!text.contains("Ethernet49 {\n        channel-group"));
        assert!(text.contains("channel-group 1 mode active"));
        assert!(text.contains("rate fast"));
        assert!(text.contains("port-priority 100"));
        assert!(text.contains("min-links 1"));
        assert!(text.contains("fallback individual"));
        assert!(text.contains("fallback-timeout 90"));
        assert!(text.contains("trunk vlans 10 20"));
        // Round-trips.
        assert_eq!(hemlock_config::parse(&text).unwrap(), t);

        // Delete tears the Po and its members' channel-groups down.
        apply_lag_edit(
            &mut t,
            &LagEdit {
                set: vec![],
                delete: vec![1],
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(!text.contains("channel-group"));
        assert!(!text.contains("Port-Channel1"));
    }

    #[test]
    fn stp_edit_writes_global_and_ports() {
        let mut t = tree("");
        apply_stp_edit(
            &mut t,
            &StpEdit {
                mode: Some("mstp".into()),
                priority: Some(4096),
                mst_name: Some("QS-CORE".into()),
                mst_revision: Some(3),
                instances: Some(vec![MstInstanceEdit {
                    instance: 1,
                    vlans: vec![20, 10],
                }]),
                ports: vec![StpPortEdit {
                    name: "Ethernet1".into(),
                    portfast: Some(true),
                    bpduguard: Some(true),
                    cost: None,
                    port_priority: Some(112),
                }],
                ..StpEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("mode mstp"));
        assert!(text.contains("priority 4096"));
        assert!(text.contains("name QS-CORE"));
        assert!(text.contains("instance 1 vlans 10 20"));
        assert!(text.contains("portfast"));
        assert!(text.contains("port-priority 112"));
        // Invalid priorities are rejected.
        assert!(apply_stp_edit(
            &mut t,
            &StpEdit {
                priority: Some(4095),
                ..StpEdit::default()
            }
        )
        .is_err());
        assert!(apply_stp_edit(
            &mut t,
            &StpEdit {
                ports: vec![StpPortEdit {
                    name: "Ethernet1".into(),
                    portfast: None,
                    bpduguard: None,
                    cost: None,
                    port_priority: Some(100),
                }],
                ..StpEdit::default()
            }
        )
        .is_err());
    }

    #[test]
    fn mac_table_edit_sets_and_deletes() {
        let mut t = tree("");
        apply_mac_table_edit(
            &mut t,
            &MacTableEdit {
                aging_time: Some(600),
                set: vec![StaticFdbSet {
                    mac: "0050.56BE.EF01".into(),
                    vlan: 10,
                    port: Some("Ethernet3".into()),
                    drop: false,
                }],
                ..MacTableEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("aging-time 600"));
        assert!(text.contains("static 00:50:56:be:ef:01 vlan 10 interface Ethernet3"));

        apply_mac_table_edit(
            &mut t,
            &MacTableEdit {
                clear_aging: true,
                delete: vec![StaticFdbKey {
                    mac: "00:50:56:be:ef:01".into(),
                    vlan: 10,
                }],
                ..MacTableEdit::default()
            },
        )
        .unwrap();
        assert_eq!(t.to_text(), "");
        // Multicast statics are rejected.
        assert!(apply_mac_table_edit(
            &mut t,
            &MacTableEdit {
                set: vec![StaticFdbSet {
                    mac: "01:00:5e:00:00:01".into(),
                    vlan: 10,
                    port: Some("Ethernet1".into()),
                    drop: false,
                }],
                ..MacTableEdit::default()
            }
        )
        .is_err());
    }

    #[test]
    fn snooping_edit_mirrors_cli_shapes() {
        let mut t = tree("");
        apply_snooping_edit(
            &mut t,
            &SnoopingEdit {
                family: "igmp".into(),
                set: vec![SnoopingVlanSet {
                    vlan: 10,
                    disabled: None,
                    fast_leave: Some(true),
                    querier: Some(true),
                    querier_address: Some("10.0.20.1".into()),
                    mrouters: Some(vec!["Port-Channel1".into()]),
                }],
                ..SnoopingEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("igmp-snooping"));
        assert!(text.contains("fast-leave"));
        assert!(text.contains("querier address 10.0.20.1"));
        assert!(text.contains("mrouter interface Port-Channel1"));
        // Deleting the VLAN empties the family and the block.
        apply_snooping_edit(
            &mut t,
            &SnoopingEdit {
                family: "igmp".into(),
                delete: vec![10],
                ..SnoopingEdit::default()
            },
        )
        .unwrap();
        assert_eq!(t.to_text(), "");
        // MLD rejects an IPv4 querier address.
        assert!(apply_snooping_edit(
            &mut t,
            &SnoopingEdit {
                family: "mld".into(),
                set: vec![SnoopingVlanSet {
                    vlan: 10,
                    disabled: None,
                    fast_leave: None,
                    querier: Some(true),
                    querier_address: Some("10.0.0.1".into()),
                    mrouters: None,
                }],
                ..SnoopingEdit::default()
            }
        )
        .is_err());
    }

    #[test]
    fn storm_and_mirror_edits() {
        let mut t = tree("");
        apply_storm_edit(
            &mut t,
            &StormEdit {
                set: vec![StormSet {
                    name: "Ethernet1".into(),
                    kind: "broadcast".into(),
                    level: "10".into(),
                }],
            },
        )
        .unwrap();
        assert!(t.to_text().contains("broadcast level 10.00"));
        apply_storm_edit(
            &mut t,
            &StormEdit {
                set: vec![StormSet {
                    name: "Ethernet1".into(),
                    kind: "broadcast".into(),
                    level: String::new(),
                }],
            },
        )
        .unwrap();
        // The level clears; the (now empty) interface node is fine —
        // the CLI leaves those too.
        assert!(!t.to_text().contains("storm-control"));

        apply_mirror_edit(
            &mut t,
            &MirrorEdit {
                set: vec![MirrorSet {
                    session: 1,
                    destination: Some("Ethernet4".into()),
                    sources: Some(vec![
                        MirrorSourceSpec {
                            port: "Ethernet1".into(),
                            direction: None,
                        },
                        MirrorSourceSpec {
                            port: "Ethernet2".into(),
                            direction: Some("rx".into()),
                        },
                    ]),
                }],
                delete: vec![],
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("session 1"));
        assert!(text.contains("source Ethernet1 both"));
        assert!(text.contains("source Ethernet2 rx"));
        assert!(text.contains("destination Ethernet4"));
        apply_mirror_edit(
            &mut t,
            &MirrorEdit {
                set: vec![],
                delete: vec![1],
            },
        )
        .unwrap();
        assert!(!t.to_text().contains("mirror"));
    }
}
