//! The SAI actor: a dedicated OS thread that owns the backend.
//!
//! SAI calls are synchronous C calls that may block for milliseconds (or,
//! during switch create, seconds). Confining them to one thread keeps the
//! async executor clean and gives the vendor library the single-threaded
//! access pattern it expects. The async side talks to the actor through a
//! command channel; oper-status notifications flow back out through a
//! broadcast channel.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Context, Result};
use hemlock_platform::Platform;
use hemlock_sai::{
    AclAction, AclFamily, AclFields, AclStage, FdbAction, FdbEventKind, IpPrefix, Oid,
    PolicerSpec, PolicerStats, PortCounters, PortId, QueueCounters, RouteTarget, SaiBackend,
    SaiCapabilities, SaiError, SaiEvent, StormClass, StpPortState, SwitchInfo, TrapKind,
};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::state::{
    name_for, FdbDynamicEntry, PortState, SharedFdb, SharedPorts, SharedVlans, SwitchMeta,
};

/// One port's stat sweep result.
pub struct PortStatsSample {
    pub name: String,
    pub counters: PortCounters,
    pub queues: Vec<QueueCounters>,
}

pub enum SaiCmd {
    SetAdminState {
        port: PortId,
        up: bool,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    /// Batched counter sweep for the stats engine: one actor round-trip
    /// per 5s sampling tick instead of one per port.
    PortStats {
        ports: Vec<(String, PortId)>,
        reply: oneshot::Sender<Vec<PortStatsSample>>,
    },
    CreateRouterInterface {
        port: PortId,
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    RemoveRouterInterface {
        port: PortId,
        rif: Oid,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    CreateRoute {
        dest: IpPrefix,
        target: RouteTarget,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    RemoveRoute {
        dest: IpPrefix,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    CreateVlan {
        id: u16,
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    RemoveVlan {
        vlan: Oid,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    AddVlanMember {
        vlan: Oid,
        port: PortId,
        tagged: bool,
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    RemoveVlanMember {
        member: Oid,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    SetPortPvid {
        port: PortId,
        vlan_number: u16,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    RemovePortDefaultVlan {
        port: PortId,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    RestorePortDefaultVlan {
        port: PortId,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    CreateVlanRif {
        /// None = the default VLAN.
        vlan: Option<Oid>,
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    RemoveVlanRif {
        rif: Oid,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    SetFdbAging {
        secs: u32,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    AddFdbEntry {
        vlan: Option<Oid>,
        mac: [u8; 6],
        action: FdbAction,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    RemoveFdbEntry {
        vlan: Option<Oid>,
        mac: [u8; 6],
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    FlushFdb {
        vlan: Option<Oid>,
        port: Option<PortId>,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    SetPortStorm {
        port: PortId,
        class: StormClass,
        kbps: Option<u64>,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    PortStormDrops {
        port: PortId,
        class: StormClass,
        reply: oneshot::Sender<Result<u64, SaiError>>,
    },
    CreateMirrorSession {
        monitor: PortId,
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    RemoveMirrorSession {
        session: Oid,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    SetPortMirror {
        port: PortId,
        ingress: Option<Oid>,
        egress: Option<Oid>,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    SetPortTpid {
        port: PortId,
        tpid: u16,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    CreateLag {
        reply: oneshot::Sender<Result<PortId, SaiError>>,
    },
    RemoveLag {
        lag: PortId,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    AddLagMember {
        lag: PortId,
        port: PortId,
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    RemoveLagMember {
        member: Oid,
        port: PortId,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    SetLagMemberState {
        member: Oid,
        enabled: bool,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    CreateStpInstance {
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    RemoveStpInstance {
        stp: Oid,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    SetVlanStpInstance {
        vlan: Option<Oid>,
        stp: Option<Oid>,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    SetStpPortState {
        stp: Option<Oid>,
        port: PortId,
        state: StpPortState,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    CreateL2mcGroup {
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    RemoveL2mcGroup {
        group: Oid,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    AddL2mcMember {
        group: Oid,
        port: PortId,
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    RemoveL2mcMember {
        member: Oid,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    SetL2mcEntry {
        vlan: Option<Oid>,
        group_ip: std::net::IpAddr,
        l2mc: Option<Oid>,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    SetVlanUnknownMcast {
        vlan: Option<Oid>,
        l2mc: Option<Oid>,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    CreateNeighbor {
        rif: Oid,
        ip: std::net::IpAddr,
        mac: [u8; 6],
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    RemoveNeighbor {
        rif: Oid,
        ip: std::net::IpAddr,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    CreateNextHop {
        rif: Oid,
        ip: std::net::IpAddr,
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    RemoveNextHop {
        next_hop: Oid,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    CreateNextHopGroup {
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    RemoveNextHopGroup {
        group: Oid,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    AddNextHopGroupMember {
        group: Oid,
        next_hop: Oid,
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    RemoveNextHopGroupMember {
        member: Oid,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    CreateMyMac {
        vlan_id: Option<u16>,
        mac: [u8; 6],
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    RemoveMyMac {
        my_mac: Oid,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    CreateAclTable {
        stage: AclStage,
        family: AclFamily,
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    RemoveAclTable {
        table: Oid,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    CreateAclEntry {
        table: Oid,
        priority: u32,
        fields: Box<AclFields>,
        action: AclAction,
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    SetAclEntryAction {
        entry: Oid,
        action: AclAction,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    RemoveAclEntry {
        entry: Oid,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    CreateAclCounter {
        table: Oid,
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    RemoveAclCounter {
        counter: Oid,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    GetAclCounter {
        counter: Oid,
        reply: oneshot::Sender<Result<u64, SaiError>>,
    },
    BindPortAcl {
        port: PortId,
        stage: AclStage,
        table: Option<Oid>,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    AclAvailableEntries {
        stage: AclStage,
        reply: oneshot::Sender<Result<u32, SaiError>>,
    },
    CreatePolicer {
        spec: PolicerSpec,
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    SetPolicer {
        policer: Oid,
        spec: PolicerSpec,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    RemovePolicer {
        policer: Oid,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    PolicerStats {
        policer: Oid,
        reply: oneshot::Sender<Result<PolicerStats, SaiError>>,
    },
    CreateHostifTrapGroup {
        policer: Option<Oid>,
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    CreateHostifTrap {
        kind: TrapKind,
        trap_only: bool,
        group: Oid,
        reply: oneshot::Sender<Result<Oid, SaiError>>,
    },
    SetDefaultTrapGroupPolicer {
        policer: Option<Oid>,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    SetPortLearnLimit {
        port: PortId,
        limit: Option<u32>,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
    SetPortLearning {
        port: PortId,
        learn: bool,
        reply: oneshot::Sender<Result<(), SaiError>>,
    },
}

/// A port's oper status changed (already applied to shared state).
#[derive(Debug, Clone)]
pub struct OperEvent {
    pub name: String,
    pub oper_up: bool,
}

/// An FDB change, resolved to display names (already applied to the
/// software mirror). Feeds `WatchFdbEvents`.
#[derive(Debug, Clone)]
pub struct FdbNotify {
    pub kind: FdbEventKind,
    pub vlan: u16,
    /// Colon-separated lowercase.
    pub mac: String,
    pub port: Option<String>,
}

/// A port at its learn limit saw a new source MAC (port-security).
#[derive(Debug, Clone)]
pub struct ViolationNotify {
    pub port: String,
    /// Colon-separated lowercase.
    pub mac: String,
}

pub struct SaiHandle {
    pub switch: SwitchMeta,
    pub backend_name: String,
    pub platform_id: String,
    /// What the platform's SAI supports (probed at startup).
    pub capabilities: SaiCapabilities,
    /// The default 802.1Q VLAN's object id (scoped FDB flushes on VLAN 1).
    pub default_vlan_oid: u64,
    pub ports: SharedPorts,
    /// Created VLANs (the default VLAN appears only if named).
    pub vlans: SharedVlans,
    /// Software mirror of the hardware FDB (dynamics from SAI events).
    pub fdb: SharedFdb,
    /// Mirror sessions keyed by operator-visible id.
    pub mirrors: crate::state::SharedMirrors,
    /// Port-channels keyed by group number.
    pub lags: crate::state::SharedLags,
    /// MST instances keyed by instance number.
    pub stps: crate::state::SharedStps,
    /// Snooping-programmed multicast groups keyed by (vlan, group IP).
    pub l2mc: crate::state::SharedL2mc,
    /// Per-VLAN unknown-multicast restriction groups.
    pub unknown_mcast: crate::state::SharedUnknownMcast,
    /// The transit FIB (routes, deduplicated next hops/groups,
    /// neighbors, My-MAC entries) as programmed by orch.
    pub fib: crate::state::SharedFib,
    /// The security suite's shared model: user ACLs + bindings and the
    /// per-port materialized programs, CoPP class state, per-port
    /// port-security state.
    pub acls: crate::state::SharedAcls,
    pub copp: crate::state::SharedCopp,
    pub port_security: crate::state::SharedPortSecurity,
    cmd_tx: mpsc::Sender<SaiCmd>,
    pub events: broadcast::Sender<OperEvent>,
    pub fdb_events: broadcast::Sender<FdbNotify>,
    /// Learn-limit violations (port-security engine input).
    pub violations: broadcast::Sender<ViolationNotify>,
}

impl SaiHandle {
    pub fn initial_ports(&self) -> usize {
        self.ports.read().map(|p| p.len()).unwrap_or(0)
    }

    pub async fn set_admin_state(&self, port: PortId, up: bool) -> Result<(), SaiError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(SaiCmd::SetAdminState { port, up, reply })
            .await
            .map_err(|_| SaiError::Other("SAI actor is gone".into()))?;
        rx.await
            .map_err(|_| SaiError::Other("SAI actor dropped the reply".into()))?
    }

    /// Sweep hardware counters for the given ports.
    pub async fn port_stats(
        &self,
        ports: Vec<(String, PortId)>,
    ) -> Result<Vec<PortStatsSample>, SaiError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(SaiCmd::PortStats { ports, reply })
            .await
            .map_err(|_| SaiError::Other("SAI actor is gone".into()))?;
        rx.await
            .map_err(|_| SaiError::Other("SAI actor dropped the reply".into()))
    }

    /// Send one command and await its reply (every L3/L2 mutation
    /// shares this shape).
    async fn call<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, SaiError>>) -> SaiCmd,
    ) -> Result<T, SaiError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(make(reply))
            .await
            .map_err(|_| SaiError::Other("SAI actor is gone".into()))?;
        rx.await
            .map_err(|_| SaiError::Other("SAI actor dropped the reply".into()))?
    }

    pub async fn create_router_interface(&self, port: PortId) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::CreateRouterInterface { port, reply })
            .await
    }

    pub async fn create_vlan(&self, id: u16) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::CreateVlan { id, reply }).await
    }

    pub async fn remove_vlan(&self, vlan: Oid) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveVlan { vlan, reply }).await
    }

    pub async fn add_vlan_member(
        &self,
        vlan: Oid,
        port: PortId,
        tagged: bool,
    ) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::AddVlanMember {
            vlan,
            port,
            tagged,
            reply,
        })
        .await
    }

    pub async fn remove_vlan_member(&self, member: Oid) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveVlanMember { member, reply })
            .await
    }

    pub async fn set_port_pvid(&self, port: PortId, vlan_number: u16) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::SetPortPvid {
            port,
            vlan_number,
            reply,
        })
        .await
    }

    pub async fn remove_port_default_vlan(&self, port: PortId) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemovePortDefaultVlan { port, reply })
            .await
    }

    pub async fn restore_port_default_vlan(&self, port: PortId) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RestorePortDefaultVlan { port, reply })
            .await
    }

    pub async fn create_vlan_rif(&self, vlan: Option<Oid>) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::CreateVlanRif { vlan, reply })
            .await
    }

    pub async fn set_fdb_aging(&self, secs: u32) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::SetFdbAging { secs, reply }).await
    }

    pub async fn add_fdb_entry(
        &self,
        vlan: Option<Oid>,
        mac: [u8; 6],
        action: FdbAction,
    ) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::AddFdbEntry {
            vlan,
            mac,
            action,
            reply,
        })
        .await
    }

    pub async fn remove_fdb_entry(&self, vlan: Option<Oid>, mac: [u8; 6]) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveFdbEntry { vlan, mac, reply })
            .await
    }

    pub async fn flush_fdb(&self, vlan: Option<Oid>, port: Option<PortId>) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::FlushFdb { vlan, port, reply })
            .await
    }

    pub async fn set_port_storm(
        &self,
        port: PortId,
        class: StormClass,
        kbps: Option<u64>,
    ) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::SetPortStorm {
            port,
            class,
            kbps,
            reply,
        })
        .await
    }

    pub async fn port_storm_drops(&self, port: PortId, class: StormClass) -> Result<u64, SaiError> {
        self.call(|reply| SaiCmd::PortStormDrops { port, class, reply })
            .await
    }

    pub async fn create_mirror_session(&self, monitor: PortId) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::CreateMirrorSession { monitor, reply })
            .await
    }

    pub async fn remove_mirror_session(&self, session: Oid) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveMirrorSession { session, reply })
            .await
    }

    pub async fn set_port_mirror(
        &self,
        port: PortId,
        ingress: Option<Oid>,
        egress: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::SetPortMirror {
            port,
            ingress,
            egress,
            reply,
        })
        .await
    }

    pub async fn set_port_tpid(&self, port: PortId, tpid: u16) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::SetPortTpid { port, tpid, reply })
            .await
    }

    pub async fn create_lag(&self) -> Result<PortId, SaiError> {
        self.call(|reply| SaiCmd::CreateLag { reply }).await
    }

    pub async fn remove_lag(&self, lag: PortId) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveLag { lag, reply }).await
    }

    pub async fn add_lag_member(&self, lag: PortId, port: PortId) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::AddLagMember { lag, port, reply })
            .await
    }

    pub async fn remove_lag_member(&self, member: Oid, port: PortId) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveLagMember {
            member,
            port,
            reply,
        })
        .await
    }

    pub async fn set_lag_member_state(&self, member: Oid, enabled: bool) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::SetLagMemberState {
            member,
            enabled,
            reply,
        })
        .await
    }

    pub async fn create_stp_instance(&self) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::CreateStpInstance { reply }).await
    }

    pub async fn remove_stp_instance(&self, stp: Oid) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveStpInstance { stp, reply })
            .await
    }

    pub async fn set_vlan_stp_instance(
        &self,
        vlan: Option<Oid>,
        stp: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::SetVlanStpInstance { vlan, stp, reply })
            .await
    }

    pub async fn set_stp_port_state(
        &self,
        stp: Option<Oid>,
        port: PortId,
        state: StpPortState,
    ) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::SetStpPortState {
            stp,
            port,
            state,
            reply,
        })
        .await
    }

    pub async fn create_l2mc_group(&self) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::CreateL2mcGroup { reply }).await
    }

    pub async fn remove_l2mc_group(&self, group: Oid) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveL2mcGroup { group, reply })
            .await
    }

    pub async fn add_l2mc_member(&self, group: Oid, port: PortId) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::AddL2mcMember { group, port, reply })
            .await
    }

