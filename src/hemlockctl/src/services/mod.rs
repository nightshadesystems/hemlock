//! The services-suite show family: `show lldp` and its neighbor
//! views, plus the matching `clear` verbs.
//!
//! Same shape as the routing, security and QoS families: one serde
//! data model feeds both the EOS-style text renderers and `| json`,
//! pinned by golden files. LLDP is an orch engine, so its state comes
//! from orch's `GetLldpState`.

pub mod cmd;
mod fetch;
mod model;
mod render;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod golden_tests;
#[cfg(test)]
mod prop_tests;
