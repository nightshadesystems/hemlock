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
    PortCounters, PortId, QueueCounters, SaiBackend, SaiError, SaiEvent, SwitchInfo,
};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::state::{name_for, PortState, SharedPorts, SwitchMeta};

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
    for (name, id) in ids {
        backend
            .set_port_admin_state(id, true)
            .with_context(|| format!("admin-up {name}"))?;
        if let Some(p) = ports.get_mut(&name) {
            p.admin_up = true;
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
