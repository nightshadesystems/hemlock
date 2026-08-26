//! Pure-Rust mock SAI backend.
//!
//! Behaves like a well-mannered ASIC: `create_switch` "boots" it, `ports()`
//! returns one port per platform port-table entry (SAI ports are created
//! from config.bcm on real hardware, so the mock synthesizes the same
//! outcome from the manifest), and admin-state changes produce the
//! corresponding oper-status notifications — links come up when enabled,
//! exactly what the layers above need for end-to-end testing.

use std::collections::HashMap;
use std::net::IpAddr;

use hemlock_platform::PortDef;
use tokio::sync::mpsc;

use crate::{
    AclAction, AclFamily, AclFields, AclStage, CablePair, CablePairState, FdbAction, IpPrefix, Oid,
    PolicerSpec, PolicerStats, PortCounters, PortId, QosMapType, QueueCounters, RouteTarget,
    SaiBackend, SaiCapabilities, SaiError, SaiEvent, SaiPort, SchedulerSpec, StormClass,
    StpPortState, SwitchInfo, TrapKind, WredSpec,
};

/// Synthetic OIDs: obviously fake, stable, and readable in logs.
const MOCK_SWITCH_OID: u64 = 0x2100_0000_0000_0000;
const MOCK_PORT_OID_BASE: u64 = 0x2100_0000_0000_1000;
const MOCK_HOSTIF_OID_BASE: u64 = 0x2100_0000_0000_2000;
const MOCK_RIF_OID_BASE: u64 = 0x2100_0000_0000_3000;
const MOCK_VLAN_OID_BASE: u64 = 0x2100_0000_0000_4000;
const MOCK_VLAN_MEMBER_OID_BASE: u64 = 0x2100_0000_0000_5000;

const MOCK_MIRROR_OID_BASE: u64 = 0x2100_0000_0000_6000;
const MOCK_LAG_OID_BASE: u64 = 0x2100_0000_0000_7000;
const MOCK_LAG_MEMBER_OID_BASE: u64 = 0x2100_0000_0000_8000;
const MOCK_STP_OID_BASE: u64 = 0x2100_0000_0000_9000;
const MOCK_L2MC_OID_BASE: u64 = 0x2100_0000_0000_a000;
const MOCK_L2MC_MEMBER_OID_BASE: u64 = 0x2100_0000_0000_b000;
const MOCK_NEXT_HOP_OID_BASE: u64 = 0x2100_0000_0000_c000;
const MOCK_NHG_OID_BASE: u64 = 0x2100_0000_0000_d000;
const MOCK_NHG_MEMBER_OID_BASE: u64 = 0x2100_0000_0000_e000;
const MOCK_MY_MAC_OID_BASE: u64 = 0x2100_0000_0000_f000;
const MOCK_ACL_TABLE_OID_BASE: u64 = 0x2100_0000_0001_0000;
const MOCK_ACL_ENTRY_OID_BASE: u64 = 0x2100_0000_0001_1000;
const MOCK_ACL_COUNTER_OID_BASE: u64 = 0x2100_0000_0001_2000;
const MOCK_POLICER_OID_BASE: u64 = 0x2100_0000_0001_3000;
const MOCK_TRAP_GROUP_OID_BASE: u64 = 0x2100_0000_0001_4000;
const MOCK_TRAP_OID_BASE: u64 = 0x2100_0000_0001_5000;
const MOCK_QOS_MAP_OID_BASE: u64 = 0x2100_0000_0001_6000;
const MOCK_SCHEDULER_OID_BASE: u64 = 0x2100_0000_0001_7000;
const MOCK_WRED_OID_BASE: u64 = 0x2100_0000_0001_8000;
const MOCK_SAMPLE_OID_BASE: u64 = 0x2100_0000_0001_9000;

/// The mock TCAM's entry capacity per stage — Helix4-shaped numbers so
/// `show acl summary` has something honest to report (IPv6 entries
/// count double-wide, like the real ContentAware slices).
const MOCK_ACL_INGRESS_CAPACITY: u32 = 512;
const MOCK_ACL_EGRESS_CAPACITY: u32 = 256;

/// The default 802.1Q VLAN's synthetic OID (the base itself; `alloc`
/// starts above it), so FDB events on VLAN 1 can be mapped back.
const MOCK_DEFAULT_VLAN_OID: u64 = MOCK_VLAN_OID_BASE;

/// The default 802.1Q VLAN every port starts in.
const DEFAULT_VLAN: u16 = 1;

/// What an unscripted port reports from a TDR sweep: four terminated
/// pairs on a 42 m run — a healthy patch, which is what a switch
/// mostly finds.
fn default_cable_diag() -> Vec<CablePair> {
    vec![
        CablePair {
            state: CablePairState::Ok,
            length_m: 42,
        };
        4
    ]
}

pub struct MockSai {
    port_table: Vec<PortDef>,
    ports: Vec<SaiPort>,
    created: bool,
    events_tx: mpsc::UnboundedSender<SaiEvent>,
    events_rx: Option<mpsc::UnboundedReceiver<SaiEvent>>,
    /// L3 model, mirroring what the real ASIC tracks: punt install,
    /// per-port hostif netdev names, per-port RIFs (a routed port has
    /// left the default 802.1Q bridge), and the default-VR route table.
    punt_installed: bool,
    hostifs: HashMap<PortId, (Oid, String)>,
    rifs: HashMap<PortId, Oid>,
    routes: HashMap<IpPrefix, RouteTarget>,
    /// L2 model: created VLANs, memberships, PVIDs, and which ports
    /// still hold their boot-time untagged default-VLAN membership.
    vlans: HashMap<u16, Oid>,
    /// SVI RIFs: rif oid -> the VLAN it fronts (None = default VLAN).
    vlan_rifs: HashMap<Oid, Option<Oid>>,
    vlan_members: HashMap<Oid, (Oid, PortId, bool)>,
    default_members: std::collections::HashSet<PortId>,
    pvids: HashMap<PortId, u16>,
    /// Switching-suite model: capability posture, FDB aging + statics,
    /// storm policer rates, mirror sessions/attachments, port TPIDs.
    capabilities: SaiCapabilities,
    fdb_aging: u32,
    fdb_statics: HashMap<(Option<Oid>, [u8; 6]), FdbAction>,
    storm: HashMap<(PortId, StormClass), u64>,
    mirror_sessions: HashMap<Oid, PortId>,
    port_mirrors: HashMap<PortId, (Option<Oid>, Option<Oid>)>,
    /// Scripted TDR results, keyed by port. Absent ports answer with
    /// the healthy default, so a test that only cares that the sweep
    /// ran does not have to script one.
    cable_diag: HashMap<PortId, Vec<CablePair>>,
    /// sFlow model: samplepacket sessions (by rate) and their ingress
    /// port bindings, both inspectable so tests can assert what the
    /// engine programmed.
    sample_sessions: HashMap<Oid, u32>,
    port_samples: HashMap<PortId, Oid>,
    tpids: HashMap<PortId, u16>,
    /// Forced link parameters, as programmed: pinned speed, forced
    /// duplex (true = full), autoneg state, and L2 MTU. Absent = the
    /// port is running whatever config.bcm created it with.
    forced_speeds: HashMap<PortId, u32>,
    forced_duplex: HashMap<PortId, bool>,
    autoneg: HashMap<PortId, bool>,
    mtus: HashMap<PortId, u32>,
    /// LAGs (port-like ids) and their members:
    /// member oid -> (lag, port, collect/distribute gate).
    lags: std::collections::HashSet<PortId>,
    lag_members: HashMap<Oid, (PortId, PortId, bool)>,
    /// STP model: created instances, VLAN assignments, per-instance
    /// port states (None instance key = the default instance).
    stp_instances: std::collections::HashSet<Oid>,
    vlan_stp: HashMap<Option<Oid>, Option<Oid>>,
    stp_port_states: HashMap<(Option<Oid>, PortId), StpPortState>,
    /// L2MC model: groups, their members, (*, G) entries, and each
    /// VLAN's unknown-multicast flood group.
    l2mc_groups: std::collections::HashSet<Oid>,
    l2mc_members: HashMap<Oid, (Oid, PortId)>,
    l2mc_entries: HashMap<(Option<Oid>, IpAddr), Oid>,
    vlan_unknown_mcast: HashMap<Option<Oid>, Oid>,
    /// FIB model: neighbor entries per RIF, next hops, ECMP groups
    /// (member oid -> (group, next hop)), and My-MAC entries.
    neighbors: HashMap<(Oid, IpAddr), [u8; 6]>,
    next_hops: HashMap<Oid, (Oid, IpAddr)>,
    nh_groups: std::collections::HashSet<Oid>,
    nh_group_members: HashMap<Oid, (Oid, Oid)>,
    my_macs: HashMap<Oid, (Option<u16>, [u8; 6])>,
    /// Security-suite model: ACL tables/entries/counters and port
    /// bindings, standalone policers, hostif trap groups + traps, and
    /// per-port learn limits/learning state.
    acl_tables: HashMap<Oid, (AclStage, AclFamily)>,
    acl_entries: HashMap<Oid, (Oid, u32, AclFields, AclAction)>,
    acl_counters: HashMap<Oid, (Oid, u64)>,
    acl_bindings: HashMap<(PortId, AclStage), Oid>,
    policers: HashMap<Oid, (PolicerSpec, PolicerStats)>,
    trap_groups: HashMap<Oid, Option<Oid>>,
    traps: HashMap<Oid, (TrapKind, bool, Oid)>,
    learn_limits: HashMap<PortId, u32>,
    learning_off: std::collections::HashSet<PortId>,
    default_trap_group_policer: Option<Oid>,
    /// QoS model: global map objects, their per-port bindings, and each
    /// port's default traffic class.
    qos_maps: HashMap<Oid, (QosMapType, Vec<(u8, u8)>)>,
    port_qos_maps: HashMap<(PortId, QosMapType), Oid>,
    port_default_tc: HashMap<PortId, u8>,
    /// Egress scheduling model: scheduler profile objects, their
    /// per-(port, queue) bindings, and the port-level shaper rate.
    schedulers: HashMap<Oid, SchedulerSpec>,
    queue_schedulers: HashMap<(PortId, u32), Oid>,
    port_shapers: HashMap<PortId, u64>,
    /// WRED profile objects and their per-(port, queue) bindings.
    wreds: HashMap<Oid, WredSpec>,
    queue_wreds: HashMap<(PortId, u32), Oid>,
    /// Injected per-queue counters (the mock forwards nothing, so
    /// queue stats are whatever a test asked for).
    queue_counters: HashMap<PortId, Vec<QueueCounters>>,
    synthetic_counters: bool,
    next_oid: u64,
}

