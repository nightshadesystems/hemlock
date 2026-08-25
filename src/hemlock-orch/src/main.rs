//! hemlock-orch — the control-plane protocol engines.
//!
//! Hosts the LACP engine (STP and IGMP/MLD snooping follow the same
//! shape): mgmtd pushes desired state after every commit and at boot
//! replay; the engines run their state machines and drive syncd —
//! never SAI directly. Protocol frames ride the member ports' hostif
//! netdevs: syncd owns the SAI traps that punt them, and orch opens
//! raw sockets on those netdevs directly rather than proxying packets
//! over gRPC.

mod dot1x;
mod frrshow;
mod lacp;
mod lldp;
mod rib;
mod snoop;
mod snoopsec;
mod stp;
#[cfg(target_os = "linux")]
mod transport;

use anyhow::Result;
use clap::Parser;
use hemlock_common::ipc::{Daemon, IpcEndpoint};
use hemlock_common::proto::v1 as pb;
use hemlock_common::proto::v1::orch_server::{Orch, OrchServer};
use tokio::time::Duration;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

#[derive(Parser)]
#[command(name = "hemlock-orch", version = hemlock_common::VERSION, about)]
struct Args {
    /// gRPC endpoint to serve (unix:/path or tcp:host:port).
    #[arg(long)]
    listen: Option<String>,

    /// syncd gRPC endpoint (unix:/path or tcp:host:port).
    #[arg(long)]
    syncd: Option<String>,
}

struct OrchService {
    engine: lacp::Engine,
    stp: stp::Engine,
    lldp: lldp::Engine,
    igmp: snoop::Engine,
    mld: snoop::Engine,
    rib: rib::Engine,
    dot1x: dot1x::Engine,
    snoopsec: snoopsec::Engine,
}

impl OrchService {
    // A gRPC helper naturally speaks `Status`; its size is tonic's business.
    #[allow(clippy::result_large_err)]
    fn snoop_engine(&self, family: &str) -> Result<&snoop::Engine, Status> {
        match family {
            "igmp" | "" => Ok(&self.igmp),
            "mld" => Ok(&self.mld),
            other => Err(Status::invalid_argument(format!(
                "bad snooping family {other:?}"
            ))),
        }
    }
}

/// `"32768,2c:dd:e9:4a:1b:00"`.
fn system_id(priority: u16, mac: [u8; 6]) -> String {
    format!("{priority},{}", mac.map(|b| format!("{b:02x}")).join(":"))
}

#[tonic::async_trait]
impl Orch for OrchService {
    async fn get_status(
        &self,
        _request: Request<pb::GetOrchStatusRequest>,
    ) -> Result<Response<pb::OrchStatus>, Status> {
        let lags = self.engine.snapshot().lags.len();
        let stp_mode = self
            .stp
            .snapshot()
            .map(|s| s.mode.word())
            .unwrap_or("unknown");
        Ok(Response::new(pb::OrchStatus {
            running: true,
            detail: format!("lacp engine: {lags} lag(s); stp mode {stp_mode}"),
        }))
    }

    async fn set_lag_configs(
        &self,
        request: Request<pb::SetLagConfigsRequest>,
    ) -> Result<Response<pb::SetLagConfigsResponse>, Status> {
        let req = request.into_inner();
        let system_priority = match req.system_priority {
            0 => 32768,
            other => {
                u16::try_from(other).map_err(|_| Status::invalid_argument("bad system-priority"))?
            }
        };
        let mut configs = Vec::with_capacity(req.lags.len());
        for lag in req.lags {
            let group = u16::try_from(lag.group)
                .ok()
                .filter(|g| (1..=64).contains(g))
                .ok_or_else(|| Status::invalid_argument(format!("bad group {}", lag.group)))?;
            let mut members = std::collections::BTreeMap::new();
            for member in lag.members {
                let mode = match pb::LacpConfigMode::try_from(member.mode) {
                    Ok(pb::LacpConfigMode::Active) => lacp::Mode::Active,
                    Ok(pb::LacpConfigMode::Passive) => lacp::Mode::Passive,
                    Ok(pb::LacpConfigMode::On) => lacp::Mode::On,
                    _ => return Err(Status::invalid_argument("unspecified lacp mode")),
                };
                members.insert(
                    member.port,
                    lacp::MemberConfig {
                        mode,
                        rate_fast: member.rate_fast,
                        port_priority: match member.port_priority {
                            0 => 32768,
                            other => u16::try_from(other)
                                .map_err(|_| Status::invalid_argument("bad port-priority"))?,
                        },
                    },
                );
            }
            configs.push(lacp::LagConfig {
                group,
                min_links: u8::try_from(lag.min_links)
                    .map_err(|_| Status::invalid_argument("bad min-links"))?,
                fallback: match pb::LagFallbackMode::try_from(lag.fallback) {
                    Ok(pb::LagFallbackMode::Static) => Some(lacp::Fallback::Static),
                    Ok(pb::LagFallbackMode::Individual) => Some(lacp::Fallback::Individual),
                    _ => None,
                },
                fallback_timeout: Duration::from_secs(u64::from(
                    if lag.fallback_timeout_secs == 0 {
                        90
                    } else {
                        lag.fallback_timeout_secs
                    },
                )),
                members,
            });
        }
        self.engine.set_configs(system_priority, configs);
        Ok(Response::new(pb::SetLagConfigsResponse {}))
    }

    async fn set_lldp_config(
        &self,
        request: Request<pb::SetLldpConfigRequest>,
    ) -> Result<Response<pb::SetLldpConfigResponse>, Status> {
        let req = request.into_inner();
        let tx_interval = match req.tx_interval {
            0 => lldp::DEFAULT_TX_INTERVAL,
            other => other,
        };
        if !(5..=300).contains(&tx_interval) {
            return Err(Status::invalid_argument(format!(
                "bad tx-interval {tx_interval}"
            )));
        }
        let hold_multiplier = match req.hold_multiplier {
            0 => lldp::DEFAULT_HOLD_MULTIPLIER,
            other => u8::try_from(other)
                .ok()
                .filter(|n| (2..=10).contains(n))
                .ok_or_else(|| Status::invalid_argument(format!("bad hold-multiplier {other}")))?,
        };
        self.lldp.set_config(lldp::Config {
            disabled: req.disabled,
            tx_interval: Duration::from_secs(u64::from(tx_interval)),
            hold_multiplier,
            disabled_ports: req.disabled_ports.into_iter().collect(),
        });
        Ok(Response::new(pb::SetLldpConfigResponse {}))
    }

