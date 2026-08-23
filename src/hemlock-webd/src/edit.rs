//! Config edits driven by the web console: interface attributes and
//! the VLAN table.
//!
//! Edits write the same leaves and phrases hemlockctl does (`no
//! shutdown`, `switchport { mode trunk; trunk vlans ... }`), are based
//! on the running config, and go through mgmtd's normal SetCandidate +
//! Commit path — so validation, the rollback ring, and `show
//! configuration` behave exactly as if the change came from the CLI.

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
        if !(name.starts_with("Ethernet") || name.starts_with("Management")) {
            return Err(format!("{name}: not an editable interface"));
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
    if let Some(id) = edit.native_vlan {
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
    if let Some(id) = edit.native_vlan {
        if let Some(sp) = block_children_mut(eth, "switchport") {
            ConfigTree::set_phrase(sp, "native", "vlan", vec![id.to_string()]);
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
}

/// Create/update and delete VLANs in one request.
#[derive(Debug, Default, Deserialize)]
pub struct VlanEdit {
    #[serde(default)]
    pub set: Vec<VlanSet>,
    #[serde(default)]
    pub delete: Vec<u16>,
}

#[derive(Debug, Deserialize)]
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
            names: vec!["Vlan10".into()],
            ..InterfaceEdit::default()
        })
        .contains("not an editable"));
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
                description: None
            }],
            delete: vec![]
        })
        .contains("bad VLAN id"));
    }
}
