//! EOS-style text renderers for the services-suite show family.

use crate::interfaces::table::{pad, Col, Text};

use super::model::{LldpState, NtpState};

/// The abbreviated interface form for tabular output
/// ("Ethernet1" -> "Et1").
fn short_name(interface: &str) -> String {
    crate::interfaces::name::parse_one(interface)
        .map(|id| id.abbrev())
        .unwrap_or_else(|| interface.to_string())
}

/// A TLV subtype token as English, for the detail block
/// ("mac" -> "MAC address").
fn subtype_text(subtype: &str) -> String {
    match subtype {
        "mac" => "MAC address".into(),
        "interface-name" => "interface name".into(),
        "interface-alias" => "interface alias".into(),
        "network-address" => "network address".into(),
        "chassis-component" => "chassis component".into(),
        "port-component" => "port component".into(),
        "agent-circuit-id" => "agent circuit id".into(),
        "local" => "locally assigned".into(),
        other => other.replace('-', " "),
    }
}

/// One `Name           : value` line of a detail block. Names wider
/// than the field push the colon right rather than being clipped.
fn field(name: &str, value: &str) -> String {
    format!("  {} : {}", pad(name, Col::left(14)), value)
}

/// `show lldp` — the global settings plus the per-port frame counters.
pub fn lldp(state: &LldpState) -> String {
    const COLS: [Col; 6] = [
        Col::left(7),
        Col::left(10),
        Col::left(8),
        Col::left(8),
        Col::left(11),
        Col::left(9),
    ];
    let mut out = Text::new();
    out.line(format!(
        "LLDP is {}",
        if state.enabled { "enabled" } else { "disabled" }
    ));
    out.line(format!(
        "Tx interval: {}s   Hold multiplier: {} (TTL {}s)",
        state.tx_interval,
        state.hold_multiplier,
        state.ttl()
    ));
    out.line(format!(
        "Chassis ID: {}   System name: {}",
        state.chassis_id, state.system_name
    ));
    out.blank();
    out.row(
        &COLS,
        &[
            "Port",
            "State",
            "Tx",
            "Rx",
            "Discarded",
            "Ageouts",
            "Neighbors",
        ],
    );
    out.row(
        &COLS,
        &[
            "-----",
            "--------",
            "------",
            "------",
            "---------",
            "-------",
            "---------",
        ],
    );
    for port in &state.ports {
        out.row(
            &COLS,
            &[
                &short_name(&port.port),
                if port.enabled { "enabled" } else { "disabled" },
                &port.frames_tx.to_string(),
                &port.frames_rx.to_string(),
                &port.frames_discarded.to_string(),
                &port.ageouts.to_string(),
                &port.neighbors.len().to_string(),
            ],
        );
    }
    out.finish()
}

/// `show lldp neighbors` — one row per neighbor, in port order.
pub fn lldp_neighbors(state: &LldpState) -> String {
    const COLS: [Col; 3] = [Col::left(7), Col::left(23), Col::left(19)];
    let mut out = Text::new();
    out.row(&COLS, &["Port", "Neighbor Device", "Neighbor Port", "TTL"]);
    out.row(
        &COLS,
        &[
            "-----",
            "---------------------",
            "-----------------",
            "----",
        ],
    );
    for neighbor in state.neighbors() {
        // A neighbor that advertises no system name is identified by
        // its chassis id, which every LLDPDU carries.
        let device = if neighbor.system_name.is_empty() {
            neighbor.chassis_id.as_str()
        } else {
            neighbor.system_name.as_str()
        };
        out.row(
            &COLS,
            &[
                &short_name(&neighbor.port),
                device,
                &neighbor.port_id,
                &neighbor.ttl.to_string(),
            ],
        );
    }
    out.finish()
}

/// `show lldp neighbors detail` — one block per port that has heard
/// anything, each listing that port's neighbors in full.
pub fn lldp_neighbors_detail(state: &LldpState) -> String {
    let mut out = Text::new();
    let mut first = true;
    for port in state.ports.iter().filter(|p| !p.neighbors.is_empty()) {
        if !first {
            out.blank();
        }
        first = false;
        out.line(format!(
            "Interface {} detected {} LLDP neighbor{}:",
            short_name(&port.port),
            port.neighbors.len(),
            if port.neighbors.len() == 1 { "" } else { "s" }
        ));
        for neighbor in &port.neighbors {
            out.blank();
            out.line(format!(
                "  Neighbor {} age {} seconds",
                neighbor.chassis_id, neighbor.age_secs
            ));
            out.line(field(
                "Chassis ID",
                &format!(
                    "{} ({})",
                    neighbor.chassis_id,
                    subtype_text(&neighbor.chassis_id_subtype)
                ),
            ));
            out.line(field(
                "Port ID",
                &format!(
                    "{} ({})",
                    neighbor.port_id,
                    subtype_text(&neighbor.port_id_subtype)
                ),
            ));
            if !neighbor.port_description.is_empty() {
                out.line(field("Port Description", &neighbor.port_description));
            }
            if !neighbor.system_name.is_empty() {
                out.line(field("System Name", &neighbor.system_name));
            }
            if !neighbor.system_description.is_empty() {
                out.line(field("System Description", &neighbor.system_description));
            }
            if !neighbor.management_address.is_empty() {
                out.line(field("Management Address", &neighbor.management_address));
            }
            out.line(field("TTL", &format!("{} seconds", neighbor.ttl)));
        }
    }
    if first {
        out.line("No LLDP neighbors.");
    }
    out.finish()
}

// ------------------------------------------------- NTP

/// Microseconds as milliseconds with three decimals, the resolution
/// timesyncd itself reports at ("-412us" -> "-0.412").
fn millis(usecs: i64) -> String {
    let sign = if usecs < 0 { "-" } else { "" };
    let magnitude = usecs.unsigned_abs();
    format!("{sign}{}.{:03}", magnitude / 1000, magnitude % 1000)
}

/// A short age: `4m12s`, `2h04m`, `3d05h`, `41s`.
fn age(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m{:02}s", s / 60, s % 60),
        s if s < 86_400 => format!("{}h{:02}m", s / 3_600, (s % 3_600) / 60),
        s => format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3_600),
    }
}

/// `show ntp` — the configured servers plus timesyncd's live sync.
pub fn ntp(state: &NtpState) -> String {
    let mut out = Text::new();
    if state.servers.is_empty() {
        out.line("NTP is disabled (no servers configured)");
        return out.finish();
    }
    // Servers are configured but the unit is not running: say which of
    // the two it is rather than printing a bare "disabled".
    if state.enabled {
        out.line("NTP is enabled (systemd-timesyncd)");
    } else {
        out.line("NTP is configured but systemd-timesyncd is not running");
    }
    out.line(format!("Servers: {}", state.servers.join(", ")));
    out.blank();
    if !state.synchronized {
        out.line("Not synchronized");
        return out.finish();
    }
    out.line(format!("Synchronized to {}", state.server));
    out.line(format!(
        "  Stratum {}, poll interval {}s",
        state.stratum, state.poll_interval_secs
    ));
    out.line(format!(
        "  Offset {} ms, delay {} ms, jitter {} ms",
        millis(state.offset_usecs),
        millis(i64::try_from(state.delay_usecs).unwrap_or(i64::MAX)),
        millis(i64::try_from(state.jitter_usecs).unwrap_or(i64::MAX)),
    ));
    match state.last_sync_secs_ago {
        Some(secs) => out.line(format!("  Last sync: {} ago", age(secs))),
        None => out.line("  Last sync: unknown"),
    }
    out.finish()
}
