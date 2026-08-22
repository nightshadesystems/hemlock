//! The `show interfaces` command family.
//!
//! - [`name`]: interface identity (display names, abbreviations, parsing,
//!   ranges, natural sort)
//! - [`model`]: the data model shared by text renderers and `| json`
//! - [`fmt`] / [`table`]: EOS-parity value formatting and column layout
//! - [`render`]: one renderer per command, golden-file tested
//! - [`fetch`]: builds the model from the daemons (syncd, pmon)
//! - [`cmd`]: subcommand parsing and dispatch for the CLI

pub mod cmd;
pub mod fetch;
pub mod fmt;
pub mod model;
pub mod name;
pub mod render;
pub mod table;

#[cfg(test)]
pub(crate) mod fixtures;
#[cfg(test)]
mod golden_tests;
#[cfg(test)]
mod prop_tests;
