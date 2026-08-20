//! SAI backend abstraction.
//!
//! Everything above this crate talks to the switch ASIC through the safe
//! [`SaiBackend`] trait. The `real-sai` feature provides the vendor-library
//! implementation (bindgen FFI + dlopen); the default `mock-sai` feature
//! provides a pure-Rust in-memory implementation so the whole stack builds
//! and tests without hardware or vendor blobs.