    async fn get_lldp_state(
        &self,
        request: Request<pb::GetLldpStateRequest>,
    ) -> Result<Response<pb::GetLldpStateResponse>, Status> {
        let filter = request.into_inner().port;
        let snapshot = self
            .lldp
            .snapshot()
            .ok_or_else(|| Status::internal("lldp engine unavailable"))?;
        Ok(Response::new(pb::GetLldpStateResponse {
            enabled: snapshot.enabled,
            tx_interval: snapshot.tx_interval_secs,
            hold_multiplier: u32::from(snapshot.hold_multiplier),
            chassis_id: snapshot.chassis_id,
            system_name: snapshot.system_name,
            system_description: snapshot.system_description,
            management_address: snapshot.management_address,
            ports: snapshot
                .ports
                .into_iter()
                .filter(|port| filter.is_empty() || port.port == filter)
                .map(|port| pb::LldpPortState {
                    port: port.port,
                    enabled: port.enabled,
                    frames_tx: port.frames_tx,
                    frames_rx: port.frames_rx,
                    frames_discarded: port.frames_discarded,
                    ageouts: port.ageouts,
                    neighbors: port
                        .neighbors
                        .into_iter()
                        .map(|entry| pb::LldpNeighborState {
                            chassis_id: entry.neighbor.chassis_id,
                            chassis_id_subtype: entry.neighbor.chassis_id_subtype,
                            port_id: entry.neighbor.port_id,
                            port_id_subtype: entry.neighbor.port_id_subtype,
                            port_description: entry.neighbor.port_description,
                            system_name: entry.neighbor.system_name,
                            system_description: entry.neighbor.system_description,
                            management_address: entry.neighbor.management_address,
                            ttl: u32::from(entry.neighbor.ttl),
                            age_secs: entry.age_secs,
                        })
                        .collect(),
                })
                .collect(),
        }))
    }

    async fn clear_lldp_counters(
        &self,
        request: Request<pb::ClearLldpCountersRequest>,
    ) -> Result<Response<pb::ClearLldpCountersResponse>, Status> {
        let cleared = self.lldp.clear_counters(&request.into_inner().port);
        Ok(Response::new(pb::ClearLldpCountersResponse { cleared }))
    }

    async fn set_stp_config(
        &self,
        request: Request<pb::SetStpConfigRequest>,
    ) -> Result<Response<pb::SetStpConfigResponse>, Status> {
        let req = request.into_inner();
        let mode = match req.mode.as_str() {
            "mstp" | "" => stp::Mode::Mstp,
            "rstp" => stp::Mode::Rstp,
            "none" => stp::Mode::None,
            other => {
                return Err(Status::invalid_argument(format!("bad stp mode {other:?}")));
            }
        };
        let mut ports = std::collections::BTreeMap::new();
        for port in req.ports {
            ports.insert(
                port.port,
                stp::PortConfig {
                    portfast: port.portfast,
                    bpduguard: port.bpduguard,
                    cost: (port.cost > 0).then_some(port.cost),
                    priority: match port.priority {
                        0 => 128,
                        other => u8::try_from(other)
                            .map_err(|_| Status::invalid_argument("bad port priority"))?,
                    },
                },
            );
        }
        let mut instances = std::collections::BTreeMap::new();
        for map in req.instances {
            let instance = u8::try_from(map.instance)
                .map_err(|_| Status::invalid_argument("bad mst instance"))?;
            let vlans = map
                .vlans
                .iter()
                .filter_map(|v| u16::try_from(*v).ok())
                .collect();
            instances.insert(instance, vlans);
        }
        self.stp.set_config(stp::Config {
            mode,
            priority: u16::try_from(if req.priority == 0 {
                32768
            } else {
                req.priority
            })
            .map_err(|_| Status::invalid_argument("bad priority"))?,
            hello_time: u8::try_from(if req.hello_time == 0 {
                2
            } else {
                req.hello_time
            })
            .map_err(|_| Status::invalid_argument("bad hello-time"))?,
            max_age: u8::try_from(if req.max_age == 0 { 20 } else { req.max_age })
                .map_err(|_| Status::invalid_argument("bad max-age"))?,
            forward_time: u8::try_from(if req.forward_time == 0 {
                15
            } else {
                req.forward_time
            })
            .map_err(|_| Status::invalid_argument("bad forward-time"))?,
            mst_name: req.mst_name,
            mst_revision: u16::try_from(req.mst_revision)
                .map_err(|_| Status::invalid_argument("bad mst revision"))?,
            instances,
            ports,
        });
        Ok(Response::new(pb::SetStpConfigResponse {}))
    }

    async fn get_stp_state(
        &self,
        _request: Request<pb::GetStpStateRequest>,
    ) -> Result<Response<pb::GetStpStateResponse>, Status> {
        let snapshot = self
            .stp
            .snapshot()
            .ok_or_else(|| Status::internal("stp engine unavailable"))?;
        let mac = |mac: [u8; 6]| mac.map(|b| format!("{b:02x}")).join(":");
        Ok(Response::new(pb::GetStpStateResponse {
            mode: snapshot.mode.word().into(),
            bridge_priority: u32::from(snapshot.bridge.priority),
            bridge_mac: mac(snapshot.bridge.mac),
            root_priority: u32::from(snapshot.root.priority),
            root_mac: mac(snapshot.root.mac),
            is_root: snapshot.root == snapshot.bridge,
            root_cost: snapshot.root_cost,
            root_port: snapshot.root_port.unwrap_or_default(),
            hello_time: u32::from(snapshot.hello_time),
            max_age: u32::from(snapshot.max_age),
            forward_time: u32::from(snapshot.forward_time),
            mst_name: snapshot.mst_name,
            mst_revision: u32::from(snapshot.mst_revision),
            instances: snapshot
                .instances
                .iter()
                .map(|(instance, vlans)| pb::MstInstanceMap {
                    instance: u32::from(*instance),
                    vlans: vlans.iter().map(|v| u32::from(*v)).collect(),
                })
                .collect(),
            topology_changes: snapshot.topology_changes,
            seconds_since_tc: snapshot.seconds_since_tc,
            last_tc_port: snapshot.last_tc_port.unwrap_or_default(),
            ports: snapshot
                .ports
                .iter()
                .map(|p| pb::StpPortStateInfo {
                    port: p.port.clone(),
                    role: p.role.word().into(),
                    state: p.state.word().into(),
                    cost: p.cost,
                    priority: u32::from(p.priority),
                    portfast: p.portfast,
                    bpduguard: p.bpduguard,
                    bpdus_rx: p.bpdus_rx,
                    bpdus_tx: p.bpdus_tx,
                    errdisabled: p.errdisabled,
                })
                .collect(),
        }))
    }

    async fn set_snooping_config(
        &self,
        request: Request<pb::SetSnoopingConfigRequest>,
    ) -> Result<Response<pb::SetSnoopingConfigResponse>, Status> {
        let req = request.into_inner();
        let engine = self.snoop_engine(&req.family)?;
        let mut vlans = std::collections::BTreeMap::new();
        for vlan in req.vlans {
            let id = u16::try_from(vlan.vlan)
                .ok()
                .filter(|v| (1..=4094).contains(v))
                .ok_or_else(|| Status::invalid_argument(format!("bad vlan {}", vlan.vlan)))?;
            let querier_address = if vlan.querier_address.is_empty() {
                None
            } else {
                Some(vlan.querier_address.parse().map_err(|_| {
                    Status::invalid_argument(format!(
                        "bad querier address {:?}",
                        vlan.querier_address
                    ))
                })?)
            };
            vlans.insert(
                id,
                snoop::VlanConfig {
                    disabled: vlan.disabled,
                    fast_leave: vlan.fast_leave,
                    querier: vlan.querier,
                    querier_address,
                    static_mrouters: vlan.mrouters,
                },
            );
        }
        engine.set_config(snoop::Config {
            disabled: req.disabled,
            robustness: u8::try_from(if req.robustness == 0 {
                2
            } else {
                req.robustness
            })
            .map_err(|_| Status::invalid_argument("bad robustness"))?,
            vlans,
            ..snoop::Config::default()
        });
        Ok(Response::new(pb::SetSnoopingConfigResponse {}))
    }

