//! Live QoS state from syncd's `GetQosState` — the global maps, the
//! WRED profiles with their queue references, and per-port effective
//! config with live per-queue counters.

use anyhow::{Context, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;

use super::model::{
    MapEntry, MapState, MapTable, PortQos, PortQosState, QueueQos, WredProfile, WredState,
};

async fn syncd_client(
    syncd: &IpcEndpoint,
) -> Result<pb::syncd_client::SyncdClient<tonic::transport::Channel>> {
    let channel = syncd.connect().await.context("connecting to syncd")?;
    Ok(pb::syncd_client::SyncdClient::new(channel))
}

async fn qos_state(syncd: &IpcEndpoint) -> Result<pb::GetQosStateResponse> {
    Ok(syncd_client(syncd)
        .await?
        .get_qos_state(pb::GetQosStateRequest {})
        .await?
        .into_inner())
}

fn entries(list: &[pb::QosMapEntry]) -> Vec<MapEntry> {
    let mut out: Vec<MapEntry> = list
        .iter()
        .map(|entry| MapEntry {
            key: u8::try_from(entry.key).unwrap_or(0),
            value: u8::try_from(entry.value).unwrap_or(0),
        })
        .collect();
    out.sort_by_key(|entry| entry.key);
    out
}

/// The four global map tables, in classification-then-rewrite order.
pub async fn maps(syncd: &IpcEndpoint) -> Result<MapState> {
    let state = qos_state(syncd).await?;
    let table =
        |table: &str, title: &str, key: &str, value: &str, note: &str, list: &[_]| MapTable {
            table: table.into(),
            title: title.into(),
            key_label: key.into(),
            value_label: value.into(),
            default_note: note.into(),
            entries: entries(list),
        };
    Ok(MapState {
        tables: vec![
            table(
                "dscp-to-tc",
                "DSCP to Traffic-Class map",
                "DSCP",
                "TC",
                "0",
                &state.dscp_to_tc,
            ),
            table(
                "cos-to-tc",
                "CoS to Traffic-Class map",
                "CoS",
                "TC",
                "0",
                &state.cos_to_tc,
            ),
            table(
                "tc-to-dscp",
                "Traffic-Class to DSCP rewrite map",
                "TC",
                "DSCP",
                "no rewrite",
                &state.tc_to_dscp,
            ),
            table(
                "tc-to-cos",
                "Traffic-Class to CoS rewrite map",
                "TC",
                "CoS",
                "no rewrite",
                &state.tc_to_cos,
            ),
        ],
    })
}

/// The abbreviated interface form for tabular output
/// ("Ethernet1" -> "Et1").
fn short_name(interface: &str) -> String {
    crate::interfaces::name::parse_one(interface)
        .map(|id| id.abbrev())
        .unwrap_or_else(|| interface.to_string())
}

/// The WRED profiles, each with the queues that reference it. A
/// physical port carrying its Port-Channel's program is credited to the
/// Port-Channel, so a reference is never listed twice.
pub async fn wred(syncd: &IpcEndpoint) -> Result<WredState> {
    let state = qos_state(syncd).await?;
    let profiles = state
        .wred_profiles
        .iter()
        .map(|profile| WredProfile {
            name: profile.name.clone(),
            min_threshold: profile.min_threshold_kb,
            max_threshold: profile.max_threshold_kb,
            drop_probability: profile.drop_probability,
            ecn: profile.ecn,
            references: state
                .ports
                .iter()
                .filter(|port| port.via_port_channel.is_empty())
                .flat_map(|port| {
                    port.queues
                        .iter()
                        .filter(|queue| queue.wred_profile == profile.name)
                        .map(move |queue| format!("{} (q{})", short_name(&port.port), queue.queue))
                })
                .collect(),
        })
        .collect();
    Ok(WredState {
        profiles,
        buffer_kb: state.buffer_kb,
        supported: state.wred_supported,
    })
}

/// Per-port effective QoS. `filter` scopes to one full display name;
/// empty keeps every row.
pub async fn ports(syncd: &IpcEndpoint, filter: &str) -> Result<PortQosState> {
    let state = qos_state(syncd).await?;
    let ports: Vec<PortQos> = state
        .ports
        .iter()
        .filter(|port| filter.is_empty() || port.port == filter)
        .map(port_qos)
        .collect();
    Ok(PortQosState {
        ports,
        default_ports: state.default_ports,
    })
}

fn port_qos(port: &pb::PortQosState) -> PortQos {
    PortQos {
        port: port.port.clone(),
        trust: port.trust.clone(),
        default_tc: u8::try_from(port.default_tc).unwrap_or(0),
        shaper: port.shape_bps.map(hemlock_common::net::display_shape_rate),
        queues: port.queues.iter().map(queue_qos).collect(),
        configured: port.configured,
        via_port_channel: (!port.via_port_channel.is_empty())
            .then(|| port.via_port_channel.clone()),
    }
}

fn queue_qos(queue: &pb::QueueQosState) -> QueueQos {
    QueueQos {
        queue: u8::try_from(queue.queue).unwrap_or(0),
        mode: if queue.strict { "strict" } else { "dwrr" }.into(),
        // A strict queue takes no DWRR share, so it carries no weight.
        weight: (!queue.strict).then_some(queue.weight),
        shaper: queue.shape_bps.map(hemlock_common::net::display_shape_rate),
        wred_profile: (!queue.wred_profile.is_empty()).then(|| queue.wred_profile.clone()),
        ecn: queue.ecn,
        tx_packets: queue.tx_packets,
        tx_bytes: queue.tx_bytes,
        dropped: queue.dropped,
        wred_dropped: queue.wred_dropped,
        ecn_marked: queue.ecn_marked,
    }
}
