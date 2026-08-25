//! Live services state from orch: LLDP settings, per-port frame
//! counters and the aged neighbor table.

use anyhow::{Context, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;

use super::model::{LldpNeighbor, LldpPort, LldpState};

async fn orch_client(
    orch: &IpcEndpoint,
) -> Result<pb::orch_client::OrchClient<tonic::transport::Channel>> {
    let channel = orch.connect().await.context("connecting to orch")?;
    Ok(pb::orch_client::OrchClient::new(channel))
}

/// The LLDP state, optionally scoped to one port's full display name.
pub async fn lldp_state(orch: &IpcEndpoint, port: &str) -> Result<LldpState> {
    let response = orch_client(orch)
        .await?
        .get_lldp_state(pb::GetLldpStateRequest {
            port: port.to_string(),
        })
        .await?
        .into_inner();
    Ok(LldpState {
        enabled: response.enabled,
        tx_interval: response.tx_interval,
        hold_multiplier: response.hold_multiplier,
        chassis_id: response.chassis_id,
        system_name: response.system_name,
        system_description: response.system_description,
        management_address: response.management_address,
        ports: response
            .ports
            .into_iter()
            .map(|port| LldpPort {
                neighbors: port
                    .neighbors
                    .into_iter()
                    .map(|neighbor| LldpNeighbor {
                        port: port.port.clone(),
                        chassis_id: neighbor.chassis_id,
                        chassis_id_subtype: neighbor.chassis_id_subtype,
                        port_id: neighbor.port_id,
                        port_id_subtype: neighbor.port_id_subtype,
                        port_description: neighbor.port_description,
                        system_name: neighbor.system_name,
                        system_description: neighbor.system_description,
                        management_address: neighbor.management_address,
                        ttl: neighbor.ttl,
                        age_secs: neighbor.age_secs,
                    })
                    .collect(),
                port: port.port,
                enabled: port.enabled,
                frames_tx: port.frames_tx,
                frames_rx: port.frames_rx,
                frames_discarded: port.frames_discarded,
                ageouts: port.ageouts,
            })
            .collect(),
    })
}

/// `clear lldp counters`: zero the per-port frame counters.
pub async fn clear_lldp_counters(orch: &IpcEndpoint) -> Result<u32> {
    Ok(orch_client(orch)
        .await?
        .clear_lldp_counters(pb::ClearLldpCountersRequest {
            port: String::new(),
        })
        .await?
        .into_inner()
        .cleared)
}