impl MockSai {
    /// Build a mock ASIC whose config.bcm "created" exactly the ports in
    /// the platform port table.
    pub fn new(port_table: Vec<PortDef>) -> Self {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        Self {
            port_table,
            ports: Vec::new(),
            created: false,
            events_tx,
            events_rx: Some(events_rx),
            punt_installed: false,
            hostifs: HashMap::new(),
            rifs: HashMap::new(),
            routes: HashMap::new(),
            vlans: HashMap::new(),
            vlan_rifs: HashMap::new(),
            vlan_members: HashMap::new(),
            default_members: std::collections::HashSet::new(),
            pvids: HashMap::new(),
            capabilities: SaiCapabilities::all(),
            cable_diag: HashMap::new(),
            fdb_aging: 300,
            fdb_statics: HashMap::new(),
            storm: HashMap::new(),
            mirror_sessions: HashMap::new(),
            port_mirrors: HashMap::new(),
            sample_sessions: HashMap::new(),
            port_samples: HashMap::new(),
            tpids: HashMap::new(),
            forced_speeds: HashMap::new(),
            forced_duplex: HashMap::new(),
            autoneg: HashMap::new(),
            mtus: HashMap::new(),
            lags: std::collections::HashSet::new(),
            lag_members: HashMap::new(),
            stp_instances: std::collections::HashSet::new(),
            vlan_stp: HashMap::new(),
            stp_port_states: HashMap::new(),
            l2mc_groups: std::collections::HashSet::new(),
            l2mc_members: HashMap::new(),
            l2mc_entries: HashMap::new(),
            vlan_unknown_mcast: HashMap::new(),
            neighbors: HashMap::new(),
            next_hops: HashMap::new(),
            nh_groups: std::collections::HashSet::new(),
            nh_group_members: HashMap::new(),
            my_macs: HashMap::new(),
            acl_tables: HashMap::new(),
            acl_entries: HashMap::new(),
            acl_counters: HashMap::new(),
            acl_bindings: HashMap::new(),
            policers: HashMap::new(),
            trap_groups: HashMap::new(),
            traps: HashMap::new(),
            learn_limits: HashMap::new(),
            learning_off: std::collections::HashSet::new(),
            default_trap_group_policer: None,
            qos_maps: HashMap::new(),
            port_qos_maps: HashMap::new(),
            port_default_tc: HashMap::new(),
            schedulers: HashMap::new(),
            queue_schedulers: HashMap::new(),
            port_shapers: HashMap::new(),
            wreds: HashMap::new(),
            queue_wreds: HashMap::new(),
            queue_counters: HashMap::new(),
            synthetic_counters: false,
            next_oid: 0,
        }
    }

    /// The synthetic port id of the `index`-th platform port-table
    /// entry, so tests can address a port before `create_switch`.
    pub fn port_id_of(&self, index: usize) -> PortId {
        PortId(MOCK_PORT_OID_BASE + index as u64)
    }

    /// Inject the per-queue stat counters a port reports (the mock
    /// forwards nothing, so they are 0 until a test says otherwise).
    pub fn set_queue_counters(&mut self, port: PortId, queues: Vec<QueueCounters>) {
        self.queue_counters.insert(port, queues);
    }

    /// A created QoS map object's type and value list.
    pub fn qos_map(&self, map: Oid) -> Option<(QosMapType, Vec<(u8, u8)>)> {
        self.qos_maps.get(&map).cloned()
    }

    /// The map object bound on a port for one map type.
    pub fn port_qos_map(&self, port: PortId, kind: QosMapType) -> Option<Oid> {
        self.port_qos_maps.get(&(port, kind)).copied()
    }

    /// A port's default traffic class (0 until set).
    /// The forced link parameters programmed on a port: pinned speed
    /// (Mb/s), forced duplex (true = full), autoneg, and L2 MTU. `None`
    /// where nothing was pinned.
    pub fn port_link_params(
        &self,
        port: PortId,
    ) -> (Option<u32>, Option<bool>, Option<bool>, Option<u32>) {
        (
            self.forced_speeds.get(&port).copied(),
            self.forced_duplex.get(&port).copied(),
            self.autoneg.get(&port).copied(),
            self.mtus.get(&port).copied(),
        )
    }

    pub fn port_default_tc(&self, port: PortId) -> u8 {
        self.port_default_tc.get(&port).copied().unwrap_or(0)
    }

    /// A created scheduler profile's program.
    pub fn scheduler(&self, scheduler: Oid) -> Option<SchedulerSpec> {
        self.schedulers.get(&scheduler).copied()
    }

    /// The scheduler profile bound on one of a port's queues.
    pub fn queue_scheduler(&self, port: PortId, queue: u32) -> Option<Oid> {
        self.queue_schedulers.get(&(port, queue)).copied()
    }

    /// A port's shaper rate in bits/sec, when one is set.
    pub fn port_shaper(&self, port: PortId) -> Option<u64> {
        self.port_shapers.get(&port).copied()
    }

    /// A created WRED profile's program.
    pub fn wred(&self, wred: Oid) -> Option<WredSpec> {
        self.wreds.get(&wred).copied()
    }

    /// The WRED profile bound on one of a port's queues.
    pub fn queue_wred(&self, port: PortId, queue: u32) -> Option<Oid> {
        self.queue_wreds.get(&(port, queue)).copied()
    }

    /// Override the capability posture (tests: prove that commits
    /// needing an absent capability fail cleanly).
    pub fn set_capabilities(&mut self, capabilities: SaiCapabilities) {
        self.capabilities = capabilities;
    }

    /// Script the TDR result one port reports (tests: a healthy run, an
    /// open pair, a short).
    pub fn set_cable_diag(&mut self, port: PortId, pairs: Vec<CablePair>) {
        self.cable_diag.insert(port, pairs);
    }

    /// A sender that injects SAI events as if the ASIC produced them
    /// (tests: simulated FDB learns/ages/moves, learn-limit
    /// violations).
    pub fn event_injector(&self) -> mpsc::UnboundedSender<SaiEvent> {
        self.events_tx.clone()
    }

    /// The samplepacket sessions the mock holds: (oid, 1-in-N rate).
    pub fn sample_sessions(&self) -> Vec<(Oid, u32)> {
        let mut sessions: Vec<(Oid, u32)> = self
            .sample_sessions
            .iter()
            .map(|(oid, rate)| (*oid, *rate))
            .collect();
        sessions.sort_by_key(|(oid, _)| oid.0);
        sessions
    }

    /// Which ports are bound to a sampling session, and to which.
    pub fn port_sample_bindings(&self) -> Vec<(PortId, Oid)> {
        let mut bindings: Vec<(PortId, Oid)> = self
            .port_samples
            .iter()
            .map(|(port, oid)| (*port, *oid))
            .collect();
        bindings.sort_by_key(|(port, _)| port.0);
        bindings
    }

    /// Deliver one hardware-sampled packet, as the punt path would.
    /// Ports with no sampling session bound drop it, so a test cannot
    /// accidentally sample an unbound port.
    pub fn inject_sampled_packet(
        &self,
        port: PortId,
        original_length: u32,
        bytes: Vec<u8>,
    ) -> bool {
        if !self.port_samples.contains_key(&port) {
            return false;
        }
        self.events_tx
            .send(SaiEvent::SampledPacket {
                port,
                original_length,
                bytes,
            })
            .is_ok()
    }

    /// Make ACL match counters and policer stats advance on every read
    /// (tests: clear-counters baselines, CoPP accumulation). The mock
    /// forwards nothing, so counters honestly stay 0 unless asked.
    pub fn enable_synthetic_counters(&mut self) {
        self.synthetic_counters = true;
    }

    /// The synthetic OID the mock reports for a VLAN number (the
    /// default VLAN included), for building injected FDB events.
    pub fn vlan_oid_of(&self, vlan: u16) -> Option<u64> {
        if vlan == DEFAULT_VLAN {
            return Some(MOCK_DEFAULT_VLAN_OID);
        }
        self.vlans.get(&vlan).map(|oid| oid.0)
    }

    fn port_mut(&mut self, id: PortId) -> Result<&mut SaiPort, SaiError> {
        self.ports
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or(SaiError::UnknownPort(id))
    }

    fn require_switch(&self) -> Result<(), SaiError> {
        if self.created {
            Ok(())
        } else {
            Err(SaiError::NoSwitch)
        }
    }

    fn require_port(&self, id: PortId) -> Result<(), SaiError> {
        if self.ports.iter().any(|p| p.id == id) {
            Ok(())
        } else {
            Err(SaiError::UnknownPort(id))
        }
    }

    /// A physical port or a LAG (the L2 bridging calls accept both).
    fn require_port_like(&self, id: PortId) -> Result<(), SaiError> {
        if self.ports.iter().any(|p| p.id == id) || self.lags.contains(&id) {
            Ok(())
        } else {
            Err(SaiError::UnknownPort(id))
        }
    }

    fn alloc(&mut self, base: u64) -> Oid {
        self.next_oid += 1;
        Oid(base + self.next_oid)
    }

    /// Any router interface — a routed port's or an SVI's.
    fn rif_exists(&self, rif: Oid) -> bool {
        self.rifs.values().any(|r| *r == rif) || self.vlan_rifs.contains_key(&rif)
    }

    /// The pinned SAI's posture on v6: reject when the probed
    /// capability says unsupported, like the vendor library would.
    fn require_ipv6(&self, ip: IpAddr) -> Result<(), SaiError> {
        if ip.is_ipv6() && !self.capabilities.ipv6 {
            return Err(SaiError::Other("IPv6 is not supported".into()));
        }
        Ok(())
    }

    /// An entry's counter/policer references must name live objects in
    /// the same table's world.
    fn require_acl_action_refs(&self, table: Oid, action: &AclAction) -> Result<(), SaiError> {
        if let Some(counter) = action.counter {
            match self.acl_counters.get(&counter) {
                Some((counter_table, _)) if *counter_table == table => {}
                Some(_) => {
                    return Err(SaiError::Other(format!(
                        "ACL counter {counter} belongs to another table"
                    )))
                }
                None => return Err(SaiError::Other(format!("no such ACL counter {counter}"))),
            }
        }
        if let Some(policer) = action.policer {
            if !self.policers.contains_key(&policer) {
                return Err(SaiError::Other(format!("no such policer {policer}")));
            }
        }
        Ok(())
    }

