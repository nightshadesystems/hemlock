//! `hemlockctl show ...` — read-only views of daemon state.

use anyhow::{Context, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;

fn speed_str(mbps: u32) -> String {
    if mbps >= 1000 && mbps % 1000 == 0 {
        format!("{}G", mbps / 1000)
    } else {
        format!("{mbps}M")
    }
}

fn admin_str(state: i32) -> &'static str {
    match pb::AdminState::try_from(state) {
        Ok(pb::AdminState::Up) => "up",
        Ok(pb::AdminState::Down) => "down",
        _ => "?",
    }
}

fn oper_str(state: i32) -> &'static str {
    match pb::OperStatus::try_from(state) {
        Ok(pb::OperStatus::Up) => "up",
        Ok(pb::OperStatus::Down) => "down",
        _ => "?",
    }
}

pub async fn interfaces(endpoint: IpcEndpoint) -> Result<()> {
    let channel = endpoint.connect().await.context("connecting to syncd")?;
    let mut client = pb::syncd_client::SyncdClient::new(channel);
    let ports = client
        .list_ports(pb::ListPortsRequest {})
        .await?
        .into_inner()
        .ports;

    println!(
        "{:<12} {:>5} {:>6} {:>5} {:>4}  Description",
        "Interface", "Index", "Speed", "Admin", "Oper"
    );
    for p in ports {
        println!(
            "{:<12} {:>5} {:>6} {:>5} {:>4}  {}",
            p.name,
            p.index,
            speed_str(p.speed_mbps),
            admin_str(p.admin_state),
            oper_str(p.oper_status),
            p.description
        );
    }
    Ok(())
}

/// EOS-style `show interfaces status` summary table.
pub async fn interfaces_status(endpoint: IpcEndpoint) -> Result<()> {
    let channel = endpoint.connect().await.context("connecting to syncd")?;
    let mut client = pb::syncd_client::SyncdClient::new(channel);
    let mut ports = client
        .list_ports(pb::ListPortsRequest {})
        .await?
        .into_inner()
        .ports;
    ports.sort_by_key(|p| p.index);

    println!(
        "{:<12} {:<29} {:<12} {:<8} {:<6} {:<6} {:<15} {:<5} Encapsulation",
        "Port", "Name", "Status", "Vlan", "Duplex", "Speed", "Type", "Flags"
    );
    for p in &ports {
        let admin_up = p.admin_state == pb::AdminState::Up as i32;
        let oper_up = p.oper_status == pb::OperStatus::Up as i32;
        let mut name = p.description.clone();
        if name.len() > 29 {
            name.truncate(26);
            name.push_str("...");
        }
        // Phase 1 has no VLAN objects yet (every port sits in the default
        // VLAN) and no flags/encapsulation state.
        println!(
            "{:<12} {:<29} {:<12} {:<8} {:<6} {:<6} {:<15}",
            p.name,
            name,
            crate::cli::status_word(admin_up, oper_up),
            "1",
            "full",
            speed_str(p.speed_mbps),
            p.media,
        );
    }
    Ok(())
}

/// Software + platform summary. Daemon state is best-effort: `show
/// version` must work even when syncd is down.
pub async fn version(endpoint: IpcEndpoint) {
    println!("Hemlock  {}", hemlock_common::VERSION);
    match endpoint.connect().await {
        Ok(channel) => {
            let mut client = pb::syncd_client::SyncdClient::new(channel);
            match client.get_switch_info(pb::GetSwitchInfoRequest {}).await {
                Ok(info) => {
                    let info = info.into_inner();
                    println!("Platform:  {}", info.platform_id);
                    println!("Backend:   {}", info.backend);
                    println!("Ports:     {}", info.port_count);
                }
                Err(e) => println!("(syncd unavailable: {})", e.message()),
            }
        }
        Err(_) => println!("(syncd not running)"),
    }
}

