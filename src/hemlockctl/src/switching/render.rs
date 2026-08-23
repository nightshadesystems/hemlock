//! EOS-style text renderers for the switching-suite show family.

use crate::interfaces::table::{Col, Text};

use super::model::{MacTable, MirrorSession, StormRow, VlanRow};

/// `show vlan` (rows already filtered for `show vlan id <set>`).
pub fn vlan(rows: &[VlanRow]) -> String {
    const COLS: [Col; 3] = [Col::left(6), Col::left(33), Col::left(10)];
    let mut out = Text::new();
    out.row(&COLS, &["VLAN", "Name", "Status", "Ports"]);
    out.row(
        &COLS,
        &[
            "-----",
            "--------------------------------",
            "---------",
            "----------------------------",
        ],
    );
    for row in rows {
        let ports = row
            .ports
            .iter()
            .map(|id| id.abbrev())
            .collect::<Vec<_>>()
            .join(", ");
        out.row(
            &COLS,
            &[
                &row.id.to_string(),
                &row.display_name(),
                row.status_word(),
                &ports,
            ],
        );
    }
    out.finish()
}

/// `show vlan summary`.
pub fn vlan_summary(rows: &[VlanRow]) -> String {
    let user = rows.iter().filter(|r| r.id != 1).count();
    let mut out = Text::new();
    out.line(format!(
        "{:<33}: {}",
        "Number of existing VLANs",
        rows.len()
    ));
    out.line(format!("{:<33}: {}", "Number of existing user VLANs", user));
    out.finish()
}

/// `h:mm:ss` duration (the Last Move column).
fn hms(secs: u64) -> String {
    format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// `show mac address-table` (entries already filtered).
pub fn mac_address_table(table: &MacTable) -> String {
    const COLS: [Col; 5] = [
        Col::left(8),
        Col::left(19),
        Col::left(10),
        Col::left(9),
        Col::left(7),
    ];
    let mut out = Text::new();
    out.line("          Mac Address Table");
    out.line("-".repeat(66));
    out.blank();
    out.row(
        &COLS,
        &["Vlan", "Mac Address", "Type", "Ports", "Moves", "Last Move"],
    );
    out.row(
        &COLS,
        &["----", "-----------", "----", "-----", "-----", "---------"],
    );
    for entry in &table.entries {
        let port = match (&entry.port, entry.drop) {
            (_, true) => "Drop".to_string(),
            (Some(id), _) => id.abbrev(),
            (None, _) => String::new(),
        };
        let kind = if entry.is_static { "STATIC" } else { "DYNAMIC" };
        let moves = if entry.is_static {
            String::new()
        } else {
            entry.moves.to_string()
        };
        let last_move = entry
            .last_move_secs
            .map(|secs| format!("{} ago", hms(secs)))
            .unwrap_or_default();
        out.row(
            &COLS,
            &[
                &format!("{:>4}", entry.vlan),
                &entry.mac,
                kind,
                &port,
                &moves,
                &last_move,
            ],
        );
    }
    out.line(format!(
        "Total Mac Addresses for this criterion: {}",
        table.entries.len()
    ));
    out.finish()
}

/// `show mac address-table count`.
pub fn mac_count(table: &MacTable) -> String {
    let dynamic = table.entries.iter().filter(|e| !e.is_static).count();
    let statics = table.entries.len() - dynamic;
    let mut out = Text::new();
    out.line("MAC Entries for all vlans:");
    out.line(format!("Dynamic Address Count: {dynamic}"));
    out.line(format!("Unicast Static Address Count: {statics}"));
    out.line(format!("Total MAC Addresses: {}", table.entries.len()));
    out.finish()
}

/// `show mac address-table aging-time`.
pub fn mac_aging_time(table: &MacTable) -> String {
    let mut out = Text::new();
    out.line(format!("Global Aging Time: {}", table.aging_time_secs));
    out.finish()
}

/// `show storm-control`.
pub fn storm_control(rows: &[StormRow]) -> String {
    const COLS: [Col; 5] = [
        Col::left(7),
        Col::left(18),
        Col::left(10),
        Col::left(14),
        Col::left(14),
    ];
    let mut out = Text::new();
    out.row(
        &COLS,
        &["Port", "Type", "Level", "Rate (Mbps)", "Drops", "Status"],
    );
    out.row(
        &COLS,
        &[
            "-----",
            "----------------",
            "--------",
            "------------",
            "------------",
            "------",
        ],
    );
    for row in rows {
        out.row(
            &COLS,
            &[
                &row.port.abbrev(),
                &row.kind,
                &format!("{}%", row.level),
                &(row.rate_kbps / 1000).to_string(),
                &row.drops.to_string(),
                if row.active { "active" } else { "inactive" },
            ],
        );
    }
    out.finish()
}

use super::model::{lacp_state_word, LacpSystem, PortChannel, StpBridge};

/// The instance header (`MST0` under mstp, `RSTP` otherwise).
fn stp_header(bridge: &StpBridge) -> &'static str {
    if bridge.mode == "rstp" {
        "RSTP"
    } else {
        "MST0"
    }
}

