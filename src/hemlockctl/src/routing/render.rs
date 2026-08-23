//! EOS-style text renderers for the routing-suite show family.

use crate::interfaces::table::{Col, Text};

use super::model::{RouteEntry, RouteSummary, RouteTable};

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

/// `show ip route <prefix>`: the one matching entry.
pub fn route_entry(route: &RouteEntry) -> String {
    let mut out = Text::new();
    entry(&mut out, route);
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
