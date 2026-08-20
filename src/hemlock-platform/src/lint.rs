//! Manifest validation beyond what serde can express, surfaced as
//! diagnostics for `hemlockctl platform lint`.
//!
//! Errors are structural problems that would break daemons at runtime.
//! Warnings cover things a fresh checkout legitimately lacks — most notably
//! vendor blobs (config.bcm, LED microcode), which are never committed.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::{quirks, schema::BusRef, Platform};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tag = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(f, "{tag}: {}", self.message)
    }
}

#[derive(Debug, Default)]
pub struct LintReport {
    pub diagnostics: Vec<Diagnostic>,
}

impl LintReport {
    pub fn passed(&self) -> bool {
        !self
            .diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }

    fn error(&mut self, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            severity: Severity::Error,
            message: message.into(),
        });
    }

    fn warning(&mut self, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            severity: Severity::Warning,
            message: message.into(),
        });
    }
}

/// Validate a loaded platform. Load errors (unparsable TOML, bad schema
/// version, ragged port groups) are already fatal before lint runs.
pub fn lint(platform: &Platform) -> LintReport {
    let mut report = LintReport::default();
    let m = &platform.manifest;

    // --- [platform] ---
    let id = &m.platform.id;
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || id.is_empty()
    {
        report.error(format!(
            "platform.id {id:?} must be non-empty lowercase [a-z0-9-]"
        ));
    }
    if let Some(dir_name) = platform.dir.file_name().and_then(|n| n.to_str()) {
        if dir_name != id && dir_name != "_template" {
            report.warning(format!(
                "platform.id {id:?} does not match directory name {dir_name:?}"
            ));
        }
    }
    let onie = &m.platform.onie_machine;
    let onie_ok = onie.rsplit_once("-r").is_some_and(|(head, rev)| {
        !head.is_empty()
            && !rev.is_empty()
            && rev.chars().all(|c| c.is_ascii_digit())
            && onie
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    });
    if !onie_ok {
        report.error(format!(
            "platform.onie_machine {onie:?} does not look like an ONIE machine string \
             (expected e.g. \"x86_64-cel_e1031-r0\")"
        ));
    }
    for (field, value) in [
        ("vendor", &m.platform.vendor),
        ("model", &m.platform.model),
        ("asic_family", &m.platform.asic_family),
        ("asic", &m.platform.asic),
    ] {
        if value.trim().is_empty() {
            report.error(format!("platform.{field} must not be empty"));
        }
    }
    if m.platform.asic_family != "broadcom-xgs" {
        report.warning(format!(
            "platform.asic_family {:?} is not a known family (known: broadcom-xgs)",
            m.platform.asic_family
        ));
    }

    // --- [sai] ---
    if m.sai.package.trim().is_empty() {
        report.error("sai.package must not be empty");
    }
    if m.sai.version_pin.trim().is_empty() {
        report.error("sai.version_pin must not be empty");
    }
    if !m.sai.libsai_path.has_root() {
        report.error(format!(
            "sai.libsai_path {:?} must be an absolute path inside the image",
            m.sai.libsai_path
        ));
    }
    if m.sai.config_bcm.has_root() {
        report.error(format!(
            "sai.config_bcm {:?} must be relative to the platform directory",
            m.sai.config_bcm
        ));
    }
    // Vendor data files are never committed; missing ones are warnings.
    for file in std::iter::once(&m.sai.config_bcm).chain(m.sai.extra_files.iter()) {
        if file.has_root() {
            report.error(format!("sai file {file:?} must be a relative path"));
        } else if !platform.dir.join(file).exists() {
            report.warning(format!(
                "vendor data file {} is absent (expected next to platform.toml; \
                 see vendor/sai/README.md)",
                file.display()
            ));
        }
    }

    // --- [ports] ---
    let mut names = HashSet::new();
    let mut indexes = HashSet::new();
    let mut lanes_seen: HashMap<u32, String> = HashMap::new();
    for port in &platform.ports {
        if !names.insert(port.name.clone()) {
            report.error(format!("duplicate port name {:?}", port.name));
        }
        if !indexes.insert(port.index) {
            report.error(format!(
                "duplicate front-panel index {} (port {:?})",
                port.index, port.name
            ));
        }
        if port.speed_mbps == 0 {
            report.error(format!("port {:?} has speed_mbps = 0", port.name));
        }
        if port.lanes.is_empty() {
            report.error(format!("port {:?} has no lanes", port.name));
        }
        for lane in &port.lanes {
            if let Some(other) = lanes_seen.insert(*lane, port.name.clone()) {
                report.error(format!(
                    "lane {lane} assigned to both {other:?} and {:?}",
                    port.name
                ));
            }
        }
    }

    // --- [hardware] cross-references ---
    let hw = &m.hardware;

    let mut known_buses: HashSet<u32> = HashSet::new();
    for mux in &hw.i2c.muxes {
        for ch in 0..mux.channels {
            if !known_buses.insert(mux.child_bus_base + ch) {
                report.error(format!(
                    "i2c mux {:?}: child bus {} claimed by more than one mux channel",
                    mux.name,
                    mux.child_bus_base + ch
                ));
            }
        }
        if let BusRef::Named(name) = &mux.parent_bus {
            if name != "root" {
                report.error(format!(
                    "i2c mux {:?}: parent_bus {name:?} (only \"root\" or a bus number)",
                    mux.name
                ));
            }
        }
    }
    for mux in &hw.i2c.muxes {
        if let BusRef::Number(parent) = mux.parent_bus {
            if !known_buses.contains(&parent) {
                report.warning(format!(
                    "i2c mux {:?}: parent bus {parent} is not produced by any declared mux \
                     (assuming it exists on the platform)",
                    mux.name
                ));
            }
        }
    }
    if !hw.i2c.muxes.is_empty() && hw.i2c.root_adapter.is_none() {
        report.warning("i2c topology declares muxes but no root_adapter name to anchor them");
    }
    for dev in &hw.i2c.devices {
        if !known_buses.contains(&dev.bus) {
            report.warning(format!(
                "i2c device {} ({}) on bus {} which no declared mux produces",
                dev.purpose, dev.driver, dev.bus
            ));
        }
    }

    let sensor_names: HashSet<&str> = hw.thermal.sensors.iter().map(|s| s.name.as_str()).collect();
    for s in &hw.thermal.sensors {
        if s.warn_c >= s.crit_c {
            report.error(format!(
                "thermal sensor {:?}: warn_c {} must be below crit_c {}",
                s.name, s.warn_c, s.crit_c
            ));
        }
    }
    if let Some(fc) = &hw.thermal.fan_control {
        if !sensor_names.contains(fc.sensor.as_str()) {
            report.error(format!(
                "fan_control.sensor {:?} is not a declared thermal sensor",
                fc.sensor
            ));
        }
        if fc.curve.is_empty() {
            report.error("fan_control.curve must have at least one point");
        }
        for pair in fc.curve.windows(2) {
            if pair[1].temp_c <= pair[0].temp_c {
                report.error(format!(
                    "fan_control.curve temperatures must be strictly increasing \
                     ({} then {})",
                    pair[0].temp_c, pair[1].temp_c
                ));
            }
        }
        for point in &fc.curve {
            if point.pwm_percent > 100 {
                report.error(format!(
                    "fan_control.curve pwm_percent {} exceeds 100",
                    point.pwm_percent
                ));
            }
        }
        if fc.interval_secs == 0 {
            report.error("fan_control.interval_secs must be >= 1");
        }
        if hw.thermal.fans.is_empty() {
            report.warning("fan_control declared but no [[hardware.thermal.fan]] entries");
        }
    }

    for t in &hw.transceivers {
        if !names.contains(&t.port) {
            report.error(format!("transceiver references unknown port {:?}", t.port));
        }
        if !known_buses.contains(&t.bus) {
            report.warning(format!(
                "transceiver for {} on i2c bus {} which no declared mux produces",
                t.port, t.bus
            ));
        }
    }

    // --- [hardware.quirks] ---
    if quirks::by_name(&hw.quirks.driver).is_none() {
        report.error(format!(
            "quirks driver {:?} is not registered (known: {})",
            hw.quirks.driver,
            quirks::known_names().join(", ")
        ));
    }

    report
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::Platform;

    fn platform_from(toml_text: &str) -> Platform {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("platform.toml"), toml_text).unwrap();
        Platform::load(dir.path()).unwrap()
    }

    fn base(extra: &str) -> String {
        format!(
            r#"
schema_version = 1

[platform]
id = "test-sw"
onie_machine = "x86_64-test_sw-r0"
vendor = "Test"
model = "TSW-1"
asic_family = "broadcom-xgs"
asic = "helix4"

[sai]
package = "libsaibcm"
version_pin = "3.7.x-helix4"
libsai_path = "/usr/lib/libsai.so.1"
config_bcm = "config.bcm"

[[ports.group]]
prefix = "Ethernet"
name_start = 0
index_start = 1
speed_mbps = 1000
lanes = [1, 2]

{extra}
"#
        )
    }

    #[test]
    fn clean_manifest_passes_with_blob_warning() {
        let report = lint(&platform_from(&base("")));
        assert!(report.passed(), "{:?}", report.diagnostics);
        // config.bcm absent from a fresh checkout -> warning, never error.
        assert!(report
            .diagnostics
            .iter()
            .any(|d| d.severity == Severity::Warning && d.message.contains("config.bcm")));
    }

    #[test]
    fn duplicate_lanes_fail() {
        let p = platform_from(&base(
            "[[ports.port]]\nname = \"Ethernet9\"\nindex = 9\nspeed_mbps = 1000\nlanes = [2]",
        ));
        let report = lint(&p);
        assert!(!report.passed());
        assert!(report
            .diagnostics
            .iter()
            .any(|d| d.message.contains("lane 2")));
    }

    #[test]
    fn fan_curve_must_reference_declared_sensor() {
        let p = platform_from(&base(
            r#"
[hardware.thermal.fan_control]
sensor = "ghost"
[[hardware.thermal.fan_control.curve]]
temp_c = 30.0
pwm_percent = 40
"#,
        ));
        let report = lint(&p);
        assert!(!report.passed());
    }

    #[test]
    fn unknown_quirks_driver_fails() {
        let p = platform_from(&base("[hardware.quirks]\ndriver = \"nonexistent\""));
        assert!(!lint(&p).passed());
    }

    #[test]
    fn bad_onie_machine_fails() {
        let text = base("").replace("x86_64-test_sw-r0", "not a machine string");
        let p = platform_from(&text);
        assert!(!lint(&p).passed());
    }
}
