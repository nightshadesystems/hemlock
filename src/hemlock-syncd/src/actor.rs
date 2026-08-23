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
    IpPrefix, Oid, PortCounters, PortId, QueueCounters, RouteTarget, SaiBackend, SaiError,
    SaiEvent, SwitchInfo,
};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::state::{name_for, PortState, SharedPorts, SharedVlans, SwitchMeta};

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
}

/// A port's oper status changed (already applied to shared state).
#[derive(Debug, Clone)]
pub struct OperEvent {
    pub name: String,
    pub oper_up: bool,
}

pub struct SaiHandle {
    pub switch: SwitchMeta,
    pub backend_name: String,
    pub platform_id: String,
    pub ports: SharedPorts,
    /// Created VLANs (the default VLAN appears only if named).
    pub vlans: SharedVlans,
    cmd_tx: mpsc::Sender<SaiCmd>,
    pub events: broadcast::Sender<OperEvent>,
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

    pub async fn remove_vlan_rif(&self, rif: Oid) -> Result<(), SaiError> {
        self.call(|reply| SaiCmd::RemoveVlanRif { rif, reply }).await
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
            oneshot::channel::<Result<(SwitchInfo, HashMap<String, PortState>)>>();

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
                    }
                }
                debug!("sai actor thread exiting");
            })
            .context("spawning sai-actor thread")?;

        let (switch, port_map) = init_rx.await.context("SAI init aborted")??;
        let ports: SharedPorts = std::sync::Arc::new(std::sync::RwLock::new(port_map));

        // Event pump: apply SAI notifications to shared state, then fan out.
        let (events, _) = broadcast::channel(256);
        tokio::spawn(pump_events(sai_events, ports.clone(), events.clone()));

        Ok(SaiHandle {
            switch: SwitchMeta { oid: switch.oid },
            backend_name,
            platform_id,
            ports,
            vlans: SharedVlans::default(),
            cmd_tx,
            events,
        })
    }
}

/// Create the switch, correlate ASIC ports with the manifest port table by
/// lane set, and bring every mapped port to its default admin state (up).
fn init_switch(
    backend: &mut dyn SaiBackend,
    port_defs: &[hemlock_platform::PortDef],
) -> Result<(SwitchInfo, HashMap<String, PortState>)> {
    let switch = backend.create_switch().context("create_switch")?;
    info!(oid = format_args!("{:#x}", switch.oid), "switch created");

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

    Ok((switch, ports))
}

async fn pump_events(
    mut sai_events: mpsc::UnboundedReceiver<SaiEvent>,
    ports: SharedPorts,
    out: broadcast::Sender<OperEvent>,
) {
    while let Some(event) = sai_events.recv().await {
        match event {
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
        }
    }
}
