//! gRPC surface of syncd (`hemlock.v1.Syncd`).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use hemlock_common::proto::v1 as pb;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tonic::{Request, Response, Status};

use crate::actor::SaiHandle;
use crate::ifstats::{utilization_pct, RawCounters, SharedEngine, Snapshot};
use crate::netdev::NetdevSample;
use crate::state::{L3State, PortState};

/// L2 MTU reported for front-panel ports. Matches the KNET default the
/// platform loads; per-interface MTU intents arrive in a later phase.
const PORT_MTU: u32 = 9214;

/// Platform facts the interface RPCs need, resolved once at startup.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    /// `<vendor> <model>` for `show interfaces capabilities`.
    pub platform_model: String,
    /// Management port: (display name, OS netdev).
    pub management: Option<(String, String)>,
    /// Egress queues per front-panel port, from the platform definition.
    pub uc_queues: u32,
    pub mc_queues: u32,
    /// Switch base MAC (front-panel ports share it), colon-lowercase.
    pub mac: Option<String>,
}

/// Latest kernel-interface samples, refreshed by the collector task.
pub type SharedNetdevs = Arc<RwLock<HashMap<String, NetdevSample>>>;

pub struct SyncdService {
    handle: Arc<SaiHandle>,
    engine: SharedEngine,
    netdevs: SharedNetdevs,
    inventory: Inventory,
}

impl SyncdService {
    pub fn new(
        handle: Arc<SaiHandle>,
        engine: SharedEngine,
        netdevs: SharedNetdevs,
        inventory: Inventory,
    ) -> Self {
        Self {
            handle,
            engine,
            netdevs,
            inventory,
        }
    }

    fn to_proto(port: &PortState) -> pb::Port {
        pb::Port {
            name: port.def.name.clone(),
            index: port.def.index,
            lanes: port.def.lanes.clone(),
            speed_mbps: port.def.speed_mbps,
            admin_state: if port.admin_up {
                pb::AdminState::Up
            } else {
                pb::AdminState::Down
            } as i32,
            oper_status: if port.oper_up {
                pb::OperStatus::Up
            } else {
                pb::OperStatus::Down
            } as i32,
            description: port.description.clone(),
            sai_oid: port.sai_id.0,
            media: port.def.media.clone().unwrap_or_default(),
        }
    }

    fn counters_to_proto(c: &RawCounters) -> pb::InterfaceCounters {
        pb::InterfaceCounters {
            in_pkts: c.in_pkts,
            in_octets: c.in_octets,
            in_ucast_pkts: c.in_ucast_pkts,
            in_mcast_pkts: c.in_mcast_pkts,
            in_bcast_pkts: c.in_bcast_pkts,
            in_discards: c.in_discards,
            in_errors: c.in_errors,
            in_crc_errors: c.in_crc_errors,
            in_alignment_errors: c.in_alignment_errors,
            in_symbol_errors: c.in_symbol_errors,
            in_runts: c.in_runts,
            in_giants: c.in_giants,
            in_pause: c.in_pause,
            out_pkts: c.out_pkts,
            out_octets: c.out_octets,
            out_ucast_pkts: c.out_ucast_pkts,
            out_mcast_pkts: c.out_mcast_pkts,
            out_bcast_pkts: c.out_bcast_pkts,
            out_discards: c.out_discards,
            out_errors: c.out_errors,
            out_pause: c.out_pause,
            collisions: c.collisions,
            late_collisions: c.late_collisions,
            deferred: c.deferred,
        }
    }

    fn rates_to_proto(snap: &Snapshot, speed_mbps: u64) -> pb::RateStats {
        pb::RateStats {
            interval_secs: snap.load_interval_secs,
            in_bps: snap.in_bps,
            in_pps: snap.in_pps.round() as u64,
            in_util_pct: utilization_pct(snap.in_bps, snap.in_pps, speed_mbps),
            out_bps: snap.out_bps,
            out_pps: snap.out_pps.round() as u64,
            out_util_pct: utilization_pct(snap.out_bps, snap.out_pps, speed_mbps),
        }
    }

