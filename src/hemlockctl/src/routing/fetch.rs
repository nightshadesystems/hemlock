//! Route state for the routing-suite shows.
//!
//! The source of truth is orch's RIB snapshot (`GetRib`) — statics,
//! connected, and FRR routes uniformly, with FIB state. On hosts where
//! the RIB pipeline is not running (orch down, or a dev host without
//! the kernel feed) the fetch falls back to the config-derived view:
//! connected routes from syncd's interface addresses, statics from the
//! running config.

use std::collections::BTreeMap;
use std::net::IpAddr;

use anyhow::{Context, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;

use super::model::{NeighborEntry, NeighborTable, NextHop, RouteEntry, RouteTable};

/// One connected subnet: (network, length, interface name).
type Connected = (IpAddr, u8, String);

/// The route table for one address family: the orch RIB when it has
/// one, else the config-derived fallback.
pub async fn route_table(
    mgmtd: &IpcEndpoint,
    syncd: &IpcEndpoint,
    orch: &IpcEndpoint,
    v6: bool,
) -> Result<RouteTable> {
    if let Ok(Some(table)) = rib_table(orch, v6).await {
        return Ok(table);
    }
    config_route_table(mgmtd, syncd, v6).await
}

/// orch's RIB snapshot; Ok(None) when the pipeline holds nothing (no
/// kernel feed yet), so callers fall back.
async fn rib_table(orch: &IpcEndpoint, v6: bool) -> Result<Option<RouteTable>> {
    let channel = orch.connect().await.context("connecting to orch")?;
    let response = pb::orch_client::OrchClient::new(channel)
        .get_rib(pb::GetRibRequest {
            ipv6: v6,
            page_size: 0,
            page_token: String::new(),
        })
        .await?
        .into_inner();
    if response.routes.is_empty() {
        return Ok(None);
    }
    let routes = response
        .routes
        .into_iter()
        .map(|route| RouteEntry {
            protocol: route.protocol,
            interface: if route.fib == "drop" {
                Some("Null0".into())
            } else if route.interface.is_empty() {
                None
            } else {
                Some(route.interface)
            },
            prefix: route.prefix,
            distance: route.distance,
            metric: route.metric,
            next_hops: route
                .next_hops
                .into_iter()
                .map(|hop| NextHop {
                    via: hop.via,
                    interface: (!hop.interface.is_empty()).then_some(hop.interface),
                })
                .collect(),
            fib: Some(route.fib),
        })
        .collect();
    Ok(Some(RouteTable { routes }))
}

/// The neighbor table for one family, from orch.
pub async fn neighbors(orch: &IpcEndpoint, v6: bool) -> Result<NeighborTable> {
    let channel = orch.connect().await.context("connecting to orch")?;
    let response = pb::orch_client::OrchClient::new(channel)
        .get_neighbors(pb::GetNeighborsRequest { ipv6: v6 })
        .await?
        .into_inner();
    Ok(NeighborTable {
        entries: response
            .neighbors
            .into_iter()
            .map(|entry| NeighborEntry {
                ip: entry.ip,
                mac: entry.mac,
                interface: entry.interface,
                is_static: entry.permanent,
                age_secs: entry.age_secs,
            })
            .collect(),
    })
}

/// Hardware next-hop-group count for the summary; 0 when syncd is
/// unreachable (the counts degrade, the summary still renders).
pub async fn next_hop_groups(syncd: &IpcEndpoint) -> u32 {
    let Ok(channel) = syncd.connect().await else {
        return 0;
    };
    pb::syncd_client::SyncdClient::new(channel)
        .get_fib_summary(pb::GetFibSummaryRequest {})
        .await
        .map(|response| response.into_inner().next_hop_groups)
        .unwrap_or(0)
}

/// The config-derived fallback table.
async fn config_route_table(
    mgmtd: &IpcEndpoint,
    syncd: &IpcEndpoint,
    v6: bool,
) -> Result<RouteTable> {
    let channel = syncd.connect().await.context("connecting to syncd")?;
    let response = pb::syncd_client::SyncdClient::new(channel)
        .get_interfaces(pb::GetInterfacesRequest { names: vec![] })
        .await?
        .into_inner();

    // Connected subnets — both route entries themselves and how next
    // hops resolve to an egress interface.
    let mut connected: Vec<Connected> = Vec::new();
    for iface in &response.interfaces {
        for cidr in &iface.ip_addresses {
            let Ok((addr, len)) = hemlock_common::net::parse_cidr(cidr) else {
                continue;
            };
            if addr.is_ipv4() == v6 {
                continue;
            }
            connected.push((
                hemlock_common::net::network(addr, len),
                len,
                iface.name.clone(),
            ));
        }
    }

    let mut routes: Vec<RouteEntry> = connected
        .iter()
        .map(|(net, len, name)| RouteEntry {
            protocol: "connected".into(),
            prefix: format!("{net}/{len}"),
            distance: 0,
            metric: 0,
            next_hops: Vec::new(),
            interface: Some(name.clone()),
            fib: None,
        })
        .collect();

    let channel = mgmtd.connect().await.context("connecting to mgmtd")?;
    let text = pb::mgmt_client::MgmtClient::new(channel)
        .get_config(pb::GetConfigRequest {
            source: pb::ConfigSource::Running as i32,
        })
        .await?
        .into_inner()
        .text;
    if let Ok(tree) = hemlock_config::parse(&text) {
        routes.extend(static_routes(&tree, v6, &connected));
    }

    routes.sort_by_key(|route| prefix_key(&route.prefix));
    Ok(RouteTable { routes })
}

/// Static routes from the running config tree. The config was validated
/// at commit, so anything malformed is silently skipped rather than
/// re-diagnosed here.
fn static_routes(
    tree: &hemlock_config::ConfigTree,
    v6: bool,
    connected: &[Connected],
) -> Vec<RouteEntry> {
    struct Static {
        hops: Vec<String>,
        drop: bool,
        distance: u32,
    }
    let mut statics: BTreeMap<String, Static> = BTreeMap::new();
    let Some((_, routing)) = tree.block("routing") else {
        return Vec::new();
    };
    for (_, children) in hemlock_config::ConfigTree::blocks_named(routing, "static") {
        for item in children {
            let hemlock_config::Item::Leaf {
                name: prefix,
                values,
            } = item
            else {
                continue;
            };
            let Ok((addr, _)) = hemlock_common::net::parse_cidr(prefix) else {
                continue;
            };
            if addr.is_ipv4() == v6 {
                continue;
            }
            let entry = statics.entry(prefix.clone()).or_insert(Static {
                hops: Vec::new(),
                drop: false,
                distance: 1,
            });
            match values.as_slice() {
                [keyword] if keyword == "drop" => entry.drop = true,
                [next_hop, rest @ ..] => {
                    if !entry.hops.contains(next_hop) {
                        entry.hops.push(next_hop.clone());
                    }
                    if let [keyword, value] = rest {
                        if keyword == "distance" {
                            if let Ok(distance) = value.parse() {
                                entry.distance = distance;
                            }
                        }
                    }
                }
                [] => {}
            }
        }
    }
    statics
        .into_iter()
        .map(|(prefix, mut route)| {
            route.hops.sort_by_key(|hop| hop.parse::<IpAddr>().ok());
            RouteEntry {
                protocol: "static".into(),
                prefix,
                distance: route.distance,
                metric: 0,
                next_hops: route
                    .hops
                    .iter()
                    .map(|via| NextHop {
                        via: via.clone(),
                        interface: egress(connected, via).map(str::to_string),
                    })
                    .collect(),
                interface: route.drop.then(|| "Null0".to_string()),
                fib: None,
            }
        })
        .collect()
}

/// The egress interface a next hop resolves onto: the longest connected
/// subnet containing it.
fn egress<'c>(connected: &'c [Connected], via: &str) -> Option<&'c str> {
    let addr: IpAddr = via.parse().ok()?;
    connected
        .iter()
        .filter(|(net, len, _)| {
            net.is_ipv4() == addr.is_ipv4() && hemlock_common::net::network(addr, *len) == *net
        })
        .max_by_key(|(_, len, _)| *len)
        .map(|(_, _, name)| name.as_str())
}

