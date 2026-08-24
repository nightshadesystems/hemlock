//! The routing-suite show family: `show ip route`, `show ipv6 route`,
//! and their summaries.
//!
//! Same shape as the switching family: one serde data model feeds both
//! the EOS-style text renderers and `| json`, pinned by golden files.
//! Phase 1 renders a kernel-only snapshot built from the running config
//! (statics) and syncd's interface addresses (connected); the orch RIB
//! snapshot takes over as the source when the FIB pipeline lands.

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