    async fn get_snooping_state(
        &self,
        request: Request<pb::GetSnoopingStateRequest>,
    ) -> Result<Response<pb::GetSnoopingStateResponse>, Status> {
        let req = request.into_inner();
        let engine = self.snoop_engine(&req.family)?;
        let snapshot = engine
            .snapshot()
            .ok_or_else(|| Status::internal("snooping engine unavailable"))?;
        Ok(Response::new(pb::GetSnoopingStateResponse {
            enabled: snapshot.enabled,
            robustness: u32::from(snapshot.robustness),
            vlans: snapshot
                .vlans
                .iter()
                .map(|vlan| pb::SnoopingVlanState {
                    vlan: u32::from(vlan.vlan),
                    enabled: vlan.enabled,
                    fast_leave: vlan.fast_leave,
                    querier_enabled: vlan.querier_enabled,
                    querier_address: vlan
                        .querier_address
                        .map(|a| a.to_string())
                        .unwrap_or_default(),
                    querier_active: vlan.querier_active,
                    static_mrouters: vlan.static_mrouters.clone(),
                    dynamic_mrouters: vlan.dynamic_mrouters.clone(),
                    groups: vlan
                        .groups
                        .iter()
                        .map(|group| pb::SnoopingGroupState {
                            group: group.group.to_string(),
                            version: u32::from(group.version),
                            ports: group.ports.clone(),
                        })
                        .collect(),
                })
                .collect(),
        }))
    }

    async fn get_rib(
        &self,
        request: Request<pb::GetRibRequest>,
    ) -> Result<Response<pb::GetRibResponse>, Status> {
        let req = request.into_inner();
        let mut views = self.rib.snapshot(req.ipv6);
        if !req.page_token.is_empty() {
            if let Some(position) = views.iter().position(|v| v.prefix == req.page_token) {
                views.drain(..=position);
            }
        }
        let mut next_page_token = String::new();
        if req.page_size > 0 && views.len() > req.page_size as usize {
            views.truncate(req.page_size as usize);
            next_page_token = views.last().map(|v| v.prefix.clone()).unwrap_or_default();
        }
        Ok(Response::new(pb::GetRibResponse {
            routes: views
                .into_iter()
                .map(|view| pb::RibRoute {
                    prefix: view.prefix,
                    protocol: view.protocol,
                    distance: view.distance,
                    metric: view.metric,
                    uptime_secs: view.uptime_secs,
                    next_hops: view
                        .hops
                        .into_iter()
                        .map(|hop| pb::RibNextHop {
                            via: hop.via,
                            interface: hop.interface,
                            resolved: hop.resolved,
                        })
                        .collect(),
                    fib: view.fib.to_string(),
                    interface: view.interface.unwrap_or_default(),
                })
                .collect(),
            next_page_token,
        }))
    }

    async fn get_neighbors(
        &self,
        request: Request<pb::GetNeighborsRequest>,
    ) -> Result<Response<pb::GetNeighborsResponse>, Status> {
        let req = request.into_inner();
        Ok(Response::new(pb::GetNeighborsResponse {
            neighbors: self
                .rib
                .neighbors(req.ipv6)
                .into_iter()
                .map(|view| pb::NeighborEntry {
                    ip: view.ip,
                    mac: view.mac,
                    interface: view.interface,
                    permanent: view.permanent,
                    age_secs: view.age_secs,
                })
                .collect(),
        }))
    }

    async fn clear_neighbors(
        &self,
        request: Request<pb::ClearNeighborsRequest>,
    ) -> Result<Response<pb::ClearNeighborsResponse>, Status> {
        let req = request.into_inner();
        let ip = (!req.ip.is_empty()).then_some(req.ip.as_str());
        let flushed = rib::flush_neighbors(ip).await;
        Ok(Response::new(pb::ClearNeighborsResponse { flushed }))
    }

    async fn get_ospf_state(
        &self,
        _request: Request<pb::GetOspfStateRequest>,
    ) -> Result<Response<pb::GetOspfStateResponse>, Status> {
        frrshow::ospf_state()
            .await
            .map(Response::new)
            .map_err(Status::failed_precondition)
    }

    async fn get_bgp_state(
        &self,
        request: Request<pb::GetBgpStateRequest>,
    ) -> Result<Response<pb::GetBgpStateResponse>, Status> {
        let req = request.into_inner();
        frrshow::bgp_state(&req.neighbor)
            .await
            .map(Response::new)
            .map_err(Status::failed_precondition)
    }

    async fn get_vrrp_state(
        &self,
        _request: Request<pb::GetVrrpStateRequest>,
    ) -> Result<Response<pb::GetVrrpStateResponse>, Status> {
        frrshow::vrrp_state()
            .await
            .map(Response::new)
            .map_err(Status::failed_precondition)
    }

    async fn clear_bgp(
        &self,
        request: Request<pb::ClearBgpRequest>,
    ) -> Result<Response<pb::ClearBgpResponse>, Status> {
        let req = request.into_inner();
        Ok(Response::new(pb::ClearBgpResponse {
            cleared: frrshow::clear_bgp(&req.neighbor).await,
        }))
    }

    async fn set_dot1x_config(
        &self,
        request: Request<pb::SetDot1xConfigRequest>,
    ) -> Result<Response<pb::SetDot1xConfigResponse>, Status> {
        let req = request.into_inner();
        let mut radius = Vec::with_capacity(req.radius_servers.len());
        for server in req.radius_servers {
            radius.push(dot1x::Radius {
                ip: server.ip,
                key: server.key,
                port: u16::try_from(server.port)
                    .map_err(|_| Status::invalid_argument("bad radius port"))?,
                timeout: u16::try_from(server.timeout_secs)
                    .map_err(|_| Status::invalid_argument("bad radius timeout"))?,
                retransmit: u16::try_from(server.retransmit)
                    .map_err(|_| Status::invalid_argument("bad radius retransmit"))?,
            });
        }
        self.dot1x.set_config(dot1x::Config {
            radius,
            reauth_interval: req.reauth_interval,
            ports: req.ports.into_iter().collect(),
        });
        Ok(Response::new(pb::SetDot1xConfigResponse {}))
    }

