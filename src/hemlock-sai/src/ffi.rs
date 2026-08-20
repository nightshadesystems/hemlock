//! Raw bindgen output over the vendored SAI headers.
//!
//! This module (plus `vendor.rs`) is the only place in Hemlock where
//! `unsafe` is permitted. Nothing outside this crate sees these types.

#![allow(unsafe_code)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(clippy::all)]

include!(concat!(env!("OUT_DIR"), "/sai_bindings.rs"));