    /// TCAM slices consumed at a stage (IPv6 entries are double-wide).
    fn acl_entries_used(&self, stage: AclStage) -> u32 {
        self.acl_entries
            .values()
            .filter_map(|(table, ..)| self.acl_tables.get(table))
            .filter(|(table_stage, _)| *table_stage == stage)
            .map(|(_, family)| if *family == AclFamily::Ipv6 { 2 } else { 1 })
            .sum()
    }

    fn acl_capacity(&self, stage: AclStage) -> u32 {
        match stage {
            AclStage::Ingress => MOCK_ACL_INGRESS_CAPACITY,
            AclStage::Egress => MOCK_ACL_EGRESS_CAPACITY,
        }
    }

    /// The capability posture on qos maps, per direction — a mock
    /// configured without them refuses the calls like a vendor blob
    /// missing the attribute would.
    fn require_qos_map(&self, kind: QosMapType) -> Result<(), SaiError> {
        let supported = if kind.ingress() {
            self.capabilities.qos_map_ingress
        } else {
            self.capabilities.qos_map_egress
        };
        if supported {
            Ok(())
        } else {
            Err(SaiError::Other(format!(
                "{} QoS maps are not supported",
                if kind.ingress() { "ingress" } else { "egress" }
            )))
        }
    }
}

impl SaiBackend for MockSai {
    fn name(&self) -> String {
        "mock".into()
    }

    fn create_switch(&mut self) -> Result<SwitchInfo, SaiError> {
        if self.created {
            return Err(SaiError::Other("switch already created".into()));
        }
        self.created = true;
        self.ports = self
            .port_table
            .iter()
            .enumerate()
            .map(|(i, def)| SaiPort {
                id: PortId(MOCK_PORT_OID_BASE + i as u64),
                lanes: def.lanes.clone(),
                speed_mbps: def.speed_mbps,
                admin_up: false,
                oper_up: false,
            })
            .collect();
        // Like the real ASIC: every port boots as an untagged member of
        // the default VLAN with a matching PVID.
        for port in &self.ports {
            self.default_members.insert(port.id);
            self.pvids.insert(port.id, DEFAULT_VLAN);
        }
        Ok(SwitchInfo {
            oid: MOCK_SWITCH_OID,
            default_vlan_oid: MOCK_DEFAULT_VLAN_OID,
        })
    }

    fn ports(&mut self) -> Result<Vec<SaiPort>, SaiError> {
        if !self.created {
            return Err(SaiError::NoSwitch);
        }
        Ok(self.ports.clone())
    }

    fn set_port_admin_state(&mut self, id: PortId, up: bool) -> Result<(), SaiError> {
        if !self.created {
            return Err(SaiError::NoSwitch);
        }
        let tx = self.events_tx.clone();
        let port = self.port_mut(id)?;
        port.admin_up = up;
        // The mock's links follow admin state; notify like the real ASIC
        // would (from a callback thread, hence the channel).
        if port.oper_up != up {
            port.oper_up = up;
            let _ = tx.send(SaiEvent::PortOperStatus { port: id, up });
        }
        Ok(())
    }

    fn port_counters(&mut self, id: PortId) -> Result<PortCounters, SaiError> {
        if !self.created {
            return Err(SaiError::NoSwitch);
        }
        // The mock ASIC forwards nothing, so its counters honestly read 0.
        self.port_mut(id)?;
        Ok(PortCounters::default())
    }

    fn port_queue_counters(&mut self, id: PortId) -> Result<Vec<QueueCounters>, SaiError> {
        if !self.created {
            return Err(SaiError::NoSwitch);
        }
        self.port_mut(id)?;
        Ok(self.queue_counters.get(&id).cloned().unwrap_or_default())
    }

    fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<SaiEvent>> {
        self.events_rx.take()
    }

