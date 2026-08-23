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

/// Celestica E1031 (Haliburton). The SMC CPLD powers up with
/// `LED_OPMOD = 0`, a forced-default mode that drives the SFP+ port LEDs
/// solid green and ignores the system-LED register. Switch it to normal
/// operation (bench-verified 2026-08-23; see docs/e1031-led-bringup.md):
/// port LEDs then follow their real sources (PHY pins for copper, the
/// ASIC LED processor for SFP+), and the system LEDs follow `LED_FPS`,
/// which must therefore be driven or it reads back as "off".
pub struct HaliburtonQuirks;

/// SMC CPLD LPC io-port registers (platforms/cel-e1031/kmod/.../smc.c).
const SMC_LED_OPMOD: u64 = 0x0208;
const SMC_LED_FPS: u64 = 0x020a;

/// LED_OPMOD: 1 = normal operation (0 = power-on forced default).
const LED_OPMOD_NORMAL: u8 = 0x01;
/// LED_FPS: [3:2] status = green, [1:0] master = green.
const LED_FPS_GREEN: u8 = 0x05;

impl PlatformQuirks for HaliburtonQuirks {
    fn name(&self) -> &'static str {
        "haliburton"
    }

    fn post_hw_init(&self, _platform: &Platform) -> Result<(), QuirkError> {
        // LEDs are cosmetic: log-and-continue, never fail pmon over them.
        for (name, port, value) in [
            ("LED_OPMOD normal mode", SMC_LED_OPMOD, LED_OPMOD_NORMAL),
            ("LED_FPS system green", SMC_LED_FPS, LED_FPS_GREEN),
        ] {
            match write_io_port(port, value) {
                Ok(()) => tracing::info!(register = name, value, "SMC CPLD LED write"),
                Err(e) => {
                    tracing::warn!(register = name, error = %e, "SMC CPLD LED write failed")
                }
            }
        }
        Ok(())
    }
}

/// One byte to a legacy x86 io port via /dev/port (root only; the SMC
/// CPLD sits on the LPC bus).
#[cfg(unix)]
fn write_io_port(port: u64, value: u8) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new().write(true).open("/dev/port")?;
    file.seek(SeekFrom::Start(port))?;
    file.write_all(&[value])
}

#[cfg(not(unix))]
fn write_io_port(_port: u64, _value: u8) -> std::io::Result<()> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

/// Look up a quirks implementation by registry name.
pub fn by_name(name: &str) -> Option<Box<dyn PlatformQuirks>> {
    match name {
        "generic" => Some(Box::new(GenericQuirks)),
        "haliburton" => Some(Box::new(HaliburtonQuirks)),
        _ => None,
    }
}

/// Names known to the registry, for lint diagnostics.
pub fn known_names() -> &'static [&'static str] {
    &["generic", "haliburton"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_resolves_all_known_names() {
        for name in known_names() {
            let quirks = by_name(name).expect("known name resolves");
            assert_eq!(&quirks.name(), name);
        }
        assert!(by_name("bogus").is_none());
    }
}
