//! webd's view of the TLS material: the shared implementation lives in
//! `hemlock_common::cert`, because mgmtd regenerates the same pair for
//! `request certificate regenerate`.

#[allow(unused_imports)]
pub use hemlock_common::cert::{current_fingerprint, ensure_cert, fingerprint, regenerate};
