//! Shared port state between the SAI actor and the gRPC service.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use hemlock_platform::PortDef;
use hemlock_sai::{Oid, PortId};

/// One front-panel port: manifest definition + live ASIC state + the
/// operator-facing attributes syncd tracks (description, L3 mode).
#[derive(Debug, Clone)]
pub struct PortState {
    pub def: PortDef,
    pub sai_id: PortId,
    pub admin_up: bool,
    pub oper_up: bool,
    pub description: String,
    /// Present when the port is routed (has an address).
    pub l3: Option<L3State>,
}

/// A routed port's L3 objects: its router interface and the address
/// whose IP2ME + subnet routes are programmed.
#[derive(Debug, Clone)]
pub struct L3State {
    pub rif: Oid,
    /// The interface address in CIDR form.
    pub address: String,
}

#[derive(Debug, Clone, Copy)]
pub struct SwitchMeta {
    pub oid: u64,
}

/// Port table keyed by port name, shared via `Arc<RwLock<...>>`.
pub type SharedPorts = Arc<RwLock<HashMap<String, PortState>>>;

/// Resolve a SAI port id back to a port name (for event handling).
pub fn name_for(ports: &HashMap<String, PortState>, id: PortId) -> Option<String> {
    ports
        .iter()
        .find(|(_, p)| p.sai_id == id)
        .map(|(name, _)| name.clone())
}