/// Numeric sort key for a canonical prefix (unparsable sorts last).
fn prefix_key(prefix: &str) -> (bool, Option<IpAddr>, u8) {
    match hemlock_common::net::parse_cidr(prefix) {
        Ok((addr, len)) => (false, Some(addr), len),
        Err(_) => (true, None, u8::MAX),
    }
}

// ------------------------------------------------- FRR protocol detail

async fn orch_client(
    orch: &IpcEndpoint,
) -> Result<pb::orch_client::OrchClient<tonic::transport::Channel>> {
    let channel = orch.connect().await.context("connecting to orch")?;
    Ok(pb::orch_client::OrchClient::new(channel))
}

/// Live OSPF state from orch (which queries vtysh). A dead FRR comes
/// back as the RPC's failed-precondition message ("ospf is not
/// running"), surfaced by the command layer.
pub async fn ospf_state(orch: &IpcEndpoint) -> Result<super::model::OspfState, String> {
    let mut client = orch_client(orch).await.map_err(|e| format!("{e:#}"))?;
    let state = client
        .get_ospf_state(pb::GetOspfStateRequest {})
        .await
        .map_err(|e| e.message().to_string())?
        .into_inner();
    Ok(super::model::OspfState {
        router_id: state.router_id,
        spf_runs: state.spf_runs,
        areas: state
            .areas
            .into_iter()
            .map(|area| super::model::OspfArea {
                id: area.id,
                interfaces: area.interfaces,
            })
            .collect(),
        neighbors: state
            .neighbors
            .into_iter()
            .map(|n| super::model::OspfNeighbor {
                router_id: n.router_id,
                priority: n.priority,
                state: n.state,
                dead_time_msecs: n.dead_time_msecs,
                address: n.address,
                interface: n.interface,
            })
            .collect(),
        interfaces: state
            .interfaces
            .into_iter()
            .map(|i| super::model::OspfInterface {
                name: i.name,
                up: i.up,
                address: i.address,
                area: i.area,
                router_id: i.router_id,
                network_type: i.network_type,
                cost: i.cost,
                state: i.state,
                priority: i.priority,
                dr_id: i.dr_id,
                dr_address: i.dr_address,
                hello_interval: i.hello_interval,
                dead_interval: i.dead_interval,
                neighbors: i.neighbors,
                adjacent: i.adjacent,
            })
            .collect(),
    })
}

