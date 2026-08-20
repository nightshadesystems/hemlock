//! Escape hatch for boards whose CPLD/LED/reset behavior can't be expressed
//! as manifest data. The default [`GenericQuirks`] does nothing; a board
//! needing special handling registers a named impl here and selects it via
//! `[hardware.quirks] driver = "<name>"`.
//!
//! Keep this registry small: if several boards need the same hook, promote
//! the behavior to manifest data instead.

use crate::Platform;

/// Board-specific hooks invoked at well-defined points of daemon lifecycles.
///
/// All hooks default to no-ops; implementations override only what their
/// board needs. Hooks must be idempotent — daemons may restart at any time.
pub trait PlatformQuirks: Send + Sync {
    /// Registry name, matching `[hardware.quirks] driver`.
    fn name(&self) -> &'static str;

    /// Runs in syncd before the SAI switch is created (e.g. take the ASIC
    /// out of CPLD-held reset).
    fn pre_asic_init(&self, _platform: &Platform) -> Result<(), QuirkError> {
        Ok(())
    }

    /// Runs in syncd after the SAI switch is created (e.g. load LED
    /// microcode that SAI itself does not handle).
    fn post_asic_init(&self, _platform: &Platform) -> Result<(), QuirkError> {
        Ok(())
    }

    /// Runs in pmon after the i2c topology is instantiated.
    fn post_hw_init(&self, _platform: &Platform) -> Result<(), QuirkError> {
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("quirk {quirk} failed during {stage}: {message}")]
pub struct QuirkError {
    pub quirk: &'static str,
    pub stage: &'static str,
    pub message: String,
}

/// The default: a board fully described by its manifest.
pub struct GenericQuirks;

impl PlatformQuirks for GenericQuirks {
    fn name(&self) -> &'static str {
        "generic"
    }
}

/// Look up a quirks implementation by registry name.
pub fn by_name(name: &str) -> Option<Box<dyn PlatformQuirks>> {
    match name {
        "generic" => Some(Box::new(GenericQuirks)),
        _ => None,
    }
}

/// Names known to the registry, for lint diagnostics.
pub fn known_names() -> &'static [&'static str] {
    &["generic"]
}
