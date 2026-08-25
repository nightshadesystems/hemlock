//! Config edits driven by the web console: interface attributes and
//! the VLAN table.
//!
//! Edits write the same leaves and phrases hemlockctl does (`no
//! shutdown`, `switchport { mode trunk; trunk vlans ... }`), are based
//! on the running config, and go through mgmtd's normal SetCandidate +
//! Commit path — so validation, the rollback ring, and `show
//! configuration` behave exactly as if the change came from the CLI.

use hemlock_common::link;
use hemlock_config::{ConfigTree, Item};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Access,
    Trunk,
    /// QinQ tunnel port; the S-VLAN is the access VLAN.
    Dot1qTunnel,
    Routed,
}

/// One edit applied to every interface in `names`. Absent fields stay
/// untouched; an empty `description`/`address` clears the leaf.
#[derive(Debug, Default, Deserialize)]
pub struct InterfaceEdit {
    pub names: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub admin_up: Option<bool>,
    #[serde(default)]
    pub mode: Option<Mode>,
    #[serde(default)]
    pub access_vlan: Option<u16>,
    #[serde(default)]
    pub trunk_vlans: Option<Vec<u16>>,
    #[serde(default)]
    pub native_vlan: Option<u16>,
    #[serde(default)]
    pub address: Option<String>,
    /// Pinned line rate in Mb/s; `0` = auto-negotiate. Front-panel
    /// ports only.
    #[serde(default)]
    pub speed_mbps: Option<u32>,
    /// `"auto"`, `"full"` or `"half"`. Front-panel ports only.
    #[serde(default)]
    pub duplex: Option<String>,
    /// L2 MTU in bytes; `0` = back to the platform default.
    #[serde(default)]
    pub mtu: Option<u32>,
}

fn valid_vlan(id: u16) -> Result<(), String> {
    if (1..=4094).contains(&id) {
        Ok(())
    } else {
        Err(format!("bad VLAN id {id} (1..4094)"))
    }
}

pub fn apply_interface_edit(tree: &mut ConfigTree, edit: &InterfaceEdit) -> Result<(), String> {
    if edit.names.is_empty() {
        return Err("no interfaces selected".into());
    }
    for name in &edit.names {
        let front_panel = name.starts_with("Ethernet");
        if !(front_panel || name.starts_with("Management") || name.starts_with("Vlan")) {
            return Err(format!("{name}: not an editable interface"));
        }
        if !front_panel && (edit.speed_mbps.is_some() || edit.duplex.is_some()) {
            return Err(format!(
                "{name}: speed and duplex are front-panel port settings"
            ));
        }
        if name.starts_with("Vlan") && edit.mode.is_some() {
            return Err(format!("{name}: VLAN interfaces are always routed"));
        }
        if name.starts_with("Management")
            && matches!(
                edit.mode,
                Some(Mode::Access | Mode::Trunk | Mode::Dot1qTunnel)
            )
        {
            return Err(format!("{name}: management ports are not switchports"));
        }
    }
    if let Some(id) = edit.access_vlan {
        valid_vlan(id)?;
    }
    // 0 is the console's "no native VLAN" sentinel, not a VLAN id.
    if let Some(id) = edit.native_vlan.filter(|id| *id != 0) {
        valid_vlan(id)?;
    }
    if let Some(vlans) = &edit.trunk_vlans {
        for id in vlans {
            valid_vlan(*id)?;
        }
    }
    if let Some(address) = &edit.address {
        if !address.is_empty() {
            hemlock_common::net::parse_cidr(address)?;
        }
    }
    if let Some(mtu) = edit.mtu {
        if mtu != 0 {
            link::valid_mtu(mtu)?;
        }
    }
    if let Some(duplex) = &edit.duplex {
        if !matches!(duplex.as_str(), "auto" | "full" | "half") {
            return Err(format!("bad duplex {duplex:?} (auto, full or half)"));
        }
    }

    for name in &edit.names {
        let interfaces = tree.block_mut("interfaces");
        let eth = ConfigTree::ensure_block(interfaces, name, &[]);
        apply_one(eth, edit);
    }
    Ok(())
}

