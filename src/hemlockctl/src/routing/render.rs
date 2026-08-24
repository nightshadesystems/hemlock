//! EOS-style text renderers for the routing-suite show family.

use crate::interfaces::table::{Col, Text};

use super::model::{NeighborTable, RouteEntry, RouteSummary, RouteTable};

/// `show ip route` / `show ipv6 route`.
pub fn route_table(table: &RouteTable) -> String {
    let mut out = Text::new();
    out.line("Codes: C - connected, S - static, K - kernel, O - OSPF, B - BGP");
    out.blank();
    match table.routes.iter().find(|r| r.is_default()) {
        Some(route) => {
            out.line("Gateway of last resort:");
            entry(&mut out, route);
        }
        None => out.line("Gateway of last resort is not set"),
    }
    let rest: Vec<&RouteEntry> = table.routes.iter().filter(|r| !r.is_default()).collect();
    if !rest.is_empty() {
        out.blank();
        for route in rest {
            entry(&mut out, route);
        }
    }
    out.finish()
}

/// `show ip route <prefix>`: the one matching entry plus its FIB
/// (hardware) state when the RIB pipeline reported one.
pub fn route_entry(route: &RouteEntry) -> String {
    let mut out = Text::new();
    entry(&mut out, route);
    if let Some(fib) = &route.fib {
        let state = match fib.as_str() {
            "programmed" | "connected" | "drop" => "programmed (hardware)",
            "punt" => "punt (resolving next hop)",
            _ => "kernel (not in hardware)",
        };
        out.line(format!(" FIB: {state}"));
    }
    out.finish()
}

/// `show arp` / `show ipv6 neighbors` (static entries age as `-`).
pub fn neighbor_table(table: &NeighborTable) -> String {
    const COLS: [Col; 4] = [Col::left(16), Col::right(9), Col::left(2), Col::left(19)];
    let mut out = Text::new();
    out.row(
        &COLS,
        &["Address", "Age (sec)", "", "Hardware Addr", "Interface"],
    );
    for neighbor in &table.entries {
        let age = if neighbor.is_static {
            "-".to_string()
        } else {
            neighbor
                .age_secs
                .map(|secs| secs.to_string())
                .unwrap_or_default()
        };
        out.row(
            &COLS,
            &[&neighbor.ip, &age, "", &neighbor.mac, &neighbor.interface],
        );
    }
    out.finish()
}

fn entry(out: &mut Text, route: &RouteEntry) {
    if route.protocol == "connected" {
        out.line(format!(" {:<6} {}", route.code(), route.prefix));
    } else {
        out.line(format!(
            " {:<6} {} [{}/{}]",
            route.code(),
            route.prefix,
            route.distance,
            route.metric
        ));
    }
    for hop in &route.next_hops {
        match &hop.interface {
            Some(interface) => out.line(format!("         via {}, {}", hop.via, interface)),
            None => out.line(format!("         via {}", hop.via)),
        }
    }
    if let Some(interface) = &route.interface {
        out.line(format!("         directly connected, {interface}"));
    }
}

/// `show ip route summary` / `show ipv6 route summary`.
pub fn route_summary(summary: &RouteSummary) -> String {
    const COLS: [Col; 1] = [Col::left(16)];
    let mut out = Text::new();
    out.row(&COLS, &["Route Source", "Number Of Routes"]);
    for source in &summary.sources {
        out.row(&COLS, &[&source.source, &source.routes.to_string()]);
    }
    out.blank();
    out.line(format!("Total number of routes: {}", summary.total));
    out.line(format!(
        "Number of next-hop groups in hardware: {}",
        summary.next_hop_groups
    ));
    out.finish()
}

// ------------------------------------------------- FRR protocol detail

use super::model::{BgpState, OspfState, VrrpState};

/// `show routing ospf` (the process overview).
pub fn ospf_overview(state: &OspfState) -> String {
    let mut out = Text::new();
    out.line(format!(
        "Routing Process \"ospf\" with ID {}",
        state.router_id
    ));
    out.line(format!(
        "  Number of areas in this router is {}",
        state.areas.len()
    ));
    for area in &state.areas {
        out.line(format!("  Area {}", area.id));
        out.line(format!(
            "    Number of interfaces in this area: {}",
            area.interfaces
        ));
        out.line(format!(
            "    SPF algorithm executed {} times",
            state.spf_runs
        ));
    }
    out.finish()
}

