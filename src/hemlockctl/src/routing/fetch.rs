//! Route state for the routing-suite shows. Phase 1 builds a
//! kernel-only snapshot: connected routes from syncd's interface
//! addresses, statics from the running config (the kernel's source for
//! both). The orch RIB snapshot (`GetRib`) replaces this as the source
//! when the FIB pipeline lands, adding protocol routes and uptimes.

use std::collections::BTreeMap;
use std::net::IpAddr;

use anyhow::{Context, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;

use super::model::{NextHop, RouteEntry, RouteTable};

/// One connected subnet: (network, length, interface name).
type Connected = (IpAddr, u8, String);

/// The route table for one address family.
pub async fn route_table(mgmtd: &IpcEndpoint, syncd: &IpcEndpoint, v6: bool) -> Result<RouteTable> {
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
