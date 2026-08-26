//! `show configuration` — the running configuration merged with the
//! live interface inventory.
//!
//! The rest of the once-inline system shows (`version`, `switch`,
//! `environment`) moved into the `system/` module with the rest of
//! the suite, so they have a data model, a `| json` form and goldens
//! like every other family.

use anyhow::{Context, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;

/// `show configuration` — the running configuration merged with the full
/// interface inventory, so every stock port (and the management port from
/// the platform manifest) renders in curly-brace form even before it has
/// ever been explicitly configured. Unconfigured leaves are filled from
/// live state, and a `system` block carries the hostname.
pub async fn configuration(
    syncd: IpcEndpoint,
    mgmtd: IpcEndpoint,
    platform_dir: &str,
) -> Result<()> {
    use hemlock_config::ConfigTree;

    let channel = mgmtd.connect().await.context("connecting to mgmtd")?;
    let mut client = pb::mgmt_client::MgmtClient::new(channel);
    let text = client
        .get_config(pb::GetConfigRequest {
            source: pb::ConfigSource::Running as i32,
        })
        .await?
        .into_inner()
        .text;
    let mut tree = hemlock_config::parse(&text)
        .map_err(|e| anyhow::anyhow!("running config unparsable: {e}"))?;
    tree.normalize_interfaces();

    let channel = syncd.connect().await.context("connecting to syncd")?;
    let mut client = pb::syncd_client::SyncdClient::new(channel);
    let mut ports = client
        .list_ports(pb::ListPortsRequest {})
        .await?
        .into_inner()
        .ports;
    ports.sort_by_key(|p| p.index);

    // VLANs: the default VLAN 1 always exists, so the config always
    // shows it (alongside any configured VLANs), in numeric order.
    let vlan_ids: Vec<String> = {
        let vlans = tree.block_mut("vlans");
        ConfigTree::ensure_block(vlans, "vlan", &["1"]);
        vlans.sort_by_key(|item| match item {
            hemlock_config::Item::Block { keys, .. } => keys
                .first()
                .and_then(|k| k.parse().ok())
                .unwrap_or(u16::MAX),
            hemlock_config::Item::Leaf { .. } => u16::MAX,
        });
        ConfigTree::blocks_named(vlans, "vlan")
            .filter_map(|(keys, _)| keys.first().cloned())
            .collect()
    };

    {
        let interfaces = tree.block_mut("interfaces");
        for p in &ports {
            let eth = ConfigTree::ensure_block(interfaces, p.name.as_str(), &[]);
            if !has_admin_leaf(eth) {
                if p.admin_state == pb::AdminState::Up as i32 {
                    ConfigTree::set_phrase(eth, "no", "shutdown", vec![]);
                } else {
                    ConfigTree::set_leaf(eth, "shutdown", vec![]);
                }
            }
            if !p.description.is_empty() && ConfigTree::leaf_value(eth, "description").is_none() {
                ConfigTree::set_leaf(eth, "description", vec![p.description.clone()]);
            }
            // Default L2: every unconfigured switch port is an access
            // port in VLAN 1 (routed ports excepted).
            let routed = ConfigTree::leaf_value(eth, "address").is_some();
            let has_switchport = ConfigTree::blocks_named(eth, "switchport").next().is_some();
            if !routed && !has_switchport {
                let sp = ConfigTree::ensure_block(eth, "switchport", &[]);
                ConfigTree::set_leaf(sp, "mode", vec!["access".into()]);
                ConfigTree::set_phrase(sp, "access", "vlan", vec!["1".into()]);
            }
        }

        // Every VLAN is an interface, too.
        for id in &vlan_ids {
            ConfigTree::ensure_block(interfaces, &format!("Vlan{id}"), &[]);
        }

        // Management port: an OS netdev named by the manifest, not an ASIC
        // port. Skipped quietly when no manifest is present (dev hosts).
        if let Ok(platform) = hemlock_platform::Platform::find("/", platform_dir) {
            if let Some(mgmt) = &platform.manifest.management {
                let block = ConfigTree::ensure_block(interfaces, mgmt.interface.as_str(), &[]);
                if !has_admin_leaf(block) {
                    match os_netdev_is_up(&mgmt.os_device) {
                        Some(true) => ConfigTree::set_phrase(block, "no", "shutdown", vec![]),
                        Some(false) => ConfigTree::set_leaf(block, "shutdown", vec![]),
                        None => {}
                    }
                }
            }
        }

        // Deterministic display order regardless of which ports were
        // explicitly configured first: ethernet by port number, then
        // management, then VLANs.
        sort_interface_blocks(interfaces);
    }

    // System identity: the hostname is filled from the OS when the
    // running config does not set it, so the rendered config always
    // says what the box is actually called.
    //
    // Login users are *not* synthesized from the OS any more: since the
    // system suite the config is the source of truth for them, and
    // showing OS accounts as if they were configuration would invite
    // an operator to save a file that then removes them.
    {
        let system = tree.block_mut("system");
        if ConfigTree::leaf_value(system, "hostname").is_none() {
            ConfigTree::set_leaf(system, "hostname", vec![crate::cli::read_hostname()]);
        }
    }

    // RADIUS shared secrets never render: the security suite is the
    // first family to store one, so it sets the convention — secret
    // leaves display as `<hidden>` (recorded in docs/architecture.md).
    tree.redact_secrets();

    // Canonical top-level order: system, vlans, interfaces, routing,
    // security, then anything else in its original order (sort is
    // stable).
    tree.items.sort_by_key(|item| match item.name() {
        "system" => 0,
        "vlans" => 1,
        "interfaces" => 2,
        "routing" => 3,
        "security" => 4,
        "services" => 5,
        _ => 6,
    });

    crate::pager::page(&tree.to_text());
    Ok(())
}

/// Admin-state marker present? (`shutdown` / `no shutdown`; the legacy
/// hyphenated and `admin-state` forms are normalized away before this
/// runs, but tolerate them anyway.)
fn has_admin_leaf(items: &[hemlock_config::Item]) -> bool {
    use hemlock_config::ConfigTree;
    ConfigTree::has_leaf(items, "shutdown")
        || ConfigTree::has_phrase(items, "no", "shutdown")
        || ConfigTree::has_leaf(items, "no-shutdown")
        || ConfigTree::has_leaf(items, "admin-state")
}

/// Sort an `interfaces` block for display: Ethernet ports in numeric
/// order, Management after them, then VLAN interfaces, anything
/// unrecognized last. Stable, so equal keys keep their running-config
/// order.
fn sort_interface_blocks(items: &mut [hemlock_config::Item]) {
    fn key(item: &hemlock_config::Item) -> (u8, u64, String) {
        match item {
            hemlock_config::Item::Block { name, .. } => {
                let rank = if name.starts_with("Ethernet") {
                    0
                } else if name.starts_with("Management") {
                    1
                } else if name.starts_with("Vlan") {
                    2
                } else {
                    3
                };
                let number: String = name
                    .chars()
                    .skip_while(|c| !c.is_ascii_digit())
                    .take_while(char::is_ascii_digit)
                    .collect();
                (rank, number.parse().unwrap_or(u64::MAX), name.clone())
            }
            hemlock_config::Item::Leaf { name, .. } => (4, 0, name.clone()),
        }
    }
    items.sort_by_key(key);
}

/// Admin state of a Linux netdev (IFF_UP), from sysfs. `None` when the
/// device (or sysfs) is unavailable.
fn os_netdev_is_up(dev: &str) -> Option<bool> {
    let flags = std::fs::read_to_string(format!("/sys/class/net/{dev}/flags")).ok()?;
    let flags = u32::from_str_radix(flags.trim().trim_start_matches("0x"), 16).ok()?;
    Some(flags & 0x1 != 0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn interface_blocks_sort_numerically_with_management_last() {
        // The shape `show configuration` produces when Ethernet33 was
        // configured before the inventory merge appended the rest — with
        // a legacy keyed block mixed in to prove normalization composes.
        let mut tree = hemlock_config::parse(
            "interfaces {\n Management1 { }\n ethernet Ethernet33 { }\n Ethernet1 { }\n Ethernet2 { }\n Ethernet10 { }\n}",
        )
        .unwrap();
        tree.normalize_interfaces();
        sort_interface_blocks(tree.block_mut("interfaces"));
        let (_, interfaces) = tree.block("interfaces").unwrap();
        let order: Vec<&str> = interfaces
            .iter()
            .filter_map(|item| match item {
                hemlock_config::Item::Block { name, .. } => Some(name.as_str()),
                hemlock_config::Item::Leaf { .. } => None,
            })
            .collect();
        assert_eq!(
            order,
            [
                "Ethernet1",
                "Ethernet2",
                "Ethernet10",
                "Ethernet33",
                "Management1"
            ]
        );
    }

    /// Every stored secret renders as `<hidden>`: RADIUS shared keys
    /// and both SNMP v3 passphrases. The surrounding keywords stay, so
    /// the line still reads as configuration.
    #[test]
    fn secrets_never_render() {
        let mut tree = hemlock_config::parse(
            "security { dot1x { radius-server 10.42.0.5 { key \"s3cret\" } } }
             services { snmp { community public
             user monitor auth sha \"authpass1\" priv aes \"privpass1\" } }
             system { login { user cody { role admin
             password-hash \"$6$rounds=656000$abcdefgh$ijklmnop\"
             ssh-key \"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIN0ex4mpl3 cody@mars\" } } }",
        )
        .unwrap();
        tree.redact_secrets();
        let text = tree.to_text();
        // The login hash is a stored credential like the rest.
        assert!(!text.contains("ijklmnop"), "password hash leaked: {text}");
        assert!(text.contains(r#"password-hash "<hidden>""#), "{text}");
        // The role and the (public) ssh key still read as config.
        assert!(text.contains("role admin"), "{text}");
        assert!(
            text.contains("AAAAC3NzaC1lZDI1NTE5AAAAIN0ex4mpl3"),
            "{text}"
        );
        assert!(!text.contains("s3cret"), "radius key leaked: {text}");
        assert!(
            !text.contains("authpass1"),
            "auth passphrase leaked: {text}"
        );
        assert!(
            !text.contains("privpass1"),
            "priv passphrase leaked: {text}"
        );
        // The serializer quotes `<hidden>` (it is not a bare token).
        assert!(text.contains(r#"key "<hidden>""#), "{text}");
        assert!(
            text.contains(r#"user monitor auth sha "<hidden>" priv aes "<hidden>""#),
            "{text}"
        );
        // Non-secret leaves are untouched.
        assert!(text.contains("community public"));
    }

    /// Redaction is inert on a config that carries no secrets (and on
    /// one with no `services` block at all).
    #[test]
    fn redaction_leaves_ordinary_config_alone() {
        let mut tree = hemlock_config::parse(
            "services { ntp { server 10.42.0.5 } }
vlans { vlan 10 { } }",
        )
        .unwrap();
        let before = tree.to_text();
        tree.redact_secrets();
        assert_eq!(tree.to_text(), before);
    }
}