    async fn get_dot1x_state(
        &self,
        request: Request<pb::GetDot1xStateRequest>,
    ) -> Result<Response<pb::GetDot1xStateResponse>, Status> {
        let filter = request.into_inner().port;
        let snapshot = self.dot1x.snapshot();
        if !filter.is_empty() && !snapshot.ports.iter().any(|p| p.port == filter) {
            return Err(Status::not_found(format!(
                "dot1x is not enabled on {filter}"
            )));
        }
        Ok(Response::new(pb::GetDot1xStateResponse {
            radius_servers: snapshot
                .radius
                .iter()
                .map(|r| format!("{}:{}", r.ip, r.port))
                .collect(),
            reauth_interval: snapshot.reauth_interval,
            ports: snapshot
                .ports
                .into_iter()
                .filter(|p| filter.is_empty() || p.port == filter)
                .map(|p| pb::Dot1xPortState {
                    port: p.port,
                    status: if p.authorized {
                        "authorized".into()
                    } else {
                        "unauthorized".into()
                    },
                    supplicant_mac: p.supplicant.unwrap_or_default(),
                    last_auth_secs_ago: p.last_auth.map(|t| t.elapsed().as_secs()),
                    failures: p.failures,
                })
                .collect(),
        }))
    }

    async fn dot1x_reauth(
        &self,
        request: Request<pb::Dot1xReauthRequest>,
    ) -> Result<Response<pb::Dot1xReauthResponse>, Status> {
        let port = request.into_inner().port;
        Ok(Response::new(pb::Dot1xReauthResponse {
            triggered: self.dot1x.reauth(&port),
        }))
    }

    // The closure's Err is a tonic Status; its size is tonic's business.
    #[allow(clippy::result_large_err)]
    async fn set_snoop_sec_config(
        &self,
        request: Request<pb::SetSnoopSecConfigRequest>,
    ) -> Result<Response<pb::SetSnoopSecConfigResponse>, Status> {
        let req = request.into_inner();
        let vlan16 = |raw: u32| {
            u16::try_from(raw)
                .ok()
                .filter(|v| (1..=4094).contains(v))
                .ok_or_else(|| Status::invalid_argument(format!("bad VLAN {raw}")))
        };
        let mut config = snoopsec::Config {
            dhcp_trusted: req.dhcp_trusted.into_iter().collect(),
            arp_trusted: req.arp_trusted.into_iter().collect(),
            ..snoopsec::Config::default()
        };
        for vlan in req.dhcp_vlans {
            config.dhcp_vlans.insert(vlan16(vlan)?);
        }
        for vlan in req.arp_vlans {
            config.arp_vlans.insert(vlan16(vlan)?);
        }
        for check in req.validate {
            config.validate.insert(match check.as_str() {
                "src-mac" => snoopsec::Validate::SrcMac,
                "dst-mac" => snoopsec::Validate::DstMac,
                "ip" => snoopsec::Validate::Ip,
                other => {
                    return Err(Status::invalid_argument(format!("bad validate {other:?}")));
                }
            });
        }
        for binding in req.static_bindings {
            let ip: std::net::Ipv4Addr = binding.address.parse().map_err(|_| {
                Status::invalid_argument(format!("bad address {:?}", binding.address))
            })?;
            config.statics.insert(
                (binding.mac, vlan16(binding.vlan)?),
                snoopsec::StaticBinding {
                    ip,
                    interface: binding.interface,
                },
            );
        }
        self.snoopsec.set_config(config);
        Ok(Response::new(pb::SetSnoopSecConfigResponse {}))
    }

    async fn get_snoop_sec_state(
        &self,
        _request: Request<pb::GetSnoopSecStateRequest>,
    ) -> Result<Response<pb::GetSnoopSecStateResponse>, Status> {
        let snapshot = self.snoopsec.snapshot();
        Ok(Response::new(pb::GetSnoopSecStateResponse {
            dhcp_vlans: snapshot.dhcp_vlans.iter().map(|v| u32::from(*v)).collect(),
            dhcp_trusted: snapshot.dhcp_trusted,
            bindings: snapshot
                .bindings
                .iter()
                .map(|b| pb::SnoopBindingState {
                    mac: b.mac.clone(),
                    address: b.ip.to_string(),
                    lease_secs: b.lease_secs,
                    is_static: b.lease_secs.is_none(),
                    vlan: u32::from(b.vlan),
                    interface: b.interface.clone(),
                })
                .collect(),
            dhcp_stats: snapshot
                .dhcp_stats
                .iter()
                .map(|(vlan, stats)| pb::SnoopVlanStats {
                    vlan: u32::from(*vlan),
                    packets: stats.packets,
                    dropped: stats.dropped,
                })
                .collect(),
            untrusted_server_drops: snapshot.untrusted_server_drops,
            arp_vlans: snapshot.arp_vlans.iter().map(|v| u32::from(*v)).collect(),
            validate: snapshot
                .validate
                .iter()
                .map(|v| {
                    match v {
                        snoopsec::Validate::SrcMac => "src-mac",
                        snoopsec::Validate::DstMac => "dst-mac",
                        snoopsec::Validate::Ip => "ip",
                    }
                    .to_string()
                })
                .collect(),
            arp_trusted: snapshot.arp_trusted,
            arp_stats: snapshot
                .arp_stats
                .iter()
                .map(|(vlan, stats)| pb::DaiVlanStats {
                    vlan: u32::from(*vlan),
                    forwarded: stats.forwarded,
                    dropped: stats.dropped,
                    bad_binding: stats.bad_binding,
                    bad_src_mac: stats.bad_src_mac,
                })
                .collect(),
        }))
    }

    async fn clear_snoop_binding(
        &self,
        request: Request<pb::ClearSnoopBindingRequest>,
    ) -> Result<Response<pb::ClearSnoopBindingResponse>, Status> {
        let mac = request.into_inner().mac;
        let mac = if mac.is_empty() {
            None
        } else {
            Some(hemlock_common::net::parse_mac(&mac).map_err(Status::invalid_argument)?)
        };
        Ok(Response::new(pb::ClearSnoopBindingResponse {
            cleared: self.snoopsec.clear_bindings(mac.as_deref()),
        }))
    }