    /// Queue rows padded/ordered per the platform declaration: UC0..N-1
    /// then MC0..M-1, zeros where the sweep produced nothing.
    fn queue_rows(&self, snap: &Snapshot) -> Vec<pb::QueueCounters> {
        let labels = (0..self.inventory.uc_queues)
            .map(|i| format!("UC{i}"))
            .chain((0..self.inventory.mc_queues).map(|i| format!("MC{i}")));
        labels
            .map(|label| {
                let sample = snap.queues.iter().find(|q| q.label == label);
                pb::QueueCounters {
                    queue: label,
                    pkts: sample.map(|q| q.pkts).unwrap_or(0),
                    bytes: sample.map(|q| q.bytes).unwrap_or(0),
                    dropped_pkts: sample.map(|q| q.dropped_pkts).unwrap_or(0),
                    dropped_bytes: sample.map(|q| q.dropped_bytes).unwrap_or(0),
                }
            })
            .collect()
    }

    fn stats_fields(
        &self,
        state: &mut pb::InterfaceState,
        snap: &Snapshot,
        speed_mbps: u64,
        with_queues: bool,
        with_bins: bool,
    ) {
        state.link_changes = snap.link_changes;
        state.seconds_since_change = snap.seconds_since_change;
        state.seconds_since_clear = snap.seconds_since_clear;
        state.rates = Some(Self::rates_to_proto(snap, speed_mbps));
        state.counters = Some(Self::counters_to_proto(&snap.counters));
        if with_queues {
            state.queues = self.queue_rows(snap);
        }
        if with_bins {
            state.bins = Some(pb::FrameSizeBins {
                rx: snap.counters.rx_bins.to_vec(),
                tx: snap.counters.tx_bins.to_vec(),
            });
        }
    }

    fn port_to_iface(&self, port: &PortState, now: Instant) -> pb::InterfaceState {
        let speed = if port.oper_up {
            u64::from(port.def.speed_mbps)
        } else {
            // Down with nothing negotiated: `Unconfigured` in the CLI.
            0
        };
        let mut state = pb::InterfaceState {
            name: port.def.name.clone(),
            kind: "ethernet".into(),
            index: port.def.index,
            admin_state: if port.admin_up {
                pb::AdminState::Up
            } else {
                pb::AdminState::Down
            } as i32,
            oper_status: if port.oper_up {
                pb::OperStatus::Up
            } else {
                pb::OperStatus::Down
            } as i32,
            mac: self.inventory.mac.clone().unwrap_or_default(),
            bia_mac: self.inventory.mac.clone().unwrap_or_default(),
            description: port.description.clone(),
            mtu: PORT_MTU,
            speed_mbps: speed,
            duplex: "full".into(),
            autoneg: port.def.autoneg,
            media: port.def.media.clone().unwrap_or_default(),
            phy_model: port.def.phy_model.clone().unwrap_or_default(),
            supported_modes: port.def.supported_modes.clone(),
            ip_addresses: port.l3.iter().map(|l3| l3.address.clone()).collect(),
            ..pb::InterfaceState::default()
        };
        if let Some(snap) = self.engine.snapshot(&port.def.name, now) {
            self.stats_fields(&mut state, &snap, speed, true, true);
        }
        state
    }

    fn netdev_to_iface(&self, dev: &NetdevSample, now: Instant) -> pb::InterfaceState {
        let mut state = pb::InterfaceState {
            name: dev.name.clone(),
            kind: dev.kind.into(),
            admin_state: if dev.admin_up {
                pb::AdminState::Up
            } else {
                pb::AdminState::Down
            } as i32,
            oper_status: if dev.oper_up {
                pb::OperStatus::Up
            } else {
                pb::OperStatus::Down
            } as i32,
            mac: dev.mac.clone(),
            bia_mac: dev.mac.clone(),
            mtu: dev.mtu,
            speed_mbps: dev.speed_mbps,
            duplex: dev.duplex.clone(),
            autoneg: dev.kind == "management",
            ip_addresses: dev.ip_addresses.clone(),
            media: if dev.kind == "management" {
                "10/100/1000".into()
            } else {
                String::new()
            },
            ..pb::InterfaceState::default()
        };
        if let Some(snap) = self.engine.snapshot(&dev.name, now) {
            self.stats_fields(&mut state, &snap, dev.speed_mbps, false, false);
        }
        state
    }