/// `show spanning-tree`.
pub fn spanning_tree(bridge: &StpBridge) -> String {
    let mut out = Text::new();
    if bridge.mode == "none" {
        out.line("Spanning tree is disabled (mode none)");
        return out.finish();
    }
    out.line(stp_header(bridge));
    out.line(format!("  Spanning tree enabled protocol {}", bridge.mode));
    out.line(format!(
        "  {:<11}{:<12}{}",
        "Root ID", "Priority", bridge.root_priority
    ));
    out.line(format!("  {:<11}{:<12}{}", "", "Address", bridge.root_mac));
    if bridge.is_root {
        out.line("             This bridge is the root");
    } else {
        out.line(format!("  {:<11}{:<12}{}", "", "Cost", bridge.root_cost));
        if let Some(port) = &bridge.root_port {
            out.line(format!(
                "  {:<11}{:<12}{} ({})",
                "",
                "Port",
                port.num,
                port.full_name()
            ));
        }
        out.line(format!(
            "  {:<11}Hello Time  {}.000 sec  Max Age {} sec  Forward Delay {} sec",
            "", bridge.hello_time, bridge.max_age, bridge.forward_time
        ));
    }
    out.blank();
    out.line(format!(
        "  {:<11}{:<12}{}  (priority {} sys-id-ext 0)",
        "Bridge ID", "Priority", bridge.bridge_priority, bridge.bridge_priority
    ));
    out.line(format!(
        "  {:<11}{:<12}{}",
        "", "Address", bridge.bridge_mac
    ));
    out.line(format!(
        "  {:<11}Hello Time  {}.000 sec  Max Age {} sec  Forward Delay {} sec",
        "", bridge.hello_time, bridge.max_age, bridge.forward_time
    ));
    out.blank();
    const COLS: [Col; 5] = [
        Col::left(17),
        Col::left(12),
        Col::left(12),
        Col::left(11),
        Col::left(10),
    ];
    out.row(
        &COLS,
        &["Interface", "Role", "State", "Cost", "Prio.Nbr", "Type"],
    );
    out.row(
        &COLS,
        &[
            "----------------",
            "-----------",
            "-----------",
            "----------",
            "---------",
            "----------",
        ],
    );
    for port in &bridge.ports {
        let kind = if port.portfast { "P2p Edge" } else { "P2p" };
        out.row(
            &COLS,
            &[
                &port.id.abbrev(),
                &port.role,
                &port.state,
                &port.cost.to_string(),
                &port.prio_nbr(),
                kind,
            ],
        );
    }
    out.finish()
}

