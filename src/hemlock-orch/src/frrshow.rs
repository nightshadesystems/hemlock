//! Live FRR protocol state via `vtysh -c '... json'`.
//!
//! orch owns vtysh access — hemlockctl and webd ask orch, keeping the
//! FRR query surface in one place. Parsing is defensive: FRR's JSON
//! shape drifts between releases, so absent fields degrade to
//! defaults, and a dead FRR (or a host without it) degrades to a
//! "<protocol> is not running" error that the CLI renders as
//! `% ospf is not running`.

use hemlock_common::proto::v1 as pb;
use serde_json::Value;

/// Run one vtysh show command and parse its JSON output.
async fn vtysh_json(command: &str) -> Result<Value, String> {
    let output = tokio::process::Command::new("vtysh")
        .args(["-c", command])
        .output()
        .await
        .map_err(|_| "frr is not running".to_string())?;
    if !output.status.success() {
        return Err("frr is not running".into());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(text.trim()).map_err(|_| "unexpected vtysh output".into())
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn num(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// `show ip ospf json` + neighbors + interfaces.
pub async fn ospf_state() -> Result<pb::GetOspfStateResponse, String> {
    let overview = vtysh_json("show ip ospf json").await?;
    if overview.get("routerId").is_none() {
        return Err("ospf is not running".into());
    }
    let mut response = pb::GetOspfStateResponse {
        router_id: text(&overview, "routerId"),
        spf_runs: num(&overview, "spfExecutedCounter") as u32,
        areas: Vec::new(),
        neighbors: Vec::new(),
        interfaces: Vec::new(),
    };
    if let Some(Value::Object(areas)) = overview.get("areas") {
        for (id, area) in areas {
            response.areas.push(pb::OspfArea {
                id: id.clone(),
                interfaces: num(area, "areaIfTotalCounter") as u32,
            });
        }
    }

    let neighbors = vtysh_json("show ip ospf neighbor json").await?;
    if let Some(Value::Object(entries)) = neighbors.get("neighbors") {
        for (router_id, states) in entries {
            let Value::Array(states) = states else {
                continue;
            };
            for state in states {
                // "Full/DR" -> "Full".
                let full_state = text(state, "nbrState");
                let state_word = full_state.split('/').next().unwrap_or_default();
                response.neighbors.push(pb::OspfNeighborState {
                    router_id: router_id.clone(),
                    priority: num(state, "nbrPriority") as u32,
                    state: state_word.to_string(),
                    dead_time_msecs: num(state, "routerDeadIntervalTimerDueMsec"),
                    address: text(state, "ifaceAddress"),
                    interface: text(state, "ifaceName")
                        .split(':')
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                });
            }
        }
    }

    let interfaces = vtysh_json("show ip ospf interface json").await?;
    if let Some(Value::Object(entries)) = interfaces.get("interfaces") {
        for (name, iface) in entries {
            let prefix = num(iface, "ipAddressPrefixlen");
            response.interfaces.push(pb::OspfInterfaceState {
                name: name.clone(),
                up: iface.get("ifUp").and_then(|v| v.as_bool()).unwrap_or(false),
                address: format!("{}/{prefix}", text(iface, "ipAddress")),
                area: text(iface, "area"),
                router_id: text(iface, "routerId"),
                network_type: text(iface, "networkType"),
                cost: num(iface, "cost") as u32,
                state: text(iface, "state"),
                priority: num(iface, "priority") as u32,
                dr_id: text(iface, "drId"),
                dr_address: text(iface, "drAddress"),
                hello_interval: (num(iface, "timerMsecs") / 1000) as u32,
                dead_interval: num(iface, "timerDeadSecs") as u32,
                neighbors: num(iface, "nbrCount") as u32,
                adjacent: num(iface, "nbrAdjacentCount") as u32,
            });
        }
    }
    Ok(response)
}

/// `show bgp ipv4 unicast summary json` (+ the RIB, or one neighbor's
/// detail when `neighbor` is given).
pub async fn bgp_state(neighbor: &str) -> Result<pb::GetBgpStateResponse, String> {
    let summary = vtysh_json("show bgp ipv4 unicast summary json").await?;
    let summary = summary.get("ipv4Unicast").unwrap_or(&summary).clone();
    if summary.get("routerId").is_none() {
        return Err("bgp is not running".into());
    }
    let mut response = pb::GetBgpStateResponse {
        router_id: text(&summary, "routerId"),
        as_number: num(&summary, "as") as u32,
        peers: Vec::new(),
        routes: Vec::new(),
        detail: None,
    };
    if let Some(Value::Object(peers)) = summary.get("peers") {
        for (ip, peer) in peers {
            let state = text(peer, "state");
            // Established peers report a prefix count; others report
            // the state word in the PfxRcd column (-1 marks that).
            let established = state == "Established";
            response.peers.push(pb::BgpPeerState {
                ip: ip.clone(),
                version: num(peer, "version") as u32,
                remote_as: num(peer, "remoteAs") as u32,
                msg_rcvd: num(peer, "msgRcvd"),
                msg_sent: num(peer, "msgSent"),
                in_q: num(peer, "inq") as u32,
                out_q: num(peer, "outq") as u32,
                up_down: text(peer, "peerUptime"),
                state,
                pfx_rcvd: if established {
                    num(peer, "pfxRcd") as i64
                } else {
                    -1
                },
            });
        }
    }

    if !neighbor.is_empty() {
        let detail = vtysh_json(&format!("show bgp neighbors {neighbor} json")).await?;
        let Some(entry) = detail.get(neighbor) else {
            return Err(format!("no such neighbor {neighbor}"));
        };
        let message_stats = entry.get("messageStats").cloned().unwrap_or_default();
        let af = entry
            .get("addressFamilyInfo")
            .and_then(|v| v.get("ipv4Unicast"))
            .cloned()
            .unwrap_or_default();
        response.detail = Some(pb::BgpNeighborDetail {
            ip: neighbor.to_string(),
            remote_as: num(entry, "remoteAs") as u32,
            description: text(entry, "nbrDesc"),
            state: text(entry, "bgpState"),
            uptime: text(entry, "bgpTimerUpString"),
            msg_rcvd: num(&message_stats, "totalRecv"),
            msg_sent: num(&message_stats, "totalSent"),
            prefixes_received: num(entry, "prefixReceivedCount") as i64,
            prefixes_accepted: num(&af, "acceptedPrefixCounter") as i64,
            prefixes_advertised: num(&af, "sentPrefixCounter") as i64,
            next_hop_self: af
                .get("routerAlwaysNextHop")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            ebgp_multihop: num(entry, "externalBgpNbrMaxHopsAway") as u32,
        });
        return Ok(response);
    }

    let rib = vtysh_json("show bgp ipv4 unicast json").await?;
    if let Some(Value::Object(routes)) = rib.get("routes") {
        for (network, paths) in routes {
            let Value::Array(paths) = paths else { continue };
            for path in paths {
                let next_hop = path
                    .get("nexthops")
                    .and_then(|v| v.as_array())
                    .and_then(|hops| hops.first())
                    .map(|hop| text(hop, "ip"))
                    .filter(|ip| !ip.is_empty())
                    .unwrap_or_else(|| "-".into());
                let origin_code = match text(path, "origin").as_str() {
                    "IGP" => "i",
                    "EGP" => "e",
                    _ => "?",
                };
                let as_path = text(path, "path");
                let path_column = if as_path.is_empty() {
                    origin_code.to_string()
                } else {
                    format!("{as_path} {origin_code}")
                };
                response.routes.push(pb::BgpRibEntry {
                    network: network.clone(),
                    next_hop,
                    metric: path
                        .get("metric")
                        .and_then(|v| v.as_u64())
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| "-".into()),
                    loc_pref: path
                        .get("locPrf")
                        .and_then(|v| v.as_u64())
                        .map(|l| l.to_string())
                        .unwrap_or_else(|| "100".into()),
                    path: path_column,
                    valid: path.get("valid").and_then(|v| v.as_bool()).unwrap_or(false),
                    best: path
                        .get("bestpath")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                });
            }
        }
    }
    Ok(response)
}

/// `show vrrp json`.
pub async fn vrrp_state() -> Result<pb::GetVrrpStateResponse, String> {
    let groups = vtysh_json("show vrrp json").await?;
    let Value::Array(entries) = groups else {
        return Err("vrrp is not running".into());
    };
    let mut response = pb::GetVrrpStateResponse { groups: Vec::new() };
    for entry in entries {
        let v4 = entry.get("v4").cloned().unwrap_or_default();
        let group = num(&entry, "vrid") as u32;
        response.groups.push(pb::VrrpGroupState {
            interface: text(&entry, "interface"),
            group,
            priority: num(&entry, "priority") as u32,
            effective_priority: num(&v4, "effectivePriority") as u32,
            advertisement_interval_ms: num(&entry, "advertisementIntervalCs") as u32 * 10,
            preempt: entry
                .get("preemptMode")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            state: text(&v4, "status"),
            addresses: v4
                .get("addresses")
                .and_then(|v| v.as_array())
                .map(|addresses| {
                    addresses
                        .iter()
                        .filter_map(|a| a.as_str())
                        .map(|a| a.split('/').next().unwrap_or(a).to_string())
                        .collect()
                })
                .unwrap_or_default(),
            virtual_mac: format!("00:00:5e:00:01:{group:02x}"),
            skew_time_ms: (num(&v4, "skewTimeCs") as u32) * 10,
            master_down_interval_ms: (num(&v4, "masterDownIntervalCs") as u32) * 10,
            seconds_since_transition: v4.get("statusLastChangeSecs").and_then(|v| v.as_u64()),
        });
    }
    Ok(response)
}

/// `clear bgp *` / `clear bgp <ip>`.
pub async fn clear_bgp(neighbor: &str) -> bool {
    let target = if neighbor.is_empty() { "*" } else { neighbor };
    matches!(
        tokio::process::Command::new("vtysh")
            .args(["-c", &format!("clear bgp {target}")])
            .status()
            .await,
        Ok(status) if status.success()
    )
}
