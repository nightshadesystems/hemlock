//! Live state for the switching-suite shows, fetched from syncd.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;

use crate::interfaces::name;

use super::model::{MacEntry, MacTable, MirrorSession, StormRow, VlanRow};

async fn client(
    syncd: &IpcEndpoint,
) -> Result<pb::syncd_client::SyncdClient<tonic::transport::Channel>> {
    let channel = syncd.connect().await.context("connecting to syncd")?;
    Ok(pb::syncd_client::SyncdClient::new(channel))
}

/// The VLAN table: every VLAN with its display name, status, and member
/// ports (untagged and tagged alike).
pub async fn vlans(syncd: &IpcEndpoint) -> Result<Vec<VlanRow>> {
    let response = client(syncd)
        .await?
        .get_interfaces(pb::GetInterfacesRequest { names: vec![] })
        .await?
        .into_inner();

    let mut rows: BTreeMap<u32, VlanRow> = response
        .active_vlans
        .iter()
        .map(|&id| {
            (
                id,
                VlanRow {
                    id,
                    name: response.vlan_names.get(&id).cloned(),
                    suspended: response.suspended_vlans.contains(&id),
                    ports: Vec::new(),
                },
            )
        })
        .collect();

    for iface in &response.interfaces {
        if !matches!(iface.kind.as_str(), "ethernet" | "port-channel") {
            continue;
        }
        // Routed ports are not bridge members.
        if !iface.ip_addresses.is_empty() {
            continue;
        }
        let Some(id) = name::parse_one(&iface.name) else {
            continue;
        };
        let default = |vlan: u32| if vlan == 0 { 1 } else { vlan };
        let mut member_of: Vec<u32> = Vec::new();
        if iface.switchport_mode == "trunk" {
            member_of.push(default(iface.native_vlan));
            member_of.extend(iface.trunk_vlans.iter().copied());
        } else {
            member_of.push(default(iface.access_vlan));
        }
        for vlan in member_of {
            if let Some(row) = rows.get_mut(&vlan) {
                if !row.ports.contains(&id) {
                    row.ports.push(id);
                }
            }
        }
    }
    let mut rows: Vec<VlanRow> = rows.into_values().collect();
    for row in &mut rows {
        row.ports.sort();
    }
    Ok(rows)
}

/// The MAC address table, filtered server-side.
pub async fn mac_table(
    syncd: &IpcEndpoint,
    vlan: u32,
    port: Option<&str>,
    mac: Option<&str>,
    kind: pb::FdbEntryKind,
) -> Result<MacTable> {
    let response = client(syncd)
        .await?
        .dump_fdb(pb::DumpFdbRequest {
            vlan,
            port: port.unwrap_or_default().to_string(),
            mac: mac.unwrap_or_default().to_string(),
            kind: kind as i32,
            page_size: 0,
            page_token: String::new(),
        })
        .await?
        .into_inner();
    Ok(MacTable {
        aging_time_secs: response.aging_time_secs,
        entries: response
            .entries
            .iter()
            .map(|e| MacEntry {
                vlan: e.vlan,
                mac: e.mac.clone(),
                is_static: e.is_static,
                port: (!e.port.is_empty())
                    .then(|| name::parse_one(&e.port))
                    .flatten(),
                drop: e.drop,
                moves: e.moves,
                last_move_secs: e.seconds_since_move,
            })
            .collect(),
    })
}

/// The storm-control table.
pub async fn storm_control(syncd: &IpcEndpoint) -> Result<Vec<StormRow>> {
    let response = client(syncd)
        .await?
        .get_storm_control(pb::GetStormControlRequest {})
        .await?
        .into_inner();
    Ok(response
        .entries
        .iter()
        .filter_map(|e| {
            Some(StormRow {
                port: name::parse_one(&e.name)?,
                kind: match e.class {
                    c if c == pb::StormClass::Broadcast as i32 => "broadcast",
                    c if c == pb::StormClass::Multicast as i32 => "multicast",
                    _ => "unknown-unicast",
                }
                .into(),
                level: e.level.clone(),
                rate_kbps: e.rate_kbps,
                drops: e.drops,
                active: e.active,
            })
        })
        .collect())
}

use super::model::{LacpSystem, PortChannel, PortChannelMember};

async fn orch_client(
    orch: &IpcEndpoint,
) -> Result<pb::orch_client::OrchClient<tonic::transport::Channel>> {
    let channel = orch.connect().await.context("connecting to orch")?;
    Ok(pb::orch_client::OrchClient::new(channel))
}

