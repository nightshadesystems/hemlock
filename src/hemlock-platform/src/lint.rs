//! Manifest validation beyond what serde can express, surfaced as
//! diagnostics for `hemlockctl platform lint`.
//!
//! Errors are structural problems that would break daemons at runtime.
//! Warnings cover things a fresh checkout legitimately lacks — most notably
//! vendor blobs (config.bcm, LED microcode), which are never committed.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::quirks;
use crate::schema::{AsicAttach, BusRef, SaiBackendKind, DEFAULT_ROOT_NAME, KNOWN_CPU_ARCHES};
use crate::Platform;

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
    // cpu_arch drives the rootfs, the boot artifacts and the installer
    // target, and the ONIE machine string already encodes the same fact.
    // Two sources of truth that can disagree silently is how you ship an
    // x86 image for an ARM box, so cross-check them.
    match KNOWN_CPU_ARCHES
        .iter()
        .find(|(arch, _)| *arch == m.platform.cpu_arch)
    {
        None => report.error(format!(
            "platform.cpu_arch {:?} is not known (known: {})",
            m.platform.cpu_arch,
            KNOWN_CPU_ARCHES
                .iter()
                .map(|(arch, _)| *arch)
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Some((arch, prefix)) => {
            if !onie.starts_with(prefix) {
                report.error(format!(
                    "platform.cpu_arch {arch:?} expects an onie_machine starting {prefix:?}, \
                     but it is {onie:?}"
                ));
            }
        }
    }

    // --- [sai] ---
    // Which [sai] fields are required, and which are meaningless, depends
    // on the backend. Saying so here keeps the type simple and the error
    // specific: a manifest can never half-declare a backend.
    match m.sai.backend {
        SaiBackendKind::Sai => {
            for (field, value) in [
                ("package", m.sai.package.as_deref()),
                ("version_pin", m.sai.version_pin.as_deref()),
            ] {
                match value {
                    None => report.error(format!("sai.{field} is required for backend \"sai\"")),
                    Some(v) if v.trim().is_empty() => {
                        report.error(format!("sai.{field} must not be empty"))
                    }
                    Some(_) => {}
                }
            }
            match &m.sai.libsai_path {
                None => report.error("sai.libsai_path is required for backend \"sai\""),
                Some(path) if !path.has_root() => report.error(format!(
                    "sai.libsai_path {path:?} must be an absolute path inside the image"
                )),
                Some(_) => {}
            }
            for field in ["shim_path", "abi_major"] {
                let set = match field {
                    "shim_path" => m.sai.shim_path.is_some(),
                    _ => m.sai.abi_major.is_some(),
                };
                if set {
                    report.error(format!("sai.{field} applies only to backend \"openbcm\""));
                }
            }
        }
        SaiBackendKind::Openbcm => {
            match &m.sai.shim_path {
                None => report.error("sai.shim_path is required for backend \"openbcm\""),
                Some(path) if !path.has_root() => report.error(format!(
                    "sai.shim_path {path:?} must be an absolute path inside the image"
                )),
                Some(_) => {}
            }
            if m.sai.abi_major.is_none() {
                report.error("sai.abi_major is required for backend \"openbcm\"");
            }
            // There is no vendor SAI package to pin, and no SAI headers to
            // compile against; carrying them would imply a blob exists.
            for (field, set) in [
                ("package", m.sai.package.is_some()),
                ("version_pin", m.sai.version_pin.is_some()),
                ("api_headers", m.sai.api_headers.is_some()),
                ("libsai_path", m.sai.libsai_path.is_some()),
            ] {
                if set {
                    report.error(format!(
                        "sai.{field} applies only to backend \"sai\" (no vendor SAI exists for an openbcm platform)"
                    ));
                }
            }
            // The ASIC-presence probe is what stops --auto-mock mocking a
            // live switch; an on-SoC CMIC has no PCI device to find.
            if m.platform.asic_attach != AsicAttach::Soc {
                report.warning(
                    "backend \"openbcm\" with platform.asic_attach = \"pcie\": \
                     an on-die CMIC has no PCI device, so --auto-mock would mock real hardware",
                );
            }
        }
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

    // sdk_names is the manifest's half of the startup assertion against
    // what the backend reports, so a short list would silently stop
    // checking the ports it does not cover.
    for group in &m.ports.groups {
        if group.sdk_names.is_empty() {
            continue;
        }
        let port_count = group.lanes.len() / group.lanes_per_port.max(1) as usize;
        if group.sdk_names.len() != port_count {
            report.error(format!(
                "port group {:?}: sdk_names has {} entries but the group expands to {port_count} \
                 ports (one SDK name per port, in expansion order)",
                group.prefix,
                group.sdk_names.len()
            ));
        }
    }
    let mut sdk_names_seen: HashMap<&str, &str> = HashMap::new();
    for port in &platform.ports {
        if let Some(sdk) = &port.sdk_name {
            if sdk.trim().is_empty() {
                report.error(format!("port {:?} has an empty sdk_name", port.name));
            }
            if let Some(other) = sdk_names_seen.insert(sdk.as_str(), port.name.as_str()) {
                report.error(format!(
                    "sdk_name {sdk:?} assigned to both {other:?} and {:?}",
                    port.name
                ));
            }
        }
    }
    if m.sai.backend == SaiBackendKind::Openbcm && sdk_names_seen.is_empty() {
        report.warning(
            "backend \"openbcm\" but no sdk_names are declared: syncd cannot check the \
             port map against what the SDK reports",
        );
    }

    // --- [hardware] cross-references ---
    let hw = &m.hardware;

    // Declared root adapters: `root_adapter` is shorthand for one named
    // `root`, and `[[hardware.i2c.root]]` names the rest.
    let mut known_roots: HashSet<&str> = HashSet::new();
    if hw.i2c.root_adapter.is_some() {
        known_roots.insert(DEFAULT_ROOT_NAME);
    }
    for root in &hw.i2c.roots {
        if root.name.trim().is_empty() {
            report.error("i2c root has an empty name");
        }
        if root.adapter.trim().is_empty() {
            report.error(format!(
                "i2c root {:?} has an empty adapter name",
                root.name
            ));
        }
        if !known_roots.insert(root.name.as_str()) {
            let clash = if root.name == DEFAULT_ROOT_NAME && hw.i2c.root_adapter.is_some() {
                " (root_adapter already declares a root by that name)"
            } else {
                ""
            };
            report.error(format!("duplicate i2c root name {:?}{clash}", root.name));
        }
    }

    // Every `bus = "name"` must name one of them.
    let check_root_ref = |bus: &BusRef, what: &str, report: &mut LintReport| {
        if let BusRef::Named(name) = bus {
            if !known_roots.contains(name.as_str()) {
                report.error(format!(
                    "{what} references i2c root {name:?}, which is not declared \
                     (declared: {})",
                    if known_roots.is_empty() {
                        "none".to_string()
                    } else {
                        let mut names: Vec<&str> = known_roots.iter().copied().collect();
                        names.sort_unstable();
                        names.join(", ")
                    }
                ));
            }
        }
    };

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
        check_root_ref(
            &mux.parent_bus,
            &format!("i2c mux {:?} parent_bus", mux.name),
            &mut report,
        );
    }
    for write in &hw.i2c.pre_writes {
        check_root_ref(
            &write.bus,
            &format!("i2c pre_write {:?}", write.purpose),
            &mut report,
        );
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
    if !hw.i2c.muxes.is_empty() && known_roots.is_empty() {
        report.warning("i2c topology declares muxes but no root adapter to anchor them");
    }
    for dev in &hw.i2c.devices {
        check_root_ref(
            &dev.bus,
            &format!("i2c device {} ({})", dev.purpose, dev.driver),
            &mut report,
        );
        if let BusRef::Number(bus) = dev.bus {
            if !known_buses.contains(&bus) {
                report.warning(format!(
                    "i2c device {} ({}) on bus {bus} which no declared mux produces",
                    dev.purpose, dev.driver
                ));
            }
        }
    }
    for psu in &hw.psus {
        check_root_ref(&psu.bus, &format!("psu {:?}", psu.name), &mut report);
    }

    for fan in &hw.thermal.fans {
        if fan.pwm_max == 0 {
            report.error(format!(
                "fan {:?}: pwm_max must be >= 1 (255 for standard hwmon pwmN, \
                 100 for a percentage attribute)",
                fan.name
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
        check_root_ref(&t.bus, &format!("transceiver for {}", t.port), &mut report);
        if let BusRef::Number(bus) = t.bus {
            if !known_buses.contains(&bus) {
                report.warning(format!(
                    "transceiver for {} on i2c bus {bus} which no declared mux produces",
                    t.port
                ));
            }
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

    /// Every message a failing lint produced, for assertions that care
    /// about *why* it failed rather than just that it did.
    fn messages(report: &LintReport) -> String {
        report
            .diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The defaults keep an existing manifest meaning exactly what it did
    /// before the backend/arch fields existed.
    #[test]
    fn defaults_are_the_pre_existing_behavior() {
        let p = platform_from(&base(""));
        assert_eq!(p.manifest.sai.backend, SaiBackendKind::Sai);
        assert_eq!(p.manifest.platform.cpu_arch, "amd64");
        assert_eq!(p.manifest.platform.asic_attach, AsicAttach::Pcie);
        assert!(lint(&p).passed());
    }

    #[test]
    fn cpu_arch_must_agree_with_the_onie_machine_string() {
        // armhf on an x86_64- machine string: the mismatch that would
        // ship an ARM image for an x86 box.
        let p = platform_from(&base("").replace(
            "asic = \"helix4\"",
            "asic = \"helix4\"\ncpu_arch = \"armhf\"",
        ));
        let report = lint(&p);
        assert!(!report.passed());
        assert!(
            messages(&report).contains("cpu_arch"),
            "{}",
            messages(&report)
        );

        // ...and the matching pair passes.
        let ok = platform_from(
            &base("")
                .replace("x86_64-test_sw-r0", "arm-test_sw-r0")
                .replace(
                    "asic = \"helix4\"",
                    "asic = \"helix4\"\ncpu_arch = \"armhf\"",
                ),
        );
        assert!(lint(&ok).passed(), "{}", messages(&lint(&ok)));
    }

    #[test]
    fn unknown_cpu_arch_fails() {
        let p = platform_from(&base("").replace(
            "asic = \"helix4\"",
            "asic = \"helix4\"\ncpu_arch = \"sparc64\"",
        ));
        assert!(!lint(&p).passed());
    }

    /// An openbcm manifest, as the AS4610's will be shaped.
    fn openbcm_base(extra: &str) -> String {
        base(extra)
            .replace("x86_64-test_sw-r0", "arm-test_sw-r0")
            .replace(
                "asic = \"helix4\"",
                "asic = \"helix4\"\ncpu_arch = \"armhf\"\nasic_attach = \"soc\"",
            )
            .replace(
                "package = \"libsaibcm\"\nversion_pin = \"3.7.x-helix4\"\nlibsai_path = \"/usr/lib/libsai.so.1\"",
                "backend = \"openbcm\"\nshim_path = \"/usr/lib/libhemlockbcm.so.1\"\nabi_major = 1",
            )
    }

    #[test]
    fn openbcm_manifest_passes() {
        let p = platform_from(&openbcm_base(""));
        assert_eq!(p.manifest.sai.backend, SaiBackendKind::Openbcm);
        let report = lint(&p);
        assert!(report.passed(), "{}", messages(&report));
    }

    #[test]
    fn openbcm_requires_its_own_fields() {
        let p = platform_from(&openbcm_base("").replace("abi_major = 1", ""));
        let report = lint(&p);
        assert!(!report.passed());
        assert!(messages(&report).contains("abi_major"));

        let p = platform_from(
            &openbcm_base("").replace("shim_path = \"/usr/lib/libhemlockbcm.so.1\"", ""),
        );
        assert!(messages(&lint(&p)).contains("shim_path"));
    }

    /// The two backends' fields do not mix: carrying a SAI pin on an
    /// openbcm platform would imply a vendor blob that does not exist.
    #[test]
    fn backend_fields_do_not_mix() {
        let p = platform_from(
            &openbcm_base("").replace("abi_major = 1", "abi_major = 1\nversion_pin = \"8.4.50.0\""),
        );
        let report = lint(&p);
        assert!(!report.passed());
        assert!(messages(&report).contains("version_pin"));

        let p = platform_from(&base("").replace(
            "config_bcm = \"config.bcm\"",
            "config_bcm = \"config.bcm\"\nshim_path = \"/usr/lib/libhemlockbcm.so.1\"",
        ));
        let report = lint(&p);
        assert!(!report.passed());
        assert!(messages(&report).contains("shim_path"));
    }

    /// An on-die CMIC has no PCI device, so a pcie probe would let
    /// --auto-mock mock a live switch.
    #[test]
    fn openbcm_on_pcie_attach_warns() {
        let p = platform_from(&openbcm_base("").replace("asic_attach = \"soc\"", ""));
        let report = lint(&p);
        assert!(report.passed(), "a warning, not an error");
        assert!(
            messages(&report).contains("auto-mock"),
            "{}",
            messages(&report)
        );
    }

    #[test]
    fn named_i2c_roots_resolve_or_fail() {
        let roots = r#"
[[hardware.i2c.root]]
name = "cpld"
adapter = "Broadcom iProc SMBus adapter"
instance = 0

[[hardware.i2c.device]]
driver = "as4610_cpld"
bus = "cpld"
address = 0x30
purpose = "cpld"
"#;
        assert!(lint(&platform_from(&base(roots))).passed());

        // A device naming a root nobody declared.
        let dangling = r#"
[[hardware.i2c.device]]
driver = "as4610_cpld"
bus = "ghost"
address = 0x30
purpose = "cpld"
"#;
        let report = lint(&platform_from(&base(dangling)));
        assert!(!report.passed());
        assert!(messages(&report).contains("ghost"));
    }

    #[test]
    fn duplicate_root_names_fail() {
        let dup = r#"
[[hardware.i2c.root]]
name = "cpld"
adapter = "A"

[[hardware.i2c.root]]
name = "cpld"
adapter = "B"
"#;
        let report = lint(&platform_from(&base(dup)));
        assert!(!report.passed());
        assert!(messages(&report).contains("duplicate i2c root"));
    }

    /// `root_adapter` is shorthand for a root named `root`, so declaring
    /// both is a collision, not a merge.
    #[test]
    fn root_adapter_and_a_root_named_root_collide() {
        let clash = r#"
[hardware.i2c]
root_adapter = "SMBus iSMT adapter"

[[hardware.i2c.root]]
name = "root"
adapter = "Other adapter"
"#;
        let report = lint(&platform_from(&base(clash)));
        assert!(!report.passed());
        assert!(messages(&report).contains("root_adapter already declares"));
    }

    #[test]
    fn sdk_names_must_cover_every_port_in_the_group() {
        let short = base("").replace("lanes = [1, 2]", "lanes = [1, 2]\nsdk_names = [\"ge0\"]");
        let report = lint(&platform_from(&short));
        assert!(!report.passed());
        assert!(messages(&report).contains("sdk_names"));

        let ok = base("").replace(
            "lanes = [1, 2]",
            "lanes = [1, 2]\nsdk_names = [\"ge0\", \"ge1\"]",
        );
        let p = platform_from(&ok);
        assert!(lint(&p).passed());
        assert_eq!(p.ports[0].sdk_name.as_deref(), Some("ge0"));
        assert_eq!(p.ports[1].sdk_name.as_deref(), Some("ge1"));
    }

    #[test]
    fn duplicate_sdk_names_fail() {
        let dup = base("").replace(
            "lanes = [1, 2]",
            "lanes = [1, 2]\nsdk_names = [\"ge0\", \"ge0\"]",
        );
        let report = lint(&platform_from(&dup));
        assert!(!report.passed());
        assert!(messages(&report).contains("sdk_name"));
    }

    #[test]
    fn openbcm_without_sdk_names_warns() {
        let report = lint(&platform_from(&openbcm_base("")));
        assert!(report.passed());
        assert!(
            messages(&report).contains("sdk_names"),
            "{}",
            messages(&report)
        );
    }

    #[test]
    fn fan_pwm_max_must_be_nonzero() {
        let fan = r#"
[[hardware.thermal.fan]]
name = "FAN-1"
hwmon = "23-004d"
tach = "fan1"
pwm = "pwm1"
pwm_max = 0
"#;
        let report = lint(&platform_from(&base(fan)));
        assert!(!report.passed());
        assert!(messages(&report).contains("pwm_max"));
    }

    #[test]
    fn fan_attribute_overrides_default_to_hwmon_conventions() {
        let fan = r#"
[[hardware.thermal.fan]]
name = "FAN-1"
hwmon = "0-0030"
tach = "fan1"
pwm = "pwm1"
rpm_attr = "fan1_speed_rpm"
pwm_attr = "fan_duty_cycle_percentage"
pwm_max = 100
"#;
        let p = platform_from(&base(fan));
        assert!(lint(&p).passed());
        let fan = &p.manifest.hardware.thermal.fans[0];
        assert_eq!(fan.rpm_attr.as_deref(), Some("fan1_speed_rpm"));
        assert_eq!(fan.pwm_max, 100);

        // Absent, the standard hwmon convention still applies.
        let plain = platform_from(&base(
            "[[hardware.thermal.fan]]\nname = \"FAN-1\"\nhwmon = \"1-0020\"\ntach = \"fan1\"\npwm = \"pwm1\"",
        ));
        let fan = &plain.manifest.hardware.thermal.fans[0];
        assert!(fan.rpm_attr.is_none() && fan.pwm_attr.is_none());
        assert_eq!(fan.pwm_max, 255);
    }
}
