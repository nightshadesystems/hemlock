//! Test fixtures behind the routing-suite golden outputs: the RIB the
//! spec's seed configuration produces, plus fixture OSPF/BGP routes
//! (the protocols land later; their rendering is pinned now).

use super::model::{
    BgpNeighborDetail, BgpPeer, BgpRibEntry, BgpState, NeighborEntry, NeighborTable, NextHop,
    OspfArea, OspfInterface, OspfNeighbor, OspfState, RouteEntry, RouteTable, VrrpGroup, VrrpState,
};

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
        fib: Some(
            match (protocol, interface) {
                ("connected", _) => "connected",
                (_, Some("Null0")) => "drop",
                _ => "programmed",
            }
            .into(),
        ),
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

fn neighbor(ip: &str, age_secs: Option<u64>, mac: &str, interface: &str) -> NeighborEntry {
    NeighborEntry {
        ip: ip.into(),
        mac: mac.into(),
        interface: interface.into(),
        is_static: age_secs.is_none(),
        age_secs,
    }
}

/// The IPv4 neighbor table (numerically sorted, statics ageless).
pub fn arp_table() -> NeighborTable {
    NeighborTable {
        entries: vec![
            neighbor("10.9.9.0", Some(33), "a0:36:9f:44:be:09", "Ethernet48"),
            neighbor("10.42.10.1", Some(142), "d4:af:f7:12:9c:00", "Vlan99"),
            neighbor("10.42.10.7", Some(12), "00:1c:73:0c:aa:07", "Vlan99"),
            neighbor("10.42.10.200", None, "00:50:56:be:ef:99", "Vlan99"),
        ],
    }
}

/// The IPv6 neighbor table.
pub fn ipv6_neighbor_table() -> NeighborTable {
    NeighborTable {
        entries: vec![neighbor(
            "2001:db8:9::1",
            Some(33),
            "a0:36:9f:44:be:09",
            "Ethernet48",
        )],
    }
}

/// The OSPF oper state the spec's Part 3 samples show.
pub fn ospf_state() -> OspfState {
    OspfState {
        router_id: "10.42.0.1".into(),
        spf_runs: 7,
        areas: vec![OspfArea {
            id: "0.0.0.0".into(),
            interfaces: 1,
        }],
        neighbors: vec![OspfNeighbor {
            router_id: "10.42.0.7".into(),
            priority: 1,
            state: "Full".into(),
            dead_time_msecs: 33_000,
            address: "10.42.10.7".into(),
            interface: "Vlan99".into(),
        }],
        interfaces: vec![OspfInterface {
            name: "Vlan99".into(),
            up: true,
            address: "10.42.10.9/24".into(),
            area: "0.0.0.0".into(),
            router_id: "10.42.0.1".into(),
            network_type: "BROADCAST".into(),
            cost: 10,
            state: "Backup".into(),
            priority: 1,
            dr_id: "10.42.0.7".into(),
            dr_address: "10.42.10.7".into(),
            hello_interval: 10,
            dead_interval: 40,
            neighbors: 1,
            adjacent: 1,
        }],
    }
}

/// The BGP oper state the spec's Part 3 samples show.
pub fn bgp_state() -> BgpState {
    BgpState {
        router_id: "10.42.0.1".into(),
        as_number: 65000,
        peers: vec![BgpPeer {
            ip: "10.42.10.1".into(),
            version: 4,
            remote_as: 65001,
            msg_rcvd: 1234,
            msg_sent: 1230,
            in_q: 0,
            out_q: 0,
            up_down: "2d04h".into(),
            state: "Established".into(),
            pfx_rcvd: 42,
        }],
        routes: vec![
            BgpRibEntry {
                network: "10.42.0.0/16".into(),
                next_hop: "-".into(),
                metric: "-".into(),
                loc_pref: "100".into(),
                path: "i".into(),
                valid: true,
                best: true,
            },
            BgpRibEntry {
                network: "172.20.0.0/16".into(),
                next_hop: "10.42.10.1".into(),
                metric: "0".into(),
                loc_pref: "100".into(),
                path: "65001 i".into(),
                valid: true,
                best: true,
            },
        ],
        detail: None,
    }
}

/// [`bgp_state`] plus one neighbor's detail block.
pub fn bgp_neighbor_state() -> BgpState {
    let mut state = bgp_state();
    state.detail = Some(BgpNeighborDetail {
        ip: "10.42.10.1".into(),
        remote_as: 65001,
        description: "upstream".into(),
        state: "Established".into(),
        uptime: "2d04h".into(),
        msg_rcvd: 1234,
        msg_sent: 1230,
        prefixes_received: 42,
        prefixes_accepted: 42,
        prefixes_advertised: 12,
        next_hop_self: true,
        ebgp_multihop: 2,
    });
    state
}

/// The VRRP oper state the spec's Part 3 sample shows.
pub fn vrrp_state() -> VrrpState {
    VrrpState {
        groups: vec![VrrpGroup {
            interface: "Vlan100".into(),
            group: 10,
            priority: 200,
            effective_priority: 200,
            advertisement_interval_ms: 1000,
            preempt: true,
            state: "Master".into(),
            addresses: vec!["10.0.100.1".into()],
            virtual_mac: "00:00:5e:00:01:0a".into(),
            skew_time_ms: 210,
            master_down_interval_ms: 3210,
            seconds_since_transition: Some(252),
        }],
    }
}
