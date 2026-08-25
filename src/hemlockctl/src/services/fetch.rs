//! Live services state from orch: LLDP settings, per-port frame
//! counters and the aged neighbor table.

use anyhow::{Context, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;

use super::model::{
    DhcpLease, DhcpPool, DhcpRelayState, DhcpRelayVlan, DhcpServerState, LldpNeighbor, LldpPort,
    LldpState, NtpState, SflowState, SnmpCommunity, SnmpState,
};

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

/// The NTP client's configured servers and live sync state.
pub async fn ntp_state(orch: &IpcEndpoint) -> Result<NtpState> {
    let response = orch_client(orch)
        .await?
        .get_ntp_state(pb::GetNtpStateRequest {})
        .await?
        .into_inner();
    Ok(NtpState {
        enabled: response.enabled,
        servers: response.servers,
        synchronized: response.synchronized,
        server: response.server,
        stratum: response.stratum,
        poll_interval_secs: response.poll_interval_secs,
        offset_usecs: response.offset_usecs,
        delay_usecs: response.delay_usecs,
        jitter_usecs: response.jitter_usecs,
        last_sync_secs_ago: response.last_sync_secs_ago,
    })
}

/// The SNMP agent's settings and the subagent's request counters.
pub async fn snmp_state(orch: &IpcEndpoint) -> Result<SnmpState> {
    let response = orch_client(orch)
        .await?
        .get_snmp_state(pb::GetSnmpStateRequest {})
        .await?
        .into_inner();
    Ok(SnmpState {
        enabled: response.enabled,
        agentx_connected: response.connected,
        listen_interface: response.listen_interface,
        listen_address: response.listen_address,
        location: response.location,
        contact: response.contact,
        communities: response
            .communities
            .into_iter()
            .map(|community| SnmpCommunity {
                name: community.name,
                source: (!community.source.is_empty()).then_some(community.source),
            })
            .collect(),
        users: response.users,
        packets_in: response.packets_in,
        packets_out: response.packets_out,
        get_requests: response.get_requests,
        getnext_requests: response.getnext_requests,
        errors: response.errors,
    })
}

/// The sFlow sampler and exporter. The programmed sampler comes from
/// syncd (it owns the ASIC), the export settings and counters from
/// orch — one `show`, both halves.
pub async fn sflow_state(orch: &IpcEndpoint, syncd: &IpcEndpoint) -> Result<SflowState> {
    let export = orch_client(orch)
        .await?
        .get_sflow_export_state(pb::GetSflowExportStateRequest {})
        .await?
        .into_inner();
    // A syncd that cannot answer leaves `supported` true: the commit
    // gate already refused an unsupported platform, so a transient
    // failure here should not read as "your ASIC cannot do this".
    let supported = match syncd.connect().await {
        Ok(channel) => pb::syncd_client::SyncdClient::new(channel)
            .get_sflow_state(pb::GetSflowStateRequest {})
            .await
            .map(|response| response.into_inner().supported)
            .unwrap_or(true),
        Err(_) => true,
    };
    Ok(SflowState {
        enabled: export.enabled,
        supported,
        agent_address: export.agent_address,
        agent_interface: export.agent_interface,
        sample_rate: export.sample_rate,
        polling_interval: export.polling_interval,
        collectors: export
            .collectors
            .iter()
            .map(|collector| format!("{}:{}", collector.address, collector.port))
            .collect(),
        enabled_ports: export.enabled_ports,
        disabled_ports: export.disabled_ports,
        samples_taken: export.samples_taken,
        counter_samples: export.counter_samples,
        datagrams_sent: export.datagrams_sent,
        datagrams_failed: export.datagrams_failed,
    })
}

/// The DHCP relay's per-VLAN servers and counters. The relay is a
/// capability of the snooping engine, so its state rides that engine's
/// snapshot rather than an RPC of its own.
pub async fn dhcp_relay_state(orch: &IpcEndpoint) -> Result<DhcpRelayState> {
    let response = orch_client(orch)
        .await?
        .get_snoop_sec_state(pb::GetSnoopSecStateRequest {})
        .await?
        .into_inner();
    let mut vlans: Vec<DhcpRelayVlan> = response
        .dhcp_relay
        .into_iter()
        .map(|relay| DhcpRelayVlan {
            vlan: u16::try_from(relay.vlan).unwrap_or(0),
            servers: relay.servers,
            giaddr: relay.giaddr,
            to_server: relay.to_server,
            to_client: relay.to_client,
            dropped: relay.dropped,
        })
        .collect();
    vlans.sort_by_key(|relay| relay.vlan);
    Ok(DhcpRelayState { vlans })
}

/// The DHCP server's pools and leases.
pub async fn dhcp_server_state(orch: &IpcEndpoint) -> Result<DhcpServerState> {
    let response = orch_client(orch)
        .await?
        .get_dhcp_server_state(pb::GetDhcpServerStateRequest {})
        .await?
        .into_inner();
    Ok(DhcpServerState {
        pools: response
            .pools
            .into_iter()
            .map(|pool| {
                let config = pool.config.unwrap_or_default();
                DhcpPool {
                    name: config.name,
                    network: config.network,
                    range: format!("{} - {}", config.range_start, config.range_end),
                    gateway: config.gateway,
                    lease_time: config.lease_time,
                    dns_servers: config.dns_servers,
                    domain_name: config.domain_name,
                    in_use: pool.in_use,
                    capacity: pool.capacity,
                }
            })
            .collect(),
        leases: response
            .leases
            .into_iter()
            .map(|lease| DhcpLease {
                address: lease.address,
                mac: lease.mac,
                hostname: lease.hostname,
                expires_at: lease.expires_at,
                kind: if lease.reservation {
                    "reservation".into()
                } else {
                    "dynamic".into()
                },
                pool: lease.pool,
            })
            .collect(),
    })
}

/// `clear dhcp server lease <ip>`.
pub async fn clear_dhcp_lease(orch: &IpcEndpoint, address: String) -> Result<bool> {
    Ok(orch_client(orch)
        .await?
        .clear_dhcp_lease(pb::ClearDhcpLeaseRequest { address })
        .await?
        .into_inner()
        .cleared)
}