    async fn get_lacp_state(
        &self,
        _request: Request<pb::GetLacpStateRequest>,
    ) -> Result<Response<pb::GetLacpStateResponse>, Status> {
        let snapshot = self.engine.snapshot();
        Ok(Response::new(pb::GetLacpStateResponse {
            system_id: system_id(snapshot.system_priority, snapshot.system_mac),
            lags: snapshot
                .lags
                .iter()
                .map(|lag| pb::LagLacpState {
                    group: u32::from(lag.group),
                    lacp: lag.lacp,
                    active_mode: lag.active_mode,
                    bundled: lag.bundled,
                    total: lag.total,
                    up: lag.up,
                    min_links: u32::from(lag.min_links),
                    fallback_mode: match lag.fallback {
                        Some(lacp::Fallback::Static) => "static",
                        Some(lacp::Fallback::Individual) => "individual",
                        None => "",
                    }
                    .into(),
                    fallback_timeout_secs: lag.fallback_timeout.as_secs() as u32,
                    fallback_active: lag.fallback_active,
                    members: lag
                        .members
                        .iter()
                        .map(|m| pb::MemberLacpState {
                            port: m.port.clone(),
                            status: m.status.into(),
                            rate_fast: m.rate_fast,
                            actor_state: u32::from(m.actor_state),
                            partner_state: u32::from(m.partner_state),
                            partner_system: m
                                .partner_system
                                .map(|(priority, mac)| system_id(priority, mac))
                                .unwrap_or_default(),
                            partner_port: u32::from(m.partner_port),
                            partner_key: u32::from(m.partner_key),
                            partner_priority: u32::from(m.partner_priority),
                            pdus_rx: m.pdus_rx,
                            pdus_tx: m.pdus_tx,
                            churn: m.churn,
                        })
                        .collect(),
                })
                .collect(),
        }))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    hemlock_common::logging::init("info");
    let args = Args::parse();

    let listen: IpcEndpoint = match &args.listen {
        Some(s) => s.parse()?,
        None => Daemon::Orch.default_endpoint(),
    };
    let syncd: IpcEndpoint = match &args.syncd {
        Some(s) => s.parse()?,
        None => Daemon::Syncd.default_endpoint(),
    };

    // The actor system id wants the switch base MAC; syncd knows it.
    // Best-effort with retries — orch must come up even while syncd is
    // still booting (a locally administered placeholder until then
    // would churn partners, so we wait briefly, then fall back).
    let system_mac = resolve_system_mac(&syncd).await;

    let (engine, io) = lacp::Engine::spawn(system_mac);
    let lacp::EngineIo {
        links,
        pdu_in,
        pdu_out,
        gates,
    } = io;
    let (stp_engine, stp_io) = stp::Engine::spawn(system_mac);
    let stp::EngineIo {
        links: stp_links,
        bpdu_in,
        bpdu_out,
        states,
        errdisable,
    } = stp_io;
    let (lldp_engine, lldp_io) = lldp::Engine::spawn(system_mac);
    let lldp::EngineIo {
        links: lldp_links,
        frame_in: lldp_in,
        frame_out: lldp_out,
    } = lldp_io;
    let (igmp_engine, igmp_io) = snoop::Engine::spawn(snoop::Family::Igmp, system_mac);
    let (mld_engine, mld_io) = snoop::Engine::spawn(snoop::Family::Mld, system_mac);
    let (dot1x_engine, dot1x_io) = dot1x::Engine::spawn();
    let (snoopsec_engine, snoopsec_io) = snoopsec::Engine::spawn(Some(std::path::PathBuf::from(
        "/var/lib/hemlock/dhcp-bindings.json",
    )));

    // Gates -> syncd SetLagMembers; STP states -> SetPortStpState;
    // BPDU-guard trips -> SetPortErrdisable; snooping group/mrouter
    // updates -> the L2MC RPCs.
    tokio::spawn(push_gates(syncd.clone(), gates));
    tokio::spawn(push_stp_states(syncd.clone(), states));
    tokio::spawn(push_errdisable(syncd.clone(), errdisable));
    for (ethertype, io) in [(0x0800u16, igmp_io), (0x86ddu16, mld_io)] {
        let snoop::EngineIo {
            packet_in,
            packet_out,
            groups,
            mrouters,
        } = io;
        tokio::spawn(push_l2mc_groups(syncd.clone(), groups));
        tokio::spawn(push_unknown_mcast(syncd.clone(), mrouters));
        #[cfg(target_os = "linux")]
        transport::register_snoop(ethertype, packet_in, packet_out);
        #[cfg(not(target_os = "linux"))]
        {
            let _ = ethertype;
            drop((packet_in, packet_out));
        }
    }
    // The security engines: dot1x authorization flips ride syncd's
    // SetPortAuthorized (internal ACL entries); the hostapd runtime
    // consumes the engine's directives. Snooping/DAI redirect programs
    // ride SetSnoopRedirects, and validated frames re-inject over the
    // protocol sockets.
    let dot1x::EngineIo {
        events_in: dot1x_events,
        auth_out,
        directives,
    } = dot1x_io;
    tokio::spawn(push_port_auth(syncd.clone(), auth_out));
    tokio::spawn(dot1x::run_hostapd(directives, dot1x_events));
    let snoopsec::EngineIo {
        packet_in: snoopsec_packets,
        packet_out: snoopsec_out,
        redirects,
    } = snoopsec_io;
    tokio::spawn(push_snoop_redirects(syncd.clone(), redirects));
    #[cfg(target_os = "linux")]
    transport::register_snoopsec(snoopsec_packets, snoopsec_out);
    #[cfg(not(target_os = "linux"))]
    drop((snoopsec_packets, snoopsec_out));
    // LLDP rides the same sockets (ethertype 0x88cc).
    #[cfg(target_os = "linux")]
    transport::register_lldp(lldp_in, lldp_out);
    #[cfg(not(target_os = "linux"))]
    drop((lldp_in, lldp_out));

    // The RIB pipeline: kernel netlink (iproute2 dumps) -> engine ->
    // syncd FIB RPCs. The pusher reconciles on every engine change and
    // re-anchors against syncd's own dump periodically (restart resync).
    let (rib_engine, rib_generation) = rib::Engine::spawn();
    tokio::spawn(push_fib(syncd.clone(), rib_engine.clone(), rib_generation));
    #[cfg(target_os = "linux")]
    tokio::spawn(rib::run_feed(rib_engine.clone()));

    // syncd port events -> the engines' link states.
    tokio::spawn(watch_links(syncd.clone(), links, stp_links, lldp_links));
    // The VLAN membership view (query transmission + VLAN
    // classification of snooped frames on tag-stripped netdevs), plus
    // the RIB engine's ASIC L3 interface set.
    tokio::spawn(watch_vlans(
        syncd.clone(),
        igmp_engine.clone(),
        mld_engine.clone(),
        rib_engine.clone(),
        snoopsec_engine.clone(),
        lldp_engine.clone(),
    ));
    // Protocol frames <-> the ports' hostif netdevs (Linux only; on dev
    // hosts the engines still run, partnerless).
    #[cfg(target_os = "linux")]
    tokio::spawn(transport::run(
        engine.clone(),
        stp_engine.clone(),
        lldp_engine.clone(),
        pdu_in,
        pdu_out,
        bpdu_in,
        bpdu_out,
    ));
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pdu_in, pdu_out, bpdu_in, bpdu_out);
        warn!("no netdev packet transport on this platform; LACP/STP/snooping run partnerless");
    }

    info!(%listen, "hemlock-orch serving gRPC (lacp + stp + snooping + lldp engines up)");
    let service = OrchService {
        engine,
        stp: stp_engine,
        lldp: lldp_engine,
        igmp: igmp_engine,
        mld: mld_engine,
        rib: rib_engine,
        dot1x: dot1x_engine,
        snoopsec: snoopsec_engine,
    };
    let router = tonic::transport::Server::builder().add_service(OrchServer::new(service));
    listen
        .serve(router, async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutting down");
        })
        .await?;
    Ok(())
}