/// `show spanning-tree detail`.
pub fn spanning_tree_detail(bridge: &StpBridge) -> String {
    let mut out = Text::new();
    if bridge.mode == "none" {
        out.line("Spanning tree is disabled (mode none)");
        return out.finish();
    }
    out.line(format!(
        "{} is executing the {} compatible Spanning Tree protocol",
        stp_header(bridge),
        bridge.mode
    ));
    out.line(format!(
        "  Bridge Identifier has priority {}, address {}",
        bridge.bridge_priority, bridge.bridge_mac
    ));
    out.line(format!(
        "  Configured hello time {}, max age {}, forward delay {}",
        bridge.hello_time, bridge.max_age, bridge.forward_time
    ));
    match (bridge.seconds_since_tc, &bridge.last_tc_port) {
        (Some(secs), Some(port)) => {
            out.line(format!(
                "  Number of topology changes {} last change occurred {} ago",
                bridge.topology_changes,
                hms(secs)
            ));
            out.line(format!("          from {port}"));
        }
        _ => out.line(format!(
            "  Number of topology changes {}",
            bridge.topology_changes
        )),
    }
    for port in &bridge.ports {
        out.blank();
        out.line(format!(
            " Port {} ({}) of {} is {}",
            port.id.num,
            port.id.full_name(),
            stp_header(bridge),
            if port.errdisabled {
                "errdisabled (bpduguard)"
            } else {
                &port.state
            }
        ));
        out.line(format!(
            "   Port path cost {}, Port priority {}",
            port.cost, port.priority
        ));
        let mut features = Vec::new();
        if port.portfast {
            features.push("portfast");
        }
        if port.bpduguard {
            features.push("bpduguard");
        }
        if !features.is_empty() {
            out.line(format!("   The port is configured {}", features.join(", ")));
        }
        out.line(format!(
            "   BPDU: sent {}, received {}",
            port.bpdus_tx, port.bpdus_rx
        ));
    }
    out.finish()
}

/// `show spanning-tree blockedports`.
pub fn spanning_tree_blockedports(bridge: &StpBridge) -> String {
    const COLS: [Col; 1] = [Col::left(21)];
    let mut out = Text::new();
    out.row(&COLS, &["Name", "Blocked Interfaces List"]);
    out.row(
        &COLS,
        &[
            "--------------------",
            "------------------------------------",
        ],
    );
    let blocked: Vec<String> = bridge
        .ports
        .iter()
        .filter(|p| p.role == "alternate" && p.state == "discarding")
        .map(|p| p.id.abbrev())
        .collect();
    if !blocked.is_empty() {
        out.row(&COLS, &[stp_header(bridge), &blocked.join(", ")]);
    }
    out.blank();
    out.line(format!(
        "Number of blocked ports (segments) in the system : {}",
        blocked.len()
    ));
    out.finish()
}

/// `show spanning-tree mst configuration`.
pub fn spanning_tree_mst_configuration(bridge: &StpBridge) -> String {
    let mut out = Text::new();
    out.line(format!("Name     [{}]", bridge.mst_name));
    out.line(format!("Revision {}", bridge.mst_revision));
    const COLS: [Col; 1] = [Col::left(10)];
    out.row(&COLS, &["Instance", "Vlans mapped"]);
    out.row(&COLS, &["--------", "---------------------------------"]);
    // Instance 0 is implicit: every VLAN not mapped elsewhere.
    let mapped: std::collections::BTreeSet<u32> = bridge
        .instances
        .iter()
        .flat_map(|(_, vlans)| vlans.iter().copied())
        .collect();
    let unmapped: Vec<u32> = (1..=4094).filter(|v| !mapped.contains(v)).collect();
    out.row(
        &COLS,
        &["0", &crate::interfaces::fmt::compress_vlans(&unmapped)],
    );
    for (instance, vlans) in &bridge.instances {
        out.row(
            &COLS,
            &[
                &instance.to_string(),
                &crate::interfaces::fmt::compress_vlans(vlans),
            ],
        );
    }
    out.finish()
}