    /// An interface address's route destinations: the IP2ME host route,
    /// and the connected subnet unless it coincides with it (a /32 or
    /// /128 address has no separate subnet route).
    fn routes_for(
        addr: std::net::IpAddr,
        len: u8,
    ) -> (hemlock_sai::IpPrefix, Option<hemlock_sai::IpPrefix>) {
        let host_len: u8 = if addr.is_ipv4() { 32 } else { 128 };
        let host = (addr, host_len);
        let subnet = (hemlock_common::net::network(addr, len), len);
        (host, (subnet != host).then_some(subnet))
    }

    /// Both route destinations of a stored CIDR address (for teardown).
    fn route_dests(address: &str) -> Vec<hemlock_sai::IpPrefix> {
        let Ok((addr, len)) = hemlock_common::net::parse_cidr(address) else {
            return Vec::new();
        };
        let (host, subnet) = Self::routes_for(addr, len);
        std::iter::once(host).chain(subnet).collect()
    }
}

#[tonic::async_trait]
impl pb::syncd_server::Syncd for SyncdService {
    async fn get_switch_info(
        &self,
        _request: Request<pb::GetSwitchInfoRequest>,
    ) -> Result<Response<pb::SwitchInfo>, Status> {
        Ok(Response::new(pb::SwitchInfo {
            platform_id: self.handle.platform_id.clone(),
            backend: self.handle.backend_name.clone(),
            switch_oid: self.handle.switch.oid,
            port_count: self.handle.initial_ports() as u32,
        }))
    }

    async fn list_ports(
        &self,
        _request: Request<pb::ListPortsRequest>,
    ) -> Result<Response<pb::ListPortsResponse>, Status> {
        let table = self
            .handle
            .ports
            .read()
            .map_err(|_| Status::internal("port table poisoned"))?;
        let mut ports: Vec<pb::Port> = table.values().map(Self::to_proto).collect();
        ports.sort_by_key(|p| p.index);
        Ok(Response::new(pb::ListPortsResponse { ports }))
    }

    async fn set_port_attrs(
        &self,
        request: Request<pb::SetPortAttrsRequest>,
    ) -> Result<Response<pb::SetPortAttrsResponse>, Status> {
        let req = request.into_inner();

        let (sai_id, admin_change) = {
            let table = self
                .handle
                .ports
                .read()
                .map_err(|_| Status::internal("port table poisoned"))?;
            let port = table
                .get(&req.name)
                .ok_or_else(|| Status::not_found(format!("no port {:?}", req.name)))?;
            let admin_change = match req.admin_state.map(pb::AdminState::try_from) {
                Some(Ok(pb::AdminState::Up)) => Some(true),
                Some(Ok(pb::AdminState::Down)) => Some(false),
                Some(_) => {
                    return Err(Status::invalid_argument("unspecified admin_state"));
                }
                None => None,
            };
            (port.sai_id, admin_change)
        };

        // Drive the ASIC outside the lock.
        if let Some(up) = admin_change {
            self.handle
                .set_admin_state(sai_id, up)
                .await
                .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        }

        let mut table = self
            .handle
            .ports
            .write()
            .map_err(|_| Status::internal("port table poisoned"))?;
        let port = table
            .get_mut(&req.name)
            .ok_or_else(|| Status::not_found(format!("no port {:?}", req.name)))?;
        if let Some(up) = admin_change {
            port.admin_up = up;
        }
        if let Some(description) = req.description {
            port.description = description;
        }
        Ok(Response::new(pb::SetPortAttrsResponse {
            port: Some(Self::to_proto(port)),
        }))
    }

    type WatchPortEventsStream =
        std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<pb::PortEvent, Status>> + Send>>;