    pub async fn remove_l2mc_member(&self, member: Oid) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveL2mcMember { member, reply })
            .await
    }

    pub async fn set_l2mc_entry(
        &self,
        vlan: Option<Oid>,
        group_ip: std::net::IpAddr,
        l2mc: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::SetL2mcEntry {
            vlan,
            group_ip,
            l2mc,
            reply,
        })
        .await
    }

    pub async fn set_vlan_unknown_mcast(
        &self,
        vlan: Option<Oid>,
        l2mc: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::SetVlanUnknownMcast { vlan, l2mc, reply })
            .await
    }

    pub async fn remove_vlan_rif(&self, rif: Oid) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveVlanRif { rif, reply })
            .await
    }

    pub async fn remove_router_interface(&self, port: PortId, rif: Oid) -> Result<(), SaiError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(SaiCmd::RemoveRouterInterface { port, rif, reply })
            .await
            .map_err(|_| SaiError::Other("SAI actor is gone".into()))?;
        rx.await
            .map_err(|_| SaiError::Other("SAI actor dropped the reply".into()))?
    }

    pub async fn create_route(&self, dest: IpPrefix, target: RouteTarget) -> Result<(), SaiError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(SaiCmd::CreateRoute {
                dest,
                target,
                reply,
            })
            .await
            .map_err(|_| SaiError::Other("SAI actor is gone".into()))?;
        rx.await
            .map_err(|_| SaiError::Other("SAI actor dropped the reply".into()))?
    }

    pub async fn remove_route(&self, dest: IpPrefix) -> Result<(), SaiError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(SaiCmd::RemoveRoute { dest, reply })
            .await
            .map_err(|_| SaiError::Other("SAI actor is gone".into()))?;
        rx.await
            .map_err(|_| SaiError::Other("SAI actor dropped the reply".into()))?
    }

    pub async fn create_neighbor(
        &self,
        rif: Oid,
        ip: std::net::IpAddr,
        mac: [u8; 6],
    ) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::CreateNeighbor {
            rif,
            ip,
            mac,
            reply,
        })
        .await
    }

    pub async fn remove_neighbor(&self, rif: Oid, ip: std::net::IpAddr) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveNeighbor { rif, ip, reply })
            .await
    }

    pub async fn create_next_hop(&self, rif: Oid, ip: std::net::IpAddr) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::CreateNextHop { rif, ip, reply })
            .await
    }

    pub async fn remove_next_hop(&self, next_hop: Oid) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveNextHop { next_hop, reply })
            .await
    }

    pub async fn create_next_hop_group(&self) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::CreateNextHopGroup { reply })
            .await
    }

    pub async fn remove_next_hop_group(&self, group: Oid) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveNextHopGroup { group, reply })
            .await
    }

    pub async fn add_next_hop_group_member(
        &self,
        group: Oid,
        next_hop: Oid,
    ) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::AddNextHopGroupMember {
            group,
            next_hop,
            reply,
        })
        .await
    }

    pub async fn remove_next_hop_group_member(&self, member: Oid) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveNextHopGroupMember { member, reply })
            .await
    }

    pub async fn create_my_mac(&self, vlan_id: Option<u16>, mac: [u8; 6]) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::CreateMyMac {
            vlan_id,
            mac,
            reply,
        })
        .await
    }

    pub async fn remove_my_mac(&self, my_mac: Oid) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveMyMac { my_mac, reply })
            .await
    }

    pub async fn create_acl_table(
        &self,
        stage: AclStage,
        family: AclFamily,
    ) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::CreateAclTable {
            stage,
            family,
            reply,
        })
        .await
    }

    pub async fn remove_acl_table(&self, table: Oid) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveAclTable { table, reply })
            .await
    }

    pub async fn create_acl_entry(
        &self,
        table: Oid,
        priority: u32,
        fields: AclFields,
        action: AclAction,
    ) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::CreateAclEntry {
            table,
            priority,
            fields: Box::new(fields),
            action,
            reply,
        })
        .await
    }

    pub async fn set_acl_entry_action(
        &self,
        entry: Oid,
        action: AclAction,
    ) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::SetAclEntryAction {
            entry,
            action,
            reply,
        })
        .await
    }

    pub async fn remove_acl_entry(&self, entry: Oid) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveAclEntry { entry, reply })
            .await
    }

    pub async fn create_acl_counter(&self, table: Oid) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::CreateAclCounter { table, reply })
            .await
    }

    pub async fn remove_acl_counter(&self, counter: Oid) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveAclCounter { counter, reply })
            .await
    }

    pub async fn get_acl_counter(&self, counter: Oid) -> Result<u64, SaiError> {
        self.call(|reply| SaiCmd::GetAclCounter { counter, reply })
            .await
    }

    pub async fn bind_port_acl(
        &self,
        port: PortId,
        stage: AclStage,
        table: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::BindPortAcl {
            port,
            stage,
            table,
            reply,
        })
        .await
    }

    pub async fn acl_available_entries(&self, stage: AclStage) -> Result<u32, SaiError> {
        self.call(|reply| SaiCmd::AclAvailableEntries { stage, reply })
            .await
    }

    pub async fn create_policer(&self, spec: PolicerSpec) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::CreatePolicer { spec, reply })
            .await
    }

    pub async fn set_policer(&self, policer: Oid, spec: PolicerSpec) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::SetPolicer {
            policer,
            spec,
            reply,
        })
        .await
    }

    pub async fn remove_policer(&self, policer: Oid) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemovePolicer { policer, reply })
            .await
    }

    pub async fn policer_stats(&self, policer: Oid) -> Result<PolicerStats, SaiError> {
        self.call(|reply| SaiCmd::PolicerStats { policer, reply })
            .await
    }

    pub async fn create_hostif_trap_group(&self, policer: Option<Oid>) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::CreateHostifTrapGroup { policer, reply })
            .await
    }

    pub async fn create_hostif_trap(
        &self,
        kind: TrapKind,
        trap_only: bool,
        group: Oid,
    ) -> Result<Oid, SaiError> {
        self.call(|reply| SaiCmd::CreateHostifTrap {
            kind,
            trap_only,
            group,
            reply,
        })
        .await
    }

    pub async fn set_default_trap_group_policer(
        &self,
        policer: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::SetDefaultTrapGroupPolicer { policer, reply })
            .await
    }

    pub async fn set_port_learn_limit(
        &self,
        port: PortId,
        limit: Option<u32>,
    ) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::SetPortLearnLimit { port, limit, reply })
            .await
    }

    pub async fn set_port_learning(&self, port: PortId, learn: bool) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::SetPortLearning { port, learn, reply })
            .await
    }
}

