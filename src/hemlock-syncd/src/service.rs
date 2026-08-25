//! gRPC surface of syncd (`hemlock.v1.Syncd`).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use hemlock_common::proto::v1 as pb;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tonic::{Request, Response, Status};

use crate::actor::{FdbNotify, SaiHandle};
use crate::ifstats::{utilization_pct, RawCounters, SharedEngine, Snapshot};
use crate::netdev::NetdevSample;
use crate::state::{
    FdbStaticEntry, L3State, MirrorDir, MirrorState, PortState, StormState, SwitchportState,
    VlanState,
};

/// Fallback L2 MTU for front-panel ports, matching the KNET default the
/// platform loads (`linux-bcm-knet default_mtu=9100`). Used when a port's
/// hostif netdev cannot be read; per-interface MTU intents arrive in a
/// later phase.
const PORT_MTU: u32 = 9100;

/// A port's live MTU: its hostif netdev is named after it, so the kernel
/// has the truth (`ip link` parity). Fallback for mock backends and
/// pre-hostif states.
fn port_mtu(name: &str) -> u32 {
    std::fs::read_to_string(format!("/sys/class/net/{name}/mtu"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|&mtu| mtu > 0)
        .unwrap_or(PORT_MTU)
}

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
    pub(crate) handle: Arc<SaiHandle>,
    engine: SharedEngine,
    netdevs: SharedNetdevs,
    pub(crate) inventory: Inventory,
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
    /// One interface's live per-queue samples, baselined against the
    /// last `clear counters` (empty when nothing has been swept yet).
    pub(crate) fn queue_samples(&self, name: &str) -> Vec<crate::ifstats::QueueSample> {
        self.engine
            .snapshot(name, Instant::now())
            .map(|snap| snap.queues)
            .unwrap_or_default()
    }

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
                    wred_dropped_pkts: sample.map(|q| q.wred_dropped).unwrap_or(0),
                    ecn_marked_pkts: sample.map(|q| q.ecn_marked).unwrap_or(0),
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
            mtu: port_mtu(&port.def.name),
            speed_mbps: speed,
            duplex: "full".into(),
            autoneg: port.def.autoneg,
            media: port.def.media.clone().unwrap_or_default(),
            phy_model: port.def.phy_model.clone().unwrap_or_default(),
            supported_modes: port.def.supported_modes.clone(),
            ip_addresses: port.l3.iter().map(|l3| l3.address.clone()).collect(),
            errdisable_reason: port.errdisable_reason.clone().unwrap_or_default(),
            ..pb::InterfaceState::default()
        };
        if let Some(sp) = &port.switchport {
            state.switchport_mode = if sp.trunk {
                "trunk"
            } else if sp.dot1q_tunnel {
                "dot1q-tunnel"
            } else {
                "access"
            }
            .into();
            state.access_vlan = u32::from(sp.access_vlan);
            state.native_vlan = u32::from(sp.native_vlan);
            state.trunk_vlans = sp.trunk_vlans.iter().map(|v| u32::from(*v)).collect();
        }
        if let Some(snap) = self.engine.snapshot(&port.def.name, now) {
            self.stats_fields(&mut state, &snap, speed, true, true);
        }
        state
    }

    /// Is `port` carrying `vlan`? Explicit memberships are tracked in its
    /// switchport state; default-VLAN membership is implicit (a bridged
    /// port with no non-default access/native VLAN).
    fn port_in_vlan(port: &PortState, vlan: u16) -> bool {
        if port.l3.is_some() {
            return false;
        }
        match &port.switchport {
            None => vlan == 1,
            Some(sp) => {
                sp.members.iter().any(|(v, _, _)| *v == vlan)
                    || (vlan == 1 && {
                        let untagged = if sp.trunk {
                            sp.native_vlan
                        } else {
                            sp.access_vlan
                        };
                        untagged <= 1
                    })
            }
        }
    }

    /// A VLAN as a (synthesized) interface: up when any member port is.
    fn vlan_to_iface(
        &self,
        vlan: u16,
        name: &str,
        l3: Option<&L3State>,
        ports: &HashMap<String, PortState>,
    ) -> pb::InterfaceState {
        let oper_up = ports
            .values()
            .any(|p| p.oper_up && Self::port_in_vlan(p, vlan));
        pb::InterfaceState {
            name: format!("Vlan{vlan}"),
            kind: "vlan".into(),
            admin_state: pb::AdminState::Up as i32,
            oper_status: if oper_up {
                pb::OperStatus::Up
            } else {
                pb::OperStatus::Down
            } as i32,
            mac: self.inventory.mac.clone().unwrap_or_default(),
            bia_mac: self.inventory.mac.clone().unwrap_or_default(),
            description: name.to_string(),
            mtu: PORT_MTU,
            ip_addresses: l3.iter().map(|l3| l3.address.clone()).collect(),
            ..pb::InterfaceState::default()
        }
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

    /// The untagged member ports of a VLAN, by hostif netdev name —
    /// what the SVI's kernel bridge should enslave. Tagged trunk
    /// members are excluded: hostif punt delivery strips tags, so the
    /// kernel could not tell their VLANs apart.
    fn svi_members(vlan: u16, ports: &HashMap<String, PortState>) -> Vec<String> {
        let mut names: Vec<String> = ports
            .values()
            .filter(|p| p.l3.is_none())
            .filter(|p| match &p.switchport {
                None => vlan == 1,
                Some(sp) => {
                    let untagged = if sp.trunk {
                        sp.native_vlan
                    } else {
                        sp.access_vlan
                    };
                    let untagged = if untagged == 0 { 1 } else { untagged };
                    untagged == vlan
                }
            })
            .map(|p| p.def.name.clone())
            .collect();
        names.sort();
        names
    }

    /// Reconcile every SVI's kernel bridge against current VLAN
    /// membership. Called after anything that changes membership (SVI
    /// create, switchport program, port address set/clear). No kernel
    /// side effects under the mock backend (dev hosts).
    fn reconcile_svi_bridges(&self) {
        if self.handle.backend_name == "mock" {
            return;
        }
        let (svis, members): (Vec<u16>, Vec<Vec<String>>) = {
            let (Ok(vlans), Ok(ports)) = (self.handle.vlans.read(), self.handle.ports.read())
            else {
                return;
            };
            vlans
                .iter()
                .filter(|(_, v)| v.l3.is_some())
                .map(|(id, _)| (*id, Self::svi_members(*id, &ports)))
                .unzip()
        };
        for (id, members) in svis.iter().zip(&members) {
            crate::netdev::reconcile_svi_bridge(&format!("Vlan{id}"), members);
        }
    }

    /// Tear down an SVI's ASIC objects (routes + VLAN RIF) and its
    /// kernel bridge. The caller updates the VLAN table.
    async fn teardown_svi(&self, vlan: u16, l3: &L3State) -> Result<(), Status> {
        // Transit routes/neighbors riding this RIF go first, or the RIF
        // removal would find the ASIC objects still in use.
        self.fib_teardown_rif(l3.rif).await;
        for dest in Self::route_dests(&l3.address) {
            let _ = self.handle.remove_route(dest).await;
        }
        self.handle
            .remove_vlan_rif(l3.rif)
            .await
            .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        if self.handle.backend_name != "mock" {
            crate::netdev::delete_svi_bridge(&format!("Vlan{vlan}"));
        }
        Ok(())
    }

    /// `set_interface_address` for an SVI (`Vlan<id>`): VLAN-type
    /// router interface + IP2ME/subnet routes, then the kernel bridge.
    async fn set_svi_address(
        &self,
        vlan: u16,
        address: String,
        addr: std::net::IpAddr,
        len: u8,
    ) -> Result<Response<pb::SetInterfaceAddressResponse>, Status> {
        let sai = |e: hemlock_sai::SaiError| Status::internal(format!("SAI: {e}"));
        let (vlan_oid, existing) = {
            let vlans = self
                .handle
                .vlans
                .read()
                .map_err(|_| Status::internal("vlan table poisoned"))?;
            match vlans.get(&vlan) {
                Some(state) => (state.oid, state.l3.clone()),
                // The default VLAN always exists in hardware even
                // without a table entry.
                None if vlan == 1 => (None, None),
                None => {
                    return Err(Status::failed_precondition(format!(
                        "VLAN {vlan} is not defined (set vlans vlan {vlan})"
                    )));
                }
            }
        };
        if existing.as_ref().is_some_and(|l3| l3.address == address) {
            return Ok(Response::new(pb::SetInterfaceAddressResponse {}));
        }

        // Same shape as the port path: keep the RIF on an address
        // change, swap the routes, remove-then-add for convergence.
        if let Some(old) = &existing {
            for dest in Self::route_dests(&old.address) {
                let _ = self.handle.remove_route(dest).await;
            }
        }
        let rif = match &existing {
            Some(l3) => l3.rif,
            None => self.handle.create_vlan_rif(vlan_oid).await.map_err(sai)?,
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

        {
            let mut vlans = self
                .handle
                .vlans
                .write()
                .map_err(|_| Status::internal("vlan table poisoned"))?;
            let l3 = Some(L3State { rif, address });
            vlans
                .entry(vlan)
                .and_modify(|v| v.l3 = l3.clone())
                .or_insert(VlanState {
                    oid: None,
                    name: String::new(),
                    suspended: false,
                    l3,
                });
        }
        self.reconcile_svi_bridges();
        Ok(Response::new(pb::SetInterfaceAddressResponse {}))
    }

    /// `clear_interface_address` for an SVI.
    async fn clear_svi_address(
        &self,
        vlan: u16,
    ) -> Result<Response<pb::ClearInterfaceAddressResponse>, Status> {
        let existing = {
            let vlans = self
                .handle
                .vlans
                .read()
                .map_err(|_| Status::internal("vlan table poisoned"))?;
            vlans.get(&vlan).and_then(|v| v.l3.clone())
        };
        let Some(l3) = existing else {
            return Ok(Response::new(pb::ClearInterfaceAddressResponse {}));
        };
        self.teardown_svi(vlan, &l3).await?;
        let mut vlans = self
            .handle
            .vlans
            .write()
            .map_err(|_| Status::internal("vlan table poisoned"))?;
        if let Some(state) = vlans.get_mut(&vlan) {
            state.l3 = None;
        }
        Ok(Response::new(pb::ClearInterfaceAddressResponse {}))
    }
}

// gRPC helper fns naturally speak `Status`; its size is tonic's business.
#[allow(clippy::result_large_err)]
impl SyncdService {
    /// A capability gate: commits that need an absent SAI capability
    /// fail cleanly with the platform error, never silently no-op.
    pub(crate) fn require_capability(&self, supported: bool, family: &str) -> Result<(), Status> {
        if supported {
            Ok(())
        } else {
            Err(Status::failed_precondition(format!(
                "{family} is not supported by this platform's SAI"
            )))
        }
    }

    /// A request MAC as bytes, canonicalized.
    pub(crate) fn mac_bytes(text: &str) -> Result<([u8; 6], String), Status> {
        let canonical = hemlock_common::net::parse_mac(text).map_err(Status::invalid_argument)?;
        let mut mac = [0u8; 6];
        for (i, part) in canonical.split(':').enumerate() {
            mac[i] = u8::from_str_radix(part, 16)
                .map_err(|_| Status::invalid_argument(format!("bad MAC {text:?}")))?;
        }
        Ok((mac, canonical))
    }

    /// The backend VLAN reference for FDB programming: None = the
    /// default VLAN; other VLANs must be defined.
    fn fdb_vlan_ref(&self, vlan: u16) -> Result<Option<hemlock_sai::Oid>, Status> {
        if vlan == 1 {
            return Ok(None);
        }
        let vlans = self
            .handle
            .vlans
            .read()
            .map_err(|_| Status::internal("vlan table poisoned"))?;
        match vlans.get(&vlan).and_then(|v| v.oid) {
            Some(oid) => Ok(Some(oid)),
            None => Err(Status::failed_precondition(format!(
                "VLAN {vlan} is not defined (set vlans vlan {vlan})"
            ))),
        }
    }

    /// The RIF fronting an L3 interface name (routed port or SVI).
    fn rif_of(&self, interface: &str) -> Result<hemlock_sai::Oid, Status> {
        let no_l3 = || {
            Status::failed_precondition(format!(
                "{interface} has no router interface (no address configured)"
            ))
        };
        if let Some(vlan) = svi_vlan_id(interface) {
            let vlans = self
                .handle
                .vlans
                .read()
                .map_err(|_| Status::internal("vlan table poisoned"))?;
            let state = vlans
                .get(&vlan)
                .ok_or_else(|| Status::not_found(format!("no such VLAN {vlan}")))?;
            return state.l3.as_ref().map(|l3| l3.rif).ok_or_else(no_l3);
        }
        let ports = self
            .handle
            .ports
            .read()
            .map_err(|_| Status::internal("port table poisoned"))?;
        let port = ports
            .get(interface)
            .ok_or_else(|| Status::not_found(format!("no such interface {interface:?}")))?;
        port.l3.as_ref().map(|l3| l3.rif).ok_or_else(no_l3)
    }

    /// Take one reference on the deduplicated next hop for (rif, ip),
    /// creating it on first use.
    async fn fib_acquire_hop(
        &self,
        rif: hemlock_sai::Oid,
        ip: std::net::IpAddr,
    ) -> Result<hemlock_sai::Oid, Status> {
        let key = (rif, ip.to_string());
        let existing = {
            let mut fib = self
                .handle
                .fib
                .write()
                .map_err(|_| Status::internal("fib table poisoned"))?;
            fib.next_hops.get_mut(&key).map(|(oid, refs)| {
                *refs += 1;
                *oid
            })
        };
        if let Some(oid) = existing {
            return Ok(oid);
        }
        let oid = self
            .handle
            .create_next_hop(rif, ip)
            .await
            .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        let mut fib = self
            .handle
            .fib
            .write()
            .map_err(|_| Status::internal("fib table poisoned"))?;
        fib.next_hops.insert(key, (oid, 1));
        Ok(oid)
    }

    /// Drop one reference on a next hop, removing the ASIC object with
    /// the last one.
    async fn fib_release_hop(&self, key: &(hemlock_sai::Oid, String)) {
        let gone = {
            let Ok(mut fib) = self.handle.fib.write() else {
                return;
            };
            let last = match fib.next_hops.get_mut(key) {
                Some((_, refs)) if *refs <= 1 => true,
                Some((_, refs)) => {
                    *refs -= 1;
                    false
                }
                None => false,
            };
            if last {
                fib.next_hops.remove(key).map(|(oid, _)| oid)
            } else {
                None
            }
        };
        if let Some(oid) = gone {
            if let Err(err) = self.handle.remove_next_hop(oid).await {
                tracing::warn!(%err, "removing next hop");
            }
        }
    }

    /// Take one reference on the deduplicated ECMP group for a sorted
    /// member set, creating group + members on first use.
    async fn fib_acquire_group(
        &self,
        member_oids: &[hemlock_sai::Oid],
    ) -> Result<hemlock_sai::Oid, Status> {
        let key = member_oids.to_vec();
        let existing = {
            let mut fib = self
                .handle
                .fib
                .write()
                .map_err(|_| Status::internal("fib table poisoned"))?;
            fib.groups.get_mut(&key).map(|group| {
                group.refs += 1;
                group.oid
            })
        };
        if let Some(oid) = existing {
            return Ok(oid);
        }
        let sai = |e: hemlock_sai::SaiError| Status::internal(format!("SAI: {e}"));
        let group = self.handle.create_next_hop_group().await.map_err(sai)?;
        let mut members = Vec::with_capacity(member_oids.len());
        for next_hop in member_oids {
            match self
                .handle
                .add_next_hop_group_member(group, *next_hop)
                .await
            {
                Ok(member) => members.push(member),
                Err(err) => {
                    // Unwind the half-built group.
                    for member in members {
                        let _ = self.handle.remove_next_hop_group_member(member).await;
                    }
                    let _ = self.handle.remove_next_hop_group(group).await;
                    return Err(sai(err));
                }
            }
        }
        let mut fib = self
            .handle
            .fib
            .write()
            .map_err(|_| Status::internal("fib table poisoned"))?;
        fib.groups.insert(
            key,
            crate::state::FibGroup {
                oid: group,
                members,
                refs: 1,
            },
        );
        Ok(group)
    }

    /// Drop one reference on an ECMP group, tearing it down with the
    /// last one.
    async fn fib_release_group(&self, key: &[hemlock_sai::Oid]) {
        let gone = {
            let Ok(mut fib) = self.handle.fib.write() else {
                return;
            };
            let last = match fib.groups.get_mut(key) {
                Some(group) if group.refs <= 1 => true,
                Some(group) => {
                    group.refs -= 1;
                    false
                }
                None => false,
            };
            if last {
                fib.groups.remove(key)
            } else {
                None
            }
        };
        if let Some(group) = gone {
            for member in group.members {
                if let Err(err) = self.handle.remove_next_hop_group_member(member).await {
                    tracing::warn!(%err, "removing next-hop group member");
                }
            }
            if let Err(err) = self.handle.remove_next_hop_group(group.oid).await {
                tracing::warn!(%err, "removing next-hop group");
            }
        }
    }

    /// Release everything a route record holds (group first: members
    /// reference the next hops).
    async fn fib_release_route(&self, route: &crate::state::FibRoute) {
        if let Some(group_key) = &route.group_key {
            self.fib_release_group(group_key).await;
        }
        for key in &route.hop_keys {
            self.fib_release_hop(key).await;
        }
    }

    /// Tear down every FIB object riding a RIF that is about to be
    /// removed (interface address cleared). orch re-ensures whatever
    /// still applies on its next reconcile.
    async fn fib_teardown_rif(&self, rif: hemlock_sai::Oid) {
        let (routes, neighbors) = {
            let Ok(fib) = self.handle.fib.read() else {
                return;
            };
            let routes: Vec<String> = fib
                .routes
                .iter()
                .filter(|(_, route)| route.hop_keys.iter().any(|(r, _)| *r == rif))
                .map(|(prefix, _)| prefix.clone())
                .collect();
            let neighbors: Vec<(String, String)> = fib
                .neighbors
                .iter()
                .filter(|(_, (r, _))| *r == rif)
                .map(|(key, _)| key.clone())
                .collect();
            (routes, neighbors)
        };
        for prefix in routes {
            let record = self
                .handle
                .fib
                .write()
                .ok()
                .and_then(|mut fib| fib.routes.remove(&prefix));
            if let Some(record) = record {
                if let Ok(dest) = hemlock_common::net::parse_cidr(&prefix) {
                    let _ = self.handle.remove_route(dest).await;
                }
                self.fib_release_route(&record).await;
            }
        }
        for key in neighbors {
            let record = self
                .handle
                .fib
                .write()
                .ok()
                .and_then(|mut fib| fib.neighbors.remove(&key));
            if let Some((rif, _)) = record {
                if let Ok(ip) = key.1.parse() {
                    let _ = self.handle.remove_neighbor(rif, ip).await;
                }
            }
        }
    }

    pub(crate) fn port_sai_id(&self, name: &str) -> Result<hemlock_sai::PortId, Status> {
        let table = self
            .handle
            .ports
            .read()
            .map_err(|_| Status::internal("port table poisoned"))?;
        table
            .get(name)
            .map(|p| p.sai_id)
            .ok_or_else(|| Status::not_found(format!("no port {name:?}")))
    }

    /// A physical port's or a port-channel's port-like SAI id.
    pub(crate) fn port_like_sai_id(&self, name: &str) -> Result<hemlock_sai::PortId, Status> {
        if let Some(group) = lag_group_of(name) {
            let lags = self
                .handle
                .lags
                .read()
                .map_err(|_| Status::internal("lag table poisoned"))?;
            return lags.get(&group).map(|lag| lag.sai_id).ok_or_else(|| {
                Status::failed_precondition(format!("Port-Channel{group} not created"))
            });
        }
        self.port_sai_id(name)
    }

    /// Percent level ("10.00") -> hundredths of a percent.
    fn level_hundredths(level: &str) -> Result<u64, Status> {
        let canonical =
            hemlock_common::net::parse_storm_level(level).map_err(Status::invalid_argument)?;
        let (whole, frac) = canonical
            .split_once('.')
            .ok_or_else(|| Status::internal("level not canonical"))?;
        let whole: u64 = whole
            .parse()
            .map_err(|_| Status::internal("level not canonical"))?;
        let frac: u64 = frac
            .parse()
            .map_err(|_| Status::internal("level not canonical"))?;
        Ok(whole * 100 + frac)
    }

    /// A storm level's rate on a port: percent of the port's configured
    /// speed, in kb/s.
    fn storm_kbps(speed_mbps: u32, hundredths: u64) -> u64 {
        u64::from(speed_mbps) * hundredths / 10
    }

    /// The (vlan, group IP) key of an L2MC RPC, validated.
    fn l2mc_key(raw_vlan: u32, group: &str) -> Result<(u16, std::net::IpAddr), Status> {
        let vlan = vlan_id(raw_vlan).map_err(Status::invalid_argument)?;
        let group_ip: std::net::IpAddr = group
            .parse()
            .map_err(|_| Status::invalid_argument(format!("bad group address {group:?}")))?;
        if !group_ip.is_multicast() {
            return Err(Status::invalid_argument(format!(
                "{group_ip} is not a multicast address"
            )));
        }
        Ok((vlan, group_ip))
    }

    /// Reconcile an L2MC group's output ports declaratively; returns
    /// the resulting member map.
    async fn reconcile_l2mc_members(
        &self,
        state: crate::state::L2mcGroupState,
        wanted: &[String],
    ) -> Result<std::collections::BTreeMap<String, hemlock_sai::Oid>, Status> {
        let sai = |e: hemlock_sai::SaiError| Status::internal(format!("SAI: {e}"));
        let mut members = state.members;
        let stale: Vec<String> = members
            .keys()
            .filter(|name| !wanted.contains(name))
            .cloned()
            .collect();
        for name in stale {
            if let Some(member) = members.remove(&name) {
                self.handle.remove_l2mc_member(member).await.map_err(sai)?;
            }
        }
        for name in wanted {
            if members.contains_key(name) {
                continue;
            }
            let port_id = self.port_like_sai_id(name)?;
            let member = self
                .handle
                .add_l2mc_member(state.oid, port_id)
                .await
                .map_err(sai)?;
            members.insert(name.clone(), member);
        }
        Ok(members)
    }

    /// The shared declarative L2 program for a port-like object:
    /// reconcile VLAN memberships against the wanted mode, settle the
    /// default VLAN, classify untagged ingress. Returns the resulting
    /// membership list. VLANs not (yet) defined are skipped — they
    /// attach when the VLAN is created and the object reprogrammed;
    /// suspended VLANs are skipped the same way.
    #[allow(clippy::too_many_arguments)]
    async fn program_l2(
        &self,
        name: &str,
        sai_id: hemlock_sai::PortId,
        current: Vec<(u16, hemlock_sai::Oid, bool)>,
        trunk: bool,
        access_vlan: u16,
        native_vlan: u16,
        trunk_vlans: &[u16],
    ) -> Result<Vec<(u16, hemlock_sai::Oid, bool)>, Status> {
        let sai = |e: hemlock_sai::SaiError| Status::internal(format!("SAI: {e}"));
        let mut desired: Vec<(u16, bool)> = Vec::new();
        if trunk {
            if native_vlan != 1 {
                desired.push((native_vlan, false));
            }
            for vlan in trunk_vlans {
                if *vlan != native_vlan && *vlan != 1 {
                    desired.push((*vlan, true));
                }
            }
        } else if access_vlan != 1 {
            desired.push((access_vlan, false));
        }
        let desired: Vec<(u16, bool, hemlock_sai::Oid)> = {
            let vlans = self
                .handle
                .vlans
                .read()
                .map_err(|_| Status::internal("vlan table poisoned"))?;
            desired
                .into_iter()
                .filter_map(|(vlan, tagged)| match vlans.get(&vlan) {
                    Some(state) if state.suspended => None,
                    Some(state) => state.oid.map(|oid| (vlan, tagged, oid)),
                    None => {
                        tracing::warn!(
                            port = %name,
                            vlan,
                            "switchport references an undefined VLAN; membership skipped"
                        );
                        None
                    }
                })
                .collect()
        };

        // Reconcile: drop stale memberships, settle the default VLAN,
        // add what's missing, then classify untagged ingress.
        let mut members = Vec::new();
        for (vlan, member, tagged) in current {
            if desired.iter().any(|(v, t, _)| *v == vlan && *t == tagged) {
                members.push((vlan, member, tagged));
            } else {
                self.handle.remove_vlan_member(member).await.map_err(sai)?;
            }
        }
        let wants_default = if trunk {
            native_vlan == 1
        } else {
            access_vlan == 1
        };
        if wants_default {
            self.handle
                .restore_port_default_vlan(sai_id)
                .await
                .map_err(sai)?;
        } else {
            self.handle
                .remove_port_default_vlan(sai_id)
                .await
                .map_err(sai)?;
        }
        for (vlan, tagged, oid) in &desired {
            if !members.iter().any(|(v, _, t)| v == vlan && t == tagged) {
                let member = self
                    .handle
                    .add_vlan_member(*oid, sai_id, *tagged)
                    .await
                    .map_err(sai)?;
                members.push((*vlan, member, *tagged));
            }
        }
        let pvid = if trunk { native_vlan } else { access_vlan };
        self.handle.set_port_pvid(sai_id, pvid).await.map_err(sai)?;
        Ok(members)
    }

    /// Reprogram one port's mirror attachment from the session table:
    /// its ingress and egress sessions are derived across every session
    /// (rx in one session and tx in another is legal).
    async fn reprogram_port_mirror(&self, name: &str) -> Result<(), Status> {
        let sai_id = self.port_sai_id(name)?;
        let (ingress, egress) = {
            let mirrors = self
                .handle
                .mirrors
                .read()
                .map_err(|_| Status::internal("mirror table poisoned"))?;
            let mut ingress = None;
            let mut egress = None;
            for state in mirrors.values() {
                if let Some(direction) = state.sources.get(name) {
                    if matches!(direction, MirrorDir::Rx | MirrorDir::Both) {
                        ingress = Some(state.oid);
                    }
                    if matches!(direction, MirrorDir::Tx | MirrorDir::Both) {
                        egress = Some(state.oid);
                    }
                }
            }
            (ingress, egress)
        };
        self.handle
            .set_port_mirror(sai_id, ingress, egress)
            .await
            .map_err(|e| Status::internal(format!("SAI: {e}")))
    }

    /// Reconcile VLAN memberships when a VLAN's suspend state flips:
    /// suspending detaches every membership (frames stop forwarding),
    /// resuming re-adds what the ports' switchport programs want.
    async fn reconcile_vlan_suspension(&self, id: u16, suspend: bool) -> Result<(), Status> {
        let sai = |e: hemlock_sai::SaiError| Status::internal(format!("SAI: {e}"));
        if suspend {
            let detach: Vec<hemlock_sai::Oid> = {
                let table = self
                    .handle
                    .ports
                    .read()
                    .map_err(|_| Status::internal("port table poisoned"))?;
                table
                    .values()
                    .flat_map(|p| p.switchport.iter())
                    .flat_map(|sp| sp.members.iter())
                    .filter(|(v, _, _)| *v == id)
                    .map(|(_, member, _)| *member)
                    .collect()
            };
            for member in detach {
                self.handle.remove_vlan_member(member).await.map_err(sai)?;
            }
            let mut table = self
                .handle
                .ports
                .write()
                .map_err(|_| Status::internal("port table poisoned"))?;
            for port in table.values_mut() {
                if let Some(sp) = &mut port.switchport {
                    sp.members.retain(|(v, _, _)| *v != id);
                }
            }
        } else {
            // Resume: re-add the memberships the switchport programs
            // reference.
            let vlan_oid = {
                let vlans = self
                    .handle
                    .vlans
                    .read()
                    .map_err(|_| Status::internal("vlan table poisoned"))?;
                vlans.get(&id).and_then(|v| v.oid)
            };
            let Some(vlan_oid) = vlan_oid else {
                return Ok(());
            };
            let wanted: Vec<(String, hemlock_sai::PortId, bool)> = {
                let table = self
                    .handle
                    .ports
                    .read()
                    .map_err(|_| Status::internal("port table poisoned"))?;
                table
                    .values()
                    .filter_map(|p| {
                        let sp = p.switchport.as_ref()?;
                        if sp.members.iter().any(|(v, _, _)| *v == id) {
                            return None;
                        }
                        let tagged = if sp.trunk {
                            if sp.native_vlan == id {
                                false
                            } else if sp.trunk_vlans.contains(&id) {
                                true
                            } else {
                                return None;
                            }
                        } else if sp.access_vlan == id {
                            false
                        } else {
                            return None;
                        };
                        Some((p.def.name.clone(), p.sai_id, tagged))
                    })
                    .collect()
            };
            for (name, sai_id, tagged) in wanted {
                let member = self
                    .handle
                    .add_vlan_member(vlan_oid, sai_id, tagged)
                    .await
                    .map_err(sai)?;
                if let Ok(mut table) = self.handle.ports.write() {
                    if let Some(port) = table.get_mut(&name) {
                        if let Some(sp) = &mut port.switchport {
                            sp.members.push((id, member, tagged));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn storm_class_proto(class: hemlock_sai::StormClass) -> pb::StormClass {
    match class {
        hemlock_sai::StormClass::Broadcast => pb::StormClass::Broadcast,
        hemlock_sai::StormClass::Multicast => pb::StormClass::Multicast,
        hemlock_sai::StormClass::UnknownUnicast => pb::StormClass::UnknownUnicast,
    }
}

/// The group number of a port-channel name (`Port-Channel1` -> 1).
pub(crate) fn lag_group_of(name: &str) -> Option<u16> {
    let digits = name.strip_prefix("Port-Channel")?;
    digits
        .parse::<u16>()
        .ok()
        .filter(|group| (1..=64).contains(group) && !digits.starts_with('0'))
}

/// The VLAN id of an SVI interface name (`Vlan10` -> 10).
fn svi_vlan_id(name: &str) -> Option<u16> {
    let digits = name.strip_prefix("Vlan")?;
    digits
        .parse::<u16>()
        .ok()
        .filter(|id| (1..=4094).contains(id) && !digits.starts_with('0'))
}

#[tonic::async_trait]
impl pb::syncd_server::Syncd for SyncdService {
    async fn get_switch_info(
        &self,
        _request: Request<pb::GetSwitchInfoRequest>,
    ) -> Result<Response<pb::SwitchInfo>, Status> {
        let caps = self.handle.capabilities;
        Ok(Response::new(pb::SwitchInfo {
            platform_id: self.handle.platform_id.clone(),
            backend: self.handle.backend_name.clone(),
            switch_oid: self.handle.switch.oid,
            port_count: self.handle.initial_ports() as u32,
            capabilities: Some(pb::SwitchCapabilities {
                lag: caps.lag,
                stp: caps.stp,
                fdb_flush: caps.fdb_flush,
                fdb_aging: caps.fdb_aging,
                l2mc: caps.l2mc,
                storm_control: caps.storm_control,
                mirror: caps.mirror,
                mirror_sessions_max: caps.mirror_sessions_max,
                port_tpid: caps.port_tpid,
                ecmp_width: caps.ecmp_width,
                ipv6: caps.ipv6,
                my_mac: caps.my_mac,
                acl_ingress: caps.acl_ingress,
                acl_egress: caps.acl_egress,
                acl_entry_policer: caps.acl_entry_policer,
                port_learn_limit: caps.port_learn_limit,
                copp: caps.copp,
                buffer_bytes_total: caps.buffer_bytes_total,
                qos_map_ingress: caps.qos_map_ingress,
                qos_map_egress: caps.qos_map_egress,
                wred: caps.wred,
                ecn: caps.ecn,
                queue_shaper: caps.queue_shaper,
                wred_queue_stats: caps.wred_queue_stats,
            }),
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
        // Port-channels are interfaces too: oper up when any enabled
        // member is up, bandwidth the sum of the bundled members.
        {
            let lags = self
                .handle
                .lags
                .read()
                .map_err(|_| Status::internal("lag table poisoned"))?;
            let ports = self
                .handle
                .ports
                .read()
                .map_err(|_| Status::internal("port table poisoned"))?;
            for lag in lags.values() {
                let name = format!("Port-Channel{}", lag.group);
                if !wanted(&name) {
                    continue;
                }
                let mut members: Vec<&String> = lag.members.keys().collect();
                members.sort();
                let bundled: Vec<(&String, &PortState)> = lag
                    .members
                    .iter()
                    .filter(|(_, m)| m.enabled)
                    .filter_map(|(n, _)| ports.get(n).map(|p| (n, p)))
                    .filter(|(_, p)| p.oper_up)
                    .collect();
                let speed: u64 = bundled
                    .iter()
                    .map(|(_, p)| u64::from(p.def.speed_mbps))
                    .sum();
                let mut state = pb::InterfaceState {
                    name,
                    kind: "port-channel".into(),
                    admin_state: if lag.admin_up {
                        pb::AdminState::Up
                    } else {
                        pb::AdminState::Down
                    } as i32,
                    oper_status: if lag.admin_up && !bundled.is_empty() {
                        pb::OperStatus::Up
                    } else {
                        pb::OperStatus::Down
                    } as i32,
                    mac: self.inventory.mac.clone().unwrap_or_default(),
                    bia_mac: self.inventory.mac.clone().unwrap_or_default(),
                    description: lag.description.clone(),
                    mtu: PORT_MTU,
                    speed_mbps: speed,
                    duplex: "full".into(),
                    members: members.into_iter().cloned().collect(),
                    ..pb::InterfaceState::default()
                };
                if let Some(sp) = &lag.switchport {
                    state.switchport_mode = if sp.trunk { "trunk" } else { "access" }.into();
                    state.access_vlan = u32::from(sp.access_vlan);
                    state.native_vlan = u32::from(sp.native_vlan);
                    state.trunk_vlans = sp.trunk_vlans.iter().map(|v| u32::from(*v)).collect();
                }
                interfaces.push(state);
            }
        }

        // Every VLAN is an interface too (Vlan1 always; a kernel netdev
        // with the same name, should one ever exist, wins).
        let (active_vlans, vlan_names, suspended_vlans) = {
            let table = self
                .handle
                .ports
                .read()
                .map_err(|_| Status::internal("port table poisoned"))?;
            let vlans = self
                .handle
                .vlans
                .read()
                .map_err(|_| Status::internal("vlan table poisoned"))?;
            let taken: std::collections::HashSet<String> =
                interfaces.iter().map(|i| i.name.clone()).collect();
            let mut ids: Vec<u16> = vlans.keys().copied().collect();
            if !ids.contains(&1) {
                ids.insert(0, 1);
            }
            for id in &ids {
                let name = format!("Vlan{id}");
                if !wanted(&name) || taken.contains(&name) {
                    continue;
                }
                let display = vlans.get(id).map(|v| v.name.as_str()).unwrap_or("");
                let l3 = vlans.get(id).and_then(|v| v.l3.as_ref());
                interfaces.push(self.vlan_to_iface(*id, display, l3, &table));
            }
            let names = vlans
                .iter()
                .filter(|(_, v)| !v.name.is_empty())
                .map(|(id, v)| (u32::from(*id), v.name.clone()))
                .collect();
            let suspended = vlans
                .iter()
                .filter(|(_, v)| v.suspended)
                .map(|(id, _)| u32::from(*id))
                .collect();
            (
                ids.iter().map(|id| u32::from(*id)).collect(),
                names,
                suspended,
            )
        };
        Ok(Response::new(pb::GetInterfacesResponse {
            interfaces,
            platform_model: self.inventory.platform_model.clone(),
            active_vlans,
            vlan_names,
            suspended_vlans,
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

        if let Some(vlan) = svi_vlan_id(&req.name) {
            return self.set_svi_address(vlan, req.address, addr, len).await;
        }

        let (sai_id, existing) = {
            let table = self
                .handle
                .ports
                .read()
                .map_err(|_| Status::internal("port table poisoned"))?;
            let port = table
                .get(&req.name)
                .ok_or_else(|| Status::not_found(format!("no port {:?}", req.name)))?;
            if port.switchport.is_some() {
                return Err(Status::failed_precondition(format!(
                    "{} has switchport config; delete it before addressing the port",
                    req.name
                )));
            }
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

        {
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
        }
        // A routed port leaves its VLAN; SVI bridges shed its netdev.
        self.reconcile_svi_bridges();
        Ok(Response::new(pb::SetInterfaceAddressResponse {}))
    }

    async fn clear_interface_address(
        &self,
        request: Request<pb::ClearInterfaceAddressRequest>,
    ) -> Result<Response<pb::ClearInterfaceAddressResponse>, Status> {
        let req = request.into_inner();

        if let Some(vlan) = svi_vlan_id(&req.name) {
            return self.clear_svi_address(vlan).await;
        }

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

        // Transit routes/neighbors riding this RIF go first, or the RIF
        // removal would find the ASIC objects still in use.
        self.fib_teardown_rif(l3.rif).await;
        for dest in Self::route_dests(&l3.address) {
            let _ = self.handle.remove_route(dest).await;
        }
        self.handle
            .remove_router_interface(sai_id, l3.rif)
            .await
            .map_err(|e| Status::internal(format!("SAI: {e}")))?;

        {
            let mut table = self
                .handle
                .ports
                .write()
                .map_err(|_| Status::internal("port table poisoned"))?;
            if let Some(port) = table.get_mut(&req.name) {
                port.l3 = None;
            }
        }
        // The port re-joins its VLAN; SVI bridges pick its netdev up.
        self.reconcile_svi_bridges();
        Ok(Response::new(pb::ClearInterfaceAddressResponse {}))
    }

    async fn ensure_vlan(
        &self,
        request: Request<pb::EnsureVlanRequest>,
    ) -> Result<Response<pb::EnsureVlanResponse>, Status> {
        let req = request.into_inner();
        let id = vlan_id(req.id).map_err(Status::invalid_argument)?;
        let (exists, was_suspended) = {
            let vlans = self
                .handle
                .vlans
                .read()
                .map_err(|_| Status::internal("vlan table poisoned"))?;
            match vlans.get(&id) {
                Some(state) => (true, state.suspended),
                None => (false, false),
            }
        };
        // The default VLAN always exists in hardware; only non-default
        // VLANs are created. Drive the ASIC outside the lock.
        let oid = if exists || id == 1 {
            None
        } else {
            Some(
                self.handle
                    .create_vlan(id)
                    .await
                    .map_err(|e| Status::internal(format!("SAI: {e}")))?,
            )
        };
        {
            let mut vlans = self
                .handle
                .vlans
                .write()
                .map_err(|_| Status::internal("vlan table poisoned"))?;
            vlans
                .entry(id)
                .and_modify(|v| {
                    v.name = req.name.clone();
                    v.suspended = req.suspend;
                })
                .or_insert(VlanState {
                    oid,
                    name: req.name,
                    suspended: req.suspend,
                    l3: None,
                });
        }
        // A suspend flip reconciles memberships: suspending detaches
        // them (frames stop forwarding), resuming re-adds what the
        // ports' switchport programs want.
        if exists && was_suspended != req.suspend {
            self.reconcile_vlan_suspension(id, req.suspend).await?;
        }
        Ok(Response::new(pb::EnsureVlanResponse {}))
    }

    async fn remove_vlan(
        &self,
        request: Request<pb::RemoveVlanRequest>,
    ) -> Result<Response<pb::RemoveVlanResponse>, Status> {
        let req = request.into_inner();
        let id = vlan_id(req.id).map_err(Status::invalid_argument)?;
        let sai = |e: hemlock_sai::SaiError| Status::internal(format!("SAI: {e}"));
        let Some(state) = self
            .handle
            .vlans
            .read()
            .map_err(|_| Status::internal("vlan table poisoned"))?
            .get(&id)
            .cloned()
        else {
            return Ok(Response::new(pb::RemoveVlanResponse {}));
        };

        // Detach any port memberships still referencing it.
        let detach: Vec<hemlock_sai::Oid> = {
            let table = self
                .handle
                .ports
                .read()
                .map_err(|_| Status::internal("port table poisoned"))?;
            table
                .values()
                .flat_map(|p| p.switchport.iter())
                .flat_map(|sp| sp.members.iter())
                .filter(|(v, _, _)| *v == id)
                .map(|(_, member, _)| *member)
                .collect()
        };
        // An SVI on the VLAN goes first (its RIF references the VLAN).
        if let Some(l3) = &state.l3 {
            self.teardown_svi(id, l3).await?;
            if let Ok(mut vlans) = self.handle.vlans.write() {
                if let Some(state) = vlans.get_mut(&id) {
                    state.l3 = None;
                }
            }
        }
        for member in detach {
            self.handle.remove_vlan_member(member).await.map_err(sai)?;
        }
        {
            let mut table = self
                .handle
                .ports
                .write()
                .map_err(|_| Status::internal("port table poisoned"))?;
            for port in table.values_mut() {
                if let Some(sp) = &mut port.switchport {
                    sp.members.retain(|(v, _, _)| *v != id);
                }
            }
        }
        if let Some(oid) = state.oid {
            self.handle.remove_vlan(oid).await.map_err(sai)?;
        }
        // The VLAN's hardware FDB entries went with it; drop the
        // software mirror too.
        if let Ok(mut fdb) = self.handle.fdb.write() {
            fdb.statics.retain(|(v, _), _| *v != id);
            fdb.dynamics.retain(|(v, _), _| *v != id);
        }
        self.handle
            .vlans
            .write()
            .map_err(|_| Status::internal("vlan table poisoned"))?
            .remove(&id);
        Ok(Response::new(pb::RemoveVlanResponse {}))
    }

    async fn set_port_switchport(
        &self,
        request: Request<pb::SetPortSwitchportRequest>,
    ) -> Result<Response<pb::SetPortSwitchportResponse>, Status> {
        let req = request.into_inner();
        let sai = |e: hemlock_sai::SaiError| Status::internal(format!("SAI: {e}"));
        // dot1q-tunnel programs access-like membership (the S-VLAN is
        // the access VLAN); its TPID treatment rides on top.
        let (trunk, dot1q_tunnel) = match pb::SwitchportMode::try_from(req.mode) {
            Ok(pb::SwitchportMode::Trunk) => (true, false),
            Ok(pb::SwitchportMode::Access) => (false, false),
            Ok(pb::SwitchportMode::Dot1qTunnel) => (false, true),
            _ => return Err(Status::invalid_argument("unspecified switchport mode")),
        };
        if dot1q_tunnel {
            self.require_capability(self.handle.capabilities.port_tpid, "dot1q-tunnel")?;
        }
        let access_vlan = default_vlan_id(req.access_vlan).map_err(Status::invalid_argument)?;
        let native_vlan = default_vlan_id(req.native_vlan).map_err(Status::invalid_argument)?;
        let mut trunk_vlans: Vec<u16> = req
            .trunk_vlans
            .iter()
            .map(|v| vlan_id(*v))
            .collect::<Result<_, String>>()
            .map_err(Status::invalid_argument)?;
        trunk_vlans.sort_unstable();
        trunk_vlans.dedup();

        // Port-channels take the same declarative program; only the
        // state store differs.
        if let Some(group) = lag_group_of(&req.name) {
            if dot1q_tunnel {
                return Err(Status::invalid_argument(
                    "dot1q-tunnel is not supported on port-channel interfaces",
                ));
            }
            let (sai_id, current) = {
                let lags = self
                    .handle
                    .lags
                    .read()
                    .map_err(|_| Status::internal("lag table poisoned"))?;
                let lag = lags.get(&group).ok_or_else(|| {
                    Status::failed_precondition(format!("Port-Channel{group} not created"))
                })?;
                (
                    lag.sai_id,
                    lag.switchport
                        .as_ref()
                        .map(|sp| sp.members.clone())
                        .unwrap_or_default(),
                )
            };
            let members = self
                .program_l2(
                    &req.name,
                    sai_id,
                    current,
                    trunk,
                    access_vlan,
                    native_vlan,
                    &trunk_vlans,
                )
                .await?;
            let mut lags = self
                .handle
                .lags
                .write()
                .map_err(|_| Status::internal("lag table poisoned"))?;
            if let Some(lag) = lags.get_mut(&group) {
                lag.switchport = Some(SwitchportState {
                    trunk,
                    dot1q_tunnel: false,
                    access_vlan,
                    trunk_vlans,
                    native_vlan,
                    members,
                });
            }
            return Ok(Response::new(pb::SetPortSwitchportResponse {}));
        }

        let (sai_id, current, was_tunnel) = {
            let table = self
                .handle
                .ports
                .read()
                .map_err(|_| Status::internal("port table poisoned"))?;
            let port = table
                .get(&req.name)
                .ok_or_else(|| Status::not_found(format!("no port {:?}", req.name)))?;
            if port.l3.is_some() {
                return Err(Status::failed_precondition(format!(
                    "{} is routed; delete its address before switchport config",
                    req.name
                )));
            }
            (
                port.sai_id,
                port.switchport
                    .as_ref()
                    .map(|sp| sp.members.clone())
                    .unwrap_or_default(),
                port.switchport
                    .as_ref()
                    .map(|sp| sp.dot1q_tunnel)
                    .unwrap_or(false),
            )
        };
        let members = self
            .program_l2(
                &req.name,
                sai_id,
                current,
                trunk,
                access_vlan,
                native_vlan,
                &trunk_vlans,
            )
            .await?;

        // QinQ: a tunnel port carries the 0x88a8 outer TPID so customer
        // 0x8100 tags ride through as payload; leaving tunnel mode
        // restores the default.
        if dot1q_tunnel != was_tunnel {
            self.handle
                .set_port_tpid(sai_id, if dot1q_tunnel { 0x88a8 } else { 0x8100 })
                .await
                .map_err(sai)?;
        }

        let mut table = self
            .handle
            .ports
            .write()
            .map_err(|_| Status::internal("port table poisoned"))?;
        if let Some(port) = table.get_mut(&req.name) {
            port.switchport = Some(SwitchportState {
                trunk,
                dot1q_tunnel,
                access_vlan,
                trunk_vlans,
                native_vlan,
                members,
            });
        }
        drop(table);
        self.reconcile_svi_bridges();
        Ok(Response::new(pb::SetPortSwitchportResponse {}))
    }

    async fn clear_port_switchport(
        &self,
        request: Request<pb::ClearPortSwitchportRequest>,
    ) -> Result<Response<pb::ClearPortSwitchportResponse>, Status> {
        let req = request.into_inner();
        let sai = |e: hemlock_sai::SaiError| Status::internal(format!("SAI: {e}"));
        if let Some(group) = lag_group_of(&req.name) {
            let existing = {
                let lags = self
                    .handle
                    .lags
                    .read()
                    .map_err(|_| Status::internal("lag table poisoned"))?;
                lags.get(&group)
                    .map(|lag| (lag.sai_id, lag.switchport.clone()))
            };
            let Some((sai_id, Some(sp))) = existing else {
                return Ok(Response::new(pb::ClearPortSwitchportResponse {}));
            };
            for (_, member, _) in sp.members {
                self.handle.remove_vlan_member(member).await.map_err(sai)?;
            }
            self.handle
                .restore_port_default_vlan(sai_id)
                .await
                .map_err(sai)?;
            let mut lags = self
                .handle
                .lags
                .write()
                .map_err(|_| Status::internal("lag table poisoned"))?;
            if let Some(lag) = lags.get_mut(&group) {
                lag.switchport = None;
            }
            return Ok(Response::new(pb::ClearPortSwitchportResponse {}));
        }
        let (sai_id, existing) = {
            let table = self
                .handle
                .ports
                .read()
                .map_err(|_| Status::internal("port table poisoned"))?;
            let port = table
                .get(&req.name)
                .ok_or_else(|| Status::not_found(format!("no port {:?}", req.name)))?;
            (port.sai_id, port.switchport.clone())
        };
        let Some(sp) = existing else {
            return Ok(Response::new(pb::ClearPortSwitchportResponse {}));
        };
        for (_, member, _) in sp.members {
            self.handle.remove_vlan_member(member).await.map_err(sai)?;
        }
        if sp.dot1q_tunnel {
            self.handle
                .set_port_tpid(sai_id, 0x8100)
                .await
                .map_err(sai)?;
        }
        self.handle
            .restore_port_default_vlan(sai_id)
            .await
            .map_err(sai)?;
        let mut table = self
            .handle
            .ports
            .write()
            .map_err(|_| Status::internal("port table poisoned"))?;
        if let Some(port) = table.get_mut(&req.name) {
            port.switchport = None;
        }
        drop(table);
        self.reconcile_svi_bridges();
        Ok(Response::new(pb::ClearPortSwitchportResponse {}))
    }

    async fn set_fdb_aging_time(
        &self,
        request: Request<pb::SetFdbAgingTimeRequest>,
    ) -> Result<Response<pb::SetFdbAgingTimeResponse>, Status> {
        self.require_capability(self.handle.capabilities.fdb_aging, "mac-table aging")?;
        let secs = request.into_inner().seconds;
        self.handle
            .set_fdb_aging(secs)
            .await
            .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        if let Ok(mut fdb) = self.handle.fdb.write() {
            fdb.aging_secs = secs;
        }
        Ok(Response::new(pb::SetFdbAgingTimeResponse {}))
    }

    async fn add_static_fdb(
        &self,
        request: Request<pb::AddStaticFdbRequest>,
    ) -> Result<Response<pb::AddStaticFdbResponse>, Status> {
        let req = request.into_inner();
        let (mac, canonical) = Self::mac_bytes(&req.mac)?;
        let vlan = vlan_id(req.vlan).map_err(Status::invalid_argument)?;
        let vlan_ref = self.fdb_vlan_ref(vlan)?;
        let (action, port) = if req.drop {
            (hemlock_sai::FdbAction::Drop, None)
        } else {
            let sai_id = self.port_like_sai_id(&req.port)?;
            (hemlock_sai::FdbAction::Forward(sai_id), Some(req.port))
        };
        self.handle
            .add_fdb_entry(vlan_ref, mac, action)
            .await
            .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        if let Ok(mut fdb) = self.handle.fdb.write() {
            let key = (vlan, canonical);
            fdb.dynamics.remove(&key);
            fdb.statics.insert(key, FdbStaticEntry { port });
        }
        Ok(Response::new(pb::AddStaticFdbResponse {}))
    }

    async fn remove_static_fdb(
        &self,
        request: Request<pb::RemoveStaticFdbRequest>,
    ) -> Result<Response<pb::RemoveStaticFdbResponse>, Status> {
        let req = request.into_inner();
        let (mac, canonical) = Self::mac_bytes(&req.mac)?;
        let vlan = vlan_id(req.vlan).map_err(Status::invalid_argument)?;
        let key = (vlan, canonical);
        let exists = self
            .handle
            .fdb
            .read()
            .map_err(|_| Status::internal("fdb table poisoned"))?
            .statics
            .contains_key(&key);
        if !exists {
            return Ok(Response::new(pb::RemoveStaticFdbResponse {}));
        }
        // A since-removed VLAN took its hardware FDB entries with it;
        // only the shadow entry is left to clean up.
        if let Ok(vlan_ref) = self.fdb_vlan_ref(vlan) {
            self.handle
                .remove_fdb_entry(vlan_ref, mac)
                .await
                .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        }
        if let Ok(mut fdb) = self.handle.fdb.write() {
            fdb.statics.remove(&key);
        }
        Ok(Response::new(pb::RemoveStaticFdbResponse {}))
    }

    async fn flush_fdb(
        &self,
        request: Request<pb::FlushFdbRequest>,
    ) -> Result<Response<pb::FlushFdbResponse>, Status> {
        self.require_capability(self.handle.capabilities.fdb_flush, "mac-table flush")?;
        let req = request.into_inner();
        let vlan = match req.vlan {
            0 => None,
            raw => Some(vlan_id(raw).map_err(Status::invalid_argument)?),
        };
        let vlan_ref = match vlan {
            None => None,
            Some(1) => Some(hemlock_sai::Oid(self.handle.default_vlan_oid)),
            Some(id) => self.fdb_vlan_ref(id)?,
        };
        let port_ref = match req.port.as_str() {
            "" => None,
            name => Some(self.port_sai_id(name)?),
        };
        self.handle
            .flush_fdb(vlan_ref, port_ref)
            .await
            .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        // Drop the matching dynamics from the software mirror and tell
        // the watchers (backends may also send FLUSHED events; removal
        // is idempotent).
        let flushed: Vec<(u16, String, String)> = {
            let mut fdb = self
                .handle
                .fdb
                .write()
                .map_err(|_| Status::internal("fdb table poisoned"))?;
            let matching: Vec<(u16, String)> = fdb
                .dynamics
                .iter()
                .filter(|((v, _), entry)| {
                    vlan.is_none_or(|wanted| *v == wanted)
                        && (req.port.is_empty() || entry.port == req.port)
                })
                .map(|(key, _)| key.clone())
                .collect();
            matching
                .into_iter()
                .filter_map(|key| {
                    fdb.dynamics
                        .remove(&key)
                        .map(|entry| (key.0, key.1, entry.port))
                })
                .collect()
        };
        for (vlan, mac, port) in &flushed {
            let _ = self.handle.fdb_events.send(FdbNotify {
                kind: hemlock_sai::FdbEventKind::Flushed,
                vlan: *vlan,
                mac: mac.clone(),
                port: Some(port.clone()),
            });
        }
        Ok(Response::new(pb::FlushFdbResponse {
            flushed: flushed.len() as u32,
        }))
    }

    async fn dump_fdb(
        &self,
        request: Request<pb::DumpFdbRequest>,
    ) -> Result<Response<pb::DumpFdbResponse>, Status> {
        let req = request.into_inner();
        let mac_filter = if req.mac.is_empty() {
            None
        } else {
            Some(hemlock_common::net::parse_mac(&req.mac).map_err(Status::invalid_argument)?)
        };
        let fdb = self
            .handle
            .fdb
            .read()
            .map_err(|_| Status::internal("fdb table poisoned"))?;
        let wanted = |vlan: u16, mac: &str, port: Option<&str>, is_static: bool| {
            (req.vlan == 0 || u32::from(vlan) == req.vlan)
                && (req.port.is_empty() || port == Some(req.port.as_str()))
                && mac_filter.as_deref().is_none_or(|m| m == mac)
                && match req.kind {
                    k if k == pb::FdbEntryKind::Static as i32 => is_static,
                    k if k == pb::FdbEntryKind::Dynamic as i32 => !is_static,
                    _ => true,
                }
        };
        // Dynamics first, then statics, each in (vlan, mac) order — the
        // order `show mac address-table` prints.
        let now = Instant::now();
        let mut entries: Vec<pb::FdbEntryState> = Vec::new();
        for ((vlan, mac), entry) in &fdb.dynamics {
            if wanted(*vlan, mac, Some(entry.port.as_str()), false) {
                entries.push(pb::FdbEntryState {
                    vlan: u32::from(*vlan),
                    mac: mac.clone(),
                    port: entry.port.clone(),
                    drop: false,
                    is_static: false,
                    moves: entry.moves,
                    seconds_since_move: entry
                        .last_move
                        .map(|at| now.saturating_duration_since(at).as_secs()),
                });
            }
        }
        for ((vlan, mac), entry) in &fdb.statics {
            if wanted(*vlan, mac, entry.port.as_deref(), true) {
                entries.push(pb::FdbEntryState {
                    vlan: u32::from(*vlan),
                    mac: mac.clone(),
                    port: entry.port.clone().unwrap_or_default(),
                    drop: entry.port.is_none(),
                    is_static: true,
                    moves: 0,
                    seconds_since_move: None,
                });
            }
        }
        let total = entries.len() as u32;
        let offset: usize = req.page_token.parse().unwrap_or(0);
        let mut next_page_token = String::new();
        let entries = if req.page_size > 0 {
            let end = offset
                .saturating_add(req.page_size as usize)
                .min(entries.len());
            if end < entries.len() {
                next_page_token = end.to_string();
            }
            entries
                .get(offset..end)
                .map(<[pb::FdbEntryState]>::to_vec)
                .unwrap_or_default()
        } else {
            entries
        };
        Ok(Response::new(pb::DumpFdbResponse {
            entries,
            next_page_token,
            total,
            aging_time_secs: fdb.aging_secs,
        }))
    }

    type WatchFdbEventsStream = std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<pb::FdbEventMessage, Status>> + Send>,
    >;

    async fn watch_fdb_events(
        &self,
        _request: Request<pb::WatchFdbEventsRequest>,
    ) -> Result<Response<Self::WatchFdbEventsStream>, Status> {
        let stream =
            BroadcastStream::new(self.handle.fdb_events.subscribe()).filter_map(
                |item| match item {
                    Ok(event) => Some(Ok(pb::FdbEventMessage {
                        kind: match event.kind {
                            hemlock_sai::FdbEventKind::Learned => {
                                pb::fdb_event_message::Kind::Learned
                            }
                            hemlock_sai::FdbEventKind::Aged => pb::fdb_event_message::Kind::Aged,
                            hemlock_sai::FdbEventKind::Moved => pb::fdb_event_message::Kind::Moved,
                            hemlock_sai::FdbEventKind::Flushed => {
                                pb::fdb_event_message::Kind::Flushed
                            }
                        } as i32,
                        vlan: u32::from(event.vlan),
                        mac: event.mac,
                        port: event.port.unwrap_or_default(),
                    })),
                    Err(BroadcastStreamRecvError::Lagged(_)) => None,
                },
            );
        Ok(Response::new(Box::pin(stream)))
    }

    async fn set_port_storm_control(
        &self,
        request: Request<pb::SetPortStormControlRequest>,
    ) -> Result<Response<pb::SetPortStormControlResponse>, Status> {
        self.require_capability(self.handle.capabilities.storm_control, "storm-control")?;
        let req = request.into_inner();
        let class = match pb::StormClass::try_from(req.class) {
            Ok(pb::StormClass::Broadcast) => hemlock_sai::StormClass::Broadcast,
            Ok(pb::StormClass::Multicast) => hemlock_sai::StormClass::Multicast,
            Ok(pb::StormClass::UnknownUnicast) => hemlock_sai::StormClass::UnknownUnicast,
            _ => return Err(Status::invalid_argument("unspecified storm class")),
        };
        // A port-channel's levels apply per member (each metered at the
        // level's share of its own speed; the aggregate rate is their
        // sum).
        if let Some(group) = lag_group_of(&req.name) {
            let lag = {
                let lags = self
                    .handle
                    .lags
                    .read()
                    .map_err(|_| Status::internal("lag table poisoned"))?;
                lags.get(&group).cloned().ok_or_else(|| {
                    Status::failed_precondition(format!("Port-Channel{group} not created"))
                })?
            };
            let level = match &req.level {
                Some(level) => Some(
                    hemlock_common::net::parse_storm_level(level)
                        .map_err(Status::invalid_argument)?,
                ),
                None => None,
            };
            for name in lag.members.keys() {
                let (port_id, speed) = {
                    let table = self
                        .handle
                        .ports
                        .read()
                        .map_err(|_| Status::internal("port table poisoned"))?;
                    let port = table
                        .get(name)
                        .ok_or_else(|| Status::not_found(format!("no port {name:?}")))?;
                    (port.sai_id, port.def.speed_mbps)
                };
                let kbps = match &level {
                    Some(level) => Some(Self::storm_kbps(speed, Self::level_hundredths(level)?)),
                    None => None,
                };
                self.handle
                    .set_port_storm(port_id, class, kbps)
                    .await
                    .map_err(|e| Status::internal(format!("SAI: {e}")))?;
            }
            let mut lags = self
                .handle
                .lags
                .write()
                .map_err(|_| Status::internal("lag table poisoned"))?;
            if let Some(lag) = lags.get_mut(&group) {
                match level {
                    Some(level) => {
                        lag.storm.insert(class, StormState { level, kbps: 0 });
                    }
                    None => {
                        lag.storm.remove(&class);
                    }
                }
            }
            return Ok(Response::new(pb::SetPortStormControlResponse {}));
        }

        let (sai_id, speed_mbps) = {
            let table = self
                .handle
                .ports
                .read()
                .map_err(|_| Status::internal("port table poisoned"))?;
            let port = table
                .get(&req.name)
                .ok_or_else(|| Status::not_found(format!("no port {:?}", req.name)))?;
            (port.sai_id, port.def.speed_mbps)
        };
        let program = match &req.level {
            Some(level) => {
                let hundredths = Self::level_hundredths(level)?;
                let kbps = Self::storm_kbps(speed_mbps, hundredths);
                Some((
                    hemlock_common::net::parse_storm_level(level)
                        .map_err(Status::invalid_argument)?,
                    kbps,
                ))
            }
            None => None,
        };
        self.handle
            .set_port_storm(sai_id, class, program.as_ref().map(|(_, kbps)| *kbps))
            .await
            .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        let mut table = self
            .handle
            .ports
            .write()
            .map_err(|_| Status::internal("port table poisoned"))?;
        if let Some(port) = table.get_mut(&req.name) {
            match program {
                Some((level, kbps)) => {
                    port.storm.insert(class, StormState { level, kbps });
                }
                None => {
                    port.storm.remove(&class);
                }
            }
        }
        Ok(Response::new(pb::SetPortStormControlResponse {}))
    }

    async fn get_storm_control(
        &self,
        _request: Request<pb::GetStormControlRequest>,
    ) -> Result<Response<pb::GetStormControlResponse>, Status> {
        let rows: Vec<(
            String,
            hemlock_sai::PortId,
            hemlock_sai::StormClass,
            StormState,
            bool,
        )> = {
            let table = self
                .handle
                .ports
                .read()
                .map_err(|_| Status::internal("port table poisoned"))?;
            let mut ports: Vec<&PortState> = table.values().collect();
            ports.sort_by_key(|p| p.def.index);
            ports
                .iter()
                .flat_map(|p| {
                    p.storm.iter().map(|(class, state)| {
                        (
                            p.def.name.clone(),
                            p.sai_id,
                            *class,
                            state.clone(),
                            p.oper_up,
                        )
                    })
                })
                .collect()
        };
        let mut entries = Vec::with_capacity(rows.len());
        for (name, sai_id, class, state, active) in rows {
            let drops = self
                .handle
                .port_storm_drops(sai_id, class)
                .await
                .unwrap_or(0);
            entries.push(pb::StormControlEntry {
                name,
                class: storm_class_proto(class) as i32,
                level: state.level,
                rate_kbps: state.kbps,
                drops,
                active,
            });
        }
        // Port-channel rows aggregate over their members: the rate is
        // the sum of the member rates, the drops the sum of the member
        // policers' drops.
        let lag_rows: Vec<(String, hemlock_sai::StormClass, String, Vec<String>)> = {
            let lags = self
                .handle
                .lags
                .read()
                .map_err(|_| Status::internal("lag table poisoned"))?;
            lags.values()
                .flat_map(|lag| {
                    lag.storm.iter().map(|(class, state)| {
                        (
                            format!("Port-Channel{}", lag.group),
                            *class,
                            state.level.clone(),
                            lag.members.keys().cloned().collect(),
                        )
                    })
                })
                .collect()
        };
        for (name, class, level, members) in lag_rows {
            let hundredths = Self::level_hundredths(&level)?;
            let mut rate_kbps = 0;
            let mut drops = 0;
            let mut active = false;
            for member in &members {
                let (sai_id, speed, up) = {
                    let table = self
                        .handle
                        .ports
                        .read()
                        .map_err(|_| Status::internal("port table poisoned"))?;
                    match table.get(member) {
                        Some(port) => (port.sai_id, port.def.speed_mbps, port.oper_up),
                        None => continue,
                    }
                };
                rate_kbps += Self::storm_kbps(speed, hundredths);
                drops += self
                    .handle
                    .port_storm_drops(sai_id, class)
                    .await
                    .unwrap_or(0);
                active |= up;
            }
            entries.push(pb::StormControlEntry {
                name,
                class: storm_class_proto(class) as i32,
                level,
                rate_kbps,
                drops,
                active,
            });
        }
        Ok(Response::new(pb::GetStormControlResponse { entries }))
    }

    async fn create_mirror_session(
        &self,
        request: Request<pb::CreateMirrorSessionRequest>,
    ) -> Result<Response<pb::CreateMirrorSessionResponse>, Status> {
        self.require_capability(self.handle.capabilities.mirror, "mirror")?;
        let req = request.into_inner();
        if req.session == 0 || req.session > self.handle.capabilities.mirror_sessions_max {
            return Err(Status::invalid_argument(format!(
                "mirror session {} out of range (1..{})",
                req.session, self.handle.capabilities.mirror_sessions_max
            )));
        }
        let monitor = self.port_sai_id(&req.destination)?;
        let existing = {
            let mirrors = self
                .handle
                .mirrors
                .read()
                .map_err(|_| Status::internal("mirror table poisoned"))?;
            mirrors.get(&req.session).cloned()
        };
        if let Some(state) = &existing {
            if state.destination == req.destination {
                return Ok(Response::new(pb::CreateMirrorSessionResponse {}));
            }
            // Destination change: detach sources, swap the session
            // object, reattach.
            let sources: Vec<String> = state.sources.keys().cloned().collect();
            {
                let mut mirrors = self
                    .handle
                    .mirrors
                    .write()
                    .map_err(|_| Status::internal("mirror table poisoned"))?;
                if let Some(state) = mirrors.get_mut(&req.session) {
                    state.sources.clear();
                }
            }
            for source in &sources {
                self.reprogram_port_mirror(source).await?;
            }
            self.handle
                .remove_mirror_session(state.oid)
                .await
                .map_err(|e| Status::internal(format!("SAI: {e}")))?;
            let oid = self
                .handle
                .create_mirror_session(monitor)
                .await
                .map_err(|e| Status::internal(format!("SAI: {e}")))?;
            {
                let mut mirrors = self
                    .handle
                    .mirrors
                    .write()
                    .map_err(|_| Status::internal("mirror table poisoned"))?;
                mirrors.insert(
                    req.session,
                    MirrorState {
                        destination: req.destination,
                        oid,
                        sources: state.sources.clone(),
                    },
                );
            }
            for source in &sources {
                self.reprogram_port_mirror(source).await?;
            }
            return Ok(Response::new(pb::CreateMirrorSessionResponse {}));
        }
        let oid = self
            .handle
            .create_mirror_session(monitor)
            .await
            .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        let mut mirrors = self
            .handle
            .mirrors
            .write()
            .map_err(|_| Status::internal("mirror table poisoned"))?;
        mirrors.insert(
            req.session,
            MirrorState {
                destination: req.destination,
                oid,
                sources: std::collections::BTreeMap::new(),
            },
        );
        Ok(Response::new(pb::CreateMirrorSessionResponse {}))
    }

    async fn remove_mirror_session(
        &self,
        request: Request<pb::RemoveMirrorSessionRequest>,
    ) -> Result<Response<pb::RemoveMirrorSessionResponse>, Status> {
        let req = request.into_inner();
        let existing = {
            let mut mirrors = self
                .handle
                .mirrors
                .write()
                .map_err(|_| Status::internal("mirror table poisoned"))?;
            mirrors.remove(&req.session)
        };
        let Some(state) = existing else {
            return Ok(Response::new(pb::RemoveMirrorSessionResponse {}));
        };
        for source in state.sources.keys() {
            self.reprogram_port_mirror(source).await?;
        }
        self.handle
            .remove_mirror_session(state.oid)
            .await
            .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        Ok(Response::new(pb::RemoveMirrorSessionResponse {}))
    }

    async fn set_port_mirror(
        &self,
        request: Request<pb::SetPortMirrorRequest>,
    ) -> Result<Response<pb::SetPortMirrorResponse>, Status> {
        self.require_capability(self.handle.capabilities.mirror, "mirror")?;
        let req = request.into_inner();
        self.port_sai_id(&req.name)?;
        let direction = match pb::MirrorDirection::try_from(req.direction) {
            Ok(pb::MirrorDirection::None) => None,
            Ok(pb::MirrorDirection::Rx) => Some(MirrorDir::Rx),
            Ok(pb::MirrorDirection::Tx) => Some(MirrorDir::Tx),
            Ok(pb::MirrorDirection::Both) => Some(MirrorDir::Both),
            _ => return Err(Status::invalid_argument("unspecified mirror direction")),
        };
        {
            let mut mirrors = self
                .handle
                .mirrors
                .write()
                .map_err(|_| Status::internal("mirror table poisoned"))?;
            let session = mirrors.get_mut(&req.session).ok_or_else(|| {
                Status::failed_precondition(format!("mirror session {} not created", req.session))
            })?;
            match direction {
                Some(direction) => {
                    session.sources.insert(req.name.clone(), direction);
                }
                None => {
                    session.sources.remove(&req.name);
                }
            }
        }
        self.reprogram_port_mirror(&req.name).await?;
        Ok(Response::new(pb::SetPortMirrorResponse {}))
    }

    async fn get_mirror_sessions(
        &self,
        _request: Request<pb::GetMirrorSessionsRequest>,
    ) -> Result<Response<pb::GetMirrorSessionsResponse>, Status> {
        let mirrors = self
            .handle
            .mirrors
            .read()
            .map_err(|_| Status::internal("mirror table poisoned"))?;
        let ports = self
            .handle
            .ports
            .read()
            .map_err(|_| Status::internal("port table poisoned"))?;
        let sessions = mirrors
            .iter()
            .map(|(session, state)| pb::MirrorSessionState {
                session: *session,
                destination: state.destination.clone(),
                destination_up: ports
                    .get(&state.destination)
                    .map(|p| p.oper_up)
                    .unwrap_or(false),
                sources: state
                    .sources
                    .iter()
                    .map(|(name, direction)| pb::MirrorSourceState {
                        name: name.clone(),
                        direction: match direction {
                            MirrorDir::Rx => pb::MirrorDirection::Rx,
                            MirrorDir::Tx => pb::MirrorDirection::Tx,
                            MirrorDir::Both => pb::MirrorDirection::Both,
                        } as i32,
                    })
                    .collect(),
            })
            .collect();
        Ok(Response::new(pb::GetMirrorSessionsResponse { sessions }))
    }

    async fn create_lag(
        &self,
        request: Request<pb::CreateLagRequest>,
    ) -> Result<Response<pb::CreateLagResponse>, Status> {
        self.require_capability(self.handle.capabilities.lag, "port-channel")?;
        let req = request.into_inner();
        let group = u16::try_from(req.group)
            .ok()
            .filter(|g| (1..=64).contains(g))
            .ok_or_else(|| Status::invalid_argument(format!("bad group {}", req.group)))?;
        let exists = self
            .handle
            .lags
            .read()
            .map_err(|_| Status::internal("lag table poisoned"))?
            .contains_key(&group);
        let sai_id = if exists {
            None
        } else {
            Some(
                self.handle
                    .create_lag()
                    .await
                    .map_err(|e| Status::internal(format!("SAI: {e}")))?,
            )
        };
        let mut lags = self
            .handle
            .lags
            .write()
            .map_err(|_| Status::internal("lag table poisoned"))?;
        match lags.get_mut(&group) {
            Some(lag) => {
                lag.description = req.description;
                lag.admin_up = req.admin_up;
            }
            None => {
                lags.insert(
                    group,
                    crate::state::LagState {
                        group,
                        sai_id: sai_id.unwrap_or(hemlock_sai::PortId(0)),
                        description: req.description,
                        admin_up: req.admin_up,
                        members: std::collections::BTreeMap::new(),
                        switchport: None,
                        storm: std::collections::BTreeMap::new(),
                    },
                );
            }
        }
        Ok(Response::new(pb::CreateLagResponse {}))
    }

    async fn remove_lag(
        &self,
        request: Request<pb::RemoveLagRequest>,
    ) -> Result<Response<pb::RemoveLagResponse>, Status> {
        let req = request.into_inner();
        let group = u16::try_from(req.group)
            .ok()
            .filter(|g| (1..=64).contains(g))
            .ok_or_else(|| Status::invalid_argument(format!("bad group {}", req.group)))?;
        let sai = |e: hemlock_sai::SaiError| Status::internal(format!("SAI: {e}"));
        let existing = {
            let lags = self
                .handle
                .lags
                .read()
                .map_err(|_| Status::internal("lag table poisoned"))?;
            lags.get(&group).cloned()
        };
        let Some(lag) = existing else {
            return Ok(Response::new(pb::RemoveLagResponse {}));
        };
        // Member storm policers, switchport memberships, then members,
        // then the LAG object.
        for class in lag.storm.keys() {
            for name in lag.members.keys() {
                let port_id = self.port_sai_id(name)?;
                self.handle
                    .set_port_storm(port_id, *class, None)
                    .await
                    .map_err(sai)?;
            }
        }
        if let Some(sp) = &lag.switchport {
            for (_, member, _) in &sp.members {
                self.handle.remove_vlan_member(*member).await.map_err(sai)?;
            }
            self.handle
                .restore_port_default_vlan(lag.sai_id)
                .await
                .map_err(sai)?;
        }
        for (port, member) in &lag.members {
            let port_id = self.port_sai_id(port)?;
            self.handle
                .remove_lag_member(member.oid, port_id)
                .await
                .map_err(sai)?;
        }
        self.handle.remove_lag(lag.sai_id).await.map_err(sai)?;
        self.handle
            .lags
            .write()
            .map_err(|_| Status::internal("lag table poisoned"))?
            .remove(&group);
        Ok(Response::new(pb::RemoveLagResponse {}))
    }

    async fn set_lag_members(
        &self,
        request: Request<pb::SetLagMembersRequest>,
    ) -> Result<Response<pb::SetLagMembersResponse>, Status> {
        let req = request.into_inner();
        let group = u16::try_from(req.group)
            .ok()
            .filter(|g| (1..=64).contains(g))
            .ok_or_else(|| Status::invalid_argument(format!("bad group {}", req.group)))?;
        let sai = |e: hemlock_sai::SaiError| Status::internal(format!("SAI: {e}"));
        let lag = {
            let lags = self
                .handle
                .lags
                .read()
                .map_err(|_| Status::internal("lag table poisoned"))?;
            lags.get(&group).cloned().ok_or_else(|| {
                Status::failed_precondition(format!("Port-Channel{group} not created"))
            })?
        };
        let wanted: std::collections::BTreeMap<String, bool> = req
            .members
            .iter()
            .map(|m| (m.port.clone(), m.enabled))
            .collect();

        let mut members = lag.members.clone();
        // Remove stale members (they return to standalone default L2).
        let stale: Vec<String> = members
            .keys()
            .filter(|name| !wanted.contains_key(*name))
            .cloned()
            .collect();
        let stale_for_acls = stale.clone();
        for name in stale {
            let port_id = self.port_sai_id(&name)?;
            let member = members.remove(&name).expect("key from members");
            // The LAG's storm levels leave with the member.
            for class in lag.storm.keys() {
                self.handle
                    .set_port_storm(port_id, *class, None)
                    .await
                    .map_err(sai)?;
            }
            self.handle
                .remove_lag_member(member.oid, port_id)
                .await
                .map_err(sai)?;
        }
        // Add missing members and settle the gates.
        for (name, enabled) in &wanted {
            match members.get_mut(name) {
                Some(member) => {
                    if member.enabled != *enabled {
                        self.handle
                            .set_lag_member_state(member.oid, *enabled)
                            .await
                            .map_err(sai)?;
                        member.enabled = *enabled;
                    }
                }
                None => {
                    let port_id = self.port_sai_id(name)?;
                    {
                        let table = self
                            .handle
                            .ports
                            .read()
                            .map_err(|_| Status::internal("port table poisoned"))?;
                        if let Some(port) = table.get(name) {
                            if port.l3.is_some() {
                                return Err(Status::failed_precondition(format!(
                                    "{name} is routed; delete its address before channel-group"
                                )));
                            }
                        }
                    }
                    let oid = self
                        .handle
                        .add_lag_member(lag.sai_id, port_id)
                        .await
                        .map_err(sai)?;
                    if *enabled {
                        self.handle
                            .set_lag_member_state(oid, true)
                            .await
                            .map_err(sai)?;
                    }
                    members.insert(
                        name.clone(),
                        crate::state::LagMemberState {
                            oid,
                            enabled: *enabled,
                        },
                    );
                }
            }
        }
        // Membership changed: re-derive the LAG's per-member storm
        // policers so every member carries the levels.
        let storm = lag.storm.clone();
        for (class, state) in &storm {
            for name in wanted.keys() {
                let (port_id, speed) = {
                    let table = self
                        .handle
                        .ports
                        .read()
                        .map_err(|_| Status::internal("port table poisoned"))?;
                    let port = table
                        .get(name)
                        .ok_or_else(|| Status::not_found(format!("no port {name:?}")))?;
                    (port.sai_id, port.def.speed_mbps)
                };
                let hundredths = Self::level_hundredths(&state.level)?;
                self.handle
                    .set_port_storm(port_id, *class, Some(Self::storm_kbps(speed, hundredths)))
                    .await
                    .map_err(sai)?;
            }
        }
        {
            let mut lags = self
                .handle
                .lags
                .write()
                .map_err(|_| Status::internal("lag table poisoned"))?;
            if let Some(lag) = lags.get_mut(&group) {
                lag.members = members;
            }
        }
        // Membership changed: re-expand the Port-Channel's ACL bindings
        // (stale members drop the entries, current members carry them).
        let current: Vec<String> = wanted.keys().cloned().collect();
        self.refresh_lag_acls(group, &stale_for_acls, &current)
            .await?;
        self.refresh_lag_qos(group, &stale_for_acls, &current)
            .await?;
        Ok(Response::new(pb::SetLagMembersResponse {}))
    }

    async fn get_lags(
        &self,
        _request: Request<pb::GetLagsRequest>,
    ) -> Result<Response<pb::GetLagsResponse>, Status> {
        let lags = self
            .handle
            .lags
            .read()
            .map_err(|_| Status::internal("lag table poisoned"))?;
        let ports = self
            .handle
            .ports
            .read()
            .map_err(|_| Status::internal("port table poisoned"))?;
        let lags = lags
            .values()
            .map(|lag| pb::LagState {
                group: u32::from(lag.group),
                description: lag.description.clone(),
                admin_up: lag.admin_up,
                members: lag
                    .members
                    .iter()
                    .map(|(name, member)| pb::LagMemberState {
                        port: name.clone(),
                        enabled: member.enabled,
                        oper_up: ports.get(name).map(|p| p.oper_up).unwrap_or(false),
                    })
                    .collect(),
            })
            .collect();
        Ok(Response::new(pb::GetLagsResponse { lags }))
    }

    async fn create_stp_instance(
        &self,
        request: Request<pb::CreateStpInstanceRequest>,
    ) -> Result<Response<pb::CreateStpInstanceResponse>, Status> {
        self.require_capability(self.handle.capabilities.stp, "spanning-tree")?;
        let req = request.into_inner();
        let instance = u8::try_from(req.instance)
            .ok()
            .filter(|i| (1..=15).contains(i))
            .ok_or_else(|| Status::invalid_argument(format!("bad instance {}", req.instance)))?;
        let exists = self
            .handle
            .stps
            .read()
            .map_err(|_| Status::internal("stp table poisoned"))?
            .contains_key(&instance);
        if exists {
            return Ok(Response::new(pb::CreateStpInstanceResponse {}));
        }
        let oid = self
            .handle
            .create_stp_instance()
            .await
            .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        self.handle
            .stps
            .write()
            .map_err(|_| Status::internal("stp table poisoned"))?
            .insert(
                instance,
                crate::state::StpInstanceState {
                    oid,
                    vlans: Vec::new(),
                },
            );
        Ok(Response::new(pb::CreateStpInstanceResponse {}))
    }

    async fn remove_stp_instance(
        &self,
        request: Request<pb::RemoveStpInstanceRequest>,
    ) -> Result<Response<pb::RemoveStpInstanceResponse>, Status> {
        let req = request.into_inner();
        let instance = u8::try_from(req.instance)
            .ok()
            .filter(|i| (1..=15).contains(i))
            .ok_or_else(|| Status::invalid_argument(format!("bad instance {}", req.instance)))?;
        let existing = {
            let stps = self
                .handle
                .stps
                .read()
                .map_err(|_| Status::internal("stp table poisoned"))?;
            stps.get(&instance).cloned()
        };
        let Some(state) = existing else {
            return Ok(Response::new(pb::RemoveStpInstanceResponse {}));
        };
        // Its VLANs move back to the default instance first.
        for vlan in &state.vlans {
            let vlan_ref = self.fdb_vlan_ref(*vlan).ok().flatten();
            self.handle
                .set_vlan_stp_instance(vlan_ref, None)
                .await
                .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        }
        self.handle
            .remove_stp_instance(state.oid)
            .await
            .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        self.handle
            .stps
            .write()
            .map_err(|_| Status::internal("stp table poisoned"))?
            .remove(&instance);
        Ok(Response::new(pb::RemoveStpInstanceResponse {}))
    }

    async fn set_stp_instance_vlans(
        &self,
        request: Request<pb::SetStpInstanceVlansRequest>,
    ) -> Result<Response<pb::SetStpInstanceVlansResponse>, Status> {
        let req = request.into_inner();
        let instance = u8::try_from(req.instance)
            .ok()
            .filter(|i| (1..=15).contains(i))
            .ok_or_else(|| Status::invalid_argument(format!("bad instance {}", req.instance)))?;
        let mut wanted = Vec::with_capacity(req.vlans.len());
        for raw in &req.vlans {
            wanted.push(vlan_id(*raw).map_err(Status::invalid_argument)?);
        }
        wanted.sort_unstable();
        wanted.dedup();
        let state = {
            let stps = self
                .handle
                .stps
                .read()
                .map_err(|_| Status::internal("stp table poisoned"))?;
            stps.get(&instance).cloned().ok_or_else(|| {
                Status::failed_precondition(format!("stp instance {instance} not created"))
            })?
        };
        // Dropped VLANs return to the default instance; added ones move
        // in (VLANs not yet created attach when they appear and the
        // mapping is reprogrammed).
        for vlan in &state.vlans {
            if !wanted.contains(vlan) {
                if let Ok(vlan_ref) = self.fdb_vlan_ref(*vlan) {
                    self.handle
                        .set_vlan_stp_instance(vlan_ref, None)
                        .await
                        .map_err(|e| Status::internal(format!("SAI: {e}")))?;
                }
            }
        }
        let mut applied = Vec::with_capacity(wanted.len());
        for vlan in &wanted {
            match self.fdb_vlan_ref(*vlan) {
                Ok(vlan_ref) => {
                    self.handle
                        .set_vlan_stp_instance(vlan_ref, Some(state.oid))
                        .await
                        .map_err(|e| Status::internal(format!("SAI: {e}")))?;
                    applied.push(*vlan);
                }
                Err(_) => {
                    tracing::warn!(
                        instance,
                        vlan,
                        "mst instance references an undefined VLAN; mapping deferred"
                    );
                    applied.push(*vlan);
                }
            }
        }
        if let Ok(mut stps) = self.handle.stps.write() {
            if let Some(state) = stps.get_mut(&instance) {
                state.vlans = applied;
            }
        }
        Ok(Response::new(pb::SetStpInstanceVlansResponse {}))
    }

    async fn set_port_stp_state(
        &self,
        request: Request<pb::SetPortStpStateRequest>,
    ) -> Result<Response<pb::SetPortStpStateResponse>, Status> {
        self.require_capability(self.handle.capabilities.stp, "spanning-tree")?;
        let req = request.into_inner();
        let state = match pb::StpState::try_from(req.state) {
            Ok(pb::StpState::Blocking) => hemlock_sai::StpPortState::Blocking,
            Ok(pb::StpState::Learning) => hemlock_sai::StpPortState::Learning,
            Ok(pb::StpState::Forwarding) => hemlock_sai::StpPortState::Forwarding,
            _ => return Err(Status::invalid_argument("unspecified stp state")),
        };
        let sai_id = self.port_like_sai_id(&req.name)?;
        let instances: Vec<Option<hemlock_sai::Oid>> = if req.instance == 0 {
            // The default instance plus every created MST instance (one
            // shared state machine drives them all).
            let stps = self
                .handle
                .stps
                .read()
                .map_err(|_| Status::internal("stp table poisoned"))?;
            std::iter::once(None)
                .chain(stps.values().map(|s| Some(s.oid)))
                .collect()
        } else {
            let instance = u8::try_from(req.instance)
                .ok()
                .filter(|i| (1..=15).contains(i))
                .ok_or_else(|| {
                    Status::invalid_argument(format!("bad instance {}", req.instance))
                })?;
            let stps = self
                .handle
                .stps
                .read()
                .map_err(|_| Status::internal("stp table poisoned"))?;
            vec![Some(
                stps.get(&instance)
                    .ok_or_else(|| {
                        Status::failed_precondition(format!("stp instance {instance} not created"))
                    })?
                    .oid,
            )]
        };
        for stp in instances {
            self.handle
                .set_stp_port_state(stp, sai_id, state)
                .await
                .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        }
        Ok(Response::new(pb::SetPortStpStateResponse {}))
    }

    async fn set_port_errdisable(
        &self,
        request: Request<pb::SetPortErrdisableRequest>,
    ) -> Result<Response<pb::SetPortErrdisableResponse>, Status> {
        let req = request.into_inner();
        let sai_id = self.port_sai_id(&req.name)?;
        let disable = !req.reason.is_empty();
        self.handle
            .set_admin_state(sai_id, !disable)
            .await
            .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        let mut table = self
            .handle
            .ports
            .write()
            .map_err(|_| Status::internal("port table poisoned"))?;
        if let Some(port) = table.get_mut(&req.name) {
            port.admin_up = !disable;
            port.errdisable_reason = disable.then(|| req.reason.clone());
        }
        Ok(Response::new(pb::SetPortErrdisableResponse {}))
    }

    async fn ensure_l2mc_group(
        &self,
        request: Request<pb::EnsureL2mcGroupRequest>,
    ) -> Result<Response<pb::EnsureL2mcGroupResponse>, Status> {
        self.require_capability(self.handle.capabilities.l2mc, "igmp-snooping")?;
        let req = request.into_inner();
        let (vlan, group_ip) = Self::l2mc_key(req.vlan, &req.group)?;
        let key = (vlan, group_ip.to_string());
        let exists = self
            .handle
            .l2mc
            .read()
            .map_err(|_| Status::internal("l2mc table poisoned"))?
            .contains_key(&key);
        if exists {
            return Ok(Response::new(pb::EnsureL2mcGroupResponse {}));
        }
        let vlan_ref = self.fdb_vlan_ref(vlan)?;
        let sai = |e: hemlock_sai::SaiError| Status::internal(format!("SAI: {e}"));
        let oid = self.handle.create_l2mc_group().await.map_err(sai)?;
        if let Err(err) = self
            .handle
            .set_l2mc_entry(vlan_ref, group_ip, Some(oid))
            .await
        {
            let _ = self.handle.remove_l2mc_group(oid).await;
            return Err(sai(err));
        }
        self.handle
            .l2mc
            .write()
            .map_err(|_| Status::internal("l2mc table poisoned"))?
            .insert(
                key,
                crate::state::L2mcGroupState {
                    oid,
                    members: std::collections::BTreeMap::new(),
                },
            );
        Ok(Response::new(pb::EnsureL2mcGroupResponse {}))
    }

    async fn set_l2mc_members(
        &self,
        request: Request<pb::SetL2mcMembersRequest>,
    ) -> Result<Response<pb::SetL2mcMembersResponse>, Status> {
        let req = request.into_inner();
        let (vlan, group_ip) = Self::l2mc_key(req.vlan, &req.group)?;
        let key = (vlan, group_ip.to_string());
        let state = {
            let l2mc = self
                .handle
                .l2mc
                .read()
                .map_err(|_| Status::internal("l2mc table poisoned"))?;
            l2mc.get(&key).cloned().ok_or_else(|| {
                Status::failed_precondition(format!("no L2MC group for {group_ip} in VLAN {vlan}"))
            })?
        };
        let members = self.reconcile_l2mc_members(state, &req.ports).await?;
        if let Ok(mut l2mc) = self.handle.l2mc.write() {
            if let Some(state) = l2mc.get_mut(&key) {
                state.members = members;
            }
        }
        Ok(Response::new(pb::SetL2mcMembersResponse {}))
    }

    async fn remove_l2mc_group(
        &self,
        request: Request<pb::RemoveL2mcGroupRequest>,
    ) -> Result<Response<pb::RemoveL2mcGroupResponse>, Status> {
        let req = request.into_inner();
        let (vlan, group_ip) = Self::l2mc_key(req.vlan, &req.group)?;
        let key = (vlan, group_ip.to_string());
        let existing = {
            let mut l2mc = self
                .handle
                .l2mc
                .write()
                .map_err(|_| Status::internal("l2mc table poisoned"))?;
            l2mc.remove(&key)
        };
        let Some(state) = existing else {
            return Ok(Response::new(pb::RemoveL2mcGroupResponse {}));
        };
        let sai = |e: hemlock_sai::SaiError| Status::internal(format!("SAI: {e}"));
        // Entry first (it references the group), then members, then the
        // group itself.
        let vlan_ref = self.fdb_vlan_ref(vlan).ok().flatten();
        let _ = self.handle.set_l2mc_entry(vlan_ref, group_ip, None).await;
        for member in state.members.values() {
            self.handle.remove_l2mc_member(*member).await.map_err(sai)?;
        }
        self.handle
            .remove_l2mc_group(state.oid)
            .await
            .map_err(sai)?;
        Ok(Response::new(pb::RemoveL2mcGroupResponse {}))
    }

    async fn set_vlan_unknown_mcast(
        &self,
        request: Request<pb::SetVlanUnknownMcastRequest>,
    ) -> Result<Response<pb::SetVlanUnknownMcastResponse>, Status> {
        self.require_capability(self.handle.capabilities.l2mc, "igmp-snooping")?;
        let req = request.into_inner();
        let vlan = vlan_id(req.vlan).map_err(Status::invalid_argument)?;
        let sai = |e: hemlock_sai::SaiError| Status::internal(format!("SAI: {e}"));
        let vlan_ref = self.fdb_vlan_ref(vlan)?;
        let existing = {
            let table = self
                .handle
                .unknown_mcast
                .read()
                .map_err(|_| Status::internal("unknown-mcast table poisoned"))?;
            table.get(&vlan).cloned()
        };
        if !req.restrict {
            // Back to flood-all; drop the restriction group if present.
            self.handle
                .set_vlan_unknown_mcast(vlan_ref, None)
                .await
                .map_err(sai)?;
            if let Some(state) = existing {
                for member in state.members.values() {
                    self.handle.remove_l2mc_member(*member).await.map_err(sai)?;
                }
                self.handle
                    .remove_l2mc_group(state.oid)
                    .await
                    .map_err(sai)?;
                if let Ok(mut table) = self.handle.unknown_mcast.write() {
                    table.remove(&vlan);
                }
            }
            return Ok(Response::new(pb::SetVlanUnknownMcastResponse {}));
        }
        let state = match existing {
            Some(state) => state,
            None => {
                let oid = self.handle.create_l2mc_group().await.map_err(sai)?;
                let state = crate::state::L2mcGroupState {
                    oid,
                    members: std::collections::BTreeMap::new(),
                };
                if let Ok(mut table) = self.handle.unknown_mcast.write() {
                    table.insert(vlan, state.clone());
                }
                state
            }
        };
        let oid = state.oid;
        let members = self.reconcile_l2mc_members(state, &req.ports).await?;
        self.handle
            .set_vlan_unknown_mcast(vlan_ref, Some(oid))
            .await
            .map_err(sai)?;
        if let Ok(mut table) = self.handle.unknown_mcast.write() {
            if let Some(state) = table.get_mut(&vlan) {
                state.members = members;
            }
        }
        Ok(Response::new(pb::SetVlanUnknownMcastResponse {}))
    }

    async fn ensure_neighbor(
        &self,
        request: Request<pb::EnsureNeighborRequest>,
    ) -> Result<Response<pb::EnsureNeighborResponse>, Status> {
        let req = request.into_inner();
        let ip: std::net::IpAddr = req
            .ip
            .parse()
            .map_err(|_| Status::invalid_argument(format!("bad IP address {:?}", req.ip)))?;
        if ip.is_ipv6() {
            self.require_capability(self.handle.capabilities.ipv6, "IPv6")?;
        }
        let (mac_bytes, mac) = Self::mac_bytes(&req.mac)?;
        let rif = self.rif_of(&req.interface)?;
        // The interface may have been re-addressed since the entry was
        // first pushed; a stale entry on the old RIF goes first.
        let stale = {
            let fib = self
                .handle
                .fib
                .read()
                .map_err(|_| Status::internal("fib table poisoned"))?;
            fib.neighbors
                .get(&(req.interface.clone(), req.ip.clone()))
                .filter(|(old_rif, _)| *old_rif != rif)
                .map(|(old_rif, _)| *old_rif)
        };
        if let Some(old_rif) = stale {
            let _ = self.handle.remove_neighbor(old_rif, ip).await;
        }
        self.handle
            .create_neighbor(rif, ip, mac_bytes)
            .await
            .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        let mut fib = self
            .handle
            .fib
            .write()
            .map_err(|_| Status::internal("fib table poisoned"))?;
        fib.neighbors.insert((req.interface, req.ip), (rif, mac));
        Ok(Response::new(pb::EnsureNeighborResponse {}))
    }

    async fn remove_neighbor(
        &self,
        request: Request<pb::RemoveNeighborRequest>,
    ) -> Result<Response<pb::RemoveNeighborResponse>, Status> {
        let req = request.into_inner();
        // Idempotent: the reconciler may retry removals.
        let record = {
            let mut fib = self
                .handle
                .fib
                .write()
                .map_err(|_| Status::internal("fib table poisoned"))?;
            fib.neighbors.remove(&(req.interface, req.ip.clone()))
        };
        if let Some((rif, _)) = record {
            if let Ok(ip) = req.ip.parse() {
                if let Err(err) = self.handle.remove_neighbor(rif, ip).await {
                    tracing::warn!(%err, "removing neighbor");
                }
            }
        }
        Ok(Response::new(pb::RemoveNeighborResponse {}))
    }

    async fn ensure_route(
        &self,
        request: Request<pb::EnsureRouteRequest>,
    ) -> Result<Response<pb::EnsureRouteResponse>, Status> {
        let req = request.into_inner();
        let prefix = hemlock_common::net::require_canonical_prefix(&req.prefix)
            .map_err(Status::invalid_argument)?;
        let dest = hemlock_common::net::parse_cidr(&prefix).map_err(Status::invalid_argument)?;
        if dest.0.is_ipv6() {
            self.require_capability(self.handle.capabilities.ipv6, "IPv6 routing")?;
        }
        let targets = [req.cpu, req.drop, !req.next_hops.is_empty()];
        if targets.iter().filter(|t| **t).count() != 1 {
            return Err(Status::invalid_argument(
                "exactly one of cpu, drop, or next_hops must be given",
            ));
        }
        let width = self.handle.capabilities.ecmp_width;
        if req.next_hops.len() > 1 {
            self.require_capability(width > 0, "ECMP next-hop groups")?;
            if req.next_hops.len() as u32 > width {
                return Err(Status::failed_precondition(format!(
                    "{} next hops exceed this platform's ECMP width of {width}",
                    req.next_hops.len()
                )));
            }
        }

        // Resolve and take references on the hops (deduplicated).
        let mut hop_keys: Vec<(hemlock_sai::Oid, String)> = Vec::new();
        let mut hop_oids = Vec::new();
        for hop in &req.next_hops {
            let ip: std::net::IpAddr = hop.ip.parse().map_err(|_| {
                Status::invalid_argument(format!("bad next-hop address {:?}", hop.ip))
            })?;
            let rif = match self.rif_of(&hop.interface) {
                Ok(rif) => rif,
                Err(status) => {
                    for key in &hop_keys {
                        self.fib_release_hop(key).await;
                    }
                    return Err(status);
                }
            };
            let key = (rif, ip.to_string());
            if hop_keys.contains(&key) {
                continue;
            }
            match self.fib_acquire_hop(rif, ip).await {
                Ok(oid) => {
                    hop_keys.push(key);
                    hop_oids.push(oid);
                }
                Err(status) => {
                    for key in &hop_keys {
                        self.fib_release_hop(key).await;
                    }
                    return Err(status);
                }
            }
        }

        // The route target; several hops become a deduplicated group.
        let mut group_key = None;
        let target = if req.cpu {
            hemlock_sai::RouteTarget::Cpu
        } else if req.drop {
            hemlock_sai::RouteTarget::Drop
        } else if hop_oids.len() == 1 {
            hemlock_sai::RouteTarget::NextHop(hop_oids[0])
        } else {
            let mut members = hop_oids.clone();
            members.sort_by_key(|oid| oid.0);
            match self.fib_acquire_group(&members).await {
                Ok(group) => {
                    group_key = Some(members);
                    hemlock_sai::RouteTarget::Group(group)
                }
                Err(status) => {
                    for key in &hop_keys {
                        self.fib_release_hop(key).await;
                    }
                    return Err(status);
                }
            }
        };

        // Replace: new target objects exist before the old program goes,
        // so shared hops/groups never bounce through zero references.
        let old = {
            let mut fib = self
                .handle
                .fib
                .write()
                .map_err(|_| Status::internal("fib table poisoned"))?;
            fib.routes.remove(&prefix)
        };
        if old.is_some() {
            let _ = self.handle.remove_route(dest).await;
        }
        if let Err(err) = self.handle.create_route(dest, target).await {
            if let Some(group_key) = &group_key {
                self.fib_release_group(group_key).await;
            }
            for key in &hop_keys {
                self.fib_release_hop(key).await;
            }
            if let Some(old) = old {
                self.fib_release_route(&old).await;
            }
            return Err(Status::internal(format!("SAI: {err}")));
        }
        {
            let mut fib = self
                .handle
                .fib
                .write()
                .map_err(|_| Status::internal("fib table poisoned"))?;
            fib.routes.insert(
                prefix,
                crate::state::FibRoute {
                    hop_keys,
                    group_key,
                },
            );
        }
        if let Some(old) = old {
            self.fib_release_route(&old).await;
        }
        Ok(Response::new(pb::EnsureRouteResponse {}))
    }

    async fn remove_route(
        &self,
        request: Request<pb::RemoveRouteRequest>,
    ) -> Result<Response<pb::RemoveRouteResponse>, Status> {
        let req = request.into_inner();
        let prefix = hemlock_common::net::require_canonical_prefix(&req.prefix)
            .map_err(Status::invalid_argument)?;
        // Idempotent: the reconciler may retry removals.
        let record = {
            let mut fib = self
                .handle
                .fib
                .write()
                .map_err(|_| Status::internal("fib table poisoned"))?;
            fib.routes.remove(&prefix)
        };
        if let Some(record) = record {
            if let Ok(dest) = hemlock_common::net::parse_cidr(&prefix) {
                if let Err(err) = self.handle.remove_route(dest).await {
                    tracing::warn!(%err, prefix = %prefix, "removing route");
                }
            }
            self.fib_release_route(&record).await;
        }
        Ok(Response::new(pb::RemoveRouteResponse {}))
    }

    async fn ensure_my_mac(
        &self,
        request: Request<pb::EnsureMyMacRequest>,
    ) -> Result<Response<pb::EnsureMyMacResponse>, Status> {
        self.require_capability(self.handle.capabilities.my_mac, "vrrp (My-MAC entries)")?;
        let req = request.into_inner();
        let (mac_bytes, mac) = Self::mac_bytes(&req.mac)?;
        let vlan = match req.vlan {
            0 => None,
            raw => {
                let vlan = vlan_id(raw).map_err(Status::invalid_argument)?;
                // Validates the VLAN exists (the ref itself is unused).
                let _ = self.fdb_vlan_ref(vlan)?;
                Some(vlan)
            }
        };
        let key = (req.vlan as u16, mac.clone());
        {
            let fib = self
                .handle
                .fib
                .read()
                .map_err(|_| Status::internal("fib table poisoned"))?;
            if fib.my_macs.contains_key(&key) {
                return Ok(Response::new(pb::EnsureMyMacResponse {}));
            }
        }
        let oid = self
            .handle
            .create_my_mac(vlan, mac_bytes)
            .await
            .map_err(|e| Status::internal(format!("SAI: {e}")))?;
        let mut fib = self
            .handle
            .fib
            .write()
            .map_err(|_| Status::internal("fib table poisoned"))?;
        fib.my_macs.insert(key, oid);
        Ok(Response::new(pb::EnsureMyMacResponse {}))
    }

    async fn remove_my_mac(
        &self,
        request: Request<pb::RemoveMyMacRequest>,
    ) -> Result<Response<pb::RemoveMyMacResponse>, Status> {
        let req = request.into_inner();
        let mac = hemlock_common::net::parse_mac(&req.mac).map_err(Status::invalid_argument)?;
        let record = {
            let mut fib = self
                .handle
                .fib
                .write()
                .map_err(|_| Status::internal("fib table poisoned"))?;
            fib.my_macs.remove(&(req.vlan as u16, mac))
        };
        if let Some(oid) = record {
            if let Err(err) = self.handle.remove_my_mac(oid).await {
                tracing::warn!(%err, "removing My-MAC entry");
            }
        }
        Ok(Response::new(pb::RemoveMyMacResponse {}))
    }

    async fn dump_fib(
        &self,
        _request: Request<pb::DumpFibRequest>,
    ) -> Result<Response<pb::DumpFibResponse>, Status> {
        let fib = self
            .handle
            .fib
            .read()
            .map_err(|_| Status::internal("fib table poisoned"))?;
        Ok(Response::new(pb::DumpFibResponse {
            routes: fib.routes.keys().cloned().collect(),
            neighbors: fib
                .neighbors
                .keys()
                .map(|(interface, ip)| pb::RemoveNeighborRequest {
                    interface: interface.clone(),
                    ip: ip.clone(),
                })
                .collect(),
            my_macs: fib
                .my_macs
                .keys()
                .map(|(vlan, mac)| pb::RemoveMyMacRequest {
                    vlan: u32::from(*vlan),
                    mac: mac.clone(),
                })
                .collect(),
        }))
    }

    async fn get_fib_summary(
        &self,
        _request: Request<pb::GetFibSummaryRequest>,
    ) -> Result<Response<pb::GetFibSummaryResponse>, Status> {
        let fib = self
            .handle
            .fib
            .read()
            .map_err(|_| Status::internal("fib table poisoned"))?;
        let v6 = fib.routes.keys().filter(|p| p.contains(':')).count() as u32;
        Ok(Response::new(pb::GetFibSummaryResponse {
            routes_v4: fib.routes.len() as u32 - v6,
            routes_v6: v6,
            neighbors: fib.neighbors.len() as u32,
            next_hop_groups: fib.groups.len() as u32,
        }))
    }

    // --- ACLs (security suite) ---------------------------------------

    async fn ensure_acl(
        &self,
        request: Request<pb::EnsureAclRequest>,
    ) -> Result<Response<pb::EnsureAclResponse>, Status> {
        self.ensure_acl_impl(request.into_inner()).await?;
        Ok(Response::new(pb::EnsureAclResponse {}))
    }

    async fn remove_acl(
        &self,
        request: Request<pb::RemoveAclRequest>,
    ) -> Result<Response<pb::RemoveAclResponse>, Status> {
        self.remove_acl_impl(&request.into_inner().name).await?;
        Ok(Response::new(pb::RemoveAclResponse {}))
    }

    async fn bind_port_acl(
        &self,
        request: Request<pb::BindPortAclRequest>,
    ) -> Result<Response<pb::BindPortAclResponse>, Status> {
        self.bind_port_acl_impl(request.into_inner()).await?;
        Ok(Response::new(pb::BindPortAclResponse {}))
    }

    async fn unbind_port_acl(
        &self,
        request: Request<pb::UnbindPortAclRequest>,
    ) -> Result<Response<pb::UnbindPortAclResponse>, Status> {
        self.unbind_port_acl_impl(request.into_inner()).await?;
        Ok(Response::new(pb::UnbindPortAclResponse {}))
    }

    async fn get_acl_state(
        &self,
        _request: Request<pb::GetAclStateRequest>,
    ) -> Result<Response<pb::GetAclStateResponse>, Status> {
        Ok(Response::new(self.acl_state_impl().await?))
    }

    async fn clear_acl_counters(
        &self,
        request: Request<pb::ClearAclCountersRequest>,
    ) -> Result<Response<pb::ClearAclCountersResponse>, Status> {
        let cleared = self
            .clear_acl_counters_impl(&request.into_inner().name)
            .await?;
        Ok(Response::new(pb::ClearAclCountersResponse { cleared }))
    }

    // --- Control-plane policing --------------------------------------

    async fn set_copp_class(
        &self,
        request: Request<pb::SetCoppClassRequest>,
    ) -> Result<Response<pb::SetCoppClassResponse>, Status> {
        self.set_copp_class_impl(request.into_inner()).await?;
        Ok(Response::new(pb::SetCoppClassResponse {}))
    }

    async fn get_copp_state(
        &self,
        _request: Request<pb::GetCoppStateRequest>,
    ) -> Result<Response<pb::GetCoppStateResponse>, Status> {
        Ok(Response::new(self.copp_state_impl().await?))
    }

    async fn clear_copp_counters(
        &self,
        _request: Request<pb::ClearCoppCountersRequest>,
    ) -> Result<Response<pb::ClearCoppCountersResponse>, Status> {
        self.clear_copp_counters_impl().await?;
        Ok(Response::new(pb::ClearCoppCountersResponse {}))
    }

    // --- Port security -----------------------------------------------

    async fn set_port_security(
        &self,
        request: Request<pb::SetPortSecurityRequest>,
    ) -> Result<Response<pb::SetPortSecurityResponse>, Status> {
        self.set_port_security_impl(request.into_inner()).await?;
        Ok(Response::new(pb::SetPortSecurityResponse {}))
    }

    async fn clear_port_security(
        &self,
        request: Request<pb::ClearPortSecurityRequest>,
    ) -> Result<Response<pb::ClearPortSecurityResponse>, Status> {
        self.clear_port_security_impl(&request.into_inner().port)
            .await?;
        Ok(Response::new(pb::ClearPortSecurityResponse {}))
    }

    async fn get_port_security_state(
        &self,
        request: Request<pb::GetPortSecurityStateRequest>,
    ) -> Result<Response<pb::GetPortSecurityStateResponse>, Status> {
        Ok(Response::new(
            self.port_security_state_impl(&request.into_inner().port)
                .await?,
        ))
    }

    async fn reset_port_security(
        &self,
        request: Request<pb::ResetPortSecurityRequest>,
    ) -> Result<Response<pb::ResetPortSecurityResponse>, Status> {
        let cleared = self
            .reset_port_security_impl(&request.into_inner().port)
            .await?;
        Ok(Response::new(pb::ResetPortSecurityResponse { cleared }))
    }

    // --- 802.1X / snooping enforcement -------------------------------

    async fn set_port_authorized(
        &self,
        request: Request<pb::SetPortAuthorizedRequest>,
    ) -> Result<Response<pb::SetPortAuthorizedResponse>, Status> {
        self.set_port_authorized_impl(request.into_inner()).await?;
        Ok(Response::new(pb::SetPortAuthorizedResponse {}))
    }

    async fn set_snoop_redirects(
        &self,
        request: Request<pb::SetSnoopRedirectsRequest>,
    ) -> Result<Response<pb::SetSnoopRedirectsResponse>, Status> {
        self.set_snoop_redirects_impl(request.into_inner()).await?;
        Ok(Response::new(pb::SetSnoopRedirectsResponse {}))
    }

    // --- QoS ---------------------------------------------------------

    async fn set_qos_maps(
        &self,
        request: Request<pb::SetQosMapsRequest>,
    ) -> Result<Response<pb::SetQosMapsResponse>, Status> {
        self.set_qos_maps_impl(request.into_inner()).await?;
        Ok(Response::new(pb::SetQosMapsResponse {}))
    }

    async fn set_port_qos(
        &self,
        request: Request<pb::SetPortQosRequest>,
    ) -> Result<Response<pb::SetPortQosResponse>, Status> {
        self.set_port_qos_impl(request.into_inner()).await?;
        Ok(Response::new(pb::SetPortQosResponse {}))
    }

    async fn clear_port_qos(
        &self,
        request: Request<pb::ClearPortQosRequest>,
    ) -> Result<Response<pb::ClearPortQosResponse>, Status> {
        self.clear_port_qos_impl(&request.into_inner().port).await?;
        Ok(Response::new(pb::ClearPortQosResponse {}))
    }

    async fn ensure_wred_profile(
        &self,
        request: Request<pb::EnsureWredProfileRequest>,
    ) -> Result<Response<pb::EnsureWredProfileResponse>, Status> {
        self.ensure_wred_profile_impl(request.into_inner()).await?;
        Ok(Response::new(pb::EnsureWredProfileResponse {}))
    }

    async fn remove_wred_profile(
        &self,
        request: Request<pb::RemoveWredProfileRequest>,
    ) -> Result<Response<pb::RemoveWredProfileResponse>, Status> {
        self.remove_wred_profile_impl(&request.into_inner().name)
            .await?;
        Ok(Response::new(pb::RemoveWredProfileResponse {}))
    }

    async fn get_qos_state(
        &self,
        _request: Request<pb::GetQosStateRequest>,
    ) -> Result<Response<pb::GetQosStateResponse>, Status> {
        Ok(Response::new(self.qos_state_impl().await?))
    }
}

/// A user-supplied 802.1Q VLAN id (1..=4094).
fn vlan_id(raw: u32) -> Result<u16, String> {
    u16::try_from(raw)
        .ok()
        .filter(|id| (1..=4094).contains(id))
        .ok_or_else(|| format!("bad VLAN id {raw}"))
}

/// Like [`vlan_id`], with 0 meaning "the default VLAN".
fn default_vlan_id(raw: u32) -> Result<u16, String> {
    if raw == 0 {
        Ok(1)
    } else {
        vlan_id(raw)
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
        // Two ports plus the ever-present default VLAN interface.
        assert_eq!(response.interfaces.len(), 3);
        assert_eq!(response.interfaces[2].name, "Vlan1");
        assert_eq!(response.interfaces[2].kind, "vlan");
        assert_eq!(response.active_vlans, [1]);
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

    /// The transit-FIB pipeline over the mock: neighbors, resolve-via-
    /// punt flip, ECMP group build/dedup/teardown, My-MAC, summary.
    #[tokio::test(flavor = "multi_thread")]
    async fn fib_lifecycle_over_mock_sai() {
        let platform = test_platform();
        let backend = Box::new(hemlock_sai::mock::MockSai::new(platform.ports.clone()));
        let handle = Arc::new(SaiActor::spawn(backend, &platform).await.unwrap());
        let service = SyncdService::new(
            handle.clone(),
            Engine::new(300),
            Arc::default(),
            Inventory::default(),
        );

        // A neighbor or route against an un-addressed interface fails.
        let err = service
            .ensure_neighbor(Request::new(pb::EnsureNeighborRequest {
                interface: "Ethernet1".into(),
                ip: "10.42.10.7".into(),
                mac: "00:1c:73:0c:aa:07".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);

        service
            .set_interface_address(Request::new(pb::SetInterfaceAddressRequest {
                name: "Ethernet1".into(),
                address: "10.42.10.9/24".into(),
            }))
            .await
            .unwrap();
        service
            .ensure_neighbor(Request::new(pb::EnsureNeighborRequest {
                interface: "Ethernet1".into(),
                ip: "10.42.10.7".into(),
                mac: "00:1c:73:0c:aa:07".into(),
            }))
            .await
            .unwrap();
        service
            .ensure_neighbor(Request::new(pb::EnsureNeighborRequest {
                interface: "Ethernet1".into(),
                ip: "10.42.10.8".into(),
                mac: "00:1c:73:0c:aa:08".into(),
            }))
            .await
            .unwrap();

        // Resolve-via-punt: the route lands as CPU, then flips to a
        // next hop once orch reports the neighbor resolved.
        let ensure = |cpu: bool, hops: Vec<&str>| pb::EnsureRouteRequest {
            prefix: "10.99.0.0/16".into(),
            cpu,
            drop: false,
            next_hops: hops
                .iter()
                .map(|ip| pb::RouteNextHop {
                    interface: "Ethernet1".into(),
                    ip: ip.to_string(),
                })
                .collect(),
        };
        service
            .ensure_route(Request::new(ensure(true, vec![])))
            .await
            .unwrap();
        {
            let fib = handle.fib.read().unwrap();
            assert!(fib.routes["10.99.0.0/16"].hop_keys.is_empty());
            assert!(fib.next_hops.is_empty());
        }
        service
            .ensure_route(Request::new(ensure(false, vec!["10.42.10.7"])))
            .await
            .unwrap();
        {
            let fib = handle.fib.read().unwrap();
            assert_eq!(fib.routes["10.99.0.0/16"].hop_keys.len(), 1);
            assert_eq!(fib.next_hops.len(), 1);
            assert!(fib.groups.is_empty());
        }

        // ECMP: two hops build a group; a second prefix with the same
        // hop set shares it (deduplicated by member set).
        service
            .ensure_route(Request::new(ensure(
                false,
                vec!["10.42.10.7", "10.42.10.8"],
            )))
            .await
            .unwrap();
        service
            .ensure_route(Request::new(pb::EnsureRouteRequest {
                prefix: "10.98.0.0/16".into(),
                cpu: false,
                drop: false,
                next_hops: vec![
                    pb::RouteNextHop {
                        interface: "Ethernet1".into(),
                        ip: "10.42.10.7".into(),
                    },
                    pb::RouteNextHop {
                        interface: "Ethernet1".into(),
                        ip: "10.42.10.8".into(),
                    },
                ],
            }))
            .await
            .unwrap();
        let summary = service
            .get_fib_summary(Request::new(pb::GetFibSummaryRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(summary.routes_v4, 2);
        assert_eq!(summary.neighbors, 2);
        assert_eq!(summary.next_hop_groups, 1, "shared by member set");

        // Drop and v6 routes program without any next-hop objects.
        service
            .ensure_route(Request::new(pb::EnsureRouteRequest {
                prefix: "192.0.2.0/24".into(),
                cpu: false,
                drop: true,
                next_hops: vec![],
            }))
            .await
            .unwrap();
        service
            .ensure_route(Request::new(pb::EnsureRouteRequest {
                prefix: "2001:db8:99::/48".into(),
                cpu: true,
                drop: false,
                next_hops: vec![],
            }))
            .await
            .unwrap();
        let summary = service
            .get_fib_summary(Request::new(pb::GetFibSummaryRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!((summary.routes_v4, summary.routes_v6), (3, 1));

        // Teardown releases the shared group with its last reference;
        // removal is idempotent (the reconciler retries).
        for prefix in [
            "10.99.0.0/16",
            "10.98.0.0/16",
            "192.0.2.0/24",
            "2001:db8:99::/48",
        ] {
            service
                .remove_route(Request::new(pb::RemoveRouteRequest {
                    prefix: prefix.into(),
                }))
                .await
                .unwrap();
        }
        service
            .remove_route(Request::new(pb::RemoveRouteRequest {
                prefix: "10.99.0.0/16".into(),
            }))
            .await
            .unwrap();
        {
            let fib = handle.fib.read().unwrap();
            assert!(fib.routes.is_empty());
            assert!(fib.next_hops.is_empty());
            assert!(fib.groups.is_empty());
        }
        service
            .remove_neighbor(Request::new(pb::RemoveNeighborRequest {
                interface: "Ethernet1".into(),
                ip: "10.42.10.7".into(),
            }))
            .await
            .unwrap();
        service
            .remove_neighbor(Request::new(pb::RemoveNeighborRequest {
                interface: "Ethernet1".into(),
                ip: "10.42.10.8".into(),
            }))
            .await
            .unwrap();

        // My-MAC entries: idempotent ensure, idempotent remove.
        let my_mac = pb::EnsureMyMacRequest {
            vlan: 0,
            mac: "00:00:5e:00:01:0a".into(),
        };
        service
            .ensure_my_mac(Request::new(my_mac.clone()))
            .await
            .unwrap();
        service.ensure_my_mac(Request::new(my_mac)).await.unwrap();
        assert_eq!(handle.fib.read().unwrap().my_macs.len(), 1);
        for _ in 0..2 {
            service
                .remove_my_mac(Request::new(pb::RemoveMyMacRequest {
                    vlan: 0,
                    mac: "00:00:5e:00:01:0a".into(),
                }))
                .await
                .unwrap();
        }
        assert!(handle.fib.read().unwrap().my_macs.is_empty());
    }

    /// FIB capability gates: absent My-MAC, IPv6, and ECMP width fail
    /// with the platform error, never silently no-op.
    #[tokio::test(flavor = "multi_thread")]
    async fn fib_capabilities_gate_cleanly() {
        let platform = test_platform();
        let mut mock = hemlock_sai::mock::MockSai::new(platform.ports.clone());
        let mut caps = hemlock_sai::SaiCapabilities::all();
        caps.my_mac = false;
        caps.ipv6 = false;
        caps.ecmp_width = 1;
        mock.set_capabilities(caps);
        let handle = Arc::new(SaiActor::spawn(Box::new(mock), &platform).await.unwrap());
        let service = SyncdService::new(
            handle.clone(),
            Engine::new(300),
            Arc::default(),
            Inventory::default(),
        );
        service
            .set_interface_address(Request::new(pb::SetInterfaceAddressRequest {
                name: "Ethernet1".into(),
                address: "10.42.10.9/24".into(),
            }))
            .await
            .unwrap();

        let err = service
            .ensure_my_mac(Request::new(pb::EnsureMyMacRequest {
                vlan: 0,
                mac: "00:00:5e:00:01:0a".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err
            .message()
            .contains("not supported by this platform's SAI"));

        let err = service
            .ensure_route(Request::new(pb::EnsureRouteRequest {
                prefix: "2001:db8:99::/48".into(),
                cpu: true,
                drop: false,
                next_hops: vec![],
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("IPv6 routing"));

        let err = service
            .ensure_route(Request::new(pb::EnsureRouteRequest {
                prefix: "10.99.0.0/16".into(),
                cpu: false,
                drop: false,
                next_hops: vec![
                    pb::RouteNextHop {
                        interface: "Ethernet1".into(),
                        ip: "10.42.10.7".into(),
                    },
                    pb::RouteNextHop {
                        interface: "Ethernet1".into(),
                        ip: "10.42.10.8".into(),
                    },
                ],
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(
            err.message().contains("ECMP width of 1"),
            "{}",
            err.message()
        );
        // The failed ensure released the hop references it took.
        assert!(handle.fib.read().unwrap().next_hops.is_empty());
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

    /// VLAN + switchport lifecycle over the mock data-plane.
    #[tokio::test(flavor = "multi_thread")]
    async fn vlan_and_switchport_lifecycle_over_mock_sai() {
        let platform = test_platform();
        let backend = Box::new(hemlock_sai::mock::MockSai::new(platform.ports.clone()));
        let handle = Arc::new(SaiActor::spawn(backend, &platform).await.unwrap());
        let service = SyncdService::new(
            handle.clone(),
            Engine::new(300),
            Arc::default(),
            Inventory::default(),
        );
        let ensure = |id: u32, name: &str| {
            service.ensure_vlan(Request::new(pb::EnsureVlanRequest {
                id,
                name: name.into(),
                suspend: false,
            }))
        };
        ensure(10, "Management").await.unwrap();
        ensure(20, "Servers").await.unwrap();
        ensure(40, "").await.unwrap();
        assert!(ensure(0, "x").await.is_err());
        assert!(ensure(4095, "x").await.is_err());

        // Access on VLAN 10.
        service
            .set_port_switchport(Request::new(pb::SetPortSwitchportRequest {
                name: "Ethernet1".into(),
                mode: pb::SwitchportMode::Access as i32,
                access_vlan: 10,
                trunk_vlans: vec![],
                native_vlan: 0,
            }))
            .await
            .unwrap();
        {
            let table = handle.ports.read().unwrap();
            let sp = table["Ethernet1"].switchport.clone().unwrap();
            assert!(!sp.trunk);
            assert_eq!(sp.access_vlan, 10);
            assert_eq!(sp.members.len(), 1);
            assert_eq!(sp.members[0].0, 10);
            assert!(!sp.members[0].2, "access membership is untagged");
        }

        // Reprogram as a trunk: vlans 10,20 native 40.
        service
            .set_port_switchport(Request::new(pb::SetPortSwitchportRequest {
                name: "Ethernet1".into(),
                mode: pb::SwitchportMode::Trunk as i32,
                access_vlan: 0,
                trunk_vlans: vec![10, 20],
                native_vlan: 40,
            }))
            .await
            .unwrap();
        {
            let table = handle.ports.read().unwrap();
            let sp = table["Ethernet1"].switchport.clone().unwrap();
            assert!(sp.trunk);
            let mut vlans: Vec<(u16, bool)> = sp.members.iter().map(|(v, _, t)| (*v, *t)).collect();
            vlans.sort_unstable();
            assert_eq!(vlans, [(10, true), (20, true), (40, false)]);
        }

        // A routed port refuses switchport config, and vice versa.
        let err = service
            .set_interface_address(Request::new(pb::SetInterfaceAddressRequest {
                name: "Ethernet1".into(),
                address: "10.0.0.1/24".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        service
            .set_interface_address(Request::new(pb::SetInterfaceAddressRequest {
                name: "Ethernet2".into(),
                address: "10.0.0.1/24".into(),
            }))
            .await
            .unwrap();
        let err = service
            .set_port_switchport(Request::new(pb::SetPortSwitchportRequest {
                name: "Ethernet2".into(),
                mode: pb::SwitchportMode::Access as i32,
                access_vlan: 10,
                trunk_vlans: vec![],
                native_vlan: 0,
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);

        // Removing a VLAN detaches the trunk's membership.
        service
            .remove_vlan(Request::new(pb::RemoveVlanRequest { id: 20 }))
            .await
            .unwrap();
        {
            let table = handle.ports.read().unwrap();
            let sp = table["Ethernet1"].switchport.clone().unwrap();
            assert!(sp.members.iter().all(|(v, _, _)| *v != 20));
        }

        // Clear: back to default L2; the other VLANs can then go too.
        service
            .clear_port_switchport(Request::new(pb::ClearPortSwitchportRequest {
                name: "Ethernet1".into(),
            }))
            .await
            .unwrap();
        assert!(handle.ports.read().unwrap()["Ethernet1"]
            .switchport
            .is_none());
        service
            .remove_vlan(Request::new(pb::RemoveVlanRequest { id: 10 }))
            .await
            .unwrap();
        service
            .remove_vlan(Request::new(pb::RemoveVlanRequest { id: 40 }))
            .await
            .unwrap();
        assert!(handle.vlans.read().unwrap().is_empty());
    }

    /// A VLAN removal takes its FDB mirror entries with it, and a
    /// static whose VLAN is already gone is still removable (the
    /// hardware entry died with the VLAN; only the shadow remains).
    #[tokio::test(flavor = "multi_thread")]
    async fn static_fdb_outlives_vlan_removal() {
        let platform = test_platform();
        let mock = hemlock_sai::mock::MockSai::new(platform.ports.clone());
        let handle = Arc::new(SaiActor::spawn(Box::new(mock), &platform).await.unwrap());
        let service = SyncdService::new(
            handle.clone(),
            Engine::new(300),
            Arc::default(),
            Inventory::default(),
        );

        service
            .ensure_vlan(Request::new(pb::EnsureVlanRequest {
                id: 69,
                name: "IOT".into(),
                suspend: false,
            }))
            .await
            .unwrap();
        service
            .add_static_fdb(Request::new(pb::AddStaticFdbRequest {
                mac: "0a:bc:bc:00:6a:b2".into(),
                vlan: 69,
                port: "Ethernet1".into(),
                drop: false,
            }))
            .await
            .unwrap();
        service
            .remove_vlan(Request::new(pb::RemoveVlanRequest { id: 69 }))
            .await
            .unwrap();
        assert!(
            handle.fdb.read().unwrap().statics.is_empty(),
            "VLAN removal purges its statics"
        );

        // A zombie shadow entry (static recorded, VLAN gone) must
        // still be removable rather than failing the commit.
        handle.fdb.write().unwrap().statics.insert(
            (69, "0a:bc:bc:00:6a:b2".into()),
            FdbStaticEntry {
                port: Some("Ethernet1".into()),
            },
        );
        service
            .remove_static_fdb(Request::new(pb::RemoveStaticFdbRequest {
                mac: "0a:bc:bc:00:6a:b2".into(),
                vlan: 69,
            }))
            .await
            .unwrap();
        assert!(handle.fdb.read().unwrap().statics.is_empty());
    }

    /// The switching-suite families over the mock data-plane: FDB
    /// statics + dynamics (injected events), scoped flush, storm
    /// control with derived rates, mirror sessions, dot1q-tunnel mode,
    /// and VLAN suspend reconciliation.
    #[tokio::test(flavor = "multi_thread")]
    async fn switching_suite_over_mock_sai() {
        let platform = test_platform();
        let mock = hemlock_sai::mock::MockSai::new(platform.ports.clone());
        let injector = mock.event_injector();
        let handle = Arc::new(SaiActor::spawn(Box::new(mock), &platform).await.unwrap());
        let service = SyncdService::new(
            handle.clone(),
            Engine::new(300),
            Arc::default(),
            Inventory::default(),
        );

        // The capability probe is surfaced through GetSwitchInfo.
        let info = service
            .get_switch_info(Request::new(pb::GetSwitchInfoRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert!(info.capabilities.unwrap().storm_control);

        // FDB: aging, statics on the default VLAN and a created VLAN.
        service
            .set_fdb_aging_time(Request::new(pb::SetFdbAgingTimeRequest { seconds: 600 }))
            .await
            .unwrap();
        service
            .ensure_vlan(Request::new(pb::EnsureVlanRequest {
                id: 10,
                name: "USERS".into(),
                suspend: false,
            }))
            .await
            .unwrap();
        service
            .add_static_fdb(Request::new(pb::AddStaticFdbRequest {
                mac: "00:50:56:BE:EF:01".into(),
                vlan: 10,
                port: "Ethernet1".into(),
                drop: false,
            }))
            .await
            .unwrap();
        service
            .add_static_fdb(Request::new(pb::AddStaticFdbRequest {
                mac: "0050.56be.ef02".into(),
                vlan: 1,
                port: String::new(),
                drop: true,
            }))
            .await
            .unwrap();
        // Statics on an undefined VLAN are rejected.
        let err = service
            .add_static_fdb(Request::new(pb::AddStaticFdbRequest {
                mac: "00:50:56:be:ef:03".into(),
                vlan: 20,
                port: "Ethernet1".into(),
                drop: false,
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);

        // Dynamics arrive as SAI events; a re-learn on another port is a
        // move.
        let e1_id = handle.ports.read().unwrap()["Ethernet1"].sai_id;
        let e2_id = handle.ports.read().unwrap()["Ethernet2"].sai_id;
        let mac = [0x00u8, 0x1c, 0x73, 0x0c, 0xaa, 0x01];
        let bv_id = handle.vlans.read().unwrap()[&10].oid.unwrap().0;
        injector
            .send(hemlock_sai::SaiEvent::Fdb {
                kind: hemlock_sai::FdbEventKind::Learned,
                bv_id,
                mac,
                port: Some(e1_id),
            })
            .unwrap();
        injector
            .send(hemlock_sai::SaiEvent::Fdb {
                kind: hemlock_sai::FdbEventKind::Moved,
                bv_id,
                mac,
                port: Some(e2_id),
            })
            .unwrap();
        // The pump applies events asynchronously; wait for them.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let dump = service
                .dump_fdb(Request::new(pb::DumpFdbRequest::default()))
                .await
                .unwrap()
                .into_inner();
            let dynamic_moved = dump
                .entries
                .iter()
                .any(|e| !e.is_static && e.port == "Ethernet2" && e.moves == 2);
            if dynamic_moved {
                assert_eq!(dump.total, 3, "one dynamic + two statics");
                assert_eq!(dump.aging_time_secs, 600);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "FDB events not applied"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // Filters and paging.
        let statics = service
            .dump_fdb(Request::new(pb::DumpFdbRequest {
                kind: pb::FdbEntryKind::Static as i32,
                ..pb::DumpFdbRequest::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(statics.entries.len(), 2);
        let page = service
            .dump_fdb(Request::new(pb::DumpFdbRequest {
                page_size: 2,
                ..pb::DumpFdbRequest::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(page.entries.len(), 2);
        assert!(!page.next_page_token.is_empty());
        let rest = service
            .dump_fdb(Request::new(pb::DumpFdbRequest {
                page_size: 2,
                page_token: page.next_page_token,
                ..pb::DumpFdbRequest::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(rest.entries.len(), 1);
        assert!(rest.next_page_token.is_empty());

        // A scoped flush drops the dynamic but not the statics.
        let flushed = service
            .flush_fdb(Request::new(pb::FlushFdbRequest {
                vlan: 10,
                port: "Ethernet2".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(flushed.flushed, 1);
        let dump = service
            .dump_fdb(Request::new(pb::DumpFdbRequest::default()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(dump.total, 2);
        assert!(dump.entries.iter().all(|e| e.is_static));

        // Storm control: 10% of a 1G port = 100 Mb/s.
        service
            .set_port_storm_control(Request::new(pb::SetPortStormControlRequest {
                name: "Ethernet1".into(),
                class: pb::StormClass::Broadcast as i32,
                level: Some("10".into()),
            }))
            .await
            .unwrap();
        let storm = service
            .get_storm_control(Request::new(pb::GetStormControlRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(storm.entries.len(), 1);
        assert_eq!(storm.entries[0].level, "10.00");
        assert_eq!(storm.entries[0].rate_kbps, 100_000);
        service
            .set_port_storm_control(Request::new(pb::SetPortStormControlRequest {
                name: "Ethernet1".into(),
                class: pb::StormClass::Broadcast as i32,
                level: None,
            }))
            .await
            .unwrap();
        assert!(service
            .get_storm_control(Request::new(pb::GetStormControlRequest {}))
            .await
            .unwrap()
            .into_inner()
            .entries
            .is_empty());

        // Mirroring: session, sources, teardown.
        service
            .create_mirror_session(Request::new(pb::CreateMirrorSessionRequest {
                session: 1,
                destination: "Ethernet2".into(),
            }))
            .await
            .unwrap();
        service
            .set_port_mirror(Request::new(pb::SetPortMirrorRequest {
                name: "Ethernet1".into(),
                session: 1,
                direction: pb::MirrorDirection::Both as i32,
            }))
            .await
            .unwrap();
        let sessions = service
            .get_mirror_sessions(Request::new(pb::GetMirrorSessionsRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(sessions.sessions.len(), 1);
        assert_eq!(sessions.sessions[0].destination, "Ethernet2");
        assert_eq!(sessions.sessions[0].sources.len(), 1);
        // Detach + remove; removing again is a no-op.
        service
            .set_port_mirror(Request::new(pb::SetPortMirrorRequest {
                name: "Ethernet1".into(),
                session: 1,
                direction: pb::MirrorDirection::None as i32,
            }))
            .await
            .unwrap();
        service
            .remove_mirror_session(Request::new(pb::RemoveMirrorSessionRequest { session: 1 }))
            .await
            .unwrap();
        service
            .remove_mirror_session(Request::new(pb::RemoveMirrorSessionRequest { session: 1 }))
            .await
            .unwrap();

        // dot1q-tunnel: access-like membership on the S-VLAN, surfaced
        // as its own mode.
        service
            .set_port_switchport(Request::new(pb::SetPortSwitchportRequest {
                name: "Ethernet1".into(),
                mode: pb::SwitchportMode::Dot1qTunnel as i32,
                access_vlan: 10,
                trunk_vlans: vec![],
                native_vlan: 0,
            }))
            .await
            .unwrap();
        let response = service
            .get_interfaces(Request::new(pb::GetInterfacesRequest {
                names: vec!["Ethernet1".into()],
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(response.interfaces[0].switchport_mode, "dot1q-tunnel");

        // Suspending the VLAN detaches its memberships; resuming
        // re-adds them.
        service
            .ensure_vlan(Request::new(pb::EnsureVlanRequest {
                id: 10,
                name: "USERS".into(),
                suspend: true,
            }))
            .await
            .unwrap();
        {
            let table = handle.ports.read().unwrap();
            let sp = table["Ethernet1"].switchport.as_ref().unwrap();
            assert!(sp.members.is_empty(), "suspended VLAN detaches members");
        }
        let response = service
            .get_interfaces(Request::new(pb::GetInterfacesRequest { names: vec![] }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(response.suspended_vlans, [10]);
        service
            .ensure_vlan(Request::new(pb::EnsureVlanRequest {
                id: 10,
                name: "USERS".into(),
                suspend: false,
            }))
            .await
            .unwrap();
        {
            let table = handle.ports.read().unwrap();
            let sp = table["Ethernet1"].switchport.as_ref().unwrap();
            assert_eq!(sp.members.len(), 1, "resume re-adds the membership");
        }
    }

    /// Port-channel lifecycle over the mock data-plane: create, member
    /// reconcile with gates, LAG switchport, storm aggregation, teardown.
    #[tokio::test(flavor = "multi_thread")]
    async fn lag_lifecycle_over_mock_sai() {
        let platform = test_platform();
        let backend = Box::new(hemlock_sai::mock::MockSai::new(platform.ports.clone()));
        let handle = Arc::new(SaiActor::spawn(backend, &platform).await.unwrap());
        let service = SyncdService::new(
            handle.clone(),
            Engine::new(300),
            Arc::default(),
            Inventory::default(),
        );

        service
            .create_lag(Request::new(pb::CreateLagRequest {
                group: 1,
                description: "uplink to core".into(),
                admin_up: true,
            }))
            .await
            .unwrap();
        // Members join gated closed unless asked otherwise.
        service
            .set_lag_members(Request::new(pb::SetLagMembersRequest {
                group: 1,
                members: vec![
                    pb::LagMemberSpec {
                        port: "Ethernet1".into(),
                        enabled: false,
                    },
                    pb::LagMemberSpec {
                        port: "Ethernet2".into(),
                        enabled: true,
                    },
                ],
            }))
            .await
            .unwrap();
        {
            let lags = handle.lags.read().unwrap();
            let lag = &lags[&1];
            assert_eq!(lag.members.len(), 2);
            assert!(!lag.members["Ethernet1"].enabled);
            assert!(lag.members["Ethernet2"].enabled);
        }

        // The LAG takes a switchport program by name.
        service
            .ensure_vlan(Request::new(pb::EnsureVlanRequest {
                id: 10,
                name: String::new(),
                suspend: false,
            }))
            .await
            .unwrap();
        service
            .set_port_switchport(Request::new(pb::SetPortSwitchportRequest {
                name: "Port-Channel1".into(),
                mode: pb::SwitchportMode::Trunk as i32,
                access_vlan: 0,
                trunk_vlans: vec![10],
                native_vlan: 0,
            }))
            .await
            .unwrap();
        assert!(handle.lags.read().unwrap()[&1].switchport.is_some());

        // Storm control on the Po applies per member; the row aggregates.
        service
            .set_port_storm_control(Request::new(pb::SetPortStormControlRequest {
                name: "Port-Channel1".into(),
                class: pb::StormClass::Broadcast as i32,
                level: Some("10.00".into()),
            }))
            .await
            .unwrap();
        let storm = service
            .get_storm_control(Request::new(pb::GetStormControlRequest {}))
            .await
            .unwrap()
            .into_inner();
        let po_row = storm
            .entries
            .iter()
            .find(|e| e.name == "Port-Channel1")
            .unwrap();
        assert_eq!(po_row.rate_kbps, 200_000, "10% of two 1G members");

        // GetInterfaces surfaces the Po with its members.
        let response = service
            .get_interfaces(Request::new(pb::GetInterfacesRequest {
                names: vec!["Port-Channel1".into()],
            }))
            .await
            .unwrap()
            .into_inner();
        let po = &response.interfaces[0];
        assert_eq!(po.kind, "port-channel");
        assert_eq!(po.members, ["Ethernet1", "Ethernet2"]);
        assert_eq!(po.oper_status, pb::OperStatus::Up as i32);
        assert_eq!(po.speed_mbps, 1000, "one bundled 1G member");
        assert_eq!(po.switchport_mode, "trunk");

        // Statics can target the Po once it exists.
        service
            .add_static_fdb(Request::new(pb::AddStaticFdbRequest {
                mac: "00:50:56:be:ef:09".into(),
                vlan: 10,
                port: "Port-Channel1".into(),
                drop: false,
            }))
            .await
            .unwrap();

        // A shrunk member list restores the dropped port.
        service
            .set_lag_members(Request::new(pb::SetLagMembersRequest {
                group: 1,
                members: vec![pb::LagMemberSpec {
                    port: "Ethernet2".into(),
                    enabled: true,
                }],
            }))
            .await
            .unwrap();
        assert_eq!(handle.lags.read().unwrap()[&1].members.len(), 1);

        // Teardown unwinds members, memberships, and policers.
        service
            .remove_lag(Request::new(pb::RemoveLagRequest { group: 1 }))
            .await
            .unwrap();
        assert!(handle.lags.read().unwrap().is_empty());
        service
            .remove_lag(Request::new(pb::RemoveLagRequest { group: 1 }))
            .await
            .unwrap();
    }

    /// A commit needing an absent SAI capability fails with the exact
    /// platform error, never a silent no-op.
    #[tokio::test(flavor = "multi_thread")]
    async fn absent_capabilities_fail_cleanly() {
        let platform = test_platform();
        let mut mock = hemlock_sai::mock::MockSai::new(platform.ports.clone());
        mock.set_capabilities(hemlock_sai::SaiCapabilities {
            storm_control: false,
            port_tpid: false,
            fdb_flush: false,
            ..hemlock_sai::SaiCapabilities::all()
        });
        let handle = Arc::new(SaiActor::spawn(Box::new(mock), &platform).await.unwrap());
        let service = SyncdService::new(
            handle,
            Engine::new(300),
            Arc::default(),
            Inventory::default(),
        );
        let err = service
            .set_port_storm_control(Request::new(pb::SetPortStormControlRequest {
                name: "Ethernet1".into(),
                class: pb::StormClass::Broadcast as i32,
                level: Some("10.00".into()),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            err.message(),
            "storm-control is not supported by this platform's SAI"
        );
        let err = service
            .set_port_switchport(Request::new(pb::SetPortSwitchportRequest {
                name: "Ethernet1".into(),
                mode: pb::SwitchportMode::Dot1qTunnel as i32,
                access_vlan: 10,
                trunk_vlans: vec![],
                native_vlan: 0,
            }))
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "dot1q-tunnel is not supported by this platform's SAI"
        );
        let err = service
            .flush_fdb(Request::new(pb::FlushFdbRequest::default()))
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "mac-table flush is not supported by this platform's SAI"
        );
    }

    /// SVI lifecycle over the mock data-plane: default-VLAN SVI, a
    /// created VLAN's SVI, address surfaced via GetInterfaces, teardown
    /// on clear and on VLAN removal.
    #[tokio::test(flavor = "multi_thread")]
    async fn svi_address_lifecycle_over_mock_sai() {
        let platform = test_platform();
        let backend = Box::new(hemlock_sai::mock::MockSai::new(platform.ports.clone()));
        let handle = Arc::new(SaiActor::spawn(backend, &platform).await.unwrap());
        let service = SyncdService::new(
            handle.clone(),
            Engine::new(300),
            Arc::default(),
            Inventory::default(),
        );

        // An SVI on an undefined VLAN is rejected.
        let err = service
            .set_interface_address(Request::new(pb::SetInterfaceAddressRequest {
                name: "Vlan10".into(),
                address: "10.0.10.1/24".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);

        // Vlan1 always works (the hardware default VLAN).
        service
            .set_interface_address(Request::new(pb::SetInterfaceAddressRequest {
                name: "Vlan1".into(),
                address: "10.42.10.9/24".into(),
            }))
            .await
            .unwrap();
        let vlan1_rif = {
            let vlans = handle.vlans.read().unwrap();
            let l3 = vlans[&1].l3.clone().unwrap();
            assert_eq!(l3.address, "10.42.10.9/24");
            l3.rif
        };
        let response = service
            .get_interfaces(Request::new(pb::GetInterfacesRequest {
                names: vec!["Vlan1".into()],
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(response.interfaces[0].ip_addresses, ["10.42.10.9/24"]);

        // Address change keeps the RIF; same address is a no-op.
        service
            .set_interface_address(Request::new(pb::SetInterfaceAddressRequest {
                name: "Vlan1".into(),
                address: "10.42.10.10/24".into(),
            }))
            .await
            .unwrap();
        assert_eq!(
            handle.vlans.read().unwrap()[&1].l3.clone().unwrap().rif,
            vlan1_rif
        );

        // A created VLAN takes an SVI too.
        service
            .ensure_vlan(Request::new(pb::EnsureVlanRequest {
                id: 10,
                name: String::new(),
                suspend: false,
            }))
            .await
            .unwrap();
        service
            .set_interface_address(Request::new(pb::SetInterfaceAddressRequest {
                name: "Vlan10".into(),
                address: "10.0.10.1/24".into(),
            }))
            .await
            .unwrap();
        assert!(handle.vlans.read().unwrap()[&10].l3.is_some());

        // Clear tears the SVI down; the VLAN itself stays.
        service
            .clear_interface_address(Request::new(pb::ClearInterfaceAddressRequest {
                name: "Vlan1".into(),
            }))
            .await
            .unwrap();
        assert!(handle.vlans.read().unwrap()[&1].l3.is_none());

        // Removing a VLAN with a live SVI tears the SVI down with it
        // (the mock refuses to remove a VLAN still fronted by a RIF, so
        // success proves the ordering).
        service
            .remove_vlan(Request::new(pb::RemoveVlanRequest { id: 10 }))
            .await
            .unwrap();
        assert!(!handle.vlans.read().unwrap().contains_key(&10));
    }

    fn acl_rule(number: u32, permit: bool) -> pb::AclRule {
        pb::AclRule {
            number,
            permit,
            ..Default::default()
        }
    }

    fn edge_in_rules() -> Vec<pb::AclRule> {
        vec![
            pb::AclRule {
                protocol: Some(6),
                source: "10.0.0.0/8".into(),
                destination: "10.42.0.0/16".into(),
                destination_port_low: Some(443),
                destination_port_high: Some(443),
                ..acl_rule(10, true)
            },
            pb::AclRule {
                protocol: Some(17),
                destination_port_low: Some(67),
                destination_port_high: Some(68),
                ..acl_rule(20, true)
            },
            pb::AclRule {
                source: "192.0.2.0/24".into(),
                log: true,
                ..acl_rule(30, false)
            },
            pb::AclRule {
                police_rate: Some(10_000_000),
                police_burst: Some(256_000),
                police_pps: false,
                ..acl_rule(40, true)
            },
        ]
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn security_acl_suite_over_mock_sai() {
        let platform = test_platform();
        let backend = Box::new(hemlock_sai::mock::MockSai::new(platform.ports.clone()));
        let handle = Arc::new(SaiActor::spawn(backend, &platform).await.unwrap());
        let service = SyncdService::new(
            handle.clone(),
            Engine::new(300),
            Arc::default(),
            Inventory::default(),
        );
        use hemlock_sai::AclStage;
        let ingress = pb::AclStage::Ingress as i32;

        // Whole-ACL program + binding materializes one table with the
        // four rules and the implicit deny.
        service
            .ensure_acl(Request::new(pb::EnsureAclRequest {
                name: "EDGE-IN".into(),
                family: pb::AclFamily::Ipv4 as i32,
                rules: edge_in_rules(),
            }))
            .await
            .unwrap();
        service
            .bind_port_acl(Request::new(pb::BindPortAclRequest {
                port: "Ethernet1".into(),
                stage: ingress,
                acl: "EDGE-IN".into(),
            }))
            .await
            .unwrap();
        let (rule10, rule20) = {
            let world = handle.acls.read().unwrap();
            let table = &world.tables[&("Ethernet1".to_string(), AclStage::Ingress)];
            assert_eq!(table.entries.len(), 5);
            assert_eq!(table.user_acl.as_deref(), Some("EDGE-IN"));
            assert!(table
                .entries
                .contains_key(&crate::state::AclEntryKey::ImplicitDeny));
            // Rule 40 carries its policer.
            assert!(table.entries[&crate::state::AclEntryKey::User(40)]
                .policer
                .is_some());
            (
                table.entries[&crate::state::AclEntryKey::User(10)].clone(),
                table.entries[&crate::state::AclEntryKey::User(20)].clone(),
            )
        };

        let state = service
            .get_acl_state(Request::new(pb::GetAclStateRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(state.acls.len(), 1);
        assert_eq!(state.acls[0].rules.len(), 4);
        assert_eq!(state.acls[0].matches.len(), 4);
        assert_eq!(state.acls[0].bindings.len(), 1);
        assert_eq!(state.acls[0].bindings[0].port, "Ethernet1");
        let tcam_ingress = state.tcam.iter().find(|t| t.stage == ingress).unwrap();
        assert_eq!(tcam_ingress.used, 5);
        assert_eq!(tcam_ingress.available, 507);

        // Diff minimality: editing rule 20 leaves rule 10's entry and
        // counter objects untouched; rule 20 keeps its counter across
        // the recreate.
        let mut rules = edge_in_rules();
        rules[1].destination_port_low = Some(68);
        rules[1].destination_port_high = Some(69);
        service
            .ensure_acl(Request::new(pb::EnsureAclRequest {
                name: "EDGE-IN".into(),
                family: pb::AclFamily::Ipv4 as i32,
                rules,
            }))
            .await
            .unwrap();
        {
            let world = handle.acls.read().unwrap();
            let table = &world.tables[&("Ethernet1".to_string(), AclStage::Ingress)];
            let new10 = &table.entries[&crate::state::AclEntryKey::User(10)];
            let new20 = &table.entries[&crate::state::AclEntryKey::User(20)];
            assert_eq!(new10.entry, rule10.entry);
            assert_eq!(new10.counter, rule10.counter);
            assert_ne!(new20.entry, rule20.entry);
            assert_eq!(new20.counter, rule20.counter);
        }

        // Internal priority bands: an unauthorized dot1x port's
        // entries always outrank every user rule.
        service
            .set_port_authorized(Request::new(pb::SetPortAuthorizedRequest {
                port: "Ethernet1".into(),
                authorized: false,
            }))
            .await
            .unwrap();
        {
            let world = handle.acls.read().unwrap();
            let table = &world.tables[&("Ethernet1".to_string(), AclStage::Ingress)];
            assert_eq!(table.entries.len(), 7);
            let min_internal = table
                .entries
                .iter()
                .filter(|(k, _)| matches!(k, crate::state::AclEntryKey::Internal(_)))
                .map(|(_, o)| o.priority)
                .min()
                .unwrap();
            let max_user = table
                .entries
                .iter()
                .filter(|(k, _)| !matches!(k, crate::state::AclEntryKey::Internal(_)))
                .map(|(_, o)| o.priority)
                .max()
                .unwrap();
            assert!(
                min_internal > max_user,
                "a user rule must never shadow the dot1x internal entries"
            );
        }
        service
            .set_port_authorized(Request::new(pb::SetPortAuthorizedRequest {
                port: "Ethernet1".into(),
                authorized: true,
            }))
            .await
            .unwrap();
        assert_eq!(
            handle.acls.read().unwrap().tables[&("Ethernet1".to_string(), AclStage::Ingress)]
                .entries
                .len(),
            5
        );

        // Snooping/DAI redirects materialize internal-only tables (no
        // implicit deny), declaratively replaced.
        service
            .set_snoop_redirects(Request::new(pb::SetSnoopRedirectsRequest {
                dhcp: vec![pb::SnoopVlanProgram {
                    vlan: 10,
                    untrusted_ports: vec!["Ethernet2".into()],
                    trusted_ports: vec![],
                }],
                arp: vec![pb::SnoopVlanProgram {
                    vlan: 10,
                    untrusted_ports: vec!["Ethernet2".into()],
                    trusted_ports: vec![],
                }],
            }))
            .await
            .unwrap();
        {
            let world = handle.acls.read().unwrap();
            let table = &world.tables[&("Ethernet2".to_string(), AclStage::Ingress)];
            assert_eq!(table.entries.len(), 2);
            assert!(table.user_acl.is_none());
            assert!(!table
                .entries
                .contains_key(&crate::state::AclEntryKey::ImplicitDeny));
        }
        service
            .set_snoop_redirects(Request::new(pb::SetSnoopRedirectsRequest::default()))
            .await
            .unwrap();
        assert!(!handle
            .acls
            .read()
            .unwrap()
            .tables
            .contains_key(&("Ethernet2".to_string(), AclStage::Ingress)));

        // A Port-Channel binding expands to the members and follows
        // membership churn.
        service
            .create_lag(Request::new(pb::CreateLagRequest {
                group: 1,
                description: String::new(),
                admin_up: true,
            }))
            .await
            .unwrap();
        service
            .set_lag_members(Request::new(pb::SetLagMembersRequest {
                group: 1,
                members: vec![pb::LagMemberSpec {
                    port: "Ethernet2".into(),
                    enabled: true,
                }],
            }))
            .await
            .unwrap();
        service
            .bind_port_acl(Request::new(pb::BindPortAclRequest {
                port: "Port-Channel1".into(),
                stage: ingress,
                acl: "EDGE-IN".into(),
            }))
            .await
            .unwrap();
        assert!(handle
            .acls
            .read()
            .unwrap()
            .tables
            .contains_key(&("Ethernet2".to_string(), AclStage::Ingress)));
        service
            .set_lag_members(Request::new(pb::SetLagMembersRequest {
                group: 1,
                members: vec![],
            }))
            .await
            .unwrap();
        assert!(!handle
            .acls
            .read()
            .unwrap()
            .tables
            .contains_key(&("Ethernet2".to_string(), AclStage::Ingress)));
        service
            .unbind_port_acl(Request::new(pb::UnbindPortAclRequest {
                port: "Port-Channel1".into(),
                stage: ingress,
            }))
            .await
            .unwrap();

        // An ACL in use refuses removal; unbinding frees it and tears
        // the table down.
        let err = service
            .remove_acl(Request::new(pb::RemoveAclRequest {
                name: "EDGE-IN".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert_eq!(err.message(), "ACL EDGE-IN is in use");
        service
            .unbind_port_acl(Request::new(pb::UnbindPortAclRequest {
                port: "Ethernet1".into(),
                stage: ingress,
            }))
            .await
            .unwrap();
        assert!(handle.acls.read().unwrap().tables.is_empty());
        service
            .remove_acl(Request::new(pb::RemoveAclRequest {
                name: "EDGE-IN".into(),
            }))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn security_copp_and_port_security_over_mock_sai() {
        let platform = test_platform();
        let mut mock = hemlock_sai::mock::MockSai::new(platform.ports.clone());
        mock.enable_synthetic_counters();
        let injector = mock.event_injector();
        let handle = Arc::new(SaiActor::spawn(Box::new(mock), &platform).await.unwrap());
        crate::security::program_copp(&handle).await.unwrap();
        tokio::spawn(crate::security::port_security_watch(handle.clone()));
        let service = SyncdService::new(
            handle.clone(),
            Engine::new(300),
            Arc::default(),
            Inventory::default(),
        );

        // The compiled class table renders whole; overrides flag `*`
        // and absent values restore the defaults.
        let state = service
            .get_copp_state(Request::new(pb::GetCoppStateRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(state.classes.len(), 13);
        let bpdu = state.classes.iter().find(|c| c.class == "bpdu").unwrap();
        assert_eq!((bpdu.rate, bpdu.burst, bpdu.overridden), (512, 128, false));
        service
            .set_copp_class(Request::new(pb::SetCoppClassRequest {
                class: "bpdu".into(),
                rate: Some(999),
                burst: None,
            }))
            .await
            .unwrap();
        let state = service
            .get_copp_state(Request::new(pb::GetCoppStateRequest {}))
            .await
            .unwrap()
            .into_inner();
        let bpdu = state.classes.iter().find(|c| c.class == "bpdu").unwrap();
        assert_eq!((bpdu.rate, bpdu.burst, bpdu.overridden), (999, 128, true));
        // Counters accumulate in the mock; clearing baselines them.
        assert!(bpdu.conforming > 0);
        let before = bpdu.conforming;
        service
            .clear_copp_counters(Request::new(pb::ClearCoppCountersRequest {}))
            .await
            .unwrap();
        let state = service
            .get_copp_state(Request::new(pb::GetCoppStateRequest {}))
            .await
            .unwrap()
            .into_inner();
        let bpdu = state.classes.iter().find(|c| c.class == "bpdu").unwrap();
        assert!(bpdu.conforming < before);
        let err = service
            .set_copp_class(Request::new(pb::SetCoppClassRequest {
                class: "banana".into(),
                rate: Some(1),
                burst: None,
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // Port security: learn events fill the secure set; the learn
        // past the limit is a violation (protect freezes learning).
        service
            .set_port_security(Request::new(pb::SetPortSecurityRequest {
                port: "Ethernet2".into(),
                maximum: 2,
                shutdown: false,
            }))
            .await
            .unwrap();
        let (port_id, bv_id) = {
            let ports = handle.ports.read().unwrap();
            (ports["Ethernet2"].sai_id, handle.default_vlan_oid)
        };
        let learn = |mac: [u8; 6]| hemlock_sai::SaiEvent::Fdb {
            kind: hemlock_sai::FdbEventKind::Learned,
            bv_id,
            mac,
            port: Some(port_id),
        };
        injector
            .send(learn([0, 0x50, 0x56, 0xbe, 0xef, 1]))
            .unwrap();
        injector
            .send(learn([0, 0x50, 0x56, 0xbe, 0xef, 2]))
            .unwrap();
        injector
            .send(learn([0, 0x50, 0x56, 0xbe, 0xef, 3]))
            .unwrap();
        for _ in 0..200 {
            let done = {
                let table = handle.port_security.read().unwrap();
                table
                    .get("Ethernet2")
                    .map(|s| s.violations == 1 && s.learned.len() == 2)
                    .unwrap_or(false)
            };
            if done {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let state = service
            .get_port_security_state(Request::new(pb::GetPortSecurityStateRequest {
                port: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(state.ports.len(), 1);
        assert_eq!(state.ports[0].learned.len(), 2);
        assert_eq!(state.ports[0].violations, 1);
        assert_eq!(state.ports[0].last_violation_mac, "00:50:56:be:ef:03");
        assert!(!state.ports[0].errdisabled);

        // Shutdown action: an injected learn-limit violation
        // errdisables the port; reset re-enables and flushes.
        service
            .set_port_security(Request::new(pb::SetPortSecurityRequest {
                port: "Ethernet2".into(),
                maximum: 2,
                shutdown: true,
            }))
            .await
            .unwrap();
        injector
            .send(hemlock_sai::SaiEvent::LearnLimitViolation {
                port: port_id,
                mac: [0, 0x50, 0x56, 0xbe, 0xef, 4],
            })
            .unwrap();
        for _ in 0..200 {
            let done = handle
                .ports
                .read()
                .unwrap()
                .get("Ethernet2")
                .map(|p| p.errdisable_reason.as_deref() == Some("port-security"))
                .unwrap_or(false);
            if done {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let state = service
            .get_port_security_state(Request::new(pb::GetPortSecurityStateRequest {
                port: "Ethernet2".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(state.ports[0].errdisabled);
        assert_eq!(state.ports[0].violations, 2);
        let cleared = service
            .reset_port_security(Request::new(pb::ResetPortSecurityRequest {
                port: String::new(),
            }))
            .await
            .unwrap()
            .into_inner()
            .cleared;
        assert_eq!(cleared, 1);
        let state = service
            .get_port_security_state(Request::new(pb::GetPortSecurityStateRequest {
                port: "Ethernet2".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!state.ports[0].errdisabled);
        assert_eq!(state.ports[0].violations, 0);
        assert!(state.ports[0].learned.is_empty());

        // Unconfiguring forgets the port entirely.
        service
            .clear_port_security(Request::new(pb::ClearPortSecurityRequest {
                port: "Ethernet2".into(),
            }))
            .await
            .unwrap();
        let state = service
            .get_port_security_state(Request::new(pb::GetPortSecurityStateRequest {
                port: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(state.ports.is_empty());
    }

    /// A 4-port board with the Helix4's eight unicast egress queues,
    /// for the QoS suite (the shared `test_platform` has two).
    fn qos_platform() -> Platform {
        let toml = r#"
schema_version = 1

[platform]
id = "test-qos"
onie_machine = "x86_64-test_qos-r0"
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
uc_queues = 8
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
lanes = [1, 2, 3, 4]
"#;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("platform.toml"), toml).unwrap();
        Platform::load(dir.path()).unwrap()
    }

    fn qos_inventory() -> Inventory {
        Inventory {
            uc_queues: 8,
            mc_queues: 1,
            ..Inventory::default()
        }
    }

    fn qos_service(platform: &Platform, handle: Arc<crate::actor::SaiHandle>) -> SyncdService {
        SyncdService::new(
            handle,
            Engine::new(300),
            Arc::default(),
            Inventory {
                platform_model: platform.manifest.platform.model.clone(),
                ..qos_inventory()
            },
        )
    }

    fn map_entry(key: u32, value: u32) -> pb::QosMapEntry {
        pb::QosMapEntry { key, value }
    }

    /// The Part 1.1 seed's maps.
    fn seed_maps() -> pb::SetQosMapsRequest {
        pb::SetQosMapsRequest {
            dscp_to_tc: vec![map_entry(8, 1), map_entry(26, 3), map_entry(46, 5)],
            cos_to_tc: vec![map_entry(3, 3), map_entry(5, 5)],
            tc_to_dscp: vec![map_entry(3, 26), map_entry(5, 46)],
            tc_to_cos: vec![map_entry(3, 3), map_entry(5, 5)],
        }
    }

    fn queue_qos(queue: u32) -> pb::QueueQos {
        pb::QueueQos {
            queue,
            strict: false,
            weight: 0,
            shape_bps: None,
            wred_profile: String::new(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn qos_maps_and_classification_over_mock_sai() {
        let platform = qos_platform();
        let mock = hemlock_sai::mock::MockSai::new(platform.ports.clone());
        let handle = Arc::new(SaiActor::spawn(Box::new(mock), &platform).await.unwrap());
        let service = qos_service(&platform, handle.clone());

        service
            .set_qos_maps(Request::new(seed_maps()))
            .await
            .unwrap();
        // Four tables with entries -> four map objects. The rewrite
        // maps are global, so every port binds them; nothing binds a
        // classification map until a port trusts something.
        {
            let world = handle.qos.read().unwrap();
            assert_eq!(world.map_objects.len(), 4);
            assert_eq!(world.applied.len(), 4);
            for applied in world.applied.values() {
                assert_eq!(
                    applied.bound_maps,
                    std::collections::BTreeSet::from([
                        hemlock_sai::QosMapType::TcToDscp,
                        hemlock_sai::QosMapType::TcToDot1p,
                    ])
                );
            }
        }

        service
            .set_port_qos(Request::new(pb::SetPortQosRequest {
                port: "Ethernet1".into(),
                trust: "dscp".into(),
                default_tc: 1,
                shape_bps: None,
                queues: Vec::new(),
            }))
            .await
            .unwrap();
        {
            let world = handle.qos.read().unwrap();
            let applied = &world.applied["Ethernet1"];
            assert!(applied
                .bound_maps
                .contains(&hemlock_sai::QosMapType::DscpToTc));
            assert_eq!(applied.default_tc, 1);
            // Trusting DSCP binds no CoS map.
            assert!(!applied
                .bound_maps
                .contains(&hemlock_sai::QosMapType::Dot1pToTc));
            assert!(!world.applied["Ethernet2"]
                .bound_maps
                .contains(&hemlock_sai::QosMapType::DscpToTc));
        }

        // A value-only edit rewrites the object in place: the binding
        // set is untouched and no port is rebound.
        let mut edited = seed_maps();
        edited.dscp_to_tc = vec![map_entry(8, 1), map_entry(26, 3), map_entry(46, 6)];
        let objects_before = handle.qos.read().unwrap().map_objects.clone();
        service.set_qos_maps(Request::new(edited)).await.unwrap();
        {
            let world = handle.qos.read().unwrap();
            assert_eq!(world.map_objects, objects_before);
            assert_eq!(world.maps.dscp_to_tc[&46], 6);
        }

        // Emptying a table frees its object and unbinds the ports.
        let mut without_rewrite = seed_maps();
        without_rewrite.tc_to_dscp.clear();
        without_rewrite.tc_to_cos.clear();
        service
            .set_qos_maps(Request::new(without_rewrite))
            .await
            .unwrap();
        {
            let world = handle.qos.read().unwrap();
            assert_eq!(world.map_objects.len(), 2);
            for applied in world.applied.values() {
                assert!(!applied
                    .bound_maps
                    .contains(&hemlock_sai::QosMapType::TcToDscp));
            }
        }

        // ClearPortQos puts the port back to the platform defaults.
        service
            .clear_port_qos(Request::new(pb::ClearPortQosRequest {
                port: "Ethernet1".into(),
            }))
            .await
            .unwrap();
        assert!(!handle.qos.read().unwrap().applied.contains_key("Ethernet1"));

        let state = service
            .get_qos_state(Request::new(pb::GetQosStateRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(state.ports.len(), 4);
        assert_eq!(state.default_ports, 4);
        assert_eq!(state.queue_count, 8);
        assert_eq!(state.dscp_to_tc.len(), 3);
        assert!(state.tc_to_dscp.is_empty());
        let port = &state.ports[0];
        assert_eq!(port.trust, "untrusted");
        assert_eq!(port.queues.len(), 8);
        assert!(port.queues.iter().all(|q| q.weight == 1 && !q.strict));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn qos_scheduler_and_wred_objects_are_deduplicated() {
        let platform = qos_platform();
        let mock = hemlock_sai::mock::MockSai::new(platform.ports.clone());
        let handle = Arc::new(SaiActor::spawn(Box::new(mock), &platform).await.unwrap());
        let service = qos_service(&platform, handle.clone());

        service
            .ensure_wred_profile(Request::new(pb::EnsureWredProfileRequest {
                profile: Some(pb::WredProfile {
                    name: "BULK".into(),
                    min_threshold_kb: 64,
                    max_threshold_kb: 256,
                    drop_probability: 10,
                    ecn: true,
                }),
            }))
            .await
            .unwrap();
        // A profile nothing references costs the ASIC no object.
        assert!(handle.qos.read().unwrap().wred_profiles["BULK"]
            .oid
            .is_none());

        let program = |port: &str| pb::SetPortQosRequest {
            port: port.into(),
            trust: "dscp".into(),
            default_tc: 0,
            shape_bps: None,
            queues: vec![
                pb::QueueQos {
                    strict: true,
                    ..queue_qos(7)
                },
                pb::QueueQos {
                    weight: 40,
                    ..queue_qos(5)
                },
                pb::QueueQos {
                    weight: 30,
                    wred_profile: "BULK".into(),
                    ..queue_qos(3)
                },
            ],
        };
        service
            .set_port_qos(Request::new(program("Ethernet1")))
            .await
            .unwrap();
        service
            .set_port_qos(Request::new(program("Ethernet2")))
            .await
            .unwrap();

        // Two ports, the same three queue shapes: three scheduler
        // objects between them, each refcounted twice.
        {
            let world = handle.qos.read().unwrap();
            assert_eq!(world.schedulers.len(), 3);
            assert!(world.schedulers.values().all(|(_, refs)| *refs == 2));
            // ... and one WRED object with two queue references.
            assert_eq!(world.wred_profiles["BULK"].refs, 2);
            assert!(world.wred_profiles["BULK"].oid.is_some());
            // Queues left at the default bind nothing at all.
            assert_eq!(world.applied["Ethernet1"].queue_schedulers.len(), 3);
        }

        // The last unbind frees the objects.
        service
            .clear_port_qos(Request::new(pb::ClearPortQosRequest {
                port: "Ethernet1".into(),
            }))
            .await
            .unwrap();
        {
            let world = handle.qos.read().unwrap();
            assert_eq!(world.schedulers.len(), 3);
            assert!(world.schedulers.values().all(|(_, refs)| *refs == 1));
            assert_eq!(world.wred_profiles["BULK"].refs, 1);
        }
        // A profile still bound cannot be removed.
        let err = service
            .remove_wred_profile(Request::new(pb::RemoveWredProfileRequest {
                name: "BULK".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("still bound"));

        service
            .clear_port_qos(Request::new(pb::ClearPortQosRequest {
                port: "Ethernet2".into(),
            }))
            .await
            .unwrap();
        {
            let world = handle.qos.read().unwrap();
            assert!(world.schedulers.is_empty());
            assert_eq!(world.wred_profiles["BULK"].refs, 0);
            assert!(world.wred_profiles["BULK"].oid.is_none());
            assert!(world.applied.is_empty());
        }
        service
            .remove_wred_profile(Request::new(pb::RemoveWredProfileRequest {
                name: "BULK".into(),
            }))
            .await
            .unwrap();
        assert!(handle.qos.read().unwrap().wred_profiles.is_empty());
    }

    /// A Port-Channel QoS program expands to its members and follows
    /// membership churn.
    #[tokio::test(flavor = "multi_thread")]
    async fn qos_port_channel_program_expands_to_members() {
        let platform = qos_platform();
        let mock = hemlock_sai::mock::MockSai::new(platform.ports.clone());
        let handle = Arc::new(SaiActor::spawn(Box::new(mock), &platform).await.unwrap());
        let service = qos_service(&platform, handle.clone());

        service
            .create_lag(Request::new(pb::CreateLagRequest {
                group: 1,
                description: String::new(),
                admin_up: true,
            }))
            .await
            .unwrap();
        service
            .set_lag_members(Request::new(pb::SetLagMembersRequest {
                group: 1,
                members: vec![pb::LagMemberSpec {
                    port: "Ethernet1".into(),
                    enabled: true,
                }],
            }))
            .await
            .unwrap();
        service
            .set_port_qos(Request::new(pb::SetPortQosRequest {
                port: "Port-Channel1".into(),
                trust: "dscp".into(),
                default_tc: 2,
                shape_bps: Some(800_000_000),
                queues: Vec::new(),
            }))
            .await
            .unwrap();
        {
            let world = handle.qos.read().unwrap();
            let applied = &world.applied["Ethernet1"];
            assert_eq!(applied.source, "Port-Channel1");
            assert_eq!(applied.default_tc, 2);
            assert_eq!(applied.shape_bps, Some(800_000_000));
        }

        // Membership churn: the new member picks the program up, the
        // old one falls back to the defaults.
        service
            .set_lag_members(Request::new(pb::SetLagMembersRequest {
                group: 1,
                members: vec![pb::LagMemberSpec {
                    port: "Ethernet2".into(),
                    enabled: true,
                }],
            }))
            .await
            .unwrap();
        {
            let world = handle.qos.read().unwrap();
            assert!(!world.applied.contains_key("Ethernet1"));
            assert_eq!(world.applied["Ethernet2"].source, "Port-Channel1");
        }

        // The member row carries the Port-Channel's effective values.
        let state = service
            .get_qos_state(Request::new(pb::GetQosStateRequest {}))
            .await
            .unwrap()
            .into_inner();
        let member = state.ports.iter().find(|p| p.port == "Ethernet2").unwrap();
        assert_eq!(member.via_port_channel, "Port-Channel1");
        assert_eq!(member.default_tc, 2);
        assert!(state.ports.iter().any(|p| p.port == "Port-Channel1"));
    }

    /// A commit-confirm expiry re-applies the *previous* running text
    /// through the same appliers, so the QoS world has to land back
    /// exactly where it was: same scheduler objects, same refcounts,
    /// same map bindings.
    #[tokio::test(flavor = "multi_thread")]
    async fn qos_reapplying_a_prior_config_restores_the_same_objects() {
        let platform = qos_platform();
        let mock = hemlock_sai::mock::MockSai::new(platform.ports.clone());
        let handle = Arc::new(SaiActor::spawn(Box::new(mock), &platform).await.unwrap());
        let service = qos_service(&platform, handle.clone());

        // A snapshot of everything a commit could disturb: one port's
        // (name, source, default-tc, shaper, scheduled queues).
        type AppliedRow = (String, String, u8, Option<u64>, Vec<u8>);
        let snapshot = || {
            let world = handle.qos.read().unwrap();
            let schedulers: Vec<(hemlock_sai::SchedulerSpec, (hemlock_sai::Oid, u32))> =
                world.schedulers.iter().map(|(k, v)| (*k, *v)).collect();
            let applied: Vec<AppliedRow> = world
                .applied
                .iter()
                .map(|(port, state)| {
                    (
                        port.clone(),
                        state.source.clone(),
                        state.default_tc,
                        state.shape_bps,
                        state.queue_schedulers.keys().copied().collect(),
                    )
                })
                .collect();
            let maps = world.map_objects.clone();
            (schedulers, applied, maps)
        };

        let program_a = pb::SetPortQosRequest {
            port: "Ethernet1".into(),
            trust: "dscp".into(),
            default_tc: 1,
            shape_bps: None,
            queues: vec![
                pb::QueueQos {
                    strict: true,
                    ..queue_qos(7)
                },
                pb::QueueQos {
                    weight: 40,
                    ..queue_qos(5)
                },
            ],
        };
        let program_b = pb::SetPortQosRequest {
            port: "Ethernet1".into(),
            trust: "cos".into(),
            default_tc: 4,
            shape_bps: Some(500_000_000),
            queues: vec![pb::QueueQos {
                weight: 7,
                ..queue_qos(2)
            }],
        };

        service
            .set_qos_maps(Request::new(seed_maps()))
            .await
            .unwrap();
        service
            .set_port_qos(Request::new(program_a.clone()))
            .await
            .unwrap();
        let before = snapshot();

        // The commit that gets rolled back.
        service.set_port_qos(Request::new(program_b)).await.unwrap();
        let mut without_rewrite = seed_maps();
        without_rewrite.tc_to_dscp.clear();
        service
            .set_qos_maps(Request::new(without_rewrite))
            .await
            .unwrap();
        assert_ne!(snapshot().1, before.1);

        // Expiry: the prior text applies again.
        service
            .set_qos_maps(Request::new(seed_maps()))
            .await
            .unwrap();
        service.set_port_qos(Request::new(program_a)).await.unwrap();
        let after = snapshot();
        assert_eq!(
            after.1, before.1,
            "per-port state drifted across a rollback"
        );
        assert_eq!(
            after.0.len(),
            before.0.len(),
            "scheduler object count drifted across a rollback"
        );
        for (spec, (_, refs)) in &after.0 {
            let (_, want) = before
                .0
                .iter()
                .find(|(other, _)| other == spec)
                .map(|(_, v)| *v)
                .expect("the same scheduler shapes are back");
            assert_eq!(*refs, want, "refcount drifted for {spec:?}");
        }
        assert_eq!(
            after.2.keys().collect::<Vec<_>>(),
            before.2.keys().collect::<Vec<_>>(),
            "map objects drifted across a rollback"
        );

        // And re-pushing the same program a third time is a no-op: the
        // diff finds nothing to do, so no object churns.
        let repeat = snapshot();
        assert_eq!(repeat.0.len(), after.0.len());
    }

    /// Queue counters reach `GetQosState` from the stats engine, so the
    /// web console's per-queue view and the CLI's read the same numbers.
    #[tokio::test(flavor = "multi_thread")]
    async fn qos_state_carries_live_queue_counters() {
        let platform = qos_platform();
        let mut mock = hemlock_sai::mock::MockSai::new(platform.ports.clone());
        let port_id = mock.port_id_of(0);
        mock.set_queue_counters(
            port_id,
            vec![
                hemlock_sai::QueueCounters {
                    unicast: true,
                    index: 3,
                    pkts: 88_123,
                    bytes: 101_233_911,
                    dropped_pkts: 1204,
                    dropped_bytes: 1_812_664,
                    wred_dropped: 1187,
                    ecn_marked: 3320,
                },
                hemlock_sai::QueueCounters {
                    unicast: true,
                    index: 5,
                    pkts: 421_900,
                    bytes: 530_122_831,
                    ..hemlock_sai::QueueCounters::default()
                },
            ],
        );
        let handle = Arc::new(SaiActor::spawn(Box::new(mock), &platform).await.unwrap());
        let engine = Engine::new(300);
        let service = SyncdService::new(
            handle.clone(),
            engine.clone(),
            Arc::default(),
            qos_inventory(),
        );

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
            let queues = sample
                .queues
                .iter()
                .map(|q| crate::ifstats::QueueSample {
                    label: format!("{}{}", if q.unicast { "UC" } else { "MC" }, q.index),
                    pkts: q.pkts,
                    bytes: q.bytes,
                    dropped_pkts: q.dropped_pkts,
                    dropped_bytes: q.dropped_bytes,
                    wred_dropped: q.wred_dropped,
                    ecn_marked: q.ecn_marked,
                })
                .collect();
            engine.ingest(&sample.name, sample.counters.into(), queues, now);
        }

        let state = service
            .get_qos_state(Request::new(pb::GetQosStateRequest {}))
            .await
            .unwrap()
            .into_inner();
        let port = state
            .ports
            .iter()
            .find(|p| p.port == "Ethernet1")
            .expect("Ethernet1 row");
        let q3 = &port.queues[3];
        assert_eq!(
            (q3.tx_packets, q3.dropped, q3.wred_dropped, q3.ecn_marked),
            (88_123, 1204, 1187, 3320)
        );
        assert_eq!(port.queues[5].tx_packets, 421_900);
        // Untouched queues stay at zero rather than going missing.
        assert_eq!(port.queues[0].tx_packets, 0);

        // `clear counters` baselines them like every other counter.
        service
            .clear_counters(Request::new(pb::ClearCountersRequest { names: Vec::new() }))
            .await
            .unwrap();
        let state = service
            .get_qos_state(Request::new(pb::GetQosStateRequest {}))
            .await
            .unwrap()
            .into_inner();
        let port = state
            .ports
            .iter()
            .find(|p| p.port == "Ethernet1")
            .expect("Ethernet1 row");
        assert_eq!(port.queues[3].wred_dropped, 0);
        assert_eq!(port.queues[3].ecn_marked, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn qos_capabilities_gate_cleanly() {
        let platform = qos_platform();
        let mut mock = hemlock_sai::mock::MockSai::new(platform.ports.clone());
        mock.set_capabilities(hemlock_sai::SaiCapabilities {
            qos_map_egress: false,
            wred: false,
            queue_shaper: false,
            wred_queue_stats: false,
            buffer_bytes_total: 4 * 1024 * 1024,
            ..hemlock_sai::SaiCapabilities::all()
        });
        let handle = Arc::new(SaiActor::spawn(Box::new(mock), &platform).await.unwrap());
        let service = qos_service(&platform, handle.clone());

        // No egress rewrite maps.
        let err = service
            .set_qos_maps(Request::new(seed_maps()))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            err.message(),
            "egress rewrite QoS maps are not supported by this platform's SAI"
        );
        // Classification-only maps still land.
        let mut ingress_only = seed_maps();
        ingress_only.tc_to_dscp.clear();
        ingress_only.tc_to_cos.clear();
        service
            .set_qos_maps(Request::new(ingress_only))
            .await
            .unwrap();

        // No per-queue shapers.
        let err = service
            .set_port_qos(Request::new(pb::SetPortQosRequest {
                port: "Ethernet1".into(),
                trust: "dscp".into(),
                default_tc: 0,
                shape_bps: None,
                queues: vec![pb::QueueQos {
                    shape_bps: Some(100_000_000),
                    ..queue_qos(5)
                }],
            }))
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "per-queue shapers are not supported by this platform's SAI"
        );

        // A WRED profile definition is fine; the reference is what
        // fails, and it fails at the SAI object create.
        service
            .ensure_wred_profile(Request::new(pb::EnsureWredProfileRequest {
                profile: Some(pb::WredProfile {
                    name: "BULK".into(),
                    min_threshold_kb: 64,
                    max_threshold_kb: 256,
                    drop_probability: 10,
                    ecn: false,
                }),
            }))
            .await
            .unwrap();
        let err = service
            .set_port_qos(Request::new(pb::SetPortQosRequest {
                port: "Ethernet1".into(),
                trust: "dscp".into(),
                default_tc: 0,
                shape_bps: None,
                queues: vec![pb::QueueQos {
                    weight: 30,
                    wred_profile: "BULK".into(),
                    ..queue_qos(3)
                }],
            }))
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "WRED is not supported by this platform's SAI"
        );

        // Thresholds validate against the probed packet buffer.
        let err = service
            .ensure_wred_profile(Request::new(pb::EnsureWredProfileRequest {
                profile: Some(pb::WredProfile {
                    name: "HUGE".into(),
                    min_threshold_kb: 64,
                    max_threshold_kb: 8192,
                    drop_probability: 10,
                    ecn: false,
                }),
            }))
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "qos wred-profile HUGE: max-threshold 8192 KB exceeds this platform's 4096 KB packet buffer"
        );

        // The stat probe is off, so the two counter columns read zero.
        let state = service
            .get_qos_state(Request::new(pb::GetQosStateRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert!(!state.wred_supported);
        assert!(!state.queue_shaper_supported);
        assert_eq!(state.buffer_kb, 4096);
        assert!(state
            .ports
            .iter()
            .flat_map(|p| &p.queues)
            .all(|q| { q.wred_dropped == 0 && q.ecn_marked == 0 }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn security_capabilities_gate_cleanly() {
        let platform = test_platform();
        let mut mock = hemlock_sai::mock::MockSai::new(platform.ports.clone());
        mock.set_capabilities(hemlock_sai::SaiCapabilities {
            acl_egress: false,
            acl_entry_policer: false,
            port_learn_limit: false,
            copp: false,
            ..hemlock_sai::SaiCapabilities::all()
        });
        let handle = Arc::new(SaiActor::spawn(Box::new(mock), &platform).await.unwrap());
        let service = SyncdService::new(
            handle,
            Engine::new(300),
            Arc::default(),
            Inventory::default(),
        );
        service
            .ensure_acl(Request::new(pb::EnsureAclRequest {
                name: "EDGE-IN".into(),
                family: pb::AclFamily::Ipv4 as i32,
                rules: edge_in_rules(),
            }))
            .await
            .unwrap();

        let err = service
            .bind_port_acl(Request::new(pb::BindPortAclRequest {
                port: "Ethernet1".into(),
                stage: pb::AclStage::Egress as i32,
                acl: "EDGE-IN".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            err.message(),
            "egress ACLs are not supported by this platform's SAI"
        );

        // EDGE-IN's rule 40 polices, and this platform has no per-entry
        // policers.
        let err = service
            .bind_port_acl(Request::new(pb::BindPortAclRequest {
                port: "Ethernet1".into(),
                stage: pb::AclStage::Ingress as i32,
                acl: "EDGE-IN".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "per-rule policers are not supported by this platform's SAI"
        );

        // No ASIC learn limit: the set still applies, falling back to
        // the software engine rather than failing the commit.
        service
            .set_port_security(Request::new(pb::SetPortSecurityRequest {
                port: "Ethernet1".into(),
                maximum: 4,
                shutdown: true,
            }))
            .await
            .unwrap();
        let state = service
            .get_port_security_state(Request::new(pb::GetPortSecurityStateRequest {
                port: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(state.ports.len(), 1);
        assert_eq!(state.ports[0].port, "Ethernet1");
        assert_eq!(state.ports[0].maximum, 4);
        service
            .clear_port_security(Request::new(pb::ClearPortSecurityRequest {
                port: "Ethernet1".into(),
            }))
            .await
            .unwrap();

        let err = service
            .set_copp_class(Request::new(pb::SetCoppClassRequest {
                class: "bpdu".into(),
                rate: Some(100),
                burst: None,
            }))
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "control-plane policing is not supported by this platform's SAI"
        );
    }
}