/// Port-channels with their LACP runtime: identity/admin from syncd,
/// protocol state from orch, MAC from the interface view.
pub async fn port_channels(syncd: &IpcEndpoint, orch: &IpcEndpoint) -> Result<Vec<PortChannel>> {
    let lags = client(syncd)
        .await?
        .get_lags(pb::GetLagsRequest {})
        .await?
        .into_inner()
        .lags;
    let lacp = orch_client(orch)
        .await?
        .get_lacp_state(pb::GetLacpStateRequest {})
        .await?
        .into_inner();
    let interfaces = client(syncd)
        .await?
        .get_interfaces(pb::GetInterfacesRequest { names: vec![] })
        .await?
        .into_inner();

    let mut out = Vec::new();
    for lag in &lags {
        let name = format!("Port-Channel{}", lag.group);
        let mac = interfaces
            .interfaces
            .iter()
            .find(|i| i.name == name)
            .map(|i| i.mac.clone())
            .unwrap_or_default();
        let state = lacp.lags.iter().find(|l| l.group == lag.group);
        let members = match state {
            Some(state) => state
                .members
                .iter()
                .filter_map(|m| {
                    Some(PortChannelMember {
                        id: name::parse_one(&m.port)?,
                        status: m.status.clone(),
                        rate_fast: m.rate_fast,
                        actor_state: m.actor_state as u8,
                        partner_state: m.partner_state as u8,
                        partner_system: m.partner_system.clone(),
                        partner_port: m.partner_port,
                        partner_key: m.partner_key,
                        partner_priority: m.partner_priority,
                        pdus_rx: m.pdus_rx,
                        pdus_tx: m.pdus_tx,
                        churn: m.churn,
                    })
                })
                .collect(),
            // Orch has no protocol state yet (fresh restart): show the
            // syncd membership with gate-derived statuses.
            None => lag
                .members
                .iter()
                .filter_map(|m| {
                    Some(PortChannelMember {
                        id: name::parse_one(&m.port)?,
                        status: if m.enabled && m.oper_up {
                            "bundled"
                        } else if m.oper_up {
                            "standby"
                        } else {
                            "down"
                        }
                        .into(),
                        rate_fast: false,
                        actor_state: 0,
                        partner_state: 0,
                        partner_system: String::new(),
                        partner_port: 0,
                        partner_key: 0,
                        partner_priority: 0,
                        pdus_rx: 0,
                        pdus_tx: 0,
                        churn: 0,
                    })
                })
                .collect(),
        };
        out.push(PortChannel {
            group: lag.group,
            description: lag.description.clone(),
            admin_up: lag.admin_up,
            up: state.map(|s| s.up).unwrap_or_else(|| {
                lag.members.iter().any(|m| m.enabled && m.oper_up)
            }),
            lacp: state.map(|s| s.lacp).unwrap_or(true),
            active_mode: state.map(|s| s.active_mode).unwrap_or(false),
            bundled: state.map(|s| s.bundled).unwrap_or_else(|| {
                lag.members.iter().filter(|m| m.enabled && m.oper_up).count() as u32
            }),
            total: state
                .map(|s| s.total)
                .unwrap_or(lag.members.len() as u32),
            min_links: state.map(|s| s.min_links).unwrap_or(0),
            fallback_mode: state.map(|s| s.fallback_mode.clone()).unwrap_or_default(),
            fallback_timeout_secs: state.map(|s| s.fallback_timeout_secs).unwrap_or(90),
            fallback_active: state.map(|s| s.fallback_active).unwrap_or(false),
            mac,
            members,
        });
    }
    out.sort_by_key(|lag| lag.group);
    Ok(out)
}

/// The spanning-tree bridge view from orch.
pub async fn stp_bridge(orch: &IpcEndpoint) -> Result<super::model::StpBridge> {
    let state = orch_client(orch)
        .await?
        .get_stp_state(pb::GetStpStateRequest {})
        .await?
        .into_inner();
    let mut ports: Vec<super::model::StpPortRow> = state
        .ports
        .iter()
        .filter_map(|p| {
            Some(super::model::StpPortRow {
                id: name::parse_one(&p.port)?,
                role: p.role.clone(),
                state: p.state.clone(),
                cost: p.cost,
                priority: p.priority,
                portfast: p.portfast,
                bpduguard: p.bpduguard,
                bpdus_rx: p.bpdus_rx,
                bpdus_tx: p.bpdus_tx,
                errdisabled: p.errdisabled,
            })
        })
        .collect();
    ports.sort_by_key(|p| p.id);
    Ok(super::model::StpBridge {
        mode: state.mode,
        bridge_priority: state.bridge_priority,
        bridge_mac: state.bridge_mac,
        root_priority: state.root_priority,
        root_mac: state.root_mac,
        is_root: state.is_root,
        root_cost: state.root_cost,
        root_port: name::parse_one(&state.root_port),
        hello_time: state.hello_time,
        max_age: state.max_age,
        forward_time: state.forward_time,
        mst_name: state.mst_name,
        mst_revision: state.mst_revision,
        instances: state
            .instances
            .iter()
            .filter(|map| map.instance != 0)
            .map(|map| (map.instance, map.vlans.clone()))
            .collect(),
        topology_changes: state.topology_changes,
        seconds_since_tc: state.seconds_since_tc,
        last_tc_port: (!state.last_tc_port.is_empty()).then(|| state.last_tc_port.clone()),
        ports,
    })
}

