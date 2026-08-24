//! EOS-style text renderers for the security-suite show family.

use crate::interfaces::table::{Col, Text};

use super::model::{
    AclRule, AclState, ArpInspection, CoppState, DaiVlanStats, DhcpSnooping, DhcpStatistics,
    Dot1xState, PortSecurityEntry, SnoopBinding,
};

/// The abbreviated interface form for tabular output
/// ("Ethernet1" -> "Et1").
fn short_name(interface: &str) -> String {
    crate::interfaces::name::parse_one(interface)
        .map(|id| id.abbrev())
        .unwrap_or_else(|| interface.to_string())
}

/// `h:mm:ss` duration (violation and auth ages).
fn hms(secs: u64) -> String {
    format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// A comma-joined VLAN list ("10, 20").
fn vlan_list(vlans: &[u32]) -> String {
    vlans
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A comma-joined trusted-interface list, abbreviated ("Po1").
fn trusted_list(names: &[String]) -> String {
    if names.is_empty() {
        return "none".to_string();
    }
    names
        .iter()
        .map(|name| short_name(name))
        .collect::<Vec<_>>()
        .join(", ")
}

// ------------------------------------------------- ACLs

/// `show acl [<name>]` (blocks; the caller filters for the name form).
pub fn acl(state: &AclState) -> String {
    let mut out = Text::new();
    for (i, acl) in state.acls.iter().enumerate() {
        if i > 0 {
            out.blank();
        }
        out.line(format!("{} access list {}", acl.family_display(), acl.name));
        for rule in &acl.rules {
            out.line(rule_line(rule));
        }
        let mut implicit = String::from("        implicit deny");
        if let Some(protocol) = acl.implicit_protocol() {
            implicit.push(' ');
            implicit.push_str(protocol);
        }
        implicit.push_str(&format!(" any any [match {}]", acl.implicit_deny_matches));
        out.line(implicit);
        let applied = if acl.bindings.is_empty() {
            "none".to_string()
        } else {
            acl.bindings
                .iter()
                .map(|b| format!("{} {}", b.port, b.direction))
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.line(format!("        Applied: {applied}"));
    }
    out.finish()
}

/// One rule line: `<seq> <permit|deny> [<proto>] <src> <dst> [eq <port>]
/// [log] [police <rate> <burst>] [match N]`.
fn rule_line(rule: &AclRule) -> String {
    let mut line = format!(
        "        {} {}",
        rule.number,
        if rule.permit { "permit" } else { "deny" }
    );
    if let Some(protocol) = &rule.protocol {
        line.push(' ');
        line.push_str(protocol);
    }
    line.push(' ');
    line.push_str(&rule.source);
    line.push(' ');
    line.push_str(&rule.destination);
    if let Some(port) = &rule.port {
        line.push_str(&format!(" eq {port}"));
    }
    if rule.log {
        line.push_str(" log");
    }
    if let Some(police) = &rule.police {
        line.push_str(&format!(" police {police}"));
    }
    line.push_str(&format!(" [match {}]", rule.matches));
    line
}

/// `show acl summary`.
pub fn acl_summary(state: &AclState) -> String {
    const COLS: [Col; 3] = [Col::left(13), Col::left(8), Col::left(7)];
    let mut out = Text::new();
    out.row(&COLS, &["ACL", "Family", "Rules", "Bindings"]);
    out.row(
        &COLS,
        &["-----------", "------", "-----", "-----------------"],
    );
    for acl in &state.acls {
        let bindings = if acl.bindings.is_empty() {
            "-".to_string()
        } else {
            acl.bindings
                .iter()
                .map(|b| format!("{} ({})", short_name(&b.port), b.direction))
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.row(
            &COLS,
            &[
                &acl.name,
                &acl.family,
                &acl.rules.len().to_string(),
                &bindings,
            ],
        );
    }
    out.blank();
    out.line("TCAM utilization:");
    out.line(format!("  {:<9}{:<7}{}", "Stage", "Used", "Available"));
    for stage in &state.tcam {
        out.line(format!(
            "  {:<9}{:<7}{}",
            stage.stage, stage.used, stage.available
        ));
    }
    out.finish()
}

// ------------------------------------------------- Control-plane policing

/// `show copp`.
pub fn copp(state: &CoppState) -> String {
    const COLS: [Col; 4] = [Col::left(10), Col::left(12), Col::left(7), Col::left(15)];
    let mut out = Text::new();
    out.row(
        &COLS,
        &["Class", "Rate (pps)", "Burst", "Conforming", "Dropped"],
    );
    out.row(
        &COLS,
        &[
            "--------",
            "----------",
            "-----",
            "-------------",
            "--------",
        ],
    );
    for class in &state.classes {
        let name = if class.overridden {
            format!("{} *", class.class)
        } else {
            class.class.clone()
        };
        out.row(
            &COLS,
            &[
                &name,
                &class.rate.to_string(),
                &class.burst.to_string(),
                &class.conforming.to_string(),
                &class.dropped.to_string(),
            ],
        );
    }
    if state.classes.iter().any(|c| c.overridden) {
        out.blank();
        out.line("* rates in config override compiled defaults");
    }
    out.finish()
}

// ------------------------------------------------- Port security

/// `show port-security` (the per-port table).
pub fn port_security(rows: &[PortSecurityEntry]) -> String {
    const COLS: [Col; 5] = [
        Col::left(7),
        Col::left(5),
        Col::left(9),
        Col::left(11),
        Col::left(10),
    ];
    let mut out = Text::new();
    out.row(
        &COLS,
        &[
            "Port",
            "Max",
            "Learned",
            "Violation",
            "Action",
            "Last Violation",
        ],
    );
    out.row(
        &COLS,
        &[
            "-----",
            "---",
            "-------",
            "---------",
            "--------",
            "-----------------------------",
        ],
    );
    for row in rows {
        let last = match (&row.last_violation_mac, row.last_violation_secs_ago) {
            (Some(mac), Some(secs)) => format!("{mac} ({} ago)", hms(secs)),
            (Some(mac), None) => mac.clone(),
            _ => "-".to_string(),
        };
        out.row(
            &COLS,
            &[
                &short_name(&row.port),
                &row.maximum.to_string(),
                &row.learned.len().to_string(),
                &row.violations.to_string(),
                row.action(),
                &last,
            ],
        );
    }
    out.finish()
}

/// `show port-security interface <port>` (the detail block: learned
/// MACs with ages and the errdisable state).
pub fn port_security_detail(rows: &[PortSecurityEntry]) -> String {
    const COLS: [Col; 1] = [Col::left(19)];
    let mut out = Text::new();
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.blank();
        }
        out.line(format!(
            "Port-security on {}: maximum {}, violation {}",
            short_name(&row.port),
            row.maximum,
            row.action()
        ));
        out.line(format!(
            "Errdisabled: {}",
            if row.errdisabled { "yes" } else { "no" }
        ));
        out.blank();
        out.row(&COLS, &["MAC Address", "Age"]);
        out.row(&COLS, &["-----------------", "--------"]);
        for mac in &row.learned {
            out.row(&COLS, &[&mac.mac, &hms(mac.age_secs)]);
        }
    }
    out.finish()
}

// ------------------------------------------------- 802.1X

/// `show dot1x` (ports already filtered for the interface form).
pub fn dot1x(state: &Dot1xState) -> String {
    const COLS: [Col; 4] = [Col::left(7), Col::left(14), Col::left(19), Col::left(16)];
    let mut out = Text::new();
    let servers = if state.radius_servers.is_empty() {
        "none".to_string()
    } else {
        state.radius_servers.join(", ")
    };
    out.line(format!("RADIUS servers: {servers}"));
    if state.reauth_interval_secs == 0 {
        out.line("Reauth interval: off");
    } else {
        out.line(format!("Reauth interval: {}s", state.reauth_interval_secs));
    }
    out.blank();
    out.row(
        &COLS,
        &["Port", "State", "Supplicant MAC", "Last Auth", "Failures"],
    );
    out.row(
        &COLS,
        &[
            "-----",
            "------------",
            "-----------------",
            "--------------",
            "--------",
        ],
    );
    for port in &state.ports {
        let last = port
            .last_auth_secs_ago
            .map(|secs| format!("{} ago", hms(secs)))
            .unwrap_or_else(|| "-".to_string());
        out.row(
            &COLS,
            &[
                &short_name(&port.port),
                &port.status,
                port.supplicant_mac.as_deref().unwrap_or("-"),
                &last,
                &port.failures.to_string(),
            ],
        );
    }
    out.finish()
}

// ------------------------------------------------- DHCP snooping + DAI

/// `show dhcp snooping` (the overview block).
pub fn dhcp_snooping(dhcp: &DhcpSnooping) -> String {
    let mut out = Text::new();
    if dhcp.vlans.is_empty() {
        out.line("DHCP snooping is disabled");
    } else {
        out.line(format!(
            "DHCP snooping is enabled on VLANs: {}",
            vlan_list(&dhcp.vlans)
        ));
    }
    out.line(format!(
        "Trusted interfaces: {}",
        trusted_list(&dhcp.trusted)
    ));
    out.finish()
}

/// `show dhcp snooping binding`.
pub fn dhcp_snooping_binding(bindings: &[SnoopBinding]) -> String {
    const COLS: [Col; 5] = [
        Col::left(19),
        Col::left(15),
        Col::left(13),
        Col::left(9),
        Col::left(6),
    ];
    let mut out = Text::new();
    out.row(
        &COLS,
        &[
            "MAC Address",
            "IP Address",
            "Lease (sec)",
            "Type",
            "VLAN",
            "Interface",
        ],
    );
    out.row(
        &COLS,
        &[
            "-----------------",
            "-------------",
            "-----------",
            "-------",
            "----",
            "---------",
        ],
    );
    for binding in bindings {
        let lease = binding
            .lease_secs
            .map(|secs| secs.to_string())
            .unwrap_or_else(|| "-".to_string());
        let kind = if binding.is_static {
            "static"
        } else {
            "dynamic"
        };
        out.row(
            &COLS,
            &[
                &binding.mac,
                &binding.ip,
                &lease,
                kind,
                &binding.vlan.to_string(),
                &short_name(&binding.interface),
            ],
        );
    }
    out.line(format!("Total number of bindings: {}", bindings.len()));
    out.finish()
}

/// `show dhcp snooping statistics`.
pub fn dhcp_snooping_statistics(stats: &DhcpStatistics) -> String {
    const COLS: [Col; 2] = [Col::left(20), Col::left(10)];
    let mut out = Text::new();
    out.row(&COLS, &["", "Packets", "Dropped"]);
    for vlan in &stats.vlans {
        out.row(
            &COLS,
            &[
                &format!("Vlan {}", vlan.vlan),
                &vlan.packets.to_string(),
                &vlan.dropped.to_string(),
            ],
        );
    }
    out.line(format!(
        "Server msgs from untrusted ports dropped: {}",
        stats.untrusted_server_drops
    ));
    out.finish()
}

/// `show arp inspection` (the overview block plus the stats table).
pub fn arp_inspection(arp: &ArpInspection) -> String {
    let mut out = Text::new();
    if arp.vlans.is_empty() {
        out.line("ARP inspection is disabled");
    } else {
        out.line(format!(
            "ARP inspection is enabled on VLANs: {}",
            vlan_list(&arp.vlans)
        ));
    }
    out.line(format!("Validate: {}", arp.validate.join(", ")));
    out.line(format!(
        "Trusted interfaces: {}",
        trusted_list(&arp.trusted)
    ));
    out.blank();
    dai_stats_table(&mut out, &arp.statistics);
    out.finish()
}

/// `show arp inspection statistics` (just the stats table).
pub fn arp_inspection_statistics(stats: &[DaiVlanStats]) -> String {
    let mut out = Text::new();
    dai_stats_table(&mut out, stats);
    out.finish()
}

fn dai_stats_table(out: &mut Text, stats: &[DaiVlanStats]) {
    const COLS: [Col; 4] = [Col::left(6), Col::left(11), Col::left(9), Col::left(13)];
    out.row(
        &COLS,
        &["Vlan", "Forwarded", "Dropped", "Bad Binding", "Bad Src-MAC"],
    );
    out.row(
        &COLS,
        &["----", "---------", "-------", "-----------", "-----------"],
    );
    for row in stats {
        out.row(
            &COLS,
            &[
                &row.vlan.to_string(),
                &row.forwarded.to_string(),
                &row.dropped.to_string(),
                &row.bad_binding.to_string(),
                &row.bad_src_mac.to_string(),
            ],
        );
    }
}
