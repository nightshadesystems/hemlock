//! The QoS-suite show family: `show qos maps`, `show qos wred`,
//! `show qos interface <port>`, and `show qos interfaces`.
//!
//! Same shape as the routing and security families: one serde data
//! model feeds both the EOS-style text renderers and `| json`, pinned
//! by golden files. Everything comes from syncd's `GetQosState` — QoS
//! has no protocol state machine, so there is no orch side to it.

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