pub async fn switch(endpoint: IpcEndpoint) -> Result<()> {
    let channel = endpoint.connect().await.context("connecting to syncd")?;
    let mut client = pb::syncd_client::SyncdClient::new(channel);
    let info = client
        .get_switch_info(pb::GetSwitchInfoRequest {})
        .await?
        .into_inner();
    println!("Platform:   {}", info.platform_id);
    println!("Backend:    {}", info.backend);
    println!("Switch OID: {:#x}", info.switch_oid);
    println!("Ports:      {}", info.port_count);
    Ok(())
}

pub async fn environment(endpoint: IpcEndpoint) -> Result<()> {
    let channel = endpoint.connect().await.context("connecting to pmon")?;
    let mut client = pb::pmon_client::PmonClient::new(channel);
    let env = client
        .get_environment(pb::GetEnvironmentRequest {})
        .await?
        .into_inner();

    if !env.temperatures.is_empty() {
        println!("Temperatures:");
        for t in &env.temperatures {
            let flag = if t.celsius >= t.crit_celsius {
                "  CRIT"
            } else if t.celsius >= t.warn_celsius {
                "  WARN"
            } else {
                ""
            };
            println!(
                "  {:<28} {:>6.1} C  (warn {:.0}, crit {:.0}){flag}",
                t.name, t.celsius, t.warn_celsius, t.crit_celsius
            );
        }
    }
    if !env.fans.is_empty() {
        println!("Fans:");
        for f in &env.fans {
            println!(
                "  {:<28} {:>5} rpm  pwm {:>3}%  {}",
                f.name,
                f.rpm,
                f.pwm_percent,
                if f.ok { "ok" } else { "FAULT" }
            );
        }
    }
    if !env.psus.is_empty() {
        println!("PSUs:");
        for p in &env.psus {
            let status = match (p.present, p.ok) {
                (false, _) => "absent",
                (true, true) => "ok",
                (true, false) => "FAULT",
            };
            println!("  {:<28} {status}", p.name);
        }
    }
    Ok(())
}

pub async fn transceivers(endpoint: IpcEndpoint) -> Result<()> {
    let channel = endpoint.connect().await.context("connecting to pmon")?;
    let mut client = pb::pmon_client::PmonClient::new(channel);
    let xcvrs = client
        .list_transceivers(pb::ListTransceiversRequest {})
        .await?
        .into_inner()
        .transceivers;

    println!(
        "{:<12} {:<8} {:<6} {:<16} {:<16} Serial",
        "Port", "Present", "Type", "Vendor", "Part"
    );
    for x in xcvrs {
        println!(
            "{:<12} {:<8} {:<6} {:<16} {:<16} {}",
            x.port,
            if x.present { "yes" } else { "no" },
            x.form_factor,
            x.vendor,
            x.part_number,
            x.serial
        );
    }
    Ok(())
}