pub struct SaiActor;

impl SaiActor {
    /// Boot the ASIC and start the actor thread + event pump.
    ///
    /// Blocking init (create_switch, port enumeration, initial admin-up)
    /// happens on the actor thread; this returns once the switch is up
    /// with the correlated port table.
    pub async fn spawn(mut backend: Box<dyn SaiBackend>, platform: &Platform) -> Result<SaiHandle> {
        let backend_name = backend.name();
        let platform_id = platform.manifest.platform.id.clone();
        let port_defs = platform.ports.clone();

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<SaiCmd>(64);
        let (init_tx, init_rx) =
            oneshot::channel::<Result<(SwitchInfo, SaiCapabilities, HashMap<String, PortState>)>>();

        let sai_events = backend
            .take_events()
            .ok_or_else(|| anyhow!("backend events already taken"))?;

        std::thread::Builder::new()
            .name("sai-actor".into())
            .spawn(move || {
                let init = init_switch(backend.as_mut(), &port_defs);
                let ok = init.is_ok();
                if init_tx.send(init).is_err() || !ok {
                    return;
                }
                // Command loop: blocking recv on a tokio channel is fine
                // from a plain thread via blocking_recv.
                while let Some(cmd) = cmd_rx.blocking_recv() {
                    match cmd {
                        SaiCmd::SetAdminState { port, up, reply } => {
                            let result = backend.set_port_admin_state(port, up);
                            let _ = reply.send(result);
                        }
                        SaiCmd::PortStats { ports, reply } => {
                            let mut samples = Vec::with_capacity(ports.len());
                            for (name, id) in ports {
                                let counters = match backend.port_counters(id) {
                                    Ok(counters) => counters,
                                    Err(e) => {
                                        // Keep sweeping: one bad port must
                                        // not blank every counter.
                                        debug!(%name, error = %e, "port_counters failed");
                                        continue;
                                    }
                                };
                                let queues = backend.port_queue_counters(id).unwrap_or_default();
                                samples.push(PortStatsSample {
                                    name,
                                    counters,
                                    queues,
                                });
                            }
                            let _ = reply.send(samples);
                        }
                        SaiCmd::CreateRouterInterface { port, reply } => {
                            let _ = reply.send(backend.create_router_interface(port));
                        }
                        SaiCmd::RemoveRouterInterface { port, rif, reply } => {
                            let _ = reply.send(backend.remove_router_interface(port, rif));
                        }
                        SaiCmd::CreateRoute {
                            dest,
                            target,
                            reply,
                        } => {
                            let _ = reply.send(backend.create_route(dest, target));
                        }
                        SaiCmd::RemoveRoute { dest, reply } => {
                            let _ = reply.send(backend.remove_route(dest));
                        }
                        SaiCmd::CreateNeighbor {
                            rif,
                            ip,
                            mac,
                            reply,
                        } => {
                            let _ = reply.send(backend.create_neighbor(rif, ip, mac));
                        }
                        SaiCmd::RemoveNeighbor { rif, ip, reply } => {
                            let _ = reply.send(backend.remove_neighbor(rif, ip));
                        }
                        SaiCmd::CreateNextHop { rif, ip, reply } => {
                            let _ = reply.send(backend.create_next_hop(rif, ip));
                        }
                        SaiCmd::RemoveNextHop { next_hop, reply } => {
                            let _ = reply.send(backend.remove_next_hop(next_hop));
                        }
                        SaiCmd::CreateNextHopGroup { reply } => {
                            let _ = reply.send(backend.create_next_hop_group());
                        }
                        SaiCmd::RemoveNextHopGroup { group, reply } => {
                            let _ = reply.send(backend.remove_next_hop_group(group));
                        }
                        SaiCmd::AddNextHopGroupMember {
                            group,
                            next_hop,
                            reply,
                        } => {
                            let _ = reply.send(backend.add_next_hop_group_member(group, next_hop));
                        }
                        SaiCmd::RemoveNextHopGroupMember { member, reply } => {
                            let _ = reply.send(backend.remove_next_hop_group_member(member));
                        }
                        SaiCmd::CreateMyMac {
                            vlan_id,
                            mac,
                            reply,
                        } => {
                            let _ = reply.send(backend.create_my_mac(vlan_id, mac));
                        }
                        SaiCmd::RemoveMyMac { my_mac, reply } => {
                            let _ = reply.send(backend.remove_my_mac(my_mac));
                        }
                        SaiCmd::CreateAclTable {
                            stage,
                            family,
                            reply,
                        } => {
                            let _ = reply.send(backend.create_acl_table(stage, family));
                        }
                        SaiCmd::RemoveAclTable { table, reply } => {
                            let _ = reply.send(backend.remove_acl_table(table));
                        }
                        SaiCmd::CreateAclEntry {
                            table,
                            priority,
                            fields,
                            action,
                            reply,
                        } => {
                            let _ = reply
                                .send(backend.create_acl_entry(table, priority, &fields, &action));
                        }
                        SaiCmd::SetAclEntryAction {
                            entry,
                            action,
                            reply,
                        } => {
                            let _ = reply.send(backend.set_acl_entry_action(entry, &action));
                        }
                        SaiCmd::RemoveAclEntry { entry, reply } => {
                            let _ = reply.send(backend.remove_acl_entry(entry));
                        }
                        SaiCmd::CreateAclCounter { table, reply } => {
                            let _ = reply.send(backend.create_acl_counter(table));
                        }
                        SaiCmd::RemoveAclCounter { counter, reply } => {
                            let _ = reply.send(backend.remove_acl_counter(counter));
                        }
                        SaiCmd::GetAclCounter { counter, reply } => {
                            let _ = reply.send(backend.get_acl_counter(counter));
                        }
                        SaiCmd::BindPortAcl {
                            port,
                            stage,
                            table,
                            reply,
                        } => {
                            let _ = reply.send(backend.bind_port_acl(port, stage, table));
                        }
                        SaiCmd::AclAvailableEntries { stage, reply } => {
                            let _ = reply.send(backend.acl_available_entries(stage));
                        }
                        SaiCmd::CreatePolicer { spec, reply } => {
                            let _ = reply.send(backend.create_policer(spec));
                        }
                        SaiCmd::SetPolicer {
                            policer,
                            spec,
                            reply,
                        } => {
                            let _ = reply.send(backend.set_policer(policer, spec));
                        }
                        SaiCmd::RemovePolicer { policer, reply } => {
                            let _ = reply.send(backend.remove_policer(policer));
                        }
                        SaiCmd::PolicerStats { policer, reply } => {
                            let _ = reply.send(backend.policer_stats(policer));
                        }
                        SaiCmd::CreateHostifTrapGroup { policer, reply } => {
                            let _ = reply.send(backend.create_hostif_trap_group(policer));
                        }
                        SaiCmd::CreateHostifTrap {
                            kind,
                            trap_only,
                            group,
                            reply,
                        } => {
                            let _ = reply.send(backend.create_hostif_trap(kind, trap_only, group));
                        }
                        SaiCmd::SetDefaultTrapGroupPolicer { policer, reply } => {
                            let _ = reply.send(backend.set_default_trap_group_policer(policer));
                        }
                        SaiCmd::SetPortLearnLimit { port, limit, reply } => {
                            let _ = reply.send(backend.set_port_learn_limit(port, limit));
                        }
                        SaiCmd::SetPortLearning { port, learn, reply } => {
                            let _ = reply.send(backend.set_port_learning(port, learn));
                        }
                        SaiCmd::CreateVlan { id, reply } => {
                            let _ = reply.send(backend.create_vlan(id));
                        }
                        SaiCmd::RemoveVlan { vlan, reply } => {
                            let _ = reply.send(backend.remove_vlan(vlan));
                        }
                        SaiCmd::AddVlanMember {
                            vlan,
                            port,
                            tagged,
                            reply,
                        } => {
                            let _ = reply.send(backend.add_vlan_member(vlan, port, tagged));
                        }
                        SaiCmd::RemoveVlanMember { member, reply } => {
                            let _ = reply.send(backend.remove_vlan_member(member));
                        }
                        SaiCmd::SetPortPvid {
                            port,
                            vlan_number,
                            reply,
                        } => {
                            let _ = reply.send(backend.set_port_pvid(port, vlan_number));
                        }
                        SaiCmd::RemovePortDefaultVlan { port, reply } => {
                            let _ = reply.send(backend.remove_port_default_vlan(port));
                        }
                        SaiCmd::RestorePortDefaultVlan { port, reply } => {
                            let _ = reply.send(backend.restore_port_default_vlan(port));
                        }
                        SaiCmd::CreateVlanRif { vlan, reply } => {
                            let _ = reply.send(backend.create_vlan_router_interface(vlan));
                        }
                        SaiCmd::RemoveVlanRif { rif, reply } => {
                            let _ = reply.send(backend.remove_vlan_router_interface(rif));
                        }
                        SaiCmd::SetFdbAging { secs, reply } => {
                            let _ = reply.send(backend.set_fdb_aging(secs));
                        }
                        SaiCmd::AddFdbEntry {
                            vlan,
                            mac,
                            action,
                            reply,
                        } => {
                            let _ = reply.send(backend.add_fdb_entry(vlan, mac, action));
                        }
                        SaiCmd::RemoveFdbEntry { vlan, mac, reply } => {
                            let _ = reply.send(backend.remove_fdb_entry(vlan, mac));
                        }
                        SaiCmd::FlushFdb { vlan, port, reply } => {
                            let _ = reply.send(backend.flush_fdb(vlan, port));
                        }
                        SaiCmd::SetPortStorm {
                            port,
                            class,
                            kbps,
                            reply,
                        } => {
                            let _ = reply.send(backend.set_port_storm_control(port, class, kbps));
                        }
                        SaiCmd::PortStormDrops { port, class, reply } => {
                            let _ = reply.send(backend.port_storm_drops(port, class));
                        }
                        SaiCmd::CreateMirrorSession { monitor, reply } => {
                            let _ = reply.send(backend.create_mirror_session(monitor));
                        }
                        SaiCmd::RemoveMirrorSession { session, reply } => {
                            let _ = reply.send(backend.remove_mirror_session(session));
                        }
                        SaiCmd::SetPortMirror {
                            port,
                            ingress,
                            egress,
                            reply,
                        } => {
                            let _ = reply.send(backend.set_port_mirror(port, ingress, egress));
                        }
                        SaiCmd::SetPortTpid { port, tpid, reply } => {
                            let _ = reply.send(backend.set_port_tpid(port, tpid));
                        }
                        SaiCmd::CreateLag { reply } => {
                            let _ = reply.send(backend.create_lag());
                        }
                        SaiCmd::RemoveLag { lag, reply } => {
                            let _ = reply.send(backend.remove_lag(lag));
                        }
                        SaiCmd::AddLagMember { lag, port, reply } => {
                            let _ = reply.send(backend.add_lag_member(lag, port));
                        }
                        SaiCmd::RemoveLagMember {
                            member,
                            port,
                            reply,
                        } => {
                            let _ = reply.send(backend.remove_lag_member(member, port));
                        }
                        SaiCmd::SetLagMemberState {
                            member,
                            enabled,
                            reply,
                        } => {
                            let _ = reply.send(backend.set_lag_member_state(member, enabled));
                        }
                        SaiCmd::CreateStpInstance { reply } => {
                            let _ = reply.send(backend.create_stp_instance());
                        }
                        SaiCmd::RemoveStpInstance { stp, reply } => {
                            let _ = reply.send(backend.remove_stp_instance(stp));
                        }
                        SaiCmd::SetVlanStpInstance { vlan, stp, reply } => {
                            let _ = reply.send(backend.set_vlan_stp_instance(vlan, stp));
                        }
                        SaiCmd::SetStpPortState {
                            stp,
                            port,
                            state,
                            reply,
                        } => {
                            let _ = reply.send(backend.set_stp_port_state(stp, port, state));
                        }
                        SaiCmd::CreateL2mcGroup { reply } => {
                            let _ = reply.send(backend.create_l2mc_group());
                        }
                        SaiCmd::RemoveL2mcGroup { group, reply } => {
                            let _ = reply.send(backend.remove_l2mc_group(group));
                        }
                        SaiCmd::AddL2mcMember { group, port, reply } => {
                            let _ = reply.send(backend.add_l2mc_member(group, port));
                        }
                        SaiCmd::RemoveL2mcMember { member, reply } => {
                            let _ = reply.send(backend.remove_l2mc_member(member));
                        }
                        SaiCmd::SetL2mcEntry {
                            vlan,
                            group_ip,
                            l2mc,
                            reply,
                        } => {
                            let _ = reply.send(backend.set_l2mc_entry(vlan, group_ip, l2mc));
                        }
                        SaiCmd::SetVlanUnknownMcast { vlan, l2mc, reply } => {
                            let _ = reply.send(backend.set_vlan_unknown_mcast_group(vlan, l2mc));
                        }
                    }
                }
                debug!("sai actor thread exiting");
            })
            .context("spawning sai-actor thread")?;

        let (switch, capabilities, port_map) = init_rx.await.context("SAI init aborted")??;
        let ports: SharedPorts = std::sync::Arc::new(std::sync::RwLock::new(port_map));
        let vlans = SharedVlans::default();
        let fdb = SharedFdb::default();

        // Event pump: apply SAI notifications to shared state, then fan out.
        let (events, _) = broadcast::channel(256);
        let (fdb_events, _) = broadcast::channel(1024);
        let (violations, _) = broadcast::channel(256);
        tokio::spawn(pump_events(
            sai_events,
            ports.clone(),
            vlans.clone(),
            fdb.clone(),
            switch.default_vlan_oid,
            events.clone(),
            fdb_events.clone(),
            violations.clone(),
        ));

        Ok(SaiHandle {
            switch: SwitchMeta { oid: switch.oid },
            backend_name,
            platform_id,
            capabilities,
            default_vlan_oid: switch.default_vlan_oid,
            ports,
            vlans,
            fdb,
            mirrors: crate::state::SharedMirrors::default(),
            lags: crate::state::SharedLags::default(),
            stps: crate::state::SharedStps::default(),
            l2mc: crate::state::SharedL2mc::default(),
            unknown_mcast: crate::state::SharedUnknownMcast::default(),
            fib: crate::state::SharedFib::default(),
            acls: crate::state::SharedAcls::default(),
            copp: crate::state::SharedCopp::default(),
            port_security: crate::state::SharedPortSecurity::default(),
            cmd_tx,
            events,
            fdb_events,
            violations,
        })
    }
}

