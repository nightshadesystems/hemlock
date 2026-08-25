//! Hardware ingress sampling: the samplepacket session and its port
//! bindings.
//!
//! syncd owns the ASIC, so it owns the sampler. The whole program is
//! one shared session (SAI's `SHARED` mode) bound to every enabled
//! front-panel port: the rate is global in the config language, so a
//! session per port would cost objects and buy nothing.
//!
//! Sampled frames come back through the punt path as
//! [`SaiEvent::SampledPacket`], tagged with their ingress port and
//! their length on the wire. syncd translates the port id to its
//! display name and broadcasts them; orch's sFlow engine turns them
//! into v5 datagrams. syncd deliberately builds no datagrams itself —
//! that is export, not ASIC ownership.
//!
//! The sample trap is *not* a CoPP class: the sample rate is already
//! the rate limit, and a policer on top would silently skew the
//! statistical estimate every sFlow collector computes.

use std::collections::BTreeSet;
use std::sync::Arc;

use hemlock_sai::{Oid, TrapKind};
use tracing::{info, warn};

use crate::actor::SaiHandle;

/// One sampled frame on its way to orch.
#[derive(Debug, Clone)]
pub struct SampleNotify {
    /// Ingress port display name.
    pub port: String,
    /// The frame's length on the wire (the delivered bytes may be
    /// shorter).
    pub original_length: u32,
    pub bytes: Vec<u8>,
}

/// The programmed sFlow state: what syncd has in the ASIC right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SflowProgram {
    /// 1-in-N; 0 = sampling is off entirely.
    pub rate: u32,
    /// Ports with a sampling session bound, sorted.
    pub ports: Vec<String>,
    /// Samples the punt path has delivered.
    pub samples: u64,
}

/// The live session object and what it is bound to.
#[derive(Debug, Default)]
pub struct SflowState {
    pub session: Option<Oid>,
    pub rate: u32,
    pub ports: BTreeSet<String>,
    pub samples: u64,
    /// The punt trap, created with the first session.
    pub trap: Option<Oid>,
}

pub type SharedSflow = Arc<std::sync::RwLock<SflowState>>;

/// Apply one declarative sampling program: rate 0 (or no ports) tears
/// everything down, anything else creates or re-rates the session and
/// reconciles the port bindings.
///
/// A rate change rebuilds the session rather than mutating it: SAI
/// allows a rate set, but a rebuild is the same two calls plus a rebind
/// and keeps this function a pure function of the wanted state.
pub async fn apply(
    handle: &Arc<SaiHandle>,
    rate: u32,
    wanted_ports: &[String],
) -> Result<(), String> {
    if !handle.capabilities.sflow {
        return Err("sflow sampling is not supported by this platform's SAI".into());
    }
    let wanted: BTreeSet<String> = wanted_ports.iter().cloned().collect();
    let off = rate == 0 || wanted.is_empty();

    let (session, current_rate, current_ports) = {
        let state = handle
            .sflow
            .read()
            .map_err(|_| "sflow state poisoned".to_string())?;
        (state.session, state.rate, state.ports.clone())
    };

    // Unbind first, always: a port must leave a session before the
    // session can be freed, and a rate change rebinds everything anyway.
    let rebuild = session.is_some() && (off || current_rate != rate);
    let unbind: Vec<String> = if rebuild {
        current_ports.iter().cloned().collect()
    } else {
        current_ports.difference(&wanted).cloned().collect()
    };
    for port in &unbind {
        let Some(id) = port_id(handle, port) else {
            continue;
        };
        handle
            .set_port_sample_session(id, None)
            .await
            .map_err(|err| format!("unbinding sampling from {port}: {err}"))?;
    }
    if rebuild {
        if let Some(session) = session {
            handle
                .remove_samplepacket(session)
                .await
                .map_err(|err| format!("removing the sampling session: {err}"))?;
        }
        set_state(handle, |state| {
            state.session = None;
            state.rate = 0;
            state.ports.clear();
        })?;
    } else {
        set_state(handle, |state| {
            for port in &unbind {
                state.ports.remove(port);
            }
        })?;
    }

    if off {
        info!("sflow sampling disabled");
        return Ok(());
    }

    // Create the session (and, with it, the punt trap) on demand. The
    // read guard is dropped before the await: it is not `Send`, and a
    // lock held across a SAI round trip would be wrong anyway.
    let existing = {
        let state = handle
            .sflow
            .read()
            .map_err(|_| "sflow state poisoned".to_string())?;
        state.session
    };
    let session = match existing {
        Some(session) => session,
        None => {
            let session = handle
                .create_samplepacket(rate)
                .await
                .map_err(|err| format!("creating the sampling session: {err}"))?;
            set_state(handle, |state| {
                state.session = Some(session);
                state.rate = rate;
            })?;
            ensure_trap(handle).await;
            info!(rate, "sflow sampling session created");
            session
        }
    };

    let bound = {
        let state = handle
            .sflow
            .read()
            .map_err(|_| "sflow state poisoned".to_string())?;
        state.ports.clone()
    };
    for port in wanted.difference(&bound) {
        let Some(id) = port_id(handle, port) else {
            warn!(%port, "sflow: no such port; sampling not bound");
            continue;
        };
        handle
            .set_port_sample_session(id, Some(session))
            .await
            .map_err(|err| format!("binding sampling to {port}: {err}"))?;
        set_state(handle, |state| {
            state.ports.insert(port.clone());
        })?;
    }
    Ok(())
}

/// The punt trap that delivers samples to the CPU. Created once, with
/// the first session; a SAI that refuses it leaves sampling programmed
/// but silent, which the warning names.
async fn ensure_trap(handle: &Arc<SaiHandle>) {
    let existing = handle.sflow.read().ok().and_then(|state| state.trap);
    if existing.is_some() {
        return;
    }
    // Oid(0) = the switch default trap group. Samples are already
    // rate-limited by the sampler itself, so they get no policer.
    match handle
        .create_hostif_trap(TrapKind::SamplePacket, true, Oid(0))
        .await
    {
        Ok(trap) => {
            let _ = set_state(handle, |state| state.trap = Some(trap));
        }
        Err(err) => warn!(%err, "cannot install the sflow punt trap; samples will not arrive"),
    }
}

fn set_state(handle: &Arc<SaiHandle>, edit: impl FnOnce(&mut SflowState)) -> Result<(), String> {
    let mut state = handle
        .sflow
        .write()
        .map_err(|_| "sflow state poisoned".to_string())?;
    edit(&mut state);
    Ok(())
}

fn port_id(handle: &Arc<SaiHandle>, name: &str) -> Option<hemlock_sai::PortId> {
    handle
        .ports
        .read()
        .ok()
        .and_then(|table| table.get(name).map(|port| port.sai_id))
}

/// The program as `show sflow` and the boot replay see it.
pub fn snapshot(handle: &Arc<SaiHandle>) -> SflowProgram {
    let Ok(state) = handle.sflow.read() else {
        return SflowProgram::default();
    };
    SflowProgram {
        rate: state.rate,
        ports: state.ports.iter().cloned().collect(),
        samples: state.samples,
    }
}