/// `show configuration` — the running configuration merged with the full
/// interface inventory, so every stock port (and the management port from
/// the platform manifest) renders in curly-brace form even before it has
/// ever been explicitly configured. Unconfigured leaves are filled from
/// live state, and a `system` block carries the hostname and login users.
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

    {
        let interfaces = tree.block_mut("interfaces");
        for p in &ports {
            let eth = ConfigTree::ensure_block(interfaces, p.name.as_str(), &[]);
            if ConfigTree::leaf_value(eth, "admin-state").is_none() {
                let admin = if p.admin_state == pb::AdminState::Up as i32 {
                    "enabled"
                } else {
                    "disabled"
                };
                ConfigTree::set_leaf(eth, "admin-state", vec![admin.into()]);
            }
            if !p.description.is_empty() && ConfigTree::leaf_value(eth, "description").is_none() {
                ConfigTree::set_leaf(eth, "description", vec![p.description.clone()]);
            }
        }

        // Management port: an OS netdev named by the manifest, not an ASIC
        // port. Skipped quietly when no manifest is present (dev hosts).
        if let Ok(platform) = hemlock_platform::Platform::find("/", platform_dir) {
            if let Some(mgmt) = &platform.manifest.management {
                let block = ConfigTree::ensure_block(interfaces, mgmt.interface.as_str(), &[]);
                if ConfigTree::leaf_value(block, "admin-state").is_none() {
                    if let Some(up) = os_netdev_is_up(&mgmt.os_device) {
                        ConfigTree::set_leaf(
                            block,
                            "admin-state",
                            vec![if up { "enabled" } else { "disabled" }.into()],
                        );
                    }
                }
            }
        }

        // Deterministic display order regardless of which ports were
        // explicitly configured first: ethernet by port number, then
        // management.
        sort_interface_blocks(interfaces);
    }

    // System identity: hostname and login users, filled from the OS when
    // the running config does not set them.
    {
        let system = tree.block_mut("system");
        if ConfigTree::leaf_value(system, "hostname").is_none() {
            ConfigTree::set_leaf(system, "hostname", vec![crate::cli::read_hostname()]);
        }
        let logins = os_login_users();
        if !logins.is_empty() {
            let users = ConfigTree::ensure_block(system, "users", &[]);
            for (name, role) in logins {
                let user = ConfigTree::ensure_block(users, "user", &[name.as_str()]);
                if ConfigTree::leaf_value(user, "role").is_none() {
                    ConfigTree::set_leaf(user, "role", vec![role.into()]);
                }
            }
        }
    }

    // Canonical top-level order: system, interfaces, then anything else
    // in its original order (sort is stable).
    tree.items.sort_by_key(|item| match item.name() {
        "system" => 0,
        "interfaces" => 1,
        _ => 2,
    });

    print!("{}", tree.to_text());
    Ok(())
}

/// Sort an `interfaces` block for display: Ethernet ports in numeric
/// order, Management after them, anything unrecognized last. Stable, so
/// equal keys keep their running-config order.
fn sort_interface_blocks(items: &mut [hemlock_config::Item]) {
    fn key(item: &hemlock_config::Item) -> (u8, u64, String) {
        match item {
            hemlock_config::Item::Block { name, .. } => {
                let rank = if name.starts_with("Ethernet") {
                    0
                } else if name.starts_with("Management") {
                    1
                } else {
                    2
                };
                let number: String = name
                    .chars()
                    .skip_while(|c| !c.is_ascii_digit())
                    .take_while(char::is_ascii_digit)
                    .collect();
                (rank, number.parse().unwrap_or(u64::MAX), name.clone())
            }
            hemlock_config::Item::Leaf { name, .. } => (3, 0, name.clone()),
        }
    }
    items.sort_by_key(key);
}

/// Human login accounts from the OS: `/etc/passwd` entries in the regular
/// user UID range with a real shell. Role is `admin` for sudo-group
/// members, `operator` otherwise. Empty off-switch (no /etc/passwd).
fn os_login_users() -> Vec<(String, &'static str)> {
    let Ok(passwd) = std::fs::read_to_string("/etc/passwd") else {
        return Vec::new();
    };
    let sudoers: Vec<String> = std::fs::read_to_string("/etc/group")
        .ok()
        .and_then(|groups| {
            groups.lines().find_map(|line| {
                let mut fields = line.split(':');
                (fields.next() == Some("sudo")).then(|| {
                    fields
                        .nth(2)
                        .unwrap_or("")
                        .split(',')
                        .map(str::to_string)
                        .collect()
                })
            })
        })
        .unwrap_or_default();

    let mut users = Vec::new();
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        let [name, _, uid, _, _, _, shell] = fields.as_slice() else {
            continue;
        };
        let Ok(uid) = uid.parse::<u32>() else {
            continue;
        };
        let real_shell = !shell.ends_with("nologin") && !shell.ends_with("false");
        if (1000..60000).contains(&uid) && real_shell {
            let role = if sudoers.iter().any(|s| s == name) {
                "admin"
            } else {
                "operator"
            };
            users.push((name.to_string(), role));
        }
    }
    users.sort();
    users
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
}