fn apply_one(eth: &mut Vec<Item>, edit: &InterfaceEdit) {
    if let Some(description) = &edit.description {
        if description.is_empty() {
            ConfigTree::remove_leaf(eth, "description");
        } else {
            ConfigTree::set_leaf(eth, "description", vec![description.clone()]);
        }
    }

    match edit.admin_up {
        Some(true) => {
            ConfigTree::set_phrase(eth, "no", "shutdown", vec![]);
            ConfigTree::remove_leaf(eth, "shutdown");
        }
        Some(false) => {
            ConfigTree::set_leaf(eth, "shutdown", vec![]);
            ConfigTree::remove_leaf(eth, "no");
        }
        None => {}
    }

    match edit.mode {
        // Routed: address and switchport are mutually exclusive.
        Some(Mode::Routed) => {
            ConfigTree::remove_block(eth, "switchport", &[]);
        }
        Some(mode @ (Mode::Access | Mode::Trunk | Mode::Dot1qTunnel)) => {
            ConfigTree::remove_leaf(eth, "address");
            let sp = ConfigTree::ensure_block(eth, "switchport", &[]);
            ConfigTree::set_leaf(
                sp,
                "mode",
                vec![match mode {
                    Mode::Trunk => "trunk",
                    Mode::Dot1qTunnel => "dot1q-tunnel",
                    _ => "access",
                }
                .to_string()],
            );
            if mode == Mode::Trunk {
                // A trunk carries no access VLAN (mirrors the CLI).
                ConfigTree::remove_leaf(sp, "access");
            } else {
                // Access and dot1q-tunnel carry no trunk config; the
                // tunnel's S-VLAN is the access VLAN.
                ConfigTree::remove_leaf(sp, "trunk");
                ConfigTree::remove_leaf(sp, "native");
            }
        }
        None => {}
    }

    // VLAN membership fields land inside an existing (or just-created)
    // switchport block; without one they are meaningless and skipped.
    if let Some(id) = edit.access_vlan {
        if let Some(sp) = block_children_mut(eth, "switchport") {
            ConfigTree::set_phrase(sp, "access", "vlan", vec![id.to_string()]);
        }
    }
    if let Some(vlans) = &edit.trunk_vlans {
        if let Some(sp) = block_children_mut(eth, "switchport") {
            if vlans.is_empty() {
                ConfigTree::remove_leaf(sp, "trunk");
            } else {
                let mut sorted = vlans.clone();
                sorted.sort_unstable();
                sorted.dedup();
                // Comma-words render as `trunk vlans 10, 20, 30` (the
                // same shape the CLI stores).
                let words = sorted
                    .iter()
                    .enumerate()
                    .map(|(i, id)| {
                        if i + 1 < sorted.len() {
                            format!("{id},")
                        } else {
                            id.to_string()
                        }
                    })
                    .collect();
                ConfigTree::set_phrase(sp, "trunk", "vlans", words);
            }
        }
    }
    // 0 is not a VLAN id; the console sends it to mean "no native
    // VLAN", which an omitted field could not express.
    if let Some(id) = edit.native_vlan {
        if let Some(sp) = block_children_mut(eth, "switchport") {
            if id == 0 {
                ConfigTree::remove_leaf(sp, "native");
            } else {
                ConfigTree::set_phrase(sp, "native", "vlan", vec![id.to_string()]);
            }
        }
    }

    if let Some(address) = &edit.address {
        if address.is_empty() {
            ConfigTree::remove_leaf(eth, "address");
        } else {
            ConfigTree::remove_block(eth, "switchport", &[]);
            ConfigTree::set_leaf(eth, "address", vec![address.clone()]);
        }
    }

    // Link parameters. The "stop forcing" spellings (`0` and `auto`)
    // delete the leaf rather than writing a no-op one, so `show
    // configuration` stays as short as the CLI would leave it.
    if let Some(mbps) = edit.speed_mbps {
        if mbps == 0 {
            ConfigTree::remove_leaf(eth, "speed");
        } else {
            ConfigTree::set_leaf(eth, "speed", vec![mbps.to_string()]);
        }
    }
    if let Some(duplex) = &edit.duplex {
        if duplex == "auto" {
            ConfigTree::remove_leaf(eth, "duplex");
        } else {
            ConfigTree::set_leaf(eth, "duplex", vec![duplex.clone()]);
        }
    }
    if let Some(mtu) = edit.mtu {
        if mtu == 0 {
            ConfigTree::remove_leaf(eth, "mtu");
        } else {
            ConfigTree::set_leaf(eth, "mtu", vec![mtu.to_string()]);
        }
    }
}