    fn setup_host_punt(&mut self) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.punt_installed {
            return Err(SaiError::Other("host punt already installed".into()));
        }
        self.punt_installed = true;
        Ok(())
    }

    fn create_hostif(&mut self, port: PortId, name: &str) -> Result<Oid, SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        if self.hostifs.contains_key(&port) {
            return Err(SaiError::Other(format!("port {port} already has a hostif")));
        }
        let oid = self.alloc(MOCK_HOSTIF_OID_BASE);
        self.hostifs.insert(port, (oid, name.to_string()));
        Ok(oid)
    }

    fn create_router_interface(&mut self, port: PortId) -> Result<Oid, SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        if self.rifs.contains_key(&port) {
            return Err(SaiError::Other(format!("port {port} already has a RIF")));
        }
        // Routing a port pulls it out of the 802.1Q bridge (the vendor
        // path removes its bridge port).
        self.default_members.remove(&port);
        let oid = self.alloc(MOCK_RIF_OID_BASE);
        self.rifs.insert(port, oid);
        Ok(oid)
    }

    fn remove_router_interface(&mut self, port: PortId, rif: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        match self.rifs.get(&port) {
            Some(have) if *have == rif => {
                self.rifs.remove(&port);
                // Back to default L2 bridging.
                self.default_members.insert(port);
                self.pvids.insert(port, DEFAULT_VLAN);
                Ok(())
            }
            _ => Err(SaiError::Other(format!("port {port} has no RIF {rif}"))),
        }
    }

    fn create_vlan_router_interface(&mut self, vlan: Option<Oid>) -> Result<Oid, SaiError> {
        self.require_switch()?;
        if let Some(vlan) = vlan {
            if !self.vlans.values().any(|o| *o == vlan) {
                return Err(SaiError::Other(format!("no such VLAN {vlan}")));
            }
        }
        if self.vlan_rifs.values().any(|v| *v == vlan) {
            return Err(SaiError::Other(format!("VLAN {vlan:?} already has a RIF")));
        }
        let oid = self.alloc(MOCK_RIF_OID_BASE);
        self.vlan_rifs.insert(oid, vlan);
        Ok(oid)
    }

    fn remove_vlan_router_interface(&mut self, rif: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.vlan_rifs.remove(&rif).is_none() {
            return Err(SaiError::Other(format!("no such VLAN RIF {rif}")));
        }
        Ok(())
    }

    fn create_route(&mut self, dest: IpPrefix, target: RouteTarget) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_ipv6(dest.0)?;
        match target {
            RouteTarget::Rif(rif) if !self.rif_exists(rif) => {
                return Err(SaiError::Other(format!("no such RIF {rif}")));
            }
            RouteTarget::NextHop(next_hop) if !self.next_hops.contains_key(&next_hop) => {
                return Err(SaiError::Other(format!("no such next hop {next_hop}")));
            }
            RouteTarget::Group(group) if !self.nh_groups.contains(&group) => {
                return Err(SaiError::Other(format!("no such next-hop group {group}")));
            }
            _ => {}
        }
        // Like the real ASIC: creating an existing destination fails
        // (callers replace via remove + create).
        if self.routes.insert(dest, target).is_some() {
            return Err(SaiError::Other(format!(
                "route {}/{} exists",
                dest.0, dest.1
            )));
        }
        Ok(())
    }

    fn create_neighbor(&mut self, rif: Oid, ip: IpAddr, mac: [u8; 6]) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_ipv6(ip)?;
        if !self.rif_exists(rif) {
            return Err(SaiError::Other(format!("no such RIF {rif}")));
        }
        self.neighbors.insert((rif, ip), mac);
        Ok(())
    }

    fn remove_neighbor(&mut self, rif: Oid, ip: IpAddr) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.neighbors.remove(&(rif, ip)).is_none() {
            return Err(SaiError::Other(format!("no neighbor {ip} on {rif}")));
        }
        Ok(())
    }

    fn create_next_hop(&mut self, rif: Oid, ip: IpAddr) -> Result<Oid, SaiError> {
        self.require_switch()?;
        self.require_ipv6(ip)?;
        if !self.rif_exists(rif) {
            return Err(SaiError::Other(format!("no such RIF {rif}")));
        }
        let oid = self.alloc(MOCK_NEXT_HOP_OID_BASE);
        self.next_hops.insert(oid, (rif, ip));
        Ok(oid)
    }

    fn remove_next_hop(&mut self, next_hop: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self
            .routes
            .values()
            .any(|t| *t == RouteTarget::NextHop(next_hop))
        {
            return Err(SaiError::Other(format!(
                "next hop {next_hop} still routed to"
            )));
        }
        if self.nh_group_members.values().any(|(_, n)| *n == next_hop) {
            return Err(SaiError::Other(format!(
                "next hop {next_hop} still a group member"
            )));
        }
        if self.next_hops.remove(&next_hop).is_none() {
            return Err(SaiError::Other(format!("no such next hop {next_hop}")));
        }
        Ok(())
    }

    fn create_next_hop_group(&mut self) -> Result<Oid, SaiError> {
        self.require_switch()?;
        if self.capabilities.ecmp_width == 0 {
            return Err(SaiError::Other("next-hop groups are not supported".into()));
        }
        let oid = self.alloc(MOCK_NHG_OID_BASE);
        self.nh_groups.insert(oid);
        Ok(oid)
    }

    fn remove_next_hop_group(&mut self, group: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.nh_group_members.values().any(|(g, _)| *g == group) {
            return Err(SaiError::Other(format!("group {group} still has members")));
        }
        if self
            .routes
            .values()
            .any(|t| *t == RouteTarget::Group(group))
        {
            return Err(SaiError::Other(format!("group {group} still routed to")));
        }
        if !self.nh_groups.remove(&group) {
            return Err(SaiError::Other(format!("no such next-hop group {group}")));
        }
        Ok(())
    }

    fn add_next_hop_group_member(&mut self, group: Oid, next_hop: Oid) -> Result<Oid, SaiError> {
        self.require_switch()?;
        if !self.nh_groups.contains(&group) {
            return Err(SaiError::Other(format!("no such next-hop group {group}")));
        }
        if !self.next_hops.contains_key(&next_hop) {
            return Err(SaiError::Other(format!("no such next hop {next_hop}")));
        }
        let width = self
            .nh_group_members
            .values()
            .filter(|(g, _)| *g == group)
            .count() as u32;
        if width >= self.capabilities.ecmp_width {
            return Err(SaiError::Other(format!(
                "group {group} is at the ECMP width limit ({})",
                self.capabilities.ecmp_width
            )));
        }
        let member = self.alloc(MOCK_NHG_MEMBER_OID_BASE);
        self.nh_group_members.insert(member, (group, next_hop));
        Ok(member)
    }

    fn remove_next_hop_group_member(&mut self, member: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.nh_group_members.remove(&member).is_none() {
            return Err(SaiError::Other(format!("no such group member {member}")));
        }
        Ok(())
    }

    fn create_my_mac(&mut self, vlan_id: Option<u16>, mac: [u8; 6]) -> Result<Oid, SaiError> {
        self.require_switch()?;
        if !self.capabilities.my_mac {
            return Err(SaiError::Other("My-MAC entries are not supported".into()));
        }
        if let Some(vlan) = vlan_id {
            if vlan != DEFAULT_VLAN && !self.vlans.contains_key(&vlan) {
                return Err(SaiError::Other(format!("no such VLAN {vlan}")));
            }
        }
        let oid = self.alloc(MOCK_MY_MAC_OID_BASE);
        self.my_macs.insert(oid, (vlan_id, mac));
        Ok(oid)
    }

    fn remove_my_mac(&mut self, my_mac: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.my_macs.remove(&my_mac).is_none() {
            return Err(SaiError::Other(format!("no such My-MAC entry {my_mac}")));
        }
        Ok(())
    }

    fn create_vlan(&mut self, vlan_id: u16) -> Result<Oid, SaiError> {
        self.require_switch()?;
        if vlan_id == DEFAULT_VLAN || self.vlans.contains_key(&vlan_id) {
            return Err(SaiError::Other(format!("VLAN {vlan_id} already exists")));
        }
        let oid = self.alloc(MOCK_VLAN_OID_BASE);
        self.vlans.insert(vlan_id, oid);
        Ok(oid)
    }

    fn remove_vlan(&mut self, vlan: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.vlan_members.values().any(|(v, _, _)| *v == vlan) {
            return Err(SaiError::Other(format!("VLAN {vlan} still has members")));
        }
        if self.vlan_rifs.values().any(|v| *v == Some(vlan)) {
            return Err(SaiError::Other(format!("VLAN {vlan} still has a RIF")));
        }
        let Some(id) = self
            .vlans
            .iter()
            .find(|(_, o)| **o == vlan)
            .map(|(id, _)| *id)
        else {
            return Err(SaiError::Other(format!("no such VLAN {vlan}")));
        };
        self.vlans.remove(&id);
        Ok(())
    }

    fn add_vlan_member(&mut self, vlan: Oid, port: PortId, tagged: bool) -> Result<Oid, SaiError> {
        self.require_switch()?;
        self.require_port_like(port)?;
        if self.rifs.contains_key(&port) {
            return Err(SaiError::Other(format!(
                "port {port} is routed (no bridge port)"
            )));
        }
        if !self.vlans.values().any(|o| *o == vlan) {
            return Err(SaiError::Other(format!("no such VLAN {vlan}")));
        }
        if self
            .vlan_members
            .values()
            .any(|(v, p, _)| *v == vlan && *p == port)
        {
            return Err(SaiError::Other(format!(
                "port {port} is already a member of VLAN {vlan}"
            )));
        }
        let member = self.alloc(MOCK_VLAN_MEMBER_OID_BASE);
        self.vlan_members.insert(member, (vlan, port, tagged));
        Ok(member)
    }

    fn remove_vlan_member(&mut self, member: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.vlan_members.remove(&member).is_none() {
            return Err(SaiError::Other(format!("no such VLAN member {member}")));
        }
        Ok(())
    }

    fn set_port_pvid(&mut self, port: PortId, vlan_number: u16) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port_like(port)?;
        self.pvids.insert(port, vlan_number);
        Ok(())
    }

    fn remove_port_default_vlan(&mut self, port: PortId) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port_like(port)?;
        self.default_members.remove(&port);
        Ok(())
    }

    fn restore_port_default_vlan(&mut self, port: PortId) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port_like(port)?;
        if self.rifs.contains_key(&port) {
            return Err(SaiError::Other(format!(
                "port {port} is routed (no bridge port)"
            )));
        }
        self.default_members.insert(port);
        self.pvids.insert(port, DEFAULT_VLAN);
        Ok(())
    }

    fn remove_route(&mut self, dest: IpPrefix) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.routes.remove(&dest).is_none() {
            return Err(SaiError::Other(format!("no route {}/{}", dest.0, dest.1)));
        }
        Ok(())
    }

    fn capabilities(&mut self) -> Result<SaiCapabilities, SaiError> {
        self.require_switch()?;
        Ok(self.capabilities)
    }

    fn set_fdb_aging(&mut self, secs: u32) -> Result<(), SaiError> {
        self.require_switch()?;
        self.fdb_aging = secs;
        Ok(())
    }

    fn add_fdb_entry(
        &mut self,
        vlan: Option<Oid>,
        mac: [u8; 6],
        action: FdbAction,
    ) -> Result<(), SaiError> {
        self.require_switch()?;
        if let Some(vlan) = vlan {
            if !self.vlans.values().any(|o| *o == vlan) {
                return Err(SaiError::Other(format!("no such VLAN {vlan}")));
            }
        }
        if let FdbAction::Forward(port) = action {
            self.require_port_like(port)?;
        }
        self.fdb_statics.insert((vlan, mac), action);
        Ok(())
    }

    fn remove_fdb_entry(&mut self, vlan: Option<Oid>, mac: [u8; 6]) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.fdb_statics.remove(&(vlan, mac)).is_none() {
            return Err(SaiError::Other(format!(
                "no static FDB entry for {mac:02x?} in VLAN {vlan:?}"
            )));
        }
        Ok(())
    }

    fn flush_fdb(&mut self, _vlan: Option<Oid>, port: Option<PortId>) -> Result<(), SaiError> {
        self.require_switch()?;
        if let Some(port) = port {
            self.require_port(port)?;
        }
        // The mock forwards nothing, so it holds no dynamic entries to
        // flush; validating the scope is the behavior under test.
        Ok(())
    }

    fn set_port_storm_control(
        &mut self,
        port: PortId,
        class: StormClass,
        kbps: Option<u64>,
    ) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        match kbps {
            Some(kbps) => {
                self.storm.insert((port, class), kbps);
            }
            None => {
                self.storm.remove(&(port, class));
            }
        }
        Ok(())
    }

    fn port_storm_drops(&mut self, port: PortId, _class: StormClass) -> Result<u64, SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        // The mock forwards nothing, so its policers honestly drop 0.
        Ok(0)
    }

    fn create_mirror_session(&mut self, monitor: PortId) -> Result<Oid, SaiError> {
        self.require_switch()?;
        self.require_port(monitor)?;
        if self.mirror_sessions.len() as u32 >= self.capabilities.mirror_sessions_max {
            return Err(SaiError::Other("mirror session table full".into()));
        }
        let oid = self.alloc(MOCK_MIRROR_OID_BASE);
        self.mirror_sessions.insert(oid, monitor);
        Ok(oid)
    }

    fn remove_mirror_session(&mut self, session: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self
            .port_mirrors
            .values()
            .any(|(rx, tx)| *rx == Some(session) || *tx == Some(session))
        {
            return Err(SaiError::Other(format!(
                "mirror session {session} still has attached ports"
            )));
        }
        if self.mirror_sessions.remove(&session).is_none() {
            return Err(SaiError::Other(format!("no such mirror session {session}")));
        }
        Ok(())
    }

    fn set_port_mirror(
        &mut self,
        port: PortId,
        ingress: Option<Oid>,
        egress: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        for session in [ingress, egress].into_iter().flatten() {
            if !self.mirror_sessions.contains_key(&session) {
                return Err(SaiError::Other(format!("no such mirror session {session}")));
            }
        }
        if ingress.is_none() && egress.is_none() {
            self.port_mirrors.remove(&port);
        } else {
            self.port_mirrors.insert(port, (ingress, egress));
        }
        Ok(())
    }

    fn create_samplepacket(&mut self, rate: u32) -> Result<Oid, SaiError> {
        self.require_switch()?;
        if !self.capabilities.sflow {
            return Err(SaiError::Other("samplepacket is not supported".into()));
        }
        if rate == 0 {
            return Err(SaiError::Other("sample rate must be at least 1".into()));
        }
        let oid = self.alloc(MOCK_SAMPLE_OID_BASE);
        self.sample_sessions.insert(oid, rate);
        Ok(oid)
    }

    fn run_cable_diag(&mut self, port: PortId) -> Result<Vec<CablePair>, SaiError> {
        self.require_switch()?;
        if !self.capabilities.cable_diag {
            return Err(SaiError::Other(
                "cable diagnostics are not supported".into(),
            ));
        }
        if !self.ports.iter().any(|existing| existing.id == port) {
            return Err(SaiError::Other(format!("no such port {port:?}")));
        }
        Ok(self
            .cable_diag
            .get(&port)
            .cloned()
            .unwrap_or_else(default_cable_diag))
    }

    fn remove_samplepacket(&mut self, session: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.port_samples.values().any(|bound| *bound == session) {
            return Err(SaiError::Other(format!(
                "sample session {session} still has bound ports"
            )));
        }
        if self.sample_sessions.remove(&session).is_none() {
            return Err(SaiError::Other(format!("no such sample session {session}")));
        }
        Ok(())
    }

    fn set_port_sample_session(
        &mut self,
        port: PortId,
        session: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        match session {
            Some(session) => {
                if !self.sample_sessions.contains_key(&session) {
                    return Err(SaiError::Other(format!("no such sample session {session}")));
                }
                self.port_samples.insert(port, session);
            }
            None => {
                self.port_samples.remove(&port);
            }
        }
        Ok(())
    }

    fn set_port_tpid(&mut self, port: PortId, tpid: u16) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        if tpid == 0x8100 {
            self.tpids.remove(&port);
        } else {
            self.tpids.insert(port, tpid);
        }
        Ok(())
    }

    fn set_port_speed(&mut self, port: PortId, speed_mbps: u32) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        self.forced_speeds.insert(port, speed_mbps);
        // The reported port speed follows the pin, as the ASIC's would.
        if let Some(p) = self.ports.iter_mut().find(|p| p.id == port) {
            p.speed_mbps = speed_mbps;
        }
        Ok(())
    }

    fn set_port_duplex(&mut self, port: PortId, full: bool) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        self.forced_duplex.insert(port, full);
        Ok(())
    }

    fn set_port_autoneg(&mut self, port: PortId, on: bool) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        self.autoneg.insert(port, on);
        Ok(())
    }

    fn set_port_mtu(&mut self, port: PortId, mtu: u32) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        self.mtus.insert(port, mtu);
        Ok(())
    }

    fn create_lag(&mut self) -> Result<PortId, SaiError> {
        self.require_switch()?;
        let lag = PortId(self.alloc(MOCK_LAG_OID_BASE).0);
        self.lags.insert(lag);
        // Like a boot-time port: an untagged member of the default VLAN
        // with a matching PVID.
        self.default_members.insert(lag);
        self.pvids.insert(lag, DEFAULT_VLAN);
        Ok(lag)
    }

    fn remove_lag(&mut self, lag: PortId) -> Result<(), SaiError> {
        self.require_switch()?;
        if !self.lags.contains(&lag) {
            return Err(SaiError::Other(format!("no such LAG {lag}")));
        }
        if self.lag_members.values().any(|(l, _, _)| *l == lag) {
            return Err(SaiError::Other(format!("LAG {lag} still has members")));
        }
        if self.vlan_members.values().any(|(_, p, _)| *p == lag) {
            return Err(SaiError::Other(format!(
                "LAG {lag} still has VLAN memberships"
            )));
        }
        self.lags.remove(&lag);
        self.default_members.remove(&lag);
        self.pvids.remove(&lag);
        Ok(())
    }

    fn add_lag_member(&mut self, lag: PortId, port: PortId) -> Result<Oid, SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        if !self.lags.contains(&lag) {
            return Err(SaiError::Other(format!("no such LAG {lag}")));
        }
        if self.rifs.contains_key(&port) {
            return Err(SaiError::Other(format!("port {port} is routed")));
        }
        if self.lag_members.values().any(|(_, p, _)| *p == port) {
            return Err(SaiError::Other(format!(
                "port {port} is already a LAG member"
            )));
        }
        // The member's traffic rides the LAG's bridge port from now on.
        self.default_members.remove(&port);
        let member = self.alloc(MOCK_LAG_MEMBER_OID_BASE);
        self.lag_members.insert(member, (lag, port, false));
        Ok(member)
    }

    fn remove_lag_member(&mut self, member: Oid, port: PortId) -> Result<(), SaiError> {
        self.require_switch()?;
        match self.lag_members.remove(&member) {
            Some((_, had, _)) if had == port => {
                // Standalone default L2 again.
                self.default_members.insert(port);
                self.pvids.insert(port, DEFAULT_VLAN);
                Ok(())
            }
            Some(entry) => {
                self.lag_members.insert(member, entry);
                Err(SaiError::Other(format!(
                    "LAG member {member} does not front {port}"
                )))
            }
            None => Err(SaiError::Other(format!("no such LAG member {member}"))),
        }
    }

    fn set_lag_member_state(&mut self, member: Oid, enabled: bool) -> Result<(), SaiError> {
        self.require_switch()?;
        match self.lag_members.get_mut(&member) {
            Some((_, _, gate)) => {
                *gate = enabled;
                Ok(())
            }
            None => Err(SaiError::Other(format!("no such LAG member {member}"))),
        }
    }

    fn create_stp_instance(&mut self) -> Result<Oid, SaiError> {
        self.require_switch()?;
        let oid = self.alloc(MOCK_STP_OID_BASE);
        self.stp_instances.insert(oid);
        Ok(oid)
    }

    fn remove_stp_instance(&mut self, stp: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.vlan_stp.values().any(|s| *s == Some(stp)) {
            return Err(SaiError::Other(format!(
                "STP instance {stp} still has VLANs"
            )));
        }
        if !self.stp_instances.remove(&stp) {
            return Err(SaiError::Other(format!("no such STP instance {stp}")));
        }
        self.stp_port_states.retain(|(s, _), _| *s != Some(stp));
        Ok(())
    }

    fn set_vlan_stp_instance(
        &mut self,
        vlan: Option<Oid>,
        stp: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.require_switch()?;
        if let Some(vlan) = vlan {
            if !self.vlans.values().any(|o| *o == vlan) {
                return Err(SaiError::Other(format!("no such VLAN {vlan}")));
            }
        }
        if let Some(stp) = stp {
            if !self.stp_instances.contains(&stp) {
                return Err(SaiError::Other(format!("no such STP instance {stp}")));
            }
        }
        self.vlan_stp.insert(vlan, stp);
        Ok(())
    }

    fn set_stp_port_state(
        &mut self,
        stp: Option<Oid>,
        port: PortId,
        state: StpPortState,
    ) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port_like(port)?;
        if let Some(stp) = stp {
            if !self.stp_instances.contains(&stp) {
                return Err(SaiError::Other(format!("no such STP instance {stp}")));
            }
        }
        self.stp_port_states.insert((stp, port), state);
        Ok(())
    }

    fn create_l2mc_group(&mut self) -> Result<Oid, SaiError> {
        self.require_switch()?;
        let oid = self.alloc(MOCK_L2MC_OID_BASE);
        self.l2mc_groups.insert(oid);
        Ok(oid)
    }

    fn remove_l2mc_group(&mut self, group: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.l2mc_members.values().any(|(g, _)| *g == group) {
            return Err(SaiError::Other(format!("L2MC group {group} has members")));
        }
        if self.l2mc_entries.values().any(|g| *g == group)
            || self.vlan_unknown_mcast.values().any(|g| *g == group)
        {
            return Err(SaiError::Other(format!(
                "L2MC group {group} is still referenced"
            )));
        }
        if !self.l2mc_groups.remove(&group) {
            return Err(SaiError::Other(format!("no such L2MC group {group}")));
        }
        Ok(())
    }

    fn add_l2mc_member(&mut self, group: Oid, port: PortId) -> Result<Oid, SaiError> {
        self.require_switch()?;
        self.require_port_like(port)?;
        if !self.l2mc_groups.contains(&group) {
            return Err(SaiError::Other(format!("no such L2MC group {group}")));
        }
        if self
            .l2mc_members
            .values()
            .any(|(g, p)| *g == group && *p == port)
        {
            return Err(SaiError::Other(format!(
                "port {port} is already in L2MC group {group}"
            )));
        }
        let member = self.alloc(MOCK_L2MC_MEMBER_OID_BASE);
        self.l2mc_members.insert(member, (group, port));
        Ok(member)
    }

    fn remove_l2mc_member(&mut self, member: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.l2mc_members.remove(&member).is_none() {
            return Err(SaiError::Other(format!("no such L2MC member {member}")));
        }
        Ok(())
    }

    fn set_l2mc_entry(
        &mut self,
        vlan: Option<Oid>,
        group_ip: IpAddr,
        l2mc: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.require_switch()?;
        if let Some(vlan) = vlan {
            if !self.vlans.values().any(|o| *o == vlan) {
                return Err(SaiError::Other(format!("no such VLAN {vlan}")));
            }
        }
        match l2mc {
            Some(group) => {
                if !self.l2mc_groups.contains(&group) {
                    return Err(SaiError::Other(format!("no such L2MC group {group}")));
                }
                self.l2mc_entries.insert((vlan, group_ip), group);
            }
            None => {
                if self.l2mc_entries.remove(&(vlan, group_ip)).is_none() {
                    return Err(SaiError::Other(format!(
                        "no L2MC entry for {group_ip} in VLAN {vlan:?}"
                    )));
                }
            }
        }
        Ok(())
    }

    fn set_vlan_unknown_mcast_group(
        &mut self,
        vlan: Option<Oid>,
        l2mc: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.require_switch()?;
        if let Some(vlan) = vlan {
            if !self.vlans.values().any(|o| *o == vlan) {
                return Err(SaiError::Other(format!("no such VLAN {vlan}")));
            }
        }
        match l2mc {
            Some(group) => {
                if !self.l2mc_groups.contains(&group) {
                    return Err(SaiError::Other(format!("no such L2MC group {group}")));
                }
                self.vlan_unknown_mcast.insert(vlan, group);
            }
            None => {
                self.vlan_unknown_mcast.remove(&vlan);
            }
        }
        Ok(())
    }

    fn create_acl_table(&mut self, stage: AclStage, family: AclFamily) -> Result<Oid, SaiError> {
        self.require_switch()?;
        let oid = self.alloc(MOCK_ACL_TABLE_OID_BASE);
        self.acl_tables.insert(oid, (stage, family));
        Ok(oid)
    }

    fn remove_acl_table(&mut self, table: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if !self.acl_tables.contains_key(&table) {
            return Err(SaiError::Other(format!("no such ACL table {table}")));
        }
        // Like the real ASIC: a table with live entries, counters, or
        // bindings refuses removal.
        if self.acl_entries.values().any(|(t, ..)| *t == table) {
            return Err(SaiError::Other(format!("ACL table {table} has entries")));
        }
        if self.acl_counters.values().any(|(t, _)| *t == table) {
            return Err(SaiError::Other(format!("ACL table {table} has counters")));
        }
        if self.acl_bindings.values().any(|t| *t == table) {
            return Err(SaiError::Other(format!("ACL table {table} is bound")));
        }
        self.acl_tables.remove(&table);
        Ok(())
    }

    fn create_acl_entry(
        &mut self,
        table: Oid,
        priority: u32,
        fields: &AclFields,
        action: &AclAction,
    ) -> Result<Oid, SaiError> {
        self.require_switch()?;
        let Some((stage, family)) = self.acl_tables.get(&table).copied() else {
            return Err(SaiError::Other(format!("no such ACL table {table}")));
        };
        self.require_acl_action_refs(table, action)?;
        // TCAM capacity: refuse past the stage's slice budget, like
        // the real ContentAware would (TABLE_FULL).
        let width = if family == AclFamily::Ipv6 { 2 } else { 1 };
        if self.acl_entries_used(stage) + width > self.acl_capacity(stage) {
            return Err(SaiError::Status {
                call: "create_acl_entry",
                status: -12, // SAI_STATUS_TABLE_FULL
            });
        }
        let oid = self.alloc(MOCK_ACL_ENTRY_OID_BASE);
        self.acl_entries
            .insert(oid, (table, priority, fields.clone(), *action));
        Ok(oid)
    }

    fn set_acl_entry_action(&mut self, entry: Oid, action: &AclAction) -> Result<(), SaiError> {
        self.require_switch()?;
        let Some(&(table, ..)) = self.acl_entries.get(&entry) else {
            return Err(SaiError::Other(format!("no such ACL entry {entry}")));
        };
        self.require_acl_action_refs(table, action)?;
        if let Some((.., stored)) = self.acl_entries.get_mut(&entry) {
            *stored = *action;
        }
        Ok(())
    }

    fn remove_acl_entry(&mut self, entry: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.acl_entries.remove(&entry).is_none() {
            return Err(SaiError::Other(format!("no such ACL entry {entry}")));
        }
        Ok(())
    }

    fn create_acl_counter(&mut self, table: Oid) -> Result<Oid, SaiError> {
        self.require_switch()?;
        if !self.acl_tables.contains_key(&table) {
            return Err(SaiError::Other(format!("no such ACL table {table}")));
        }
        let oid = self.alloc(MOCK_ACL_COUNTER_OID_BASE);
        self.acl_counters.insert(oid, (table, 0));
        Ok(oid)
    }

    fn remove_acl_counter(&mut self, counter: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if !self.acl_counters.contains_key(&counter) {
            return Err(SaiError::Other(format!("no such ACL counter {counter}")));
        }
        if self
            .acl_entries
            .values()
            .any(|(.., action)| action.counter == Some(counter))
        {
            return Err(SaiError::Other(format!(
                "ACL counter {counter} is still referenced"
            )));
        }
        self.acl_counters.remove(&counter);
        Ok(())
    }

    fn get_acl_counter(&mut self, counter: Oid) -> Result<u64, SaiError> {
        self.require_switch()?;
        let synthetic = self.synthetic_counters;
        let Some((_, packets)) = self.acl_counters.get_mut(&counter) else {
            return Err(SaiError::Other(format!("no such ACL counter {counter}")));
        };
        if synthetic {
            *packets += 7;
        }
        Ok(*packets)
    }

    fn bind_port_acl(
        &mut self,
        port: PortId,
        stage: AclStage,
        table: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        match table {
            Some(table) => {
                let Some((table_stage, _)) = self.acl_tables.get(&table) else {
                    return Err(SaiError::Other(format!("no such ACL table {table}")));
                };
                if *table_stage != stage {
                    return Err(SaiError::Other(format!(
                        "ACL table {table} is not a {stage:?} table"
                    )));
                }
                self.acl_bindings.insert((port, stage), table);
            }
            None => {
                self.acl_bindings.remove(&(port, stage));
            }
        }
        Ok(())
    }

    fn acl_available_entries(&mut self, stage: AclStage) -> Result<u32, SaiError> {
        self.require_switch()?;
        Ok(self.acl_capacity(stage) - self.acl_entries_used(stage))
    }

    fn create_policer(&mut self, spec: PolicerSpec) -> Result<Oid, SaiError> {
        self.require_switch()?;
        let oid = self.alloc(MOCK_POLICER_OID_BASE);
        self.policers.insert(oid, (spec, PolicerStats::default()));
        Ok(oid)
    }

    fn set_policer(&mut self, policer: Oid, spec: PolicerSpec) -> Result<(), SaiError> {
        self.require_switch()?;
        let Some((stored, _)) = self.policers.get_mut(&policer) else {
            return Err(SaiError::Other(format!("no such policer {policer}")));
        };
        *stored = spec;
        Ok(())
    }

    fn remove_policer(&mut self, policer: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if !self.policers.contains_key(&policer) {
            return Err(SaiError::Other(format!("no such policer {policer}")));
        }
        if self.trap_groups.values().any(|p| *p == Some(policer))
            || self.default_trap_group_policer == Some(policer)
        {
            return Err(SaiError::Other(format!(
                "policer {policer} is attached to a trap group"
            )));
        }
        if self
            .acl_entries
            .values()
            .any(|(.., action)| action.policer == Some(policer))
        {
            return Err(SaiError::Other(format!(
                "policer {policer} is attached to an ACL entry"
            )));
        }
        self.policers.remove(&policer);
        Ok(())
    }

    fn policer_stats(&mut self, policer: Oid) -> Result<PolicerStats, SaiError> {
        self.require_switch()?;
        let synthetic = self.synthetic_counters;
        let Some((_, stats)) = self.policers.get_mut(&policer) else {
            return Err(SaiError::Other(format!("no such policer {policer}")));
        };
        if synthetic {
            stats.conforming += 11;
            stats.dropped += 3;
        }
        Ok(*stats)
    }

    fn create_hostif_trap_group(&mut self, policer: Option<Oid>) -> Result<Oid, SaiError> {
        self.require_switch()?;
        if let Some(policer) = policer {
            if !self.policers.contains_key(&policer) {
                return Err(SaiError::Other(format!("no such policer {policer}")));
            }
        }
        let oid = self.alloc(MOCK_TRAP_GROUP_OID_BASE);
        self.trap_groups.insert(oid, policer);
        Ok(oid)
    }

    fn remove_hostif_trap_group(&mut self, group: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if !self.trap_groups.contains_key(&group) {
            return Err(SaiError::Other(format!("no such trap group {group}")));
        }
        if self.traps.values().any(|(.., g)| *g == group) {
            return Err(SaiError::Other(format!("trap group {group} has traps")));
        }
        self.trap_groups.remove(&group);
        Ok(())
    }

    fn create_hostif_trap(
        &mut self,
        kind: TrapKind,
        trap_only: bool,
        group: Oid,
    ) -> Result<Oid, SaiError> {
        self.require_switch()?;
        // Oid(0) = the switch default trap group, which always exists.
        if group.0 != 0 && !self.trap_groups.contains_key(&group) {
            return Err(SaiError::Other(format!("no such trap group {group}")));
        }
        if self.traps.values().any(|(k, ..)| *k == kind) {
            return Err(SaiError::Other(format!("trap {kind:?} already exists")));
        }
        let oid = self.alloc(MOCK_TRAP_OID_BASE);
        self.traps.insert(oid, (kind, trap_only, group));
        Ok(oid)
    }

    fn remove_hostif_trap(&mut self, trap: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.traps.remove(&trap).is_none() {
            return Err(SaiError::Other(format!("no such trap {trap}")));
        }
        Ok(())
    }

    fn set_default_trap_group_policer(&mut self, policer: Option<Oid>) -> Result<(), SaiError> {
        self.require_switch()?;
        if let Some(policer) = policer {
            if !self.policers.contains_key(&policer) {
                return Err(SaiError::Other(format!("no such policer {policer}")));
            }
        }
        self.default_trap_group_policer = policer;
        Ok(())
    }

    fn create_qos_map(&mut self, kind: QosMapType, entries: &[(u8, u8)]) -> Result<Oid, SaiError> {
        self.require_switch()?;
        self.require_qos_map(kind)?;
        let oid = self.alloc(MOCK_QOS_MAP_OID_BASE);
        self.qos_maps.insert(oid, (kind, entries.to_vec()));
        Ok(oid)
    }

    fn set_qos_map(&mut self, map: Oid, entries: &[(u8, u8)]) -> Result<(), SaiError> {
        self.require_switch()?;
        let Some((_, values)) = self.qos_maps.get_mut(&map) else {
            return Err(SaiError::Other(format!("no such QoS map {map}")));
        };
        *values = entries.to_vec();
        Ok(())
    }

    fn remove_qos_map(&mut self, map: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.port_qos_maps.values().any(|bound| *bound == map) {
            return Err(SaiError::Other(format!("QoS map {map} is still bound")));
        }
        if self.qos_maps.remove(&map).is_none() {
            return Err(SaiError::Other(format!("no such QoS map {map}")));
        }
        Ok(())
    }

    fn set_port_qos_map_binding(
        &mut self,
        port: PortId,
        kind: QosMapType,
        map: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port_like(port)?;
        self.require_qos_map(kind)?;
        match map {
            Some(map) => {
                match self.qos_maps.get(&map) {
                    Some((map_kind, _)) if *map_kind == kind => {}
                    Some(_) => {
                        return Err(SaiError::Other(format!(
                            "QoS map {map} is not a {kind:?} map"
                        )))
                    }
                    None => return Err(SaiError::Other(format!("no such QoS map {map}"))),
                }
                self.port_qos_maps.insert((port, kind), map);
            }
            None => {
                self.port_qos_maps.remove(&(port, kind));
            }
        }
        Ok(())
    }

    fn set_port_default_tc(&mut self, port: PortId, tc: u8) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port_like(port)?;
        self.port_default_tc.insert(port, tc);
        Ok(())
    }

    fn create_scheduler(&mut self, spec: SchedulerSpec) -> Result<Oid, SaiError> {
        self.require_switch()?;
        if spec.max_rate_bps.is_some() && !self.capabilities.queue_shaper {
            return Err(SaiError::Other("egress shapers are not supported".into()));
        }
        let oid = self.alloc(MOCK_SCHEDULER_OID_BASE);
        self.schedulers.insert(oid, spec);
        Ok(oid)
    }

    fn remove_scheduler(&mut self, scheduler: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.queue_schedulers.values().any(|s| *s == scheduler) {
            return Err(SaiError::Other(format!(
                "scheduler {scheduler} is still bound"
            )));
        }
        if self.schedulers.remove(&scheduler).is_none() {
            return Err(SaiError::Other(format!("no such scheduler {scheduler}")));
        }
        Ok(())
    }

    fn bind_queue_scheduler(
        &mut self,
        port: PortId,
        queue: u32,
        scheduler: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port_like(port)?;
        match scheduler {
            Some(scheduler) => {
                if !self.schedulers.contains_key(&scheduler) {
                    return Err(SaiError::Other(format!("no such scheduler {scheduler}")));
                }
                self.queue_schedulers.insert((port, queue), scheduler);
            }
            None => {
                self.queue_schedulers.remove(&(port, queue));
            }
        }
        Ok(())
    }

    fn set_port_shaper(&mut self, port: PortId, rate_bps: Option<u64>) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port_like(port)?;
        match rate_bps {
            Some(rate) => self.port_shapers.insert(port, rate),
            None => self.port_shapers.remove(&port),
        };
        Ok(())
    }

    fn create_wred(&mut self, spec: WredSpec) -> Result<Oid, SaiError> {
        self.require_switch()?;
        if !self.capabilities.wred {
            return Err(SaiError::Other("WRED is not supported".into()));
        }
        if spec.ecn && !self.capabilities.ecn {
            return Err(SaiError::Other("ECN marking is not supported".into()));
        }
        let oid = self.alloc(MOCK_WRED_OID_BASE);
        self.wreds.insert(oid, spec);
        Ok(oid)
    }

    fn set_wred(&mut self, wred: Oid, spec: WredSpec) -> Result<(), SaiError> {
        self.require_switch()?;
        if spec.ecn && !self.capabilities.ecn {
            return Err(SaiError::Other("ECN marking is not supported".into()));
        }
        let Some(slot) = self.wreds.get_mut(&wred) else {
            return Err(SaiError::Other(format!("no such WRED profile {wred}")));
        };
        *slot = spec;
        Ok(())
    }

    fn remove_wred(&mut self, wred: Oid) -> Result<(), SaiError> {
        self.require_switch()?;
        if self.queue_wreds.values().any(|w| *w == wred) {
            return Err(SaiError::Other(format!(
                "WRED profile {wred} is still bound"
            )));
        }
        if self.wreds.remove(&wred).is_none() {
            return Err(SaiError::Other(format!("no such WRED profile {wred}")));
        }
        Ok(())
    }

    fn bind_queue_wred(
        &mut self,
        port: PortId,
        queue: u32,
        wred: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port_like(port)?;
        match wred {
            Some(wred) => {
                if !self.wreds.contains_key(&wred) {
                    return Err(SaiError::Other(format!("no such WRED profile {wred}")));
                }
                self.queue_wreds.insert((port, queue), wred);
            }
            None => {
                self.queue_wreds.remove(&(port, queue));
            }
        }
        Ok(())
    }

    fn set_port_learn_limit(&mut self, port: PortId, limit: Option<u32>) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        if !self.capabilities.port_learn_limit {
            // What a blob without the attribute answers (the Helix4
            // libsaibcm does exactly this): ATTR_NOT_IMPLEMENTED_0.
            return Err(SaiError::Status {
                call: "set_bridge_port_attribute(MAX_LEARNED_ADDRESSES)",
                status: -0x0003_0000,
            });
        }
        match limit {
            Some(limit) => {
                self.learn_limits.insert(port, limit);
            }
            None => {
                self.learn_limits.remove(&port);
            }
        }
        Ok(())
    }

    fn set_port_learning(&mut self, port: PortId, learn: bool) -> Result<(), SaiError> {
        self.require_switch()?;
        self.require_port(port)?;
        if learn {
            self.learning_off.remove(&port);
        } else {
            self.learning_off.insert(port);
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{AclPacketAction, FdbEventKind};

    #[test]
    fn acl_policer_and_trap_lifecycle() {
        let mut sai = MockSai::new(port_table(2));
        sai.create_switch().unwrap();
        let ports: Vec<PortId> = sai.ports().unwrap().into_iter().map(|p| p.id).collect();

        // Table + counter + entry, bound to a port.
        let table = sai
            .create_acl_table(AclStage::Ingress, AclFamily::Ipv4)
            .unwrap();
        let counter = sai.create_acl_counter(table).unwrap();
        let fields = AclFields {
            src_ip: Some(("10.0.0.0".parse().unwrap(), 8)),
            protocol: Some(6),
            dst_port: Some((443, 443)),
            ..Default::default()
        };
        let action = AclAction {
            action: AclPacketAction::Forward,
            counter: Some(counter),
            policer: None,
        };
        let entry = sai.create_acl_entry(table, 100, &fields, &action).unwrap();
        sai.bind_port_acl(ports[0], AclStage::Ingress, Some(table))
            .unwrap();
        assert_eq!(sai.acl_bindings[&(ports[0], AclStage::Ingress)], table);
        assert_eq!(sai.get_acl_counter(counter).unwrap(), 0);

        // Dependency rules, like the real ASIC: live entries, a
        // referenced counter, and a bound table all refuse removal.
        assert!(sai.remove_acl_table(table).is_err());
        assert!(sai.remove_acl_counter(counter).is_err());

        // TCAM accounting: one IPv4 entry used; IPv6 counts double.
        assert_eq!(sai.acl_available_entries(AclStage::Ingress).unwrap(), 511);
        let table6 = sai
            .create_acl_table(AclStage::Ingress, AclFamily::Ipv6)
            .unwrap();
        let deny = AclAction {
            action: AclPacketAction::Drop,
            counter: None,
            policer: None,
        };
        let entry6 = sai
            .create_acl_entry(table6, 10, &AclFields::default(), &deny)
            .unwrap();
        assert_eq!(sai.acl_available_entries(AclStage::Ingress).unwrap(), 509);
        assert_eq!(sai.acl_available_entries(AclStage::Egress).unwrap(), 256);

        // A stage-mismatched binding is refused.
        assert!(sai
            .bind_port_acl(ports[0], AclStage::Egress, Some(table))
            .is_err());

        // Policers attach to entries and trap groups; both hold a
        // reference that blocks removal.
        let policer = sai
            .create_policer(PolicerSpec {
                pps: true,
                rate: 512,
                burst: 128,
            })
            .unwrap();
        let group = sai.create_hostif_trap_group(Some(policer)).unwrap();
        let trap = sai.create_hostif_trap(TrapKind::Stp, true, group).unwrap();
        assert!(sai.create_hostif_trap(TrapKind::Stp, true, group).is_err());
        assert!(sai.remove_policer(policer).is_err());
        assert!(sai.remove_hostif_trap_group(group).is_err());
        sai.remove_hostif_trap(trap).unwrap();
        sai.remove_hostif_trap_group(group).unwrap();
        sai.set_policer(
            policer,
            PolicerSpec {
                pps: true,
                rate: 2000,
                burst: 500,
            },
        )
        .unwrap();
        assert_eq!(sai.policers[&policer].0.rate, 2000);
        sai.remove_policer(policer).unwrap();

        // Learn limits and learning state.
        sai.set_port_learn_limit(ports[1], Some(4)).unwrap();
        assert_eq!(sai.learn_limits[&ports[1]], 4);
        sai.set_port_learning(ports[1], false).unwrap();
        assert!(sai.learning_off.contains(&ports[1]));
        sai.set_port_learning(ports[1], true).unwrap();
        sai.set_port_learn_limit(ports[1], None).unwrap();
        assert!(sai.learn_limits.is_empty());

        // Ordered teardown: entry, counter, unbind, table.
        sai.remove_acl_entry(entry).unwrap();
        sai.remove_acl_counter(counter).unwrap();
        assert!(sai.remove_acl_table(table).is_err());
        sai.bind_port_acl(ports[0], AclStage::Ingress, None)
            .unwrap();
        sai.remove_acl_table(table).unwrap();
        sai.remove_acl_entry(entry6).unwrap();
        sai.remove_acl_table(table6).unwrap();
        assert_eq!(sai.acl_available_entries(AclStage::Ingress).unwrap(), 512);
    }

    fn port_table(n: usize) -> Vec<PortDef> {
        (0..n)
            .map(|i| PortDef {
                name: format!("Ethernet{i}"),
                index: i as u32 + 1,
                lanes: vec![i as u32 + 1],
                speed_mbps: 1000,
                alias: None,
                autoneg: false,
                media: None,
                breakout: vec![],
                phy_model: None,
                supported_modes: vec![],
            })
            .collect()
    }

    #[test]
    fn lifecycle_create_enumerate_admin_up() {
        let mut sai = MockSai::new(port_table(52));
        assert!(matches!(sai.ports(), Err(SaiError::NoSwitch)));

        let info = sai.create_switch().unwrap();
        assert_eq!(info.oid, MOCK_SWITCH_OID);
        assert!(sai.create_switch().is_err(), "double create must fail");

        let ports = sai.ports().unwrap();
        assert_eq!(ports.len(), 52);
        assert!(ports.iter().all(|p| !p.admin_up && !p.oper_up));

        let mut events = sai.take_events().unwrap();
        assert!(sai.take_events().is_none(), "receiver is single-take");

        let first = ports[0].id;
        sai.set_port_admin_state(first, true).unwrap();
        let ports = sai.ports().unwrap();
        assert!(ports[0].admin_up && ports[0].oper_up);

        match events.try_recv().unwrap() {
            SaiEvent::PortOperStatus { port, up } => {
                assert_eq!(port, first);
                assert!(up);
            }
            other => panic!("unexpected event {other:?}"),
        }

        // Re-applying the same state emits no duplicate event.
        sai.set_port_admin_state(first, true).unwrap();
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn unknown_port_is_an_error() {
        let mut sai = MockSai::new(port_table(1));
        sai.create_switch().unwrap();
        assert!(matches!(
            sai.set_port_admin_state(PortId(0xdead), true),
            Err(SaiError::UnknownPort(_))
        ));
    }

    #[test]
    fn fib_lifecycle_neighbors_next_hops_groups_my_mac() {
        let mut sai = MockSai::new(port_table(2));
        sai.create_switch().unwrap();
        let port = sai.ports().unwrap()[0].id;
        let rif = sai.create_router_interface(port).unwrap();

        // Neighbors replace per (rif, ip) and reject a dead RIF.
        let gw: IpAddr = "10.9.9.0".parse().unwrap();
        let mac = [0xa0, 0x36, 0x9f, 0x44, 0xbe, 0x09];
        sai.create_neighbor(rif, gw, mac).unwrap();
        sai.create_neighbor(rif, gw, mac).unwrap();
        assert!(sai.create_neighbor(Oid(0xbad), gw, mac).is_err());

        // Next hop + single-hop route.
        let nh = sai.create_next_hop(rif, gw).unwrap();
        let dest: IpAddr = "10.99.0.0".parse().unwrap();
        sai.create_route((dest, 16), RouteTarget::NextHop(nh))
            .unwrap();
        assert!(
            sai.remove_next_hop(nh).is_err(),
            "a routed-to next hop cannot be removed"
        );

        // ECMP: group of two, then teardown in dependency order.
        let gw2: IpAddr = "10.42.10.7".parse().unwrap();
        let nh2 = sai.create_next_hop(rif, gw2).unwrap();
        let group = sai.create_next_hop_group().unwrap();
        let m1 = sai.add_next_hop_group_member(group, nh).unwrap();
        let m2 = sai.add_next_hop_group_member(group, nh2).unwrap();
        sai.remove_route((dest, 16)).unwrap();
        sai.create_route((dest, 16), RouteTarget::Group(group))
            .unwrap();
        assert!(sai.remove_next_hop_group(group).is_err(), "members remain");
        sai.remove_route((dest, 16)).unwrap();
        sai.remove_next_hop_group_member(m1).unwrap();
        sai.remove_next_hop_group_member(m2).unwrap();
        sai.remove_next_hop_group(group).unwrap();
        sai.remove_next_hop(nh).unwrap();
        sai.remove_next_hop(nh2).unwrap();
        sai.remove_neighbor(rif, gw).unwrap();
        assert!(sai.remove_neighbor(rif, gw).is_err());

        // Drop routes need no target object.
        sai.create_route((dest, 16), RouteTarget::Drop).unwrap();
        sai.remove_route((dest, 16)).unwrap();

        // My-MAC entries (VRRP virtual MAC), VLAN-scoped.
        let vrrp_mac = [0x00, 0x00, 0x5e, 0x00, 0x01, 0x0a];
        let my_mac = sai.create_my_mac(Some(1), vrrp_mac).unwrap();
        sai.remove_my_mac(my_mac).unwrap();
        assert!(sai.remove_my_mac(my_mac).is_err());
        assert!(
            sai.create_my_mac(Some(100), vrrp_mac).is_err(),
            "no VLAN 100"
        );

        // Capability posture is enforced like the vendor library's.
        let mut caps = SaiCapabilities::all();
        caps.ipv6 = false;
        caps.my_mac = false;
        caps.ecmp_width = 1;
        sai.set_capabilities(caps);
        let v6: IpAddr = "2001:db8::1".parse().unwrap();
        assert!(sai.create_next_hop(rif, v6).is_err());
        assert!(sai.create_route((v6, 128), RouteTarget::Cpu).is_err());
        assert!(sai.create_my_mac(None, vrrp_mac).is_err());
        let nh = sai.create_next_hop(rif, gw).unwrap();
        let nh2 = sai.create_next_hop(rif, gw2).unwrap();
        let group = sai.create_next_hop_group().unwrap();
        sai.add_next_hop_group_member(group, nh).unwrap();
        assert!(
            sai.add_next_hop_group_member(group, nh2).is_err(),
            "the ECMP width limit holds"
        );
    }

    #[test]
    fn l3_lifecycle_rif_and_routes() {
        let mut sai = MockSai::new(port_table(2));
        assert!(matches!(sai.setup_host_punt(), Err(SaiError::NoSwitch)));
        sai.create_switch().unwrap();
        sai.setup_host_punt().unwrap();
        assert!(sai.setup_host_punt().is_err(), "punt installs once");

        let port = sai.ports().unwrap()[0].id;
        let hostif = sai.create_hostif(port, "Ethernet0").unwrap();
        assert!(hostif.0 >= MOCK_HOSTIF_OID_BASE);
        assert!(sai.create_hostif(port, "Ethernet0").is_err());

        let rif = sai.create_router_interface(port).unwrap();
        assert!(
            sai.create_router_interface(port).is_err(),
            "one RIF per port"
        );

        let addr: std::net::IpAddr = "10.42.10.9".parse().unwrap();
        let subnet: std::net::IpAddr = "10.42.10.0".parse().unwrap();
        sai.create_route((addr, 32), RouteTarget::Cpu).unwrap();
        sai.create_route((subnet, 24), RouteTarget::Rif(rif))
            .unwrap();
        assert!(
            sai.create_route((addr, 32), RouteTarget::Cpu).is_err(),
            "duplicate destination fails like the real ASIC"
        );
        assert!(
            sai.create_route((subnet, 25), RouteTarget::Rif(Oid(0xbad)))
                .is_err(),
            "routes must target a live RIF"
        );

        sai.remove_route((addr, 32)).unwrap();
        sai.remove_route((subnet, 24)).unwrap();
        assert!(sai.remove_route((subnet, 24)).is_err());

        sai.remove_router_interface(port, rif).unwrap();
        assert!(sai.remove_router_interface(port, rif).is_err());
        // Back to L2: a fresh RIF can be created again.
        sai.create_router_interface(port).unwrap();
    }

    #[test]
    fn switching_suite_families() {
        let mut sai = MockSai::new(port_table(4));
        assert!(matches!(sai.capabilities(), Err(SaiError::NoSwitch)));
        sai.create_switch().unwrap();
        assert!(sai.capabilities().unwrap().storm_control);
        let ports: Vec<PortId> = sai.ports().unwrap().iter().map(|p| p.id).collect();

        // FDB: aging, statics keyed by (vlan, mac), scoped flush.
        sai.set_fdb_aging(600).unwrap();
        assert_eq!(sai.fdb_aging, 600);
        let vlan10 = sai.create_vlan(10).unwrap();
        let mac = [0x00, 0x50, 0x56, 0xbe, 0xef, 0x01];
        sai.add_fdb_entry(Some(vlan10), mac, FdbAction::Forward(ports[0]))
            .unwrap();
        sai.add_fdb_entry(None, mac, FdbAction::Drop).unwrap();
        assert!(sai
            .add_fdb_entry(Some(Oid(0xbad)), mac, FdbAction::Drop)
            .is_err());
        assert!(sai
            .add_fdb_entry(Some(vlan10), mac, FdbAction::Forward(PortId(0xbad)))
            .is_err());
        sai.flush_fdb(Some(vlan10), Some(ports[0])).unwrap();
        assert!(sai.flush_fdb(None, Some(PortId(0xbad))).is_err());
        sai.remove_fdb_entry(Some(vlan10), mac).unwrap();
        assert!(sai.remove_fdb_entry(Some(vlan10), mac).is_err());
        sai.remove_fdb_entry(None, mac).unwrap();

        // Storm control per (port, class).
        sai.set_port_storm_control(ports[0], StormClass::Broadcast, Some(100_000))
            .unwrap();
        assert_eq!(sai.storm[&(ports[0], StormClass::Broadcast)], 100_000);
        sai.set_port_storm_control(ports[0], StormClass::Broadcast, None)
            .unwrap();
        assert!(sai.storm.is_empty());

        // Mirror sessions: capacity, attachment gating removal.
        let session = sai.create_mirror_session(ports[3]).unwrap();
        sai.set_port_mirror(ports[0], Some(session), Some(session))
            .unwrap();
        assert!(sai.remove_mirror_session(session).is_err(), "port attached");
        sai.set_port_mirror(ports[0], None, None).unwrap();
        sai.remove_mirror_session(session).unwrap();
        assert!(sai.set_port_mirror(ports[0], Some(session), None).is_err());

        // TPID: non-default values tracked, default clears.
        sai.set_port_tpid(ports[0], 0x88a8).unwrap();
        assert_eq!(sai.tpids[&ports[0]], 0x88a8);
        sai.set_port_tpid(ports[0], 0x8100).unwrap();
        assert!(sai.tpids.is_empty());

        // Capability posture is overridable and the injector feeds the
        // event stream.
        sai.set_capabilities(SaiCapabilities {
            storm_control: false,
            ..SaiCapabilities::all()
        });
        assert!(!sai.capabilities().unwrap().storm_control);
        let mut events = sai.take_events().unwrap();
        let bv_id = sai.vlan_oid_of(1).unwrap();
        sai.event_injector()
            .send(SaiEvent::Fdb {
                kind: FdbEventKind::Learned,
                bv_id,
                mac,
                port: Some(ports[1]),
            })
            .unwrap();
        assert!(matches!(
            events.try_recv().unwrap(),
            SaiEvent::Fdb {
                kind: FdbEventKind::Learned,
                ..
            }
        ));
    }

    #[test]
    fn lag_lifecycle_members_and_gates() {
        let mut sai = MockSai::new(port_table(4));
        sai.create_switch().unwrap();
        let ports: Vec<PortId> = sai.ports().unwrap().iter().map(|p| p.id).collect();

        let lag = sai.create_lag().unwrap();
        // The LAG is port-like: default VLAN + PVID from birth, VLAN
        // membership and PVID calls accept it.
        assert!(sai.default_members.contains(&lag));
        let vlan10 = sai.create_vlan(10).unwrap();
        sai.remove_port_default_vlan(lag).unwrap();
        let vm = sai.add_vlan_member(vlan10, lag, true).unwrap();
        sai.set_port_pvid(lag, 10).unwrap();

        // Members leave the default VLAN and start gated closed.
        let m1 = sai.add_lag_member(lag, ports[0]).unwrap();
        let m2 = sai.add_lag_member(lag, ports[1]).unwrap();
        assert!(!sai.default_members.contains(&ports[0]));
        assert!(!sai.lag_members[&m1].2);
        assert!(
            sai.add_lag_member(lag, ports[0]).is_err(),
            "one LAG per port"
        );
        sai.set_lag_member_state(m1, true).unwrap();
        assert!(sai.lag_members[&m1].2);

        // A LAG with members or memberships refuses removal.
        assert!(sai.remove_lag(lag).is_err());
        sai.remove_lag_member(m1, ports[0]).unwrap();
        sai.remove_lag_member(m2, ports[1]).unwrap();
        assert!(sai.default_members.contains(&ports[0]), "standalone again");
        assert!(sai.remove_lag(lag).is_err(), "VLAN membership remains");
        sai.remove_vlan_member(vm).unwrap();
        sai.remove_lag(lag).unwrap();
        assert!(sai.remove_lag(lag).is_err());
    }

    #[test]
    fn vlan_lifecycle_members_and_pvid() {
        let mut sai = MockSai::new(port_table(2));
        sai.create_switch().unwrap();
        let port = sai.ports().unwrap()[0].id;

        let vlan10 = sai.create_vlan(10).unwrap();
        let vlan20 = sai.create_vlan(20).unwrap();
        assert!(sai.create_vlan(10).is_err(), "duplicate VLAN id");
        assert!(sai.create_vlan(1).is_err(), "default VLAN already exists");

        // Access on VLAN 10: leave default, untagged member, PVID 10.
        sai.remove_port_default_vlan(port).unwrap();
        sai.remove_port_default_vlan(port).unwrap(); // idempotent
        let member = sai.add_vlan_member(vlan10, port, false).unwrap();
        assert!(
            sai.add_vlan_member(vlan10, port, false).is_err(),
            "one membership per (vlan, port)"
        );
        sai.set_port_pvid(port, 10).unwrap();

        // A VLAN with members refuses removal.
        assert!(sai.remove_vlan(vlan10).is_err());
        sai.remove_vlan_member(member).unwrap();
        assert!(sai.remove_vlan_member(member).is_err());

        // Trunk-ish: tagged members on both VLANs.
        let m10 = sai.add_vlan_member(vlan10, port, true).unwrap();
        let m20 = sai.add_vlan_member(vlan20, port, true).unwrap();
        sai.remove_vlan_member(m10).unwrap();
        sai.remove_vlan_member(m20).unwrap();

        // Back to default L2.
        sai.restore_port_default_vlan(port).unwrap();
        sai.remove_vlan(vlan10).unwrap();
        sai.remove_vlan(vlan20).unwrap();
        assert!(sai.remove_vlan(vlan10).is_err());

        // Routed ports have no bridge port to hang members on.
        let vlan30 = sai.create_vlan(30).unwrap();
        sai.create_router_interface(port).unwrap();
        assert!(sai.add_vlan_member(vlan30, port, false).is_err());
        assert!(sai.restore_port_default_vlan(port).is_err());
    }
}
