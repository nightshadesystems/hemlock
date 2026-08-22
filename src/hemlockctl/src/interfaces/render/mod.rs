//! Text renderers for the `show interfaces` family.
//!
//! Every function takes the shared data model (`super::model`) and returns
//! the finished text, formatted to match Arista EOS byte-for-byte (adapted
//! to Hemlock conventions: colon-separated lowercase MACs, Hemlock
//! platform strings). Golden-file tests pin each renderer's output.

pub mod counters;
pub mod detail;
pub mod l2;
pub mod phys;
pub mod summary;
pub mod transceiver;

use super::model::Interface;
use super::name::Kind;

/// Tabular order: natural sort on the abbreviated names
/// (Et1..Et52, Lo0, Ma1, Po1, Vl10).
pub fn sorted_tabular(interfaces: &[Interface]) -> Vec<&Interface> {
    let mut refs: Vec<&Interface> = interfaces.iter().collect();
    refs.sort_by_key(|i| i.id);
    refs
}

/// Detail-block order: Ethernet, Management, Loopback, Vlan,
/// Port-Channel, numerically within each family.
pub fn sorted_detail(interfaces: &[Interface]) -> Vec<&Interface> {
    let mut refs: Vec<&Interface> = interfaces.iter().collect();
    refs.sort_by_key(|i| (i.id.kind.detail_rank(), i.id.num));
    refs
}

/// Front-panel/management/LAG rows for `show interfaces status`-style
/// tables (SVIs and loopbacks are not listed there).
pub fn is_port_like(interface: &Interface) -> bool {
    matches!(
        interface.id.kind,
        Kind::Ethernet | Kind::Management | Kind::PortChannel
    )
}

/// Interfaces with hardware counters (the counters tables exclude
/// port-channels — members carry the counters — and virtual interfaces).
pub fn has_counter_rows(interface: &Interface) -> bool {
    interface.counters.is_some() && interface.id.kind != Kind::PortChannel
}