/// Create/update and delete VLANs in one request.
#[derive(Debug, Default, Deserialize)]
pub struct VlanEdit {
    #[serde(default)]
    pub set: Vec<VlanSet>,
    #[serde(default)]
    pub delete: Vec<u16>,
}

#[derive(Debug, Default, Deserialize)]
pub struct VlanSet {
    pub id: u16,
    /// The VLAN's display name (config `description`); empty clears it.
    #[serde(default)]
    pub description: Option<String>,
}

pub fn apply_vlan_edit(tree: &mut ConfigTree, edit: &VlanEdit) -> Result<(), String> {
    if edit.set.is_empty() && edit.delete.is_empty() {
        return Err("nothing to change".into());
    }
    for set in &edit.set {
        valid_vlan(set.id)?;
    }
    for id in &edit.delete {
        valid_vlan(*id)?;
        if *id == 1 {
            return Err("VLAN 1 is the default VLAN and cannot be deleted".into());
        }
    }

    for set in &edit.set {
        let id = set.id.to_string();
        let vlans = tree.block_mut("vlans");
        let vlan = ConfigTree::ensure_block(vlans, "vlan", &[&id]);
        match &set.description {
            Some(description) if description.is_empty() => {
                ConfigTree::remove_leaf(vlan, "description");
            }
            Some(description) => {
                ConfigTree::set_leaf(vlan, "description", vec![description.clone()]);
            }
            None => {}
        }
    }
    for id in &edit.delete {
        let key = id.to_string();
        let vlans = tree.block_mut("vlans");
        ConfigTree::remove_block(vlans, "vlan", &[&key]);
        // Its SVI (if any) references the VLAN and would fail commit
        // validation dangling — take it out with the VLAN.
        let interfaces = tree.block_mut("interfaces");
        ConfigTree::remove_block(interfaces, &format!("Vlan{id}"), &[]);
    }
    remove_block_if_empty(tree, "vlans");
    remove_block_if_empty(tree, "interfaces");
    Ok(())
}

