//! The security-suite show family: `show acl`, `show copp`,
//! `show port-security`, `show dot1x`, `show dhcp snooping`, and
//! `show arp inspection`, plus their `clear` verbs.
//!
//! Same shape as the routing family: one serde data model feeds both
//! the EOS-style text renderers and `| json`, pinned by golden files.
//! Dataplane state (ACLs, CoPP, port security) comes from syncd;
//! protocol state (802.1X, DHCP snooping, ARP inspection) from orch.

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