/// `show port-channel summary`.
pub fn port_channel_summary(lags: &[PortChannel]) -> String {
    const COLS: [Col; 3] = [Col::left(7), Col::left(17), Col::left(11)];
    let mut out = Text::new();
    out.line("Flags: U - in use    D - down       a - LACP active    p - LACP passive");
    out.line("       s - suspended F - fallback   ^ - individual     * - static (mode on)");
    out.blank();
    out.row(&COLS, &["Group", "Port-Channel", "Protocol", "Members"]);
    out.row(
        &COLS,
        &[
            "-----",
            "---------------",
            "---------",
            "--------------------------------",
        ],
    );
    for lag in lags {
        let mut po_flags = String::from(if lag.up { "U" } else { "D" });
        if lag.fallback_active {
            po_flags.push('F');
        }
        let protocol = if lag.lacp {
            format!("LACP({})", if lag.active_mode { "a" } else { "p" })
        } else {
            "Static(*)".to_string()
        };
        let members = lag
            .members
            .iter()
            .map(|m| {
                let flag = match m.status.as_str() {
                    "bundled" => "B",
                    "individual" => "^",
                    "standby" => "s",
                    _ => "D",
                };
                format!("{}({})", m.id.abbrev(), flag)
            })
            .collect::<Vec<_>>()
            .join(" ");
        out.row(
            &COLS,
            &[
                &lag.group.to_string(),
                &format!("Po{}({})", lag.group, po_flags),
                &protocol,
                &members,
            ],
        );
    }
    out.finish()
}

/// `show port-channel <n> detail`.
pub fn port_channel_detail(lags: &[PortChannel]) -> String {
    let mut out = Text::new();
    for (i, lag) in lags.iter().enumerate() {
        if i > 0 {
            out.blank();
        }
        out.line(format!("Port-Channel{}:", lag.group));
        let label = |name: &str, value: String| format!("  {name:<15}: {value}");
        out.line(label(
            "Admin state",
            if lag.admin_up { "up" } else { "down" }.into(),
        ));
        out.line(label(
            "Oper state",
            format!(
                "{} ({} of {} members bundled)",
                if lag.up { "up" } else { "down" },
                lag.bundled,
                lag.total
            ),
        ));
        out.line(label(
            "Protocol",
            if lag.lacp {
                format!(
                    "LACP {}",
                    if lag.active_mode { "active" } else { "passive" }
                )
            } else {
                "static (mode on)".into()
            },
        ));
        out.line(label("Min links", lag.min_links.to_string()));
        out.line(label(
            "Fallback",
            if lag.fallback_mode.is_empty() {
                "off".into()
            } else {
                format!(
                    "{} (timeout {}s)",
                    lag.fallback_mode, lag.fallback_timeout_secs
                )
            },
        ));
        out.line(label("MAC address", lag.mac.clone()));
        out.line("  Members:");
        for member in &lag.members {
            let detail = if !lag.lacp {
                "(static)".to_string()
            } else if !member.partner_system.is_empty() {
                let mut parts = vec!["current"];
                if member.actor_state & 0x10 != 0 {
                    parts.push("collecting/distributing");
                }
                format!("(LACP: {})", parts.join(", "))
            } else {
                "(LACP: defaulted)".to_string()
            };
            out.line(format!(
                "    {:<13}: {:<10}{}",
                member.id.full_name(),
                member.status,
                detail
            ));
        }
    }
    out.finish()
}

