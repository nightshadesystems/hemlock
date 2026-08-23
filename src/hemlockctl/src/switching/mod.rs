//! The switching-suite show family: `show vlan`, `show mac
//! address-table`, `show storm-control`, `show mirror`, and the
//! operational `clear` verbs (`clear counters`, `clear mac-table`).
//!
//! Same shape as the interfaces family: one serde data model feeds both
//! the EOS-style text renderers and `| json`, pinned by golden files.

pub mod cmd;
mod fetch;
mod model;
mod render;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod golden_tests;
