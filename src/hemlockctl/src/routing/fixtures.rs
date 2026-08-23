//! Test fixtures behind the routing-suite golden outputs: the RIB the
//! spec's seed configuration produces, plus fixture OSPF/BGP routes
//! (the protocols land later; their rendering is pinned now).

use super::model::{NextHop, RouteEntry, RouteTable};

fn hop(via: &str, interface: &str) -> NextHop {
    NextHop {
        via: via.into(),
        interface: Some(interface.into()),
    }
}

fn entry(
    protocol: &str,
    prefix: &str,
    distance: u32,
    metric: u32,
    next_hops: Vec<NextHop>,
    interface: Option<&str>,
) -> RouteEntry {
    RouteEntry {
        protocol: protocol.into(),
        prefix: prefix.into(),
        distance,
        metric,
        next_hops,
        interface: interface.map(str::to_string),
    }
}

fn connected(prefix: &str, interface: &str) -> RouteEntry {
    entry("connected", prefix, 0, 0, Vec::new(), Some(interface))
}

/// The IPv4 table, numerically sorted the way fetch produces it.
pub fn ip_route_table() -> RouteTable {
    RouteTable {
        routes: vec![
            entry(
                "static",
                "0.0.0.0/0",
                1,
                0,
                vec![hop("10.42.10.1", "Vlan99")],
                None,
            ),
            connected("10.0.100.0/24", "Vlan100"),
            connected("10.9.9.0/31", "Ethernet48"),
            connected("10.42.10.0/24", "Vlan99"),
            entry(
                "ospf",
                "10.50.0.0/24",
                110,
                20,
                vec![hop("10.42.10.7", "Vlan99")],
                None,
            ),
            entry(
                "static",
                "10.99.0.0/16",
                1,
                0,
                vec![hop("10.9.9.0", "Ethernet48"), hop("10.42.10.7", "Vlan99")],
                None,
            ),
            entry(
                "static",
                "172.16.0.0/12",
                250,
                0,
                vec![hop("10.42.10.1", "Vlan99")],
                None,
            ),
            entry(
                "bgp",
                "172.20.0.0/16",
                200,
                0,
                vec![hop("10.42.10.1", "Vlan99")],
                None,
            ),
            entry("static", "192.0.2.0/24", 1, 0, Vec::new(), Some("Null0")),
        ],
    }
}

/// The IPv6 table: one connected subnet and one static via it.
pub fn ipv6_route_table() -> RouteTable {
    RouteTable {
        routes: vec![
            connected("2001:db8:9::/64", "Ethernet48"),
            entry(
                "static",
                "2001:db8:99::/48",
                1,
                0,
                vec![hop("2001:db8:9::1", "Ethernet48")],
                None,
            ),
        ],
    }
}