/// Create the switch, correlate ASIC ports with the manifest port table by
/// lane set, and bring every mapped port to its default admin state (up).
fn init_switch(
    backend: &mut dyn SaiBackend,
    port_defs: &[hemlock_platform::PortDef],
) -> Result<(SwitchInfo, SaiCapabilities, HashMap<String, PortState>)> {
    let switch = backend.create_switch().context("create_switch")?;
    info!(oid = format_args!("{:#x}", switch.oid), "switch created");

    // Capability probe: which optional SAI families this platform's
    // library actually implements. Recorded once; the service gates
    // RPCs on it so unsupported commits fail cleanly.
    let capabilities = backend.capabilities().context("capability probe")?;
    info!(?capabilities, "SAI capability probe");

    let sai_ports = backend.ports().context("enumerating ports")?;

    // Lane sets identify ports regardless of creation order.
    let mut by_lanes: HashMap<Vec<u32>, hemlock_sai::SaiPort> = sai_ports
        .into_iter()
        .map(|p| {
            let mut key = p.lanes.clone();
            key.sort_unstable();
            (key, p)
        })
        .collect();

    let mut ports = HashMap::new();
    for def in port_defs {
        let mut key = def.lanes.clone();
        key.sort_unstable();
        let Some(sai_port) = by_lanes.remove(&key) else {
            bail!(
                "manifest port {} (lanes {:?}) has no matching ASIC port",
                def.name,
                def.lanes
            );
        };
        ports.insert(
            def.name.clone(),
            PortState {
                def: def.clone(),
                sai_id: sai_port.id,
                admin_up: sai_port.admin_up,
                oper_up: sai_port.oper_up,
                description: String::new(),
                l3: None,
                switchport: None,
                storm: std::collections::BTreeMap::new(),
                errdisable_reason: None,
            },
        );
    }
    for leftover in by_lanes.values() {
        // Internal/backplane ports the manifest chose not to model.
        debug!(
            id = %leftover.id,
            lanes = ?leftover.lanes,
            "ASIC port not in manifest port table; leaving untouched"
        );
    }

    // Default policy (phase 1): all front-panel ports admin up.
    let ids: Vec<(String, PortId)> = ports
        .iter()
        .map(|(name, p)| (name.clone(), p.sai_id))
        .collect();
    for (name, id) in &ids {
        backend
            .set_port_admin_state(*id, true)
            .with_context(|| format!("admin-up {name}"))?;
        if let Some(p) = ports.get_mut(name) {
            p.admin_up = true;
        }
    }

    // Host services: CPU punt traps plus one kernel netdev per port
    // (named after it), so ARP and traffic to the switch's own
    // addresses reach the Linux stack and replies transmit raw out the
    // port. Best-effort — a backend without hostif support (or a
    // missing knet module) degrades to L2-only, never a failed boot.
    if let Err(err) = backend.setup_host_punt() {
        warn!(%err, "cannot install CPU punt path; front-panel host services unavailable");
    }
    for (name, id) in &ids {
        if let Err(err) = backend.create_hostif(*id, name) {
            warn!(%err, port = %name, "cannot create host interface netdev");
        }
    }

    Ok((switch, capabilities, ports))
}