/// The switch base MAC from syncd's interface state (first Ethernet
/// port), retried briefly; zeros-with-local-bit as the last resort.
async fn resolve_system_mac(syncd: &IpcEndpoint) -> [u8; 6] {
    for _ in 0..10 {
        if let Ok(channel) = syncd.connect().await {
            let mut client = pb::syncd_client::SyncdClient::new(channel);
            if let Ok(response) = client
                .get_interfaces(pb::GetInterfacesRequest { names: vec![] })
                .await
            {
                let response = response.into_inner();
                if let Some(mac) = response
                    .interfaces
                    .iter()
                    .find(|i| i.kind == "ethernet" && !i.mac.is_empty())
                    .and_then(|i| parse_mac(&i.mac))
                {
                    return mac;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    warn!("cannot resolve the switch base MAC from syncd; using a locally administered id");
    [0x02, 0x00, 0x00, 0x00, 0x00, 0x01]
}

/// The switch's configured hostname, for the LLDP system-name TLV.
fn hostname() -> String {
    ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "hemlock".to_string())
}

fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = text.split(':');
    for byte in &mut mac {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

/// Push every gate update to syncd, retrying on failure (a syncd
/// restart replays the config through mgmtd; the engine re-emits on the
/// next change, and the declarative RPC makes repeats harmless).
async fn push_gates(
    syncd: IpcEndpoint,
    mut gates: tokio::sync::mpsc::UnboundedReceiver<lacp::GateUpdate>,
) {
    while let Some(update) = gates.recv().await {
        let request = pb::SetLagMembersRequest {
            group: u32::from(update.group),
            members: update
                .members
                .iter()
                .map(|(port, enabled)| pb::LagMemberSpec {
                    port: port.clone(),
                    enabled: *enabled,
                })
                .collect(),
        };
        let mut attempts = 0;
        loop {
            attempts += 1;
            match syncd.connect().await {
                Ok(channel) => {
                    let mut client = pb::syncd_client::SyncdClient::new(channel);
                    match client.set_lag_members(request.clone()).await {
                        Ok(_) => break,
                        Err(status) => {
                            warn!(group = update.group, %status, "SetLagMembers failed");
                            // A LAG that no longer exists is not worth
                            // retrying; mgmtd owns its lifecycle.
                            if status.code() == tonic::Code::FailedPrecondition {
                                break;
                            }
                        }
                    }
                }
                Err(e) => warn!(error = %e, "cannot reach syncd for gate push"),
            }
            if attempts >= 5 {
                warn!(group = update.group, "giving up on gate push (5 attempts)");
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

/// Push dot1x authorization flips to syncd (unauthorized installs the
/// internal permit-EAPOL + deny-all entries; authorized removes them).
async fn push_port_auth(
    syncd: IpcEndpoint,
    mut auth: tokio::sync::mpsc::UnboundedReceiver<(String, bool)>,
) {
    while let Some((port, authorized)) = auth.recv().await {
        let request = pb::SetPortAuthorizedRequest {
            port: port.clone(),
            authorized,
        };
        let mut attempts = 0;
        loop {
            attempts += 1;
            match syncd.connect().await {
                Ok(channel) => {
                    let mut client = pb::syncd_client::SyncdClient::new(channel);
                    match client.set_port_authorized(request.clone()).await {
                        Ok(_) => break,
                        Err(status) => {
                            warn!(%port, %status, "SetPortAuthorized failed");
                            if status.code() == tonic::Code::FailedPrecondition {
                                break;
                            }
                        }
                    }
                }
                Err(e) => warn!(error = %e, "cannot reach syncd for port-auth push"),
            }
            if attempts >= 5 {
                warn!(%port, "giving up on port-auth push (5 attempts)");
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

/// Push snooping/DAI redirect programs to syncd (the internal ACL
/// entries trapping DHCP/ARP on the snooped VLANs' ports).
async fn push_snoop_redirects(
    syncd: IpcEndpoint,
    mut redirects: tokio::sync::mpsc::UnboundedReceiver<snoopsec::RedirectProgram>,
) {
    while let Some(program) = redirects.recv().await {
        let request = pb::SetSnoopRedirectsRequest {
            dhcp: program
                .dhcp
                .iter()
                .map(|(vlan, untrusted, trusted)| pb::SnoopVlanProgram {
                    vlan: u32::from(*vlan),
                    untrusted_ports: untrusted.clone(),
                    trusted_ports: trusted.clone(),
                })
                .collect(),
            arp: program
                .arp
                .iter()
                .map(|(vlan, untrusted)| pb::SnoopVlanProgram {
                    vlan: u32::from(*vlan),
                    untrusted_ports: untrusted.clone(),
                    trusted_ports: vec![],
                })
                .collect(),
        };
        let mut attempts = 0;
        loop {
            attempts += 1;
            match syncd.connect().await {
                Ok(channel) => {
                    let mut client = pb::syncd_client::SyncdClient::new(channel);
                    match client.set_snoop_redirects(request.clone()).await {
                        Ok(_) => break,
                        Err(status) => {
                            warn!(%status, "SetSnoopRedirects failed");
                            if status.code() == tonic::Code::FailedPrecondition {
                                break;
                            }
                        }
                    }
                }
                Err(e) => warn!(error = %e, "cannot reach syncd for snoop-redirect push"),
            }
            if attempts >= 5 {
                warn!("giving up on snoop-redirect push (5 attempts)");
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

/// Push every STP state update to syncd (instance 0 = the shared state
/// machine's verdict applies to every instance).
async fn push_stp_states(
    syncd: IpcEndpoint,
    mut states: tokio::sync::mpsc::UnboundedReceiver<stp::StateUpdate>,
) {
    while let Some(update) = states.recv().await {
        let state = match update.state {
            stp::PortState::Discarding => pb::StpState::Blocking,
            stp::PortState::Learning => pb::StpState::Learning,
            stp::PortState::Forwarding => pb::StpState::Forwarding,
        };
        match syncd.connect().await {
            Ok(channel) => {
                let mut client = pb::syncd_client::SyncdClient::new(channel);
                if let Err(status) = client
                    .set_port_stp_state(pb::SetPortStpStateRequest {
                        name: update.port.clone(),
                        instance: 0,
                        state: state as i32,
                    })
                    .await
                {
                    warn!(port = %update.port, %status, "SetPortStpState failed");
                }
            }
            Err(e) => warn!(error = %e, "cannot reach syncd for STP state push"),
        }
    }
}

/// Errdisable BPDU-guard trips through syncd (admin-down + reason).
async fn push_errdisable(
    syncd: IpcEndpoint,
    mut events: tokio::sync::mpsc::UnboundedReceiver<stp::ErrdisableEvent>,
) {
    while let Some(event) = events.recv().await {
        match syncd.connect().await {
            Ok(channel) => {
                let mut client = pb::syncd_client::SyncdClient::new(channel);
                if let Err(status) = client
                    .set_port_errdisable(pb::SetPortErrdisableRequest {
                        name: event.port.clone(),
                        reason: event.reason.into(),
                    })
                    .await
                {
                    warn!(port = %event.port, %status, "SetPortErrdisable failed");
                }
            }
            Err(e) => warn!(error = %e, "cannot reach syncd for errdisable push"),
        }
    }
}

/// Push snooping group changes to syncd: ensure + declarative members,
/// or removal when a group ages out.
async fn push_l2mc_groups(
    syncd: IpcEndpoint,
    mut groups: tokio::sync::mpsc::UnboundedReceiver<snoop::GroupUpdate>,
) {
    while let Some(update) = groups.recv().await {
        let result: Result<()> = async {
            let channel = syncd.connect().await?;
            let mut client = pb::syncd_client::SyncdClient::new(channel);
            if update.remove {
                client
                    .remove_l2mc_group(pb::RemoveL2mcGroupRequest {
                        vlan: u32::from(update.vlan),
                        group: update.group.to_string(),
                    })
                    .await?;
            } else {
                client
                    .ensure_l2mc_group(pb::EnsureL2mcGroupRequest {
                        vlan: u32::from(update.vlan),
                        group: update.group.to_string(),
                    })
                    .await?;
                client
                    .set_l2mc_members(pb::SetL2mcMembersRequest {
                        vlan: u32::from(update.vlan),
                        group: update.group.to_string(),
                        ports: update.ports.clone(),
                    })
                    .await?;
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            warn!(vlan = update.vlan, group = %update.group, error = %e, "L2MC push failed");
        }
    }
}

/// Push a VLAN's unknown-multicast scope to syncd.
async fn push_unknown_mcast(
    syncd: IpcEndpoint,
    mut updates: tokio::sync::mpsc::UnboundedReceiver<snoop::MrouterUpdate>,
) {
    while let Some(update) = updates.recv().await {
        let result: Result<()> = async {
            let channel = syncd.connect().await?;
            let mut client = pb::syncd_client::SyncdClient::new(channel);
            client
                .set_vlan_unknown_mcast(pb::SetVlanUnknownMcastRequest {
                    vlan: u32::from(update.vlan),
                    restrict: update.restrict,
                    ports: update.ports.clone(),
                })
                .await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            warn!(vlan = update.vlan, error = %e, "unknown-mcast push failed");
        }
    }
}

/// Keep the snooping engines' VLAN membership view fresh from syncd
/// (query transmission wants the port lists; the transport classifies
/// tag-stripped frames by the ingress port's untagged VLAN).
async fn watch_vlans(
    syncd: IpcEndpoint,
    igmp: snoop::Engine,
    mld: snoop::Engine,
    rib: rib::Engine,
    snoopsec: snoopsec::Engine,
    lldp_engine: lldp::Engine,
) {
    loop {
        if let Ok(channel) = syncd.connect().await {
            let mut client = pb::syncd_client::SyncdClient::new(channel);
            if let Ok(response) = client
                .get_interfaces(pb::GetInterfacesRequest { names: vec![] })
                .await
            {
                let response = response.into_inner();
                // LLDP: every front-panel port is a candidate (the
                // engine applies the global/per-port disables), and the
                // advertised identity comes from the same view.
                lldp_engine.set_ports(
                    response
                        .interfaces
                        .iter()
                        .filter(|i| i.kind == "ethernet")
                        .map(|i| {
                            (
                                i.name.clone(),
                                lldp::PortInfo {
                                    mac: parse_mac(&i.mac).unwrap_or_default(),
                                    description: i.description.clone(),
                                },
                            )
                        })
                        .collect(),
                );
                lldp_engine.set_system(lldp::System {
                    chassis_mac: response
                        .interfaces
                        .iter()
                        .find(|i| i.kind == "ethernet" && !i.mac.is_empty())
                        .and_then(|i| parse_mac(&i.mac))
                        .unwrap_or_default(),
                    name: hostname(),
                    description: format!("Hemlock NOS version {}", hemlock_common::VERSION),
                    management_address: response
                        .interfaces
                        .iter()
                        .find(|i| i.kind == "management")
                        .and_then(|i| i.ip_addresses.first())
                        .map(|cidr| cidr.split('/').next().unwrap_or(cidr).to_string())
                        .unwrap_or_default(),
                });
                // The RIB engine's ASIC L3 interface set: routed ports
                // and SVIs, i.e. anything with an interface address.
                let l3: std::collections::BTreeSet<String> = response
                    .interfaces
                    .iter()
                    .filter(|i| {
                        matches!(i.kind.as_str(), "ethernet" | "port-channel" | "vlan")
                            && !i.ip_addresses.is_empty()
                    })
                    .map(|i| i.name.clone())
                    .collect();
                rib.set_l3_interfaces(l3);
                let mut vlan_ports: std::collections::BTreeMap<u16, Vec<String>> =
                    std::collections::BTreeMap::new();
                let mut pvids: std::collections::BTreeMap<String, u16> =
                    std::collections::BTreeMap::new();
                for iface in &response.interfaces {
                    if !matches!(iface.kind.as_str(), "ethernet" | "port-channel")
                        || !iface.ip_addresses.is_empty()
                    {
                        continue;
                    }
                    let untagged = if iface.switchport_mode == "trunk" {
                        iface.native_vlan
                    } else {
                        iface.access_vlan
                    };
                    let untagged =
                        u16::try_from(if untagged == 0 { 1 } else { untagged }).unwrap_or(1);
                    pvids.insert(iface.name.clone(), untagged);
                    vlan_ports
                        .entry(untagged)
                        .or_default()
                        .push(iface.name.clone());
                    if iface.switchport_mode == "trunk" {
                        for vlan in &iface.trunk_vlans {
                            if let Ok(vlan) = u16::try_from(*vlan) {
                                vlan_ports.entry(vlan).or_default().push(iface.name.clone());
                            }
                        }
                    }
                }
                igmp.set_vlan_ports(vlan_ports.clone());
                mld.set_vlan_ports(vlan_ports);
                // The snooping/DAI view wants physical ports only:
                // Port-Channels expand to their members (the redirect
                // entries land on member ports in syncd).
                let mut po_members: std::collections::BTreeMap<
                    String,
                    std::collections::BTreeSet<String>,
                > = std::collections::BTreeMap::new();
                for iface in &response.interfaces {
                    if iface.kind == "port-channel" {
                        po_members
                            .insert(iface.name.clone(), iface.members.iter().cloned().collect());
                    }
                }
                let mut physical_vlan_ports: std::collections::BTreeMap<
                    u16,
                    std::collections::BTreeSet<String>,
                > = std::collections::BTreeMap::new();
                for iface in &response.interfaces {
                    if !matches!(iface.kind.as_str(), "ethernet" | "port-channel")
                        || !iface.ip_addresses.is_empty()
                    {
                        continue;
                    }
                    let mut vlans: Vec<u16> = Vec::new();
                    let untagged = if iface.switchport_mode == "trunk" {
                        iface.native_vlan
                    } else {
                        iface.access_vlan
                    };
                    vlans
                        .push(u16::try_from(if untagged == 0 { 1 } else { untagged }).unwrap_or(1));
                    if iface.switchport_mode == "trunk" {
                        vlans.extend(
                            iface
                                .trunk_vlans
                                .iter()
                                .filter_map(|v| u16::try_from(*v).ok()),
                        );
                    }
                    let targets: Vec<String> = if iface.kind == "port-channel" {
                        iface.members.clone()
                    } else {
                        vec![iface.name.clone()]
                    };
                    for vlan in vlans {
                        physical_vlan_ports
                            .entry(vlan)
                            .or_default()
                            .extend(targets.iter().cloned());
                    }
                }
                snoopsec.set_l2_view(snoopsec::L2View {
                    vlan_ports: physical_vlan_ports,
                    po_members,
                });
                #[cfg(target_os = "linux")]
                transport::set_pvids(pvids);
                #[cfg(not(target_os = "linux"))]
                let _ = pvids;
            }
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// A mirror value that never matches a real program, so a re-anchored
/// entry is always re-ensured (the RPCs are idempotent).
fn unknown_route() -> rib::WantedRoute {
    rib::WantedRoute {
        cpu: true,
        drop: true,
        hops: Vec::new(),
    }
}

/// Reconcile syncd's FIB onto the RIB engine's wanted program: push the
/// diff on every engine change; periodically re-anchor the local mirror
/// on syncd's own dump so a restart on either side converges (add
/// missing, delete stale) — the resync half of the pipeline.
async fn push_fib(
    syncd: IpcEndpoint,
    engine: rib::Engine,
    mut generation: tokio::sync::watch::Receiver<u64>,
) {
    let mut mirror = rib::Program::default();
    let mut verify = tokio::time::interval(Duration::from_secs(30));
    verify.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = generation.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = verify.tick() => {
                if !engine.have_kernel() {
                    continue;
                }
                match dump_fib(&syncd).await {
                    Ok(anchored) => mirror = anchored,
                    Err(e) => {
                        warn!(error = %e, "syncd fib dump failed; reconcile deferred");
                        continue;
                    }
                }
            }
        }
        if !engine.have_kernel() {
            continue;
        }
        let wanted = engine.wanted();
        for op in rib::diff_program(&mirror, &wanted) {
            match apply_fib_op(&syncd, &op).await {
                // The mirror tracks per-op success so one persistently
                // failing entry never wedges the rest.
                Ok(()) => match op {
                    rib::FibOp::EnsureNeighbor { interface, ip, mac } => {
                        mirror.neighbors.insert((interface, ip), mac);
                    }
                    rib::FibOp::EnsureRoute { prefix, route } => {
                        mirror.routes.insert(prefix, route);
                    }
                    rib::FibOp::RemoveRoute { prefix } => {
                        mirror.routes.remove(&prefix);
                    }
                    rib::FibOp::RemoveNeighbor { interface, ip } => {
                        mirror.neighbors.remove(&(interface, ip));
                    }
                },
                Err(e) => warn!(error = %e, op = ?op, "fib push failed; will retry"),
            }
        }
    }
}

/// syncd's installed FIB, as a mirror whose entries always re-ensure
/// (targets are not dumped; the sentinel values never match).
async fn dump_fib(syncd: &IpcEndpoint) -> Result<rib::Program> {
    let channel = syncd.connect().await?;
    let dump = pb::syncd_client::SyncdClient::new(channel)
        .dump_fib(pb::DumpFibRequest {})
        .await?
        .into_inner();
    let mut program = rib::Program::default();
    for prefix in dump.routes {
        program.routes.insert(prefix, unknown_route());
    }
    for neighbor in dump.neighbors {
        program
            .neighbors
            .insert((neighbor.interface, neighbor.ip), String::new());
    }
    Ok(program)
}

async fn apply_fib_op(syncd: &IpcEndpoint, op: &rib::FibOp) -> Result<()> {
    let channel = syncd.connect().await?;
    let mut client = pb::syncd_client::SyncdClient::new(channel);
    match op {
        rib::FibOp::EnsureNeighbor { interface, ip, mac } => {
            client
                .ensure_neighbor(pb::EnsureNeighborRequest {
                    interface: interface.clone(),
                    ip: ip.clone(),
                    mac: mac.clone(),
                })
                .await?;
        }
        rib::FibOp::EnsureRoute { prefix, route } => {
            client
                .ensure_route(pb::EnsureRouteRequest {
                    prefix: prefix.clone(),
                    cpu: route.cpu,
                    drop: route.drop,
                    next_hops: route
                        .hops
                        .iter()
                        .map(|(interface, ip)| pb::RouteNextHop {
                            interface: interface.clone(),
                            ip: ip.clone(),
                        })
                        .collect(),
                })
                .await?;
        }
        rib::FibOp::RemoveRoute { prefix } => {
            client
                .remove_route(pb::RemoveRouteRequest {
                    prefix: prefix.clone(),
                })
                .await?;
        }
        rib::FibOp::RemoveNeighbor { interface, ip } => {
            client
                .remove_neighbor(pb::RemoveNeighborRequest {
                    interface: interface.clone(),
                    ip: ip.clone(),
                })
                .await?;
        }
    }
    Ok(())
}

/// Track member link states from syncd: seed from ListPorts, then follow
/// the port event stream; reconnect with backoff on any failure.
async fn watch_links(
    syncd: IpcEndpoint,
    links: tokio::sync::mpsc::UnboundedSender<lacp::LinkEvent>,
    stp_links: tokio::sync::mpsc::UnboundedSender<stp::LinkEvent>,
    lldp_links: tokio::sync::mpsc::UnboundedSender<lldp::LinkEvent>,
) {
    loop {
        match follow_links(&syncd, &links, &stp_links, &lldp_links).await {
            Ok(()) => {}
            Err(e) => warn!(error = %e, "syncd link watch lost; reconnecting"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn follow_links(
    syncd: &IpcEndpoint,
    links: &tokio::sync::mpsc::UnboundedSender<lacp::LinkEvent>,
    stp_links: &tokio::sync::mpsc::UnboundedSender<stp::LinkEvent>,
    lldp_links: &tokio::sync::mpsc::UnboundedSender<lldp::LinkEvent>,
) -> Result<()> {
    let channel = syncd.connect().await?;
    let mut client = pb::syncd_client::SyncdClient::new(channel);
    let ports = client
        .list_ports(pb::ListPortsRequest {})
        .await?
        .into_inner()
        .ports;
    for port in ports {
        let up = port.oper_status == pb::OperStatus::Up as i32;
        let _ = links.send(lacp::LinkEvent {
            port: port.name.clone(),
            up,
        });
        let _ = stp_links.send(stp::LinkEvent {
            port: port.name.clone(),
            up,
            speed_mbps: port.speed_mbps,
        });
        let _ = lldp_links.send(lldp::LinkEvent {
            port: port.name,
            up,
        });
    }
    let mut stream = client
        .watch_port_events(pb::WatchPortEventsRequest {})
        .await?
        .into_inner();
    while let Some(event) = stream.message().await? {
        let up = event.oper_status == pb::OperStatus::Up as i32;
        let _ = links.send(lacp::LinkEvent {
            port: event.name.clone(),
            up,
        });
        let _ = stp_links.send(stp::LinkEvent {
            port: event.name.clone(),
            up,
            speed_mbps: 0,
        });
        let _ = lldp_links.send(lldp::LinkEvent {
            port: event.name,
            up,
        });
    }
    Ok(())
}