/// Create/update and delete SVIs (routed VLAN interfaces) in one
/// request. An SVI is `interfaces { Vlan<id> { address ...; mtu ... } }`
/// — there is no separate object to create, so writing an address is
/// what brings one into being and clearing every leaf is what removes
/// it. The VLAN itself must already exist; mgmtd rejects a dangling SVI
/// at commit with that exact message.
#[derive(Debug, Default, Deserialize)]
pub struct SviEdit {
    #[serde(default)]
    pub set: Vec<SviSet>,
    #[serde(default)]
    pub delete: Vec<u16>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SviSet {
    /// The VLAN the interface fronts; the interface is named `Vlan<id>`.
    pub vlan: u16,
    /// Address in CIDR form; empty clears it. Absent leaves it alone.
    #[serde(default)]
    pub address: Option<String>,
    /// MTU in bytes; `0` restores the default. Absent leaves it alone.
    #[serde(default)]
    pub mtu: Option<u32>,
}

pub fn apply_svi_edit(tree: &mut ConfigTree, edit: &SviEdit) -> Result<(), String> {
    if edit.set.is_empty() && edit.delete.is_empty() {
        return Err("nothing to change".into());
    }
    for set in &edit.set {
        valid_vlan(set.vlan)?;
        if let Some(address) = &set.address {
            if !address.is_empty() {
                hemlock_common::net::parse_cidr(address)?;
            }
        }
        if let Some(mtu) = set.mtu.filter(|m| *m != 0) {
            link::valid_mtu(mtu)?;
        }
    }
    for vlan in &edit.delete {
        valid_vlan(*vlan)?;
    }

    for set in &edit.set {
        let name = format!("Vlan{}", set.vlan);
        let interfaces = tree.block_mut("interfaces");
        let svi = ConfigTree::ensure_block(interfaces, &name, &[]);
        if let Some(address) = &set.address {
            if address.is_empty() {
                ConfigTree::remove_leaf(svi, "address");
            } else {
                ConfigTree::set_leaf(svi, "address", vec![address.clone()]);
            }
        }
        if let Some(mtu) = set.mtu {
            if mtu == 0 {
                ConfigTree::remove_leaf(svi, "mtu");
            } else {
                ConfigTree::set_leaf(svi, "mtu", vec![mtu.to_string()]);
            }
        }
        // An `interfaces { Vlan10 { } }` husk would keep the SVI listed
        // with nothing configured on it.
        if svi.is_empty() {
            ConfigTree::remove_block(tree.block_mut("interfaces"), &name, &[]);
        }
    }
    for vlan in &edit.delete {
        let interfaces = tree.block_mut("interfaces");
        ConfigTree::remove_block(interfaces, &format!("Vlan{vlan}"), &[]);
    }
    remove_block_if_empty(tree, "interfaces");
    Ok(())
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
    fn edits_description_and_admin_state() {
        let mut t = tree("");
        apply_interface_edit(
            &mut t,
            &InterfaceEdit {
                names: vec!["Ethernet1".into(), "Ethernet2".into()],
                description: Some("uplink".into()),
                admin_up: Some(false),
                ..InterfaceEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("Ethernet1"));
        assert!(text.contains("description uplink"));
        assert!(text.contains("shutdown"));

        // Re-enable clears the marker; empty description clears the leaf.
        apply_interface_edit(
            &mut t,
            &InterfaceEdit {
                names: vec!["Ethernet1".into()],
                description: Some(String::new()),
                admin_up: Some(true),
                ..InterfaceEdit::default()
            },
        )
        .unwrap();
        let (_, interfaces) = t.block("interfaces").unwrap();
        let (_, e1) = ConfigTree::blocks_named(interfaces, "Ethernet1")
            .next()
            .unwrap();
        assert!(ConfigTree::has_phrase(e1, "no", "shutdown"));
        assert!(!ConfigTree::has_leaf(e1, "shutdown"));
        assert_eq!(ConfigTree::leaf_value(e1, "description"), None);
    }

    #[test]
    fn mode_edits_mirror_cli_semantics() {
        let mut t = tree("interfaces { Ethernet1 { address 10.0.0.1/24 } }");
        apply_interface_edit(
            &mut t,
            &InterfaceEdit {
                names: vec!["Ethernet1".into()],
                mode: Some(Mode::Trunk),
                trunk_vlans: Some(vec![20, 10, 20]),
                native_vlan: Some(5),
                ..InterfaceEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        // Trunk replaced the address; the list is sorted, deduplicated,
        // and rendered comma-separated.
        assert!(!text.contains("address"));
        assert!(text.contains("mode trunk"));
        assert!(text.contains("trunk vlans 10, 20"));
        assert!(text.contains("native vlan 5"));

        // Back to routed: the switchport block goes away.
        apply_interface_edit(
            &mut t,
            &InterfaceEdit {
                names: vec!["Ethernet1".into()],
                mode: Some(Mode::Routed),
                address: Some("10.0.0.1/24".into()),
                ..InterfaceEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(!text.contains("switchport"));
        assert!(text.contains("address 10.0.0.1/24"));

        // Access mode cleans the trunk leaves up.
        apply_interface_edit(
            &mut t,
            &InterfaceEdit {
                names: vec!["Ethernet1".into()],
                mode: Some(Mode::Access),
                access_vlan: Some(30),
                ..InterfaceEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("mode access"));
        assert!(text.contains("access vlan 30"));
        assert!(!text.contains("address"));
    }

    #[test]
    fn rejects_bad_interface_edits() {
        let mut t = tree("");
        let e = |edit: &InterfaceEdit| apply_interface_edit(&mut tree(""), edit).unwrap_err();
        assert!(e(&InterfaceEdit::default()).contains("no interfaces"));
        assert!(e(&InterfaceEdit {
            names: vec!["Port-Channel1".into()],
            ..InterfaceEdit::default()
        })
        .contains("not an editable"));
        // SVIs take an address and an MTU, but never a switchport mode
        // or a link pin.
        assert!(e(&InterfaceEdit {
            names: vec!["Vlan10".into()],
            mode: Some(Mode::Access),
            ..InterfaceEdit::default()
        })
        .contains("always routed"));
        assert!(e(&InterfaceEdit {
            names: vec!["Vlan10".into()],
            speed_mbps: Some(1000),
            ..InterfaceEdit::default()
        })
        .contains("front-panel"));
        assert!(e(&InterfaceEdit {
            names: vec!["Ethernet1".into()],
            mtu: Some(9999),
            ..InterfaceEdit::default()
        })
        .contains("bad MTU"));
        assert!(e(&InterfaceEdit {
            names: vec!["Ethernet1".into()],
            duplex: Some("quarter".into()),
            ..InterfaceEdit::default()
        })
        .contains("bad duplex"));
        assert!(e(&InterfaceEdit {
            names: vec!["Management1".into()],
            mode: Some(Mode::Trunk),
            ..InterfaceEdit::default()
        })
        .contains("not switchports"));
        assert!(e(&InterfaceEdit {
            names: vec!["Ethernet1".into()],
            access_vlan: Some(5000),
            ..InterfaceEdit::default()
        })
        .contains("bad VLAN id"));
        assert!(apply_interface_edit(
            &mut t,
            &InterfaceEdit {
                names: vec!["Ethernet1".into()],
                address: Some("banana".into()),
                ..InterfaceEdit::default()
            }
        )
        .is_err());
    }

    #[test]
    fn vlan_create_update_delete() {
        let mut t = tree("");
        apply_vlan_edit(
            &mut t,
            &VlanEdit {
                set: vec![VlanSet {
                    id: 10,
                    description: Some("Management".into()),
                }],
                delete: vec![],
            },
        )
        .unwrap();
        assert!(t.to_text().contains("vlan 10"));
        assert!(t.to_text().contains("description Management"));

        // Clearing the name keeps the VLAN.
        apply_vlan_edit(
            &mut t,
            &VlanEdit {
                set: vec![VlanSet {
                    id: 10,
                    description: Some(String::new()),
                }],
                delete: vec![],
            },
        )
        .unwrap();
        assert!(t.to_text().contains("vlan 10"));
        assert!(!t.to_text().contains("description"));

        // Deleting removes the VLAN, its SVI, and the emptied blocks.
        let mut t = tree("vlans { vlan 10 { } }\ninterfaces { Vlan10 { address 10.0.10.1/24 } }");
        apply_vlan_edit(
            &mut t,
            &VlanEdit {
                set: vec![],
                delete: vec![10],
            },
        )
        .unwrap();
        assert_eq!(t.to_text(), "");
    }

    #[test]
    fn rejects_bad_vlan_edits() {
        let err = |edit: &VlanEdit| apply_vlan_edit(&mut tree(""), edit).unwrap_err();
        assert!(err(&VlanEdit::default()).contains("nothing to change"));
        assert!(err(&VlanEdit {
            set: vec![],
            delete: vec![1]
        })
        .contains("default VLAN"));
        assert!(err(&VlanEdit {
            set: vec![VlanSet {
                id: 0,
                ..Default::default()
            }],
            delete: vec![]
        })
        .contains("bad VLAN id"));
    }

    #[test]
    fn clearing_the_native_vlan_removes_it() {
        let mut t = tree(
            "interfaces { Ethernet1 { switchport { mode trunk
trunk vlans 10, 20
native vlan 54 } } }",
        );
        apply_interface_edit(
            &mut t,
            &InterfaceEdit {
                names: vec!["Ethernet1".into()],
                mode: Some(Mode::Trunk),
                trunk_vlans: Some(vec![10, 20]),
                // 0 is the console's "no native VLAN" — an omitted
                // field would leave the old one in place.
                native_vlan: Some(0),
                ..InterfaceEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("trunk vlans 10, 20"));
        assert!(!text.contains("native"));
    }

    #[test]
    fn edits_link_parameters() {
        let mut t = tree("");
        apply_interface_edit(
            &mut t,
            &InterfaceEdit {
                names: vec!["Ethernet1".into()],
                speed_mbps: Some(100),
                duplex: Some("half".into()),
                mtu: Some(9216),
                ..InterfaceEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("speed 100"));
        assert!(text.contains("duplex half"));
        assert!(text.contains("mtu 9216"));

        // The "stop forcing" spellings delete the leaves outright.
        apply_interface_edit(
            &mut t,
            &InterfaceEdit {
                names: vec!["Ethernet1".into()],
                speed_mbps: Some(0),
                duplex: Some("auto".into()),
                mtu: Some(0),
                ..InterfaceEdit::default()
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(!text.contains("speed"));
        assert!(!text.contains("duplex"));
        assert!(!text.contains("mtu"));
    }

    #[test]
    fn creates_and_removes_an_svi() {
        let mut t = tree("vlans { vlan 10 { } }");
        apply_svi_edit(
            &mut t,
            &SviEdit {
                set: vec![SviSet {
                    vlan: 10,
                    address: Some("10.0.10.1/24".into()),
                    mtu: Some(9216),
                }],
                delete: vec![],
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("Vlan10"));
        assert!(text.contains("address 10.0.10.1/24"));
        assert!(text.contains("mtu 9216"));

        // Clearing every leaf takes the interface block with it, so the
        // SVI stops being listed rather than lingering empty.
        apply_svi_edit(
            &mut t,
            &SviEdit {
                set: vec![SviSet {
                    vlan: 10,
                    address: Some(String::new()),
                    mtu: Some(0),
                }],
                delete: vec![],
            },
        )
        .unwrap();
        assert!(!t.to_text().contains("Vlan10"));

        // Delete removes the whole block in one step.
        apply_svi_edit(
            &mut t,
            &SviEdit {
                set: vec![SviSet {
                    vlan: 20,
                    address: Some("10.0.20.1/24".into()),
                    mtu: None,
                }],
                delete: vec![],
            },
        )
        .unwrap();
        assert!(t.to_text().contains("Vlan20"));
        apply_svi_edit(
            &mut t,
            &SviEdit {
                set: vec![],
                delete: vec![20],
            },
        )
        .unwrap();
        assert!(!t.to_text().contains("Vlan20"));
    }

    #[test]
    fn rejects_bad_svi_edits() {
        let err = |edit: &SviEdit| apply_svi_edit(&mut tree(""), edit).unwrap_err();
        assert!(err(&SviEdit::default()).contains("nothing to change"));
        assert!(err(&SviEdit {
            set: vec![SviSet {
                vlan: 10,
                address: Some("10.0.10.1".into()),
                mtu: None,
            }],
            delete: vec![],
        })
        .contains("prefix"));
        assert!(err(&SviEdit {
            set: vec![SviSet {
                vlan: 10,
                address: None,
                mtu: Some(70_000),
            }],
            delete: vec![],
        })
        .contains("bad MTU"));
        assert!(err(&SviEdit {
            set: vec![SviSet {
                vlan: 5000,
                address: None,
                mtu: None,
            }],
            delete: vec![],
        })
        .contains("bad VLAN id"));
    }
}