/// `hh:mm:ss` from milliseconds (OSPF dead timers).
fn hms_msecs(msecs: u64) -> String {
    let secs = msecs / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// `show routing ospf neighbor`.
pub fn ospf_neighbors(state: &OspfState) -> String {
    const COLS: [Col; 6] = [
        Col::left(16),
        Col::right(3),
        Col::left(2),
        Col::left(10),
        Col::left(11),
        Col::left(16),
    ];
    let mut out = Text::new();
    out.row(
        &COLS,
        &[
            "Neighbor ID",
            "Pri",
            "",
            "State",
            "Dead Time",
            "Address",
            "Interface",
        ],
    );
    for neighbor in &state.neighbors {
        out.row(
            &COLS,
            &[
                &neighbor.router_id,
                &neighbor.priority.to_string(),
                "",
                &neighbor.state,
                &hms_msecs(neighbor.dead_time_msecs),
                &neighbor.address,
                &neighbor.interface,
            ],
        );
    }
    out.finish()
}

/// `show routing ospf interface`.
pub fn ospf_interfaces(state: &OspfState) -> String {
    let mut out = Text::new();
    for (i, iface) in state.interfaces.iter().enumerate() {
        if i > 0 {
            out.blank();
        }
        out.line(format!(
            "{} is {}",
            iface.name,
            if iface.up { "up" } else { "down" }
        ));
        out.line(format!(
            "  Internet Address {}, Area {}",
            iface.address, iface.area
        ));
        out.line(format!(
            "  Router ID {}, Network Type {}, Cost: {}",
            iface.router_id, iface.network_type, iface.cost
        ));
        out.line(format!(
            "  Designated Router (ID) {}, Interface Address {}",
            iface.dr_id, iface.dr_address
        ));
        out.line(format!(
            "  Timer intervals: Hello {}, Dead {}",
            iface.hello_interval, iface.dead_interval
        ));
        out.line(format!(
            "  Neighbor Count is {}, Adjacent neighbor count is {}",
            iface.neighbors, iface.adjacent
        ));
    }
    out.finish()
}

/// The customary short BGP state word ("Established" -> "Estab").
fn bgp_state_word(state: &str) -> &str {
    match state {
        "Established" => "Estab",
        other => other,
    }
}

/// One `show routing bgp summary` row (header and data share the
/// column spec).
#[allow(clippy::too_many_arguments)]
fn bgp_summary_row(
    neighbor: &str,
    version: &str,
    remote_as: &str,
    rcvd: &str,
    sent: &str,
    in_q: &str,
    out_q: &str,
    up_down: &str,
    state: &str,
    pfx: &str,
) -> String {
    format!(
        "{neighbor:<16}{version:<1}  {remote_as:<8}{rcvd:>7}{sent:>9}{in_q:>5}{out_q:>6}  {up_down:<10}{state:<7}{pfx:>6}"
    )
}

/// `show routing bgp summary`.
pub fn bgp_summary(state: &BgpState) -> String {
    let mut out = Text::new();
    out.line("BGP summary information");
    out.line(format!(
        "Router identifier {}, local AS number {}",
        state.router_id, state.as_number
    ));
    out.line(bgp_summary_row(
        "Neighbor", "V", "AS", "MsgRcvd", "MsgSent", "InQ", "OutQ", "Up/Down", "State", "PfxRcd",
    ));
    for peer in &state.peers {
        let pfx = if peer.pfx_rcvd >= 0 {
            peer.pfx_rcvd.to_string()
        } else {
            "-".to_string()
        };
        out.line(bgp_summary_row(
            &peer.ip,
            &peer.version.to_string(),
            &peer.remote_as.to_string(),
            &peer.msg_rcvd.to_string(),
            &peer.msg_sent.to_string(),
            &peer.in_q.to_string(),
            &peer.out_q.to_string(),
            &peer.up_down,
            bgp_state_word(&peer.state),
            &pfx,
        ));
    }
    out.finish()
}

/// `show routing bgp` (the BGP routing table).
pub fn bgp_table(state: &BgpState) -> String {
    let mut out = Text::new();
    out.line("BGP routing table information");
    out.line("Status: * - valid, > - active");
    out.line(format!(
        "    {:<20}{:<16}{:>6}{:>9}  {}",
        "Network", "Next Hop", "Metric", "LocPref", "Path"
    ));
    for route in &state.routes {
        out.line(format!(
            " {}{} {:<20}{:<16}{:>6}{:>9}  {}",
            if route.valid { '*' } else { ' ' },
            if route.best { '>' } else { ' ' },
            route.network,
            route.next_hop,
            route.metric,
            route.loc_pref,
            route.path
        ));
    }
    out.finish()
}

/// `show routing bgp neighbors <ip>` (the detail block).
pub fn bgp_neighbor_detail(state: &BgpState) -> String {
    let mut out = Text::new();
    let Some(detail) = &state.detail else {
        return out.finish();
    };
    out.line(format!(
        "BGP neighbor is {}, remote AS {}",
        detail.ip, detail.remote_as
    ));
    if !detail.description.is_empty() {
        out.line(format!(" Description: {}", detail.description));
    }
    let uptime = if detail.uptime.is_empty() {
        "never".to_string()
    } else {
        detail.uptime.clone()
    };
    out.line(format!(" BGP state = {}, up for {uptime}", detail.state));
    out.line(format!(
        " Message statistics: {} received, {} sent",
        detail.msg_rcvd, detail.msg_sent
    ));
    out.line(format!(
        " Prefixes: {} received, {} accepted, {} advertised",
        detail.prefixes_received, detail.prefixes_accepted, detail.prefixes_advertised
    ));
    let mut options = Vec::new();
    if detail.next_hop_self {
        options.push("next-hop-self".to_string());
    }
    if detail.ebgp_multihop > 1 {
        options.push(format!("ebgp-multihop {}", detail.ebgp_multihop));
    }
    if !options.is_empty() {
        out.line(format!(" Configured options: {}", options.join(", ")));
    }
    out.finish()
}

/// The abbreviated interface form for the VRRP brief table
/// ("Vlan100" -> "Vl100").
fn vrrp_abbrev(interface: &str) -> String {
    crate::interfaces::name::parse_one(interface)
        .map(|id| id.abbrev())
        .unwrap_or_else(|| interface.to_string())
}

/// `show vrrp brief`.
pub fn vrrp_brief(state: &VrrpState) -> String {
    const COLS: [Col; 7] = [
        Col::left(11),
        Col::left(7),
        Col::left(5),
        Col::left(6),
        Col::left(5),
        Col::left(9),
        Col::left(17),
    ];
    let mut out = Text::new();
    out.row(
        &COLS,
        &[
            "Interface",
            "Group",
            "Pri",
            "Adv",
            "Pre",
            "State",
            "Master addr",
            "Group addr",
        ],
    );
    for group in &state.groups {
        let master = if group.state == "Master" {
            "local"
        } else {
            "-"
        };
        out.row(
            &COLS,
            &[
                &vrrp_abbrev(&group.interface),
                &group.group.to_string(),
                &group.priority.to_string(),
                &(group.advertisement_interval_ms / 1000).to_string(),
                if group.preempt { "Y" } else { "N" },
                &group.state,
                master,
                &group.addresses.join(", "),
            ],
        );
    }
    out.finish()
}

/// `show vrrp` (detail blocks: virtual MAC, skew/master-down timers,
/// last state change).
pub fn vrrp_detail(state: &VrrpState) -> String {
    let mut out = Text::new();
    for (i, group) in state.groups.iter().enumerate() {
        if i > 0 {
            out.blank();
        }
        out.line(format!("{}, group {}", group.interface, group.group));
        out.line(format!("  State is {}", group.state));
        out.line(format!(
            "  Virtual address(es): {}",
            group.addresses.join(", ")
        ));
        out.line(format!("  Virtual MAC address is {}", group.virtual_mac));
        out.line(format!(
            "  Priority {} (effective {}), preempt {}",
            group.priority,
            group.effective_priority,
            if group.preempt { "enabled" } else { "disabled" }
        ));
        out.line(format!(
            "  Advertisement interval {:.1} s, skew time {:.1} s, master down interval {:.1} s",
            f64::from(group.advertisement_interval_ms) / 1000.0,
            f64::from(group.skew_time_ms) / 1000.0,
            f64::from(group.master_down_interval_ms) / 1000.0
        ));
        if let Some(secs) = group.seconds_since_transition {
            out.line(format!(
                "  Last state change {:02}:{:02}:{:02} ago",
                secs / 3600,
                (secs % 3600) / 60,
                secs % 60
            ));
        }
    }
    out.finish()
}
