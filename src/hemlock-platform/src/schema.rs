//! The `platform.toml` manifest schema (v1).
//!
//! Everything a board contributes to Hemlock is data in this schema plus
//! vendor files sitting next to it. See `platforms/_template/platform.toml`
//! for a fully commented example.

use std::path::PathBuf;

use serde::Deserialize;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub platform: PlatformSection,
    pub sai: SaiSection,
    #[serde(default)]
    pub kernel: KernelSection,
    pub ports: PortsSection,
    #[serde(default)]
    pub hardware: HardwareSection,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformSection {
    /// Stable identifier; must match the directory name under `platforms/`.
    pub id: String,
    /// ONIE machine string, e.g. `x86_64-cel_e1031-r0`. Matched against
    /// `machine.conf` at install time.
    pub onie_machine: String,
    pub vendor: String,
    pub model: String,
    /// ASIC family selects the syncd init strategy, e.g. `broadcom-xgs`.
    pub asic_family: String,
    /// Concrete ASIC, e.g. `helix4`.
    pub asic: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaiSection {
    /// Vendor SAI package name, e.g. `libsaibcm`.
    pub package: String,
    /// Abstract per-platform pin (e.g. `3.7.x-helix4`), resolved to a
    /// concrete vendor blob by the build pipeline. Never global.
    pub version_pin: String,
    /// Absolute path of the vendor library inside the image.
    pub libsai_path: PathBuf,
    /// ASIC init config, relative to the platform directory.
    pub config_bcm: PathBuf,
    /// Additional vendor data files (LED microcode etc.), relative paths.
    #[serde(default)]
    pub extra_files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelSection {
    /// Kernel modules the pinned SAI expects (e.g. the Broadcom BDE pair).
    #[serde(default)]
    pub required_modules: Vec<String>,
}

/// Port table. Regular boards describe ports in arithmetic *groups*; oddball
/// ports can be listed individually. Both expand to [`crate::PortDef`]s.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortsSection {
    #[serde(default, rename = "group")]
    pub groups: Vec<PortGroup>,
    #[serde(default, rename = "port")]
    pub ports: Vec<PortEntry>,
}

/// A run of ports sharing prefix, speed, and lane count.
///
/// Port `i` (0-based within the group) gets:
/// - name  = `{prefix}{name_start + i}`
/// - index = `index_start + i`
/// - alias = `{alias_prefix}{index}` (if `alias_prefix` is set)
/// - lanes = the `i`-th chunk of `lanes`, `lanes_per_port` wide
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortGroup {
    pub prefix: String,
    pub name_start: u32,
    pub index_start: u32,
    pub speed_mbps: u32,
    #[serde(default = "one")]
    pub lanes_per_port: u32,
    /// Flat lane list, length = port count x lanes_per_port. Kept explicit
    /// because real boards swap lanes in hardware (the E1031 pair-swaps its
    /// entire 1G bank).
    pub lanes: Vec<u32>,
    #[serde(default)]
    pub alias_prefix: Option<String>,
    #[serde(default)]
    pub autoneg: bool,
    /// Supported breakout modes, e.g. `["4x10G"]`. Empty = no breakout.
    #[serde(default)]
    pub breakout: Vec<String>,
}

/// A single explicitly described port.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortEntry {
    pub name: String,
    pub index: u32,
    pub speed_mbps: u32,
    pub lanes: Vec<u32>,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub autoneg: bool,
    #[serde(default)]
    pub breakout: Vec<String>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HardwareSection {
    #[serde(default)]
    pub i2c: I2cSection,
    #[serde(default)]
    pub thermal: ThermalSection,
    #[serde(default, rename = "psu")]
    pub psus: Vec<Psu>,
    #[serde(default, rename = "transceiver")]
    pub transceivers: Vec<Transceiver>,
    #[serde(default)]
    pub quirks: QuirksSection,
}

/// I2C mux/device topology as data. pmon instantiates this at startup the
/// way SONiC's platform init scripts do, but driven by the manifest.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct I2cSection {
    /// Adapter-name prefix identifying the root SMBus adapter (its kernel
    /// bus number can vary between boots, so it is matched by name).
    #[serde(default)]
    pub root_adapter: Option<String>,
    #[serde(default, rename = "mux")]
    pub muxes: Vec<I2cMux>,
    #[serde(default, rename = "device")]
    pub devices: Vec<I2cDevice>,
}

/// Reference to an i2c bus: the root adapter or a numbered bus created by a
/// mux. In TOML: `parent_bus = "root"` or `parent_bus = 8`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum BusRef {
    Number(u32),
    Named(String), // "root"
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct I2cMux {
    pub name: String,
    /// Kernel driver, e.g. `pca9548`.
    pub driver: String,
    pub parent_bus: BusRef,
    pub address: u32,
    /// First child bus number this mux fans out to.
    pub child_bus_base: u32,
    pub channels: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct I2cDevice {
    /// Kernel driver name to bind, e.g. `24lc64t`, `max6699`, `emc2305`.
    pub driver: String,
    pub bus: u32,
    pub address: u32,
    /// What this device is for, e.g. `syseeprom`; shows up in diagnostics.
    pub purpose: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThermalSection {
    #[serde(default, rename = "sensor")]
    pub sensors: Vec<ThermalSensor>,
    #[serde(default, rename = "fan")]
    pub fans: Vec<FanDef>,
    #[serde(default)]
    pub fan_control: Option<FanControl>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThermalSensor {
    pub name: String,
    /// hwmon device identity, `<bus>-<addr>` sysfs style (e.g. `11-001a`).
    pub hwmon: String,
    /// Channel within the device, e.g. `temp3`.
    pub input: String,
    pub warn_c: f64,
    pub crit_c: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanDef {
    pub name: String,
    pub hwmon: String,
    /// Tach input within the device, e.g. `fan4`.
    pub tach: String,
    /// PWM output within the device, e.g. `pwm4`.
    pub pwm: String,
}

/// Linear-interpolated fan curve driven by one named sensor.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanControl {
    pub sensor: String,
    #[serde(default = "default_interval")]
    pub interval_secs: u32,
    /// Curve points, strictly increasing temperature. Below the first point
    /// fans run at its pwm; above the last, 100%.
    #[serde(rename = "curve")]
    pub curve: Vec<CurvePoint>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurvePoint {
    pub temp_c: f64,
    pub pwm_percent: u32,
}

fn default_interval() -> u32 {
    5
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Psu {
    pub name: String,
    pub bus: u32,
    pub address: u32,
    pub driver: String,
    #[serde(default)]
    pub eeprom_address: Option<u32>,
}

/// Maps a front-panel port to the i2c bus carrying its module EEPROM.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transceiver {
    pub port: String,
    pub bus: u32,
    /// EEPROM driver, e.g. `optoe1` (QSFP) or `optoe2` (SFP).
    pub driver: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuirksSection {
    /// Name of the registered [`crate::quirks::PlatformQuirks`] impl;
    /// `generic` for boards with no special behavior.
    pub driver: String,
}

impl Default for QuirksSection {
    fn default() -> Self {
        Self {
            driver: "generic".into(),
        }
    }
}