/// Colon-separated lowercase MAC display form.
pub fn format_mac(mac: [u8; 6]) -> String {
    mac.map(|b| format!("{b:02x}")).join(":")
}

#[allow(clippy::too_many_arguments)]
async fn pump_events(
    mut sai_events: mpsc::UnboundedReceiver<SaiEvent>,
    ports: SharedPorts,
    vlans: SharedVlans,
    fdb: SharedFdb,
    default_vlan_oid: u64,
    out: broadcast::Sender<OperEvent>,
    fdb_out: broadcast::Sender<FdbNotify>,
    violations_out: broadcast::Sender<ViolationNotify>,
) {
    while let Some(event) = sai_events.recv().await {
        match event {
            SaiEvent::LearnLimitViolation { port, mac } => {
                let Some(name) = ports.read().ok().and_then(|table| name_for(&table, port))
                else {
                    warn!(%port, "learn-limit violation on unknown port");
                    continue;
                };
                let _ = violations_out.send(ViolationNotify {
                    port: name,
                    mac: format_mac(mac),
                });
            }
            SaiEvent::PortOperStatus { port, up } => {
                let name = {
                    let Ok(mut table) = ports.write() else { break };
                    match name_for(&table, port) {
                        Some(name) => {
                            if let Some(p) = table.get_mut(&name) {
                                p.oper_up = up;
                            }
                            name
                        }
                        None => {
                            warn!(%port, "oper event for unknown port");
                            continue;
                        }
                    }
                };
                debug!(%name, up, "port oper status");
                let _ = out.send(OperEvent { name, oper_up: up });
            }
            SaiEvent::Fdb {
                kind,
                bv_id,
                mac,
                port,
            } => {
                // Resolve the notification's raw ids to display terms.
                let vlan = if bv_id == default_vlan_oid {
                    Some(1)
                } else {
                    vlans.read().ok().and_then(|table| {
                        table
                            .iter()
                            .find(|(_, v)| v.oid.map(|o| o.0) == Some(bv_id))
                            .map(|(id, _)| *id)
                    })
                };
                let Some(vlan) = vlan else {
                    debug!(bv_id, "FDB event on unknown VLAN; dropped");
                    continue;
                };
                let port_name =
                    port.and_then(|id| ports.read().ok().and_then(|table| name_for(&table, id)));
                let mac_text = format_mac(mac);

                // Apply to the software mirror.
                if let Ok(mut table) = fdb.write() {
                    let key = (vlan, mac_text.clone());
                    match kind {
                        FdbEventKind::Learned | FdbEventKind::Moved => {
                            if let Some(port_name) = &port_name {
                                let now = std::time::Instant::now();
                                table
                                    .dynamics
                                    .entry(key)
                                    .and_modify(|entry| {
                                        if entry.port != *port_name {
                                            entry.port = port_name.clone();
                                            entry.moves += 1;
                                            entry.last_move = Some(now);
                                        }
                                    })
                                    .or_insert(FdbDynamicEntry {
                                        port: port_name.clone(),
                                        moves: 1,
                                        last_move: Some(now),
                                    });
                            }
                        }
                        FdbEventKind::Aged | FdbEventKind::Flushed => {
                            table.dynamics.remove(&key);
                        }
                    }
                }
                let _ = fdb_out.send(FdbNotify {
                    kind,
                    vlan,
                    mac: mac_text,
                    port: port_name,
                });
            }
        }
    }
}
