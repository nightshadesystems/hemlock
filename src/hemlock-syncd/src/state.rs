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
    /// Present when the port has explicit switchport config.
    pub switchport: Option<SwitchportState>,
}

/// A routed port's L3 objects: its router interface and the address
/// whose IP2ME + subnet routes are programmed.
#[derive(Debug, Clone)]
pub struct L3State {
    pub rif: Oid,
    /// The interface address in CIDR form.
    pub address: String,
}

/// A port's L2 switchport program, as applied: the intent plus the live
/// non-default VLAN memberships it produced.
#[derive(Debug, Clone, Default)]
pub struct SwitchportState {
    pub trunk: bool,
    /// 0 = default VLAN.
    pub access_vlan: u16,
    pub trunk_vlans: Vec<u16>,
    /// 0 = default VLAN.
    pub native_vlan: u16,
    /// (vlan id, member oid, tagged); default-VLAN membership is not
    /// tracked here (the backend owns it, idempotently).
    pub members: Vec<(u16, Oid, bool)>,
}

/// One created VLAN. `oid` is `None` for the default VLAN (it always
/// exists; only its display name is tracked).
#[derive(Debug, Clone)]
pub struct VlanState {
    pub oid: Option<Oid>,
    pub name: String,
    /// Present when the VLAN has an SVI (a VLAN router interface with
    /// an address).
    pub l3: Option<L3State>,
}

/// VLAN table keyed by 802.1Q id, shared via `Arc<RwLock<...>>`.
pub type SharedVlans = Arc<RwLock<std::collections::BTreeMap<u16, VlanState>>>;

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