/// Live BGP state (summary + RIB, or one neighbor's detail).
pub async fn bgp_state(
    orch: &IpcEndpoint,
    neighbor: &str,
) -> Result<super::model::BgpState, String> {
    let mut client = orch_client(orch).await.map_err(|e| format!("{e:#}"))?;
    let state = client
        .get_bgp_state(pb::GetBgpStateRequest {
            neighbor: neighbor.to_string(),
        })
        .await
        .map_err(|e| e.message().to_string())?
        .into_inner();
    Ok(super::model::BgpState {
        router_id: state.router_id,
        as_number: state.as_number,
        peers: state
            .peers
            .into_iter()
            .map(|p| super::model::BgpPeer {
                ip: p.ip,
                version: p.version,
                remote_as: p.remote_as,
                msg_rcvd: p.msg_rcvd,
                msg_sent: p.msg_sent,
                in_q: p.in_q,
                out_q: p.out_q,
                up_down: p.up_down,
                state: p.state,
                pfx_rcvd: p.pfx_rcvd,
            })
            .collect(),
        routes: state
            .routes
            .into_iter()
            .map(|r| super::model::BgpRibEntry {
                network: r.network,
                next_hop: r.next_hop,
                metric: r.metric,
                loc_pref: r.loc_pref,
                path: r.path,
                valid: r.valid,
                best: r.best,
            })
            .collect(),
        detail: state.detail.map(|d| super::model::BgpNeighborDetail {
            ip: d.ip,
            remote_as: d.remote_as,
            description: d.description,
            state: d.state,
            uptime: d.uptime,
            msg_rcvd: d.msg_rcvd,
            msg_sent: d.msg_sent,
            prefixes_received: d.prefixes_received,
            prefixes_accepted: d.prefixes_accepted,
            prefixes_advertised: d.prefixes_advertised,
            next_hop_self: d.next_hop_self,
            ebgp_multihop: d.ebgp_multihop,
        }),
    })
}

/// Live VRRP group state.
pub async fn vrrp_state(orch: &IpcEndpoint) -> Result<super::model::VrrpState, String> {
    let mut client = orch_client(orch).await.map_err(|e| format!("{e:#}"))?;
    let state = client
        .get_vrrp_state(pb::GetVrrpStateRequest {})
        .await
        .map_err(|e| e.message().to_string())?
        .into_inner();
    Ok(super::model::VrrpState {
        groups: state
            .groups
            .into_iter()
            .map(|g| super::model::VrrpGroup {
                interface: g.interface,
                group: g.group,
                priority: g.priority,
                effective_priority: g.effective_priority,
                advertisement_interval_ms: g.advertisement_interval_ms,
                preempt: g.preempt,
                state: g.state,
                addresses: g.addresses,
                virtual_mac: g.virtual_mac,
                skew_time_ms: g.skew_time_ms,
                master_down_interval_ms: g.master_down_interval_ms,
                seconds_since_transition: g.seconds_since_transition,
            })
            .collect(),
    })
}