    async fn watch_port_events(
        &self,
        _request: Request<pb::WatchPortEventsRequest>,
    ) -> Result<Response<Self::WatchPortEventsStream>, Status> {
        let stream = BroadcastStream::new(self.handle.events.subscribe()).filter_map(|item| {
            match item {
                Ok(event) => Some(Ok(pb::PortEvent {
                    name: event.name,
                    oper_status: if event.oper_up {
                        pb::OperStatus::Up
                    } else {
                        pb::OperStatus::Down
                    } as i32,
                })),
                // Slow consumer skipped `n` events; keep streaming.
                Err(BroadcastStreamRecvError::Lagged(_)) => None,
            }
        });
        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_interfaces(
        &self,
        request: Request<pb::GetInterfacesRequest>,
    ) -> Result<Response<pb::GetInterfacesResponse>, Status> {
        let names = request.into_inner().names;
        let wanted = |name: &str| names.is_empty() || names.iter().any(|n| n == name);
        let now = Instant::now();

        let mut interfaces = Vec::new();
        {
            let table = self
                .handle
                .ports
                .read()
                .map_err(|_| Status::internal("port table poisoned"))?;
            let mut ports: Vec<&PortState> = table.values().collect();
            ports.sort_by_key(|p| p.def.index);
            for port in ports {
                if wanted(&port.def.name) {
                    interfaces.push(self.port_to_iface(port, now));
                }
            }
        }
        {
            let devs = self
                .netdevs
                .read()
                .map_err(|_| Status::internal("netdev table poisoned"))?;
            let mut devs: Vec<&NetdevSample> = devs.values().collect();
            devs.sort_by(|a, b| a.name.cmp(&b.name));
            for dev in devs {
                if wanted(&dev.name) {
                    interfaces.push(self.netdev_to_iface(dev, now));
                }
            }
        }
        Ok(Response::new(pb::GetInterfacesResponse {
            interfaces,
            platform_model: self.inventory.platform_model.clone(),
        }))
    }

    async fn clear_counters(
        &self,
        request: Request<pb::ClearCountersRequest>,
    ) -> Result<Response<pb::ClearCountersResponse>, Status> {
        let names = request.into_inner().names;
        let cleared = self.engine.clear(&names, Instant::now());
        Ok(Response::new(pb::ClearCountersResponse { cleared }))
    }

    async fn set_interface_address(
        &self,
        request: Request<pb::SetInterfaceAddressRequest>,
    ) -> Result<Response<pb::SetInterfaceAddressResponse>, Status> {
        let req = request.into_inner();
        let (addr, len) =
            hemlock_common::net::parse_cidr(&req.address).map_err(Status::invalid_argument)?;

        let (sai_id, existing) = {
            let table = self
                .handle
                .ports
                .read()
                .map_err(|_| Status::internal("port table poisoned"))?;
            let port = table
                .get(&req.name)
                .ok_or_else(|| Status::not_found(format!("no port {:?}", req.name)))?;
            (port.sai_id, port.l3.clone())
        };
        if existing
            .as_ref()
            .is_some_and(|l3| l3.address == req.address)
        {
            return Ok(Response::new(pb::SetInterfaceAddressResponse {}));
        }

        // Drive the ASIC outside the lock. An address change keeps the
        // RIF and swaps the routes; route creation uses remove-then-add
        // so a retried half-applied change converges.
        let sai = |e: hemlock_sai::SaiError| Status::internal(format!("SAI: {e}"));
        if let Some(old) = &existing {
            for dest in Self::route_dests(&old.address) {
                let _ = self.handle.remove_route(dest).await;
            }
        }
        let rif = match &existing {
            Some(l3) => l3.rif,
            None => self
                .handle
                .create_router_interface(sai_id)
                .await
                .map_err(sai)?,
        };
        let (host, subnet) = Self::routes_for(addr, len);
        let _ = self.handle.remove_route(host).await;
        self.handle
            .create_route(host, hemlock_sai::RouteTarget::Cpu)
            .await
            .map_err(sai)?;
        if let Some(subnet) = subnet {
            let _ = self.handle.remove_route(subnet).await;
            self.handle
                .create_route(subnet, hemlock_sai::RouteTarget::Rif(rif))
                .await
                .map_err(sai)?;
        }

        let mut table = self
            .handle
            .ports
            .write()
            .map_err(|_| Status::internal("port table poisoned"))?;
        let port = table
            .get_mut(&req.name)
            .ok_or_else(|| Status::not_found(format!("no port {:?}", req.name)))?;
        port.l3 = Some(L3State {
            rif,
            address: req.address,
        });
        Ok(Response::new(pb::SetInterfaceAddressResponse {}))
    }

    async fn clear_interface_address(
        &self,
        request: Request<pb::ClearInterfaceAddressRequest>,
    ) -> Result<Response<pb::ClearInterfaceAddressResponse>, Status> {
        let req = request.into_inner();
        let (sai_id, existing) = {
            let table = self
                .handle
                .ports
                .read()
                .map_err(|_| Status::internal("port table poisoned"))?;
            let port = table
                .get(&req.name)
                .ok_or_else(|| Status::not_found(format!("no port {:?}", req.name)))?;
            (port.sai_id, port.l3.clone())
        };
        let Some(l3) = existing else {
            return Ok(Response::new(pb::ClearInterfaceAddressResponse {}));
        };

        for dest in Self::route_dests(&l3.address) {
            let _ = self.handle.remove_route(dest).await;
        }
        self.handle
            .remove_router_interface(sai_id, l3.rif)
            .await
            .map_err(|e| Status::internal(format!("SAI: {e}")))?;

        let mut table = self
            .handle
            .ports
            .write()
            .map_err(|_| Status::internal("port table poisoned"))?;
        if let Some(port) = table.get_mut(&req.name) {
            port.l3 = None;
        }
        Ok(Response::new(pb::ClearInterfaceAddressResponse {}))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::actor::SaiActor;
    use crate::ifstats::Engine;
    use hemlock_common::proto::v1::syncd_server::Syncd as _;
    use hemlock_platform::Platform;

    fn test_platform() -> Platform {
        let toml = r#"
schema_version = 1

[platform]
id = "test-sw"
onie_machine = "x86_64-test_sw-r0"
vendor = "Hemlock"
model = "TestSwitch"
asic_family = "broadcom-xgs"
asic = "helix4"

[sai]
package = "libsaibcm"
version_pin = "0"
libsai_path = "/usr/lib/libsai.so.1"
config_bcm = "config.bcm"

[ports]
uc_queues = 2
mc_queues = 1

[[ports.group]]
prefix = "Ethernet"
name_start = 1
index_start = 1
speed_mbps = 1000
autoneg = true
media = "1000BASE-T"
phy_model = "HLK-PHY-TEST"
supported_modes = ["1G/full", "auto"]
lanes = [1, 2]
"#;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("platform.toml"), toml).unwrap();
        Platform::load(dir.path()).unwrap()
    }

    /// End-to-end over the mock data-plane: actor + engine + service, no
    /// gRPC transport (the service methods are plain async fns).
    #[tokio::test(flavor = "multi_thread")]
    async fn get_interfaces_and_clear_counters_over_mock_sai() {
        let platform = test_platform();
        let backend = Box::new(hemlock_sai::mock::MockSai::new(platform.ports.clone()));
        let handle = Arc::new(SaiActor::spawn(backend, &platform).await.unwrap());
        let engine = Engine::new(300);

        // One collector-equivalent sweep.
        let ports: Vec<(String, hemlock_sai::PortId)> = handle
            .ports
            .read()
            .unwrap()
            .iter()
            .map(|(n, p)| (n.clone(), p.sai_id))
            .collect();
        let now = Instant::now();
        for sample in handle.port_stats(ports).await.unwrap() {
            engine.ingest(&sample.name, sample.counters.into(), Vec::new(), now);
        }
        if let Ok(table) = handle.ports.read() {
            for (name, port) in table.iter() {
                engine.note_link(name, port.oper_up, now);
            }
        }

        let service = SyncdService::new(
            handle,
            engine,
            Arc::default(),
            Inventory {
                platform_model: "Hemlock TestSwitch".into(),
                management: None,
                uc_queues: 2,
                mc_queues: 1,
                mac: Some("2c:dd:e9:12:00:01".into()),
            },
        );

        let response = service
            .get_interfaces(Request::new(pb::GetInterfacesRequest { names: vec![] }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(response.platform_model, "Hemlock TestSwitch");
        assert_eq!(response.interfaces.len(), 2);
        let et1 = &response.interfaces[0];
        assert_eq!(et1.name, "Ethernet1");
        assert_eq!(et1.kind, "ethernet");
        assert_eq!(et1.oper_status, pb::OperStatus::Up as i32);
        assert_eq!(et1.speed_mbps, 1000);
        assert_eq!(et1.mac, "2c:dd:e9:12:00:01");
        assert_eq!(et1.phy_model, "HLK-PHY-TEST");
        assert_eq!(et1.supported_modes, ["1G/full", "auto"]);
        // Queue rows padded per the platform declaration.
        let labels: Vec<&str> = et1.queues.iter().map(|q| q.queue.as_str()).collect();
        assert_eq!(labels, ["UC0", "UC1", "MC0"]);
        assert!(et1.counters.is_some());
        assert!(et1.rates.is_some());
        assert_eq!(et1.seconds_since_clear, None, "never cleared");

        // Name filtering.
        let one = service
            .get_interfaces(Request::new(pb::GetInterfacesRequest {
                names: vec!["Ethernet2".into()],
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(one.interfaces.len(), 1);
        assert_eq!(one.interfaces[0].name, "Ethernet2");

        // clear counters baselines both tracked ports.
        let cleared = service
            .clear_counters(Request::new(pb::ClearCountersRequest { names: vec![] }))
            .await
            .unwrap()
            .into_inner()
            .cleared;
        assert_eq!(cleared, 2);
        let response = service
            .get_interfaces(Request::new(pb::GetInterfacesRequest { names: vec![] }))
            .await
            .unwrap()
            .into_inner();
        assert!(response.interfaces[0].seconds_since_clear.is_some());
    }

    /// Address lifecycle over the mock data-plane: set, replace, clear.
    #[tokio::test(flavor = "multi_thread")]
    async fn interface_address_lifecycle_over_mock_sai() {
        let platform = test_platform();
        let backend = Box::new(hemlock_sai::mock::MockSai::new(platform.ports.clone()));
        let handle = Arc::new(SaiActor::spawn(backend, &platform).await.unwrap());
        let service = SyncdService::new(
            handle.clone(),
            Engine::new(300),
            Arc::default(),
            Inventory::default(),
        );

        // Unknown port and bad CIDR are rejected.
        let err = service
            .set_interface_address(Request::new(pb::SetInterfaceAddressRequest {
                name: "Ethernet9".into(),
                address: "10.0.0.1/24".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
        let err = service
            .set_interface_address(Request::new(pb::SetInterfaceAddressRequest {
                name: "Ethernet1".into(),
                address: "banana".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // Set: RIF + routes recorded, address visible via GetInterfaces.
        service
            .set_interface_address(Request::new(pb::SetInterfaceAddressRequest {
                name: "Ethernet1".into(),
                address: "10.42.10.9/24".into(),
            }))
            .await
            .unwrap();
        let rif = {
            let table = handle.ports.read().unwrap();
            let l3 = table["Ethernet1"].l3.clone().unwrap();
            assert_eq!(l3.address, "10.42.10.9/24");
            l3.rif
        };
        let response = service
            .get_interfaces(Request::new(pb::GetInterfacesRequest {
                names: vec!["Ethernet1".into()],
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(response.interfaces[0].ip_addresses, ["10.42.10.9/24"]);

        // Replace keeps the RIF, swaps routes; same address is a no-op.
        service
            .set_interface_address(Request::new(pb::SetInterfaceAddressRequest {
                name: "Ethernet1".into(),
                address: "192.168.1.1/24".into(),
            }))
            .await
            .unwrap();
        {
            let table = handle.ports.read().unwrap();
            let l3 = table["Ethernet1"].l3.clone().unwrap();
            assert_eq!(l3.address, "192.168.1.1/24");
            assert_eq!(l3.rif, rif, "address change keeps the RIF");
        }

        // Clear: back to L2.
        service
            .clear_interface_address(Request::new(pb::ClearInterfaceAddressRequest {
                name: "Ethernet1".into(),
            }))
            .await
            .unwrap();
        assert!(handle.ports.read().unwrap()["Ethernet1"].l3.is_none());
        // Clearing again is a no-op, and the port can be routed afresh.
        service
            .clear_interface_address(Request::new(pb::ClearInterfaceAddressRequest {
                name: "Ethernet1".into(),
            }))
            .await
            .unwrap();
        service
            .set_interface_address(Request::new(pb::SetInterfaceAddressRequest {
                name: "Ethernet1".into(),
                address: "10.0.0.1/31".into(),
            }))
            .await
            .unwrap();
    }
}