/// One snooping family's view from orch.
pub async fn snooping(
    orch: &IpcEndpoint,
    family: &str,
) -> Result<super::model::SnoopingView> {
    let state = orch_client(orch)
        .await?
        .get_snooping_state(pb::GetSnoopingStateRequest {
            family: family.to_string(),
        })
        .await?
        .into_inner();
    let ids = |names: &[String]| -> Vec<crate::interfaces::name::InterfaceId> {
        let mut out: Vec<_> = names.iter().filter_map(|n| name::parse_one(n)).collect();
        out.sort();
        out
    };
    Ok(super::model::SnoopingView {
        family: family.to_uppercase(),
        enabled: state.enabled,
        robustness: state.robustness,
        vlans: state
            .vlans
            .iter()
            .map(|vlan| super::model::SnoopVlanView {
                vlan: vlan.vlan,
                enabled: vlan.enabled,
                fast_leave: vlan.fast_leave,
                querier_enabled: vlan.querier_enabled,
                querier_address: (!vlan.querier_address.is_empty())
                    .then(|| vlan.querier_address.clone()),
                querier_active: vlan.querier_active,
                static_mrouters: ids(&vlan.static_mrouters),
                dynamic_mrouters: ids(&vlan.dynamic_mrouters),
                groups: vlan
                    .groups
                    .iter()
                    .map(|group| super::model::SnoopGroupView {
                        group: group.group.clone(),
                        version: group.version,
                        ports: ids(&group.ports),
                    })
                    .collect(),
            })
            .collect(),
    })
}

/// The LACP actor system id.
pub async fn lacp_system(orch: &IpcEndpoint) -> Result<LacpSystem> {
    let state = orch_client(orch)
        .await?
        .get_lacp_state(pb::GetLacpStateRequest {})
        .await?
        .into_inner();
    Ok(LacpSystem {
        system_id: state.system_id,
    })
}

/// The mirror sessions.
pub async fn mirror(syncd: &IpcEndpoint) -> Result<Vec<MirrorSession>> {
    let response = client(syncd)
        .await?
        .get_mirror_sessions(pb::GetMirrorSessionsRequest {})
        .await?
        .into_inner();
    Ok(response
        .sessions
        .iter()
        .map(|s| {
            let mut session = MirrorSession {
                session: s.session,
                rx: Vec::new(),
                tx: Vec::new(),
                both: Vec::new(),
                destination: name::parse_one(&s.destination),
                destination_active: s.destination_up,
            };
            for source in &s.sources {
                let Some(id) = name::parse_one(&source.name) else {
                    continue;
                };
                match source.direction {
                    d if d == pb::MirrorDirection::Rx as i32 => session.rx.push(id),
                    d if d == pb::MirrorDirection::Tx as i32 => session.tx.push(id),
                    _ => session.both.push(id),
                }
            }
            for list in [&mut session.rx, &mut session.tx, &mut session.both] {
                list.sort();
            }
            session
        })
        .collect())
}

/// The `clear counters` verb: baseline the named interfaces (empty =
/// every tracked interface).
pub async fn clear_counters(syncd: &IpcEndpoint, names: Vec<String>) -> Result<u32> {
    Ok(client(syncd)
        .await?
        .clear_counters(pb::ClearCountersRequest { names })
        .await?
        .into_inner()
        .cleared)
}

/// The `clear mac-table` verb: flush dynamic FDB entries, optionally
/// scoped. Returns the number of flushed entries.
pub async fn clear_mac_table(syncd: &IpcEndpoint, vlan: u32, port: Option<String>) -> Result<u32> {
    Ok(client(syncd)
        .await?
        .flush_fdb(pb::FlushFdbRequest {
            vlan,
            port: port.unwrap_or_default(),
        })
        .await?
        .into_inner()
        .flushed)
}

/// Known interface selectors for `clear mac-table interface <port>`.
pub async fn port_names(syncd: &IpcEndpoint) -> Result<Vec<String>> {
    Ok(client(syncd)
        .await?
        .list_ports(pb::ListPortsRequest {})
        .await?
        .into_inner()
        .ports
        .into_iter()
        .map(|p| p.name)
        .collect())
}
