//! The system-suite show family: `show system users` and its
//! siblings.
//!
//! Same shape as the routing, security, QoS and services families: one
//! serde data model feeds both the EOS-style text renderers and
//! `| json`, pinned by golden files.
//!
//! The two halves of `show system users` come from different places on
//! purpose. *Configured* users are configuration — read from mgmtd's
//! running config, which is the source of truth for membership,
//! credentials and roles. *Active sessions* are operational state,
//! read from mgmtd's session registry, which both front-ends register
//! with. Accounts that exist on the box but no config names are listed
//! separately, because they are the OS's, not Hemlock's.

pub mod cmd;
mod fetch;
mod model;
mod render;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod golden_tests;
