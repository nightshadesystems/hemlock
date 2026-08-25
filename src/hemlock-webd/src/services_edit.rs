//! Config edits for the services-suite pages: LLDP and NTP.
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