/// `show lacp neighbor`.
pub fn lacp_neighbor(lags: &[PortChannel]) -> String {
    const COLS: [Col; 6] = [
        Col::left(8),
        Col::left(9),
        Col::left(26),
        Col::left(8),
        Col::left(7),
        Col::left(7),
    ];
    let mut out = Text::new();
    out.row(
        &COLS,
        &[
            "Port",
            "Flags",
            "Partner Sys-ID",
            "Port#",
            "Key",
            "Prio",
            "State",
        ],
    );
    out.row(
        &COLS,
        &[
            "------",
            "-------",
            "------------------------",
            "------",
            "-----",
            "-----",
            "--------",
        ],
    );
    for lag in lags.iter().filter(|l| l.lacp) {
        for member in &lag.members {
            if member.partner_system.is_empty() {
                continue;
            }
            let mut flags = String::new();
            if member.partner_state & 0x02 != 0 {
                flags.push('F');
            }
            if member.partner_state & 0x01 != 0 {
                flags.push('A');
            }
            out.row(
                &COLS,
                &[
                    &member.id.abbrev(),
                    &flags,
                    &member.partner_system,
                    &member.partner_port.to_string(),
                    &member.partner_key.to_string(),
                    &member.partner_priority.to_string(),
                    &lacp_state_word(member.partner_state),
                ],
            );
        }
    }
    out.finish()
}

/// `show lacp neighbor detail`.
pub fn lacp_neighbor_detail(lags: &[PortChannel]) -> String {
    let mut out = Text::new();
    let mut first = true;
    for lag in lags.iter().filter(|l| l.lacp) {
        for member in &lag.members {
            if !first {
                out.blank();
            }
            first = false;
            out.line(format!(
                "Port-Channel{}, member {}",
                lag.group,
                member.id.full_name()
            ));
            let label = |name: &str, value: String| format!("  {name:<17}: {value}");
            if member.partner_system.is_empty() {
                out.line(label("Partner", "none (defaulted)".into()));
            } else {
                out.line(label("Partner Sys-ID", member.partner_system.clone()));
                out.line(label("Partner Port", member.partner_port.to_string()));
                out.line(label("Partner Key", member.partner_key.to_string()));
                out.line(label(
                    "Partner Priority",
                    member.partner_priority.to_string(),
                ));
                out.line(label(
                    "Partner State",
                    lacp_state_word(member.partner_state),
                ));
            }
            out.line(label("Actor State", lacp_state_word(member.actor_state)));
            out.line(label(
                "Rate",
                if member.rate_fast { "fast" } else { "normal" }.into(),
            ));
        }
    }
    out.finish()
}

/// `show lacp counters`.
pub fn lacp_counters(lags: &[PortChannel]) -> String {
    const COLS: [Col; 3] = [Col::left(8), Col::left(10), Col::left(10)];
    let mut out = Text::new();
    out.row(&COLS, &["Port", "Sent", "Recv", "Churn"]);
    out.row(&COLS, &["------", "--------", "--------", "-------"]);
    for lag in lags.iter().filter(|l| l.lacp) {
        for member in &lag.members {
            out.row(
                &COLS,
                &[
                    &member.id.abbrev(),
                    &member.pdus_tx.to_string(),
                    &member.pdus_rx.to_string(),
                    &member.churn.to_string(),
                ],
            );
        }
    }
    out.finish()
}

/// `show lacp sys-id`.
pub fn lacp_sys_id(system: &LacpSystem) -> String {
    let mut out = Text::new();
    out.line(system.system_id.clone());
    out.finish()
}

use super::model::SnoopingView;

fn enabled_word(enabled: bool) -> &'static str {
    if enabled {
        "Enabled"
    } else {
        "Disabled"
    }
}

