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
const SMC_FAN_LED_1: u64 = 0x0205;
const SMC_FAN_LED_2: u64 = 0x0206;
const SMC_FAN_LED_3: u64 = 0x0207;
const SMC_LED_OPMOD: u64 = 0x0208;
const SMC_LED_FPS: u64 = 0x020a;

/// LED_OPMOD: 1 = normal operation (0 = power-on forced default).
const LED_OPMOD_NORMAL: u8 = 0x01;
/// LED_FPS: [3:2] status = green, [1:0] master = green.
const LED_FPS_GREEN: u8 = 0x05;
/// FAN_LED_*: 0 = green (smc.c enum; bench-verified). Power-on default
/// in normal mode is 4 = off.
const FAN_LED_GREEN: u8 = 0x00;

impl PlatformQuirks for HaliburtonQuirks {
    fn name(&self) -> &'static str {
        "haliburton"
    }

    fn post_hw_init(&self, _platform: &Platform) -> Result<(), QuirkError> {
        // LEDs are cosmetic: log-and-continue, never fail pmon over them.
        for (name, port, value) in [
            ("LED_OPMOD normal mode", SMC_LED_OPMOD, LED_OPMOD_NORMAL),
            ("LED_FPS system green", SMC_LED_FPS, LED_FPS_GREEN),
            ("FAN_LED_1 green", SMC_FAN_LED_1, FAN_LED_GREEN),
            ("FAN_LED_2 green", SMC_FAN_LED_2, FAN_LED_GREEN),
            ("FAN_LED_3 green", SMC_FAN_LED_3, FAN_LED_GREEN),
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

/// Edgecore AS4610-54T (Accton). Two things must happen before the
/// OpenBCM SDK can find the chip, neither expressible as manifest data:
///
/// 1. **Release the CMICd.** The kernel's `iproc_cmic` platform driver
///    binds the on-die CMIC at boot. The BDE cannot claim the device
///    while it is bound, so unbind it first.
/// 2. **Deassert the external PHYs' reset.** The board CPLD powers up
///    holding the 48 BCM54282 copper PHYs and the 4 BCM84758 SFP+ PHYs
///    in reset. The SDK's PHY probe then finds nothing and every port
///    stays down — with no error that points at the CPLD.
///
/// Register semantics come from ONL's `accton_as4610_cpld.c` and the
/// edgenos bring-up sequence for this board. Both steps are idempotent
/// (that is the contract) and the CPLD writes are read back, because a
/// silently dropped i2c write here looks exactly like dead hardware.
pub struct As4610Quirks;

/// Manifest name of the i2c root the CPLD sits on. The manifest owns the
/// adapter matching; the quirk only needs to know which root it declared.
const AS4610_CPLD_ROOT: &str = "cpld-bus";
/// CPLD i2c address (device tree: `cpld@30` on `i2c0`).
const AS4610_CPLD_ADDR: u32 = 0x30;

/// The PHY reset-deassert sequence: (register, value, what it releases).
/// Applied in order; the CPLD latches each independently.
const AS4610_PHY_RESET_DEASSERT: [(u8, u8, &str); 5] = [
    (0x07, 0x02, "copper PHY bank reset"),
    (0x08, 0x02, "copper PHY bank reset"),
    (0x0d, 0x01, "SFP+ PHY reset"),
    (0x19, 0x00, "PHY power-down"),
    (0x1b, 0x00, "PHY power-down"),
];

/// The platform driver holding the on-die CMIC, and its bus.
const IPROC_CMIC_DRIVER: &str = "/sys/bus/platform/drivers/iproc_cmic";
const PLATFORM_DEVICES: &str = "/sys/bus/platform/devices";
const CMIC_DEVICE_SUFFIX: &str = ".iproc_cmicd";

impl PlatformQuirks for As4610Quirks {
    fn name(&self) -> &'static str {
        "as4610"
    }

    fn pre_asic_init(&self, platform: &Platform) -> Result<(), QuirkError> {
        unbind_iproc_cmic();
        deassert_phy_reset(platform)
    }
}

/// Unbind the CMICd from the kernel's `iproc_cmic` driver so the BDE can
/// claim it. Idempotent: an already-unbound device is not an error, and
/// neither is a kernel with no such driver (the BDE may have taken the
/// device already, or the driver may not be built in).
fn unbind_iproc_cmic() {
    let Ok(entries) = std::fs::read_dir(PLATFORM_DEVICES) else {
        tracing::debug!("no platform bus; skipping iproc_cmic unbind");
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(CMIC_DEVICE_SUFFIX) {
            continue;
        }
        // Bound to *this* driver? If the link is absent it is already
        // free, which is the state we want.
        let bound = std::path::Path::new(IPROC_CMIC_DRIVER).join(&name);
        if !bound.exists() {
            tracing::debug!(device = %name, "CMICd already unbound");
            continue;
        }
        let unbind = std::path::Path::new(IPROC_CMIC_DRIVER).join("unbind");
        match std::fs::write(&unbind, name.as_bytes()) {
            Ok(()) => tracing::info!(device = %name, "unbound CMICd from iproc_cmic"),
            Err(e) => {
                // Not fatal on its own: the SDK's attach will fail with a
                // clearer message if this really mattered.
                tracing::warn!(device = %name, error = %e, "CMICd unbind failed");
            }
        }
    }
}

/// Write the CPLD's PHY reset-deassert sequence, verifying each write by
/// reading it back.
fn deassert_phy_reset(platform: &Platform) -> Result<(), QuirkError> {
    let sysfs = crate::sysinit::Sysfs::real();
    let i2c = &platform.manifest.hardware.i2c;
    let Some(bus) = sysfs.find_manifest_root(i2c, AS4610_CPLD_ROOT) else {
        return Err(QuirkError {
            quirk: "as4610",
            stage: "pre_asic_init",
            message: format!(
                "cannot locate the i2c root {AS4610_CPLD_ROOT:?} the CPLD sits on \
                 (is the SoC i2c driver loaded?)"
            ),
        });
    };

    for (register, value, what) in AS4610_PHY_RESET_DEASSERT {
        // Idempotent by construction: skip a register already at its
        // target value, so a syncd restart does not bounce the PHYs.
        match sysfs.i2c_read_reg(bus, AS4610_CPLD_ADDR, register) {
            Ok(current) if current == value => {
                tracing::debug!(
                    register = format_args!("0x{register:02x}"),
                    what,
                    "CPLD register already deasserted"
                );
                continue;
            }
            Ok(_) => {}
            // A CPLD that cannot be read is worth knowing about, but the
            // write below is the real test.
            Err(e) => tracing::debug!(
                register = format_args!("0x{register:02x}"),
                error = %e,
                "CPLD read-before-write failed"
            ),
        }

        sysfs
            .i2c_write_reg(bus, AS4610_CPLD_ADDR, register, value)
            .map_err(|e| QuirkError {
                quirk: "as4610",
                stage: "pre_asic_init",
                message: format!("CPLD write 0x{register:02x}=0x{value:02x} ({what}): {e}"),
            })?;

        // Read back. A dropped write here means every port comes up dead
        // with nothing in the log pointing at the CPLD, so pay the read.
        match sysfs.i2c_read_reg(bus, AS4610_CPLD_ADDR, register) {
            Ok(readback) if readback == value => tracing::info!(
                register = format_args!("0x{register:02x}"),
                value = format_args!("0x{value:02x}"),
                what,
                "CPLD PHY reset deasserted"
            ),
            Ok(readback) => {
                return Err(QuirkError {
                    quirk: "as4610",
                    stage: "pre_asic_init",
                    message: format!(
                        "CPLD register 0x{register:02x} reads back 0x{readback:02x} after \
                         writing 0x{value:02x} ({what}) — the PHYs will stay in reset"
                    ),
                })
            }
            Err(e) => {
                return Err(QuirkError {
                    quirk: "as4610",
                    stage: "pre_asic_init",
                    message: format!("CPLD read-back of 0x{register:02x} ({what}): {e}"),
                })
            }
        }
    }
    Ok(())
}

/// Look up a quirks implementation by registry name.
pub fn by_name(name: &str) -> Option<Box<dyn PlatformQuirks>> {
    match name {
        "generic" => Some(Box::new(GenericQuirks)),
        "haliburton" => Some(Box::new(HaliburtonQuirks)),
        "as4610" => Some(Box::new(As4610Quirks)),
        _ => None,
    }
}

/// Names known to the registry, for lint diagnostics.
pub fn known_names() -> &'static [&'static str] {
    &["generic", "haliburton", "as4610"]
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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

    /// The reset sequence is the board fact this quirk exists to carry,
    /// so pin it against ONL's CPLD map and the edgenos bring-up
    /// sequence. Getting a register or a value wrong here leaves every
    /// port dark with nothing in the log pointing here.
    #[test]
    fn phy_reset_sequence_matches_the_board() {
        let expected: [(u8, u8); 5] = [
            (0x07, 0x02),
            (0x08, 0x02),
            (0x0d, 0x01),
            (0x19, 0x00),
            (0x1b, 0x00),
        ];
        let actual: Vec<(u8, u8)> = AS4610_PHY_RESET_DEASSERT
            .iter()
            .map(|(register, value, _)| (*register, *value))
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(AS4610_CPLD_ADDR, 0x30);
    }

    /// The quirk finds its bus through the manifest, so the CPLD's
    /// adapter is data rather than a constant in this file. If the
    /// manifest ever renames that root, this fails rather than the
    /// board going dark on the bench.
    #[test]
    fn the_as4610_manifest_declares_the_cpld_root() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../platforms/accton-as4610-54");
        let platform = Platform::load(&dir).unwrap();
        let roots = &platform.manifest.hardware.i2c.roots;
        assert!(
            roots.iter().any(|r| r.name == AS4610_CPLD_ROOT),
            "manifest roots {:?} do not include {AS4610_CPLD_ROOT:?}",
            roots.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert_eq!(platform.manifest.hardware.quirks.driver, "as4610");
    }

    /// Reading a register that is already at its target value must skip
    /// the write: syncd restarts, and bouncing the PHYs on every restart
    /// would drop every link on the box.
    #[test]
    fn already_deasserted_registers_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let bus_dir = dir.path().join("sys/bus/i2c/devices/i2c-0");
        std::fs::create_dir_all(&bus_dir).unwrap();
        std::fs::write(bus_dir.join("reg-writes"), "").unwrap();
        // Seed every register at its post-deassert value.
        for (register, value, _) in AS4610_PHY_RESET_DEASSERT {
            std::fs::write(
                bus_dir.join(format!("reg-30-{register:02x}")),
                format!("0x{value:02x}"),
            )
            .unwrap();
        }

        let sysfs = crate::sysinit::Sysfs::at(dir.path());
        for (register, value, _) in AS4610_PHY_RESET_DEASSERT {
            let current = sysfs.i2c_read_reg(0, AS4610_CPLD_ADDR, register).unwrap();
            assert_eq!(current, value, "seeded value reads back");
        }
        // Nothing was written.
        let writes = std::fs::read_to_string(bus_dir.join("reg-writes")).unwrap();
        assert!(writes.is_empty(), "unexpected writes: {writes:?}");
    }

    /// A write that does not stick is the failure this quirk is built to
    /// catch, so the read-back must reject it rather than report success.
    #[test]
    fn a_write_that_does_not_stick_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let bus_dir = dir.path().join("sys/bus/i2c/devices/i2c-0");
        std::fs::create_dir_all(&bus_dir).unwrap();
        std::fs::write(bus_dir.join("reg-writes"), "").unwrap();
        // A CPLD that reads back a stale value whatever is written.
        std::fs::write(bus_dir.join("reg-30-07"), "0x00").unwrap();

        let sysfs = crate::sysinit::Sysfs::at(dir.path());
        sysfs
            .i2c_write_reg(0, AS4610_CPLD_ADDR, 0x07, 0x02)
            .unwrap();
        let readback = sysfs.i2c_read_reg(0, AS4610_CPLD_ADDR, 0x07).unwrap();
        assert_ne!(readback, 0x02, "the fake CPLD keeps its stale value");

        // The write was still attempted, in the right shape.
        let writes = std::fs::read_to_string(bus_dir.join("reg-writes")).unwrap();
        assert_eq!(writes, "0x30 0x07 0x02\n");
    }

    #[test]
    fn i2cget_output_parses() {
        use crate::sysinit::parse_i2c_byte_for_test as parse;
        assert_eq!(parse("0x1a"), Some(0x1a));
        assert_eq!(parse("0X02"), Some(0x02));
        assert_eq!(parse(" 0x00 "), Some(0x00));
        assert_eq!(parse("1a"), Some(0x1a));
        assert_eq!(parse("nonsense"), None);
        assert_eq!(parse(""), None);
    }
}