/// `show igmp snooping` / `show mld snooping`.
pub fn snooping(view: &SnoopingView) -> String {
    let mut out = Text::new();
    let header = format!("Global {} Snooping configuration:", view.family);
    out.line(&header);
    out.line("-".repeat(header.len()));
    out.line(format!(
        "{:<24}: {}",
        format!("{} snooping", view.family),
        enabled_word(view.enabled)
    ));
    out.line(format!(
        "{:<24}: {}",
        "Robustness variable", view.robustness
    ));
    for vlan in &view.vlans {
        out.blank();
        let header = format!("Vlan {} :", vlan.vlan);
        out.line(&header);
        out.line("-".repeat(header.len()));
        out.line(format!(
            "{:<24}: {}",
            format!("{} snooping", view.family),
            enabled_word(vlan.enabled)
        ));
        let querier = if vlan.querier_enabled {
            match &vlan.querier_address {
                Some(address) => format!("Enabled ({address})"),
                None => "Enabled".to_string(),
            }
        } else {
            "Disabled".to_string()
        };
        out.line(format!("{:<24}: {querier}", "Querier"));
        out.line(format!(
            "{:<24}: {}",
            "Fast-leave",
            enabled_word(vlan.fast_leave)
        ));
        let mut mrouters: Vec<String> = vlan
            .static_mrouters
            .iter()
            .map(|id| format!("{} (static)", id.abbrev()))
            .collect();
        mrouters.extend(
            vlan.dynamic_mrouters
                .iter()
                .map(|id| format!("{} (dynamic)", id.abbrev())),
        );
        out.line(format!(
            "{:<24}: {}",
            "Mrouter ports",
            if mrouters.is_empty() {
                "None".to_string()
            } else {
                mrouters.join(", ")
            }
        ));
    }
    out.finish()
}

/// `show igmp|mld snooping groups`.
pub fn snooping_groups(view: &SnoopingView) -> String {
    const COLS: [Col; 4] = [Col::left(6), Col::left(17), Col::left(10), Col::left(9)];
    let mut out = Text::new();
    out.row(&COLS, &["Vlan", "Group", "Type", "Version", "Port-List"]);
    out.row(
        &COLS,
        &[
            "----",
            "---------------",
            "--------",
            "-------",
            "------------------",
        ],
    );
    let mut total = 0;
    for vlan in &view.vlans {
        for group in &vlan.groups {
            total += 1;
            let ports = group
                .ports
                .iter()
                .map(|id| id.abbrev())
                .collect::<Vec<_>>()
                .join(", ");
            out.row(
                &COLS,
                &[
                    &vlan.vlan.to_string(),
                    &group.group,
                    "Dynamic",
                    &format!("v{}", group.version),
                    &ports,
                ],
            );
        }
    }
    out.line(format!("Total number of groups: {total}"));
    out.finish()
}

/// `show igmp|mld snooping querier`.
pub fn snooping_querier(view: &SnoopingView) -> String {
    const COLS: [Col; 2] = [Col::left(6), Col::left(18)];
    let mut out = Text::new();
    out.row(&COLS, &["Vlan", "Querier Address", "State"]);
    out.row(&COLS, &["----", "----------------", "--------"]);
    for vlan in &view.vlans {
        if !vlan.querier_enabled {
            continue;
        }
        out.row(
            &COLS,
            &[
                &vlan.vlan.to_string(),
                vlan.querier_address.as_deref().unwrap_or("-"),
                if vlan.querier_active {
                    "Active"
                } else {
                    "Suppressed"
                },
            ],
        );
    }
    out.finish()
}

/// `show mirror` (alias `show monitor session`).
pub fn mirror(sessions: &[MirrorSession]) -> String {
    let mut out = Text::new();
    for (i, session) in sessions.iter().enumerate() {
        if i > 0 {
            out.blank();
        }
        out.line(format!("Session {}", session.session));
        out.line("-".repeat(24));
        out.line("Source Ports:");
        for (label, ports) in [
            ("Rx", &session.rx),
            ("Tx", &session.tx),
            ("Both", &session.both),
        ] {
            if ports.is_empty() {
                continue;
            }
            let list = ports
                .iter()
                .map(|id| id.abbrev())
                .collect::<Vec<_>>()
                .join(", ");
            out.line(format!("{:<15}{}", format!("  {label}:"), list));
        }
        out.line("Destination Ports:");
        match &session.destination {
            Some(id) => out.line(format!(
                "  {:<13}{}",
                id.abbrev(),
                if session.destination_active {
                    "active"
                } else {
                    "inactive"
                }
            )),
            None => out.line("  none"),
        }
    }
    out.finish()
}
