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
    /// The out-of-band management port, when the board has one.
    #[serde(default)]
    pub management: Option<ManagementSection>,
}

/// Out-of-band management port: how the CLI names it and which OS netdev
/// backs it (management networking is OS-level, not ASIC-level).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementSection {
    /// CLI-facing name, e.g. `Management1`.
    pub interface: String,
    /// Linux netdev behind it, e.g. `eth0`.
    pub os_device: String,
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
    /// CPU architecture of the board, in Debian's spelling (`amd64`,
    /// `armhf`). Selects the rootfs architecture, the boot artifacts
    /// (GRUB vs. FIT) and the installer target triple. Lint cross-checks
    /// it against `onie_machine`'s prefix.
    #[serde(default = "default_cpu_arch")]
    pub cpu_arch: String,
    /// How the host CPU reaches the ASIC. `pcie` is an ordinary
    /// PCI-attached switch chip; `soc` is an on-die CMIC with no PCI
    /// device to find (the Helix4 iProc boards). syncd's "is the ASIC
    /// actually here" probe differs per kind, and getting it wrong means
    /// `--auto-mock` mocking a real switch.
    #[serde(default)]
    pub asic_attach: AsicAttach,
}

fn default_cpu_arch() -> String {
    "amd64".into()
}

/// Known CPU architectures and the ONIE machine-string prefix each one
/// implies.
pub const KNOWN_CPU_ARCHES: [(&str, &str); 2] = [("amd64", "x86_64-"), ("armhf", "arm-")];

/// How the host CPU reaches the switch ASIC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AsicAttach {
    /// PCI-attached: probe for the vendor id on the PCI bus.
    #[default]
    Pcie,
    /// On-die CMIC on the SoC bus: probe for the platform device.
    Soc,
}

impl AsicAttach {
    pub fn as_str(self) -> &'static str {
        match self {
            AsicAttach::Pcie => "pcie",
            AsicAttach::Soc => "soc",
        }
    }
}

/// Which datapath library backs this platform.
///
/// SAI is the default and the preference. `openbcm` exists for boards
/// where no SAI build is published for the CPU architecture — the Helix4
/// iProc boards — and drives the OpenBCM SDK through Hemlock's own C shim.
/// Both sit behind the same `SaiBackend` trait; nothing above
/// `hemlock-sai` can tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SaiBackendKind {
    #[default]
    Sai,
    Openbcm,
}

impl SaiBackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SaiBackendKind::Sai => "sai",
            SaiBackendKind::Openbcm => "openbcm",
        }
    }
}

/// The datapath library section. Named `[sai]` for continuity: SAI is the
/// default backend, and the fields below that only apply to one backend
/// are enforced by lint rather than by the type.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaiSection {
    /// Which backend drives the ASIC.
    #[serde(default)]
    pub backend: SaiBackendKind,
    /// Vendor SAI package name, e.g. `libsaibcm`. Required for the `sai`
    /// backend; meaningless for `openbcm`, which has no vendor package.
    #[serde(default)]
    pub package: Option<String>,
    /// Abstract per-platform pin (e.g. `3.7.x-helix4`), resolved to a
    /// concrete vendor blob by the build pipeline. Never global.
    /// Required for the `sai` backend.
    #[serde(default)]
    pub version_pin: Option<String>,
    /// Absolute path of the vendor library inside the image. Required for
    /// the `sai` backend.
    #[serde(default)]
    pub libsai_path: Option<PathBuf>,
    /// Absolute path of Hemlock's OpenBCM shim inside the image, dlopened
    /// by syncd. Required for the `openbcm` backend.
    #[serde(default)]
    pub shim_path: Option<PathBuf>,
    /// Shim ABI major version this platform's image provides. syncd
    /// refuses a shim reporting a different major. Required for the
    /// `openbcm` backend.
    #[serde(default)]
    pub abi_major: Option<u32>,
    /// ASIC init config, relative to the platform directory.
    pub config_bcm: PathBuf,
    /// Additional vendor data files (LED microcode etc.), relative paths.
    #[serde(default)]
    pub extra_files: Vec<PathBuf>,
    /// Which vendored SAI header set (directory under `vendor/sai-headers/`)
    /// matches this SAI build's API. The image build selects it via
    /// `HEMLOCK_SAI_HEADERS` when compiling syncd for this platform.
    #[serde(default)]
    pub api_headers: Option<String>,
    /// Extra SAI profile key/values handed to the vendor library
    /// (`SAI_INIT_CONFIG_FILE` is always injected automatically).
    #[serde(default)]
    pub profile: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelSection {
    /// Kernel modules the pinned SAI expects (e.g. the Broadcom BDE pair).
    /// Plain module names only — parameters go in `module_args` (the image
    /// build's loadability gate feeds these strings to modprobe verbatim).
    #[serde(default)]
    pub required_modules: Vec<String>,
    /// Whitespace-separated `key=value` modprobe parameters per module,
    /// mirroring the vendor init script (e.g. linux-kernel-bde dmasize).
    #[serde(default)]
    pub module_args: std::collections::BTreeMap<String, String>,
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
    /// Unicast egress queues per front-panel port (ASIC-dependent;
    /// drives `show interfaces counters queue`).
    #[serde(default = "eight")]
    pub uc_queues: u32,
    /// Multicast egress queues per front-panel port.
    #[serde(default)]
    pub mc_queues: u32,
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
    /// Physical media, as shown in `show interfaces status` Type column
    /// (e.g. `1000BASE-T`, `SFP+`, `QSFP28`).
    #[serde(default)]
    pub media: Option<String>,
    /// Supported breakout modes, e.g. `["4x10G"]`. Empty = no breakout.
    #[serde(default)]
    pub breakout: Vec<String>,
    /// PHY model behind these ports, as shown in `show interfaces phy`
    /// (e.g. `HLK-PHY-BCM54282`). Absent for direct-attach serdes ports.
    #[serde(default)]
    pub phy_model: Option<String>,
    /// Supported speed/duplex modes for `show interfaces capabilities`
    /// and negotiation advertisements, e.g. `["10M/half", "1G/full",
    /// "auto"]`.
    #[serde(default)]
    pub supported_modes: Vec<String>,
    /// Vendor SDK port names, one per port, in the order this group
    /// expands — so for single-lane ports it runs parallel to `lanes`.
    /// On the OpenBCM backend the shim reports each port's SDK name
    /// beside its logical number and syncd asserts them against this
    /// list at startup, so a wrong port map fails loudly on first boot
    /// instead of quietly mis-cabling a rack. Empty = no assertion.
    #[serde(default)]
    pub sdk_names: Vec<String>,
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
    pub media: Option<String>,
    #[serde(default)]
    pub breakout: Vec<String>,
    #[serde(default)]
    pub phy_model: Option<String>,
    #[serde(default)]
    pub supported_modes: Vec<String>,
    /// Vendor SDK port name; see [`PortGroup::sdk_names`].
    #[serde(default)]
    pub sdk_name: Option<String>,
}

fn one() -> u32 {
    1
}

fn eight() -> u32 {
    8
}

/// Standard hwmon `pwmN` full scale.
fn default_pwm_max() -> u32 {
    255
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
    /// Shorthand for a single `[[root]]` named `root`.
    #[serde(default)]
    pub root_adapter: Option<String>,
    /// Named root adapters, for boards with more than one — SoC i2c
    /// controllers usually share a driver name, so they are told apart
    /// by `instance` rather than by prefix.
    #[serde(default, rename = "root")]
    pub roots: Vec<I2cRoot>,
    /// Raw i2c writes performed before any mux is instantiated (some boards
    /// need a wake-up/select write to a mux before the kernel driver binds).
    #[serde(default, rename = "pre_write")]
    pub pre_writes: Vec<I2cWrite>,
    #[serde(default, rename = "mux")]
    pub muxes: Vec<I2cMux>,
    #[serde(default, rename = "device")]
    pub devices: Vec<I2cDevice>,
}

/// The name Hemlock gives the adapter declared by the `root_adapter`
/// shorthand.
pub const DEFAULT_ROOT_NAME: &str = "root";

/// One root i2c adapter: an adapter the kernel provides, which the
/// manifest's topology hangs off.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct I2cRoot {
    /// Manifest-local name, referenced as `bus = "<name>"` /
    /// `parent_bus = "<name>"`.
    pub name: String,
    /// Adapter-name prefix, matched against `/sys/.../i2c-N/name`.
    pub adapter: String,
    /// Which match to take when several adapters share the prefix, in
    /// bus-number order. SoC controllers routinely do.
    #[serde(default)]
    pub instance: u32,
}

/// Reference to an i2c bus: a named root adapter, or a bus number in the
/// manifest's declared numbering (see `child_bus_base`). In TOML:
/// `bus = "root"`, `bus = "cpld"`, or `bus = 8`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum BusRef {
    Number(u32),
    Named(String),
}

impl std::fmt::Display for BusRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BusRef::Number(n) => write!(f, "{n}"),
            BusRef::Named(name) => write!(f, "{name:?}"),
        }
    }
}

/// One raw write, executed with i2cset-style block semantics.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct I2cWrite {
    pub bus: BusRef,
    pub address: u32,
    /// Bytes written: first byte is the register, the rest block data.
    pub data: Vec<u32>,
    /// Why this write exists — shown in logs.
    pub purpose: String,
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
    pub bus: BusRef,
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
    /// Tach input within the device, e.g. `fan4`; pmon reads
    /// `<tach>_input`. Ignored when `rpm_attr` is set.
    pub tach: String,
    /// PWM output within the device, e.g. `pwm4`. Ignored when `pwm_attr`
    /// is set.
    pub pwm: String,
    /// Verbatim tach attribute name, for drivers that do not follow the
    /// hwmon `<tach>_input` convention (ONL's AS4610 fan driver exposes
    /// `fan1_speed_rpm`). Overrides `tach` when present.
    #[serde(default)]
    pub rpm_attr: Option<String>,
    /// Verbatim PWM attribute name, for the same reason
    /// (`fan_duty_cycle_percentage`). Overrides `pwm` when present.
    #[serde(default)]
    pub pwm_attr: Option<String>,
    /// Full-scale value of the PWM attribute. Standard hwmon `pwmN` is
    /// 0-255; a driver taking a percentage wants 100.
    #[serde(default = "default_pwm_max")]
    pub pwm_max: u32,
    /// Absolute sysfs attribute holding tray presence (integer), e.g. a
    /// CPLD's `fan1_prs`. Absent = no presence detect (always present).
    #[serde(default)]
    pub presence_attr: Option<String>,
    /// Presence attribute polarity: true when 0 means present.
    #[serde(default)]
    pub presence_active_low: bool,
    /// Absolute sysfs attribute controlling the tray LED; accepts the
    /// driver's color words (e.g. `green` / `amber` / `off`). pmon drives
    /// it from fan health when set.
    #[serde(default)]
    pub led_attr: Option<String>,
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
    pub bus: BusRef,
    pub address: u32,
    pub driver: String,
    #[serde(default)]
    pub eeprom_address: Option<u32>,
    /// Absolute sysfs attribute holding presence as an integer (1 =
    /// present), e.g. the E1031 CPLD's `psuL_prs`. Absent = fall back to
    /// probing the pmbus device.
    #[serde(default)]
    pub presence_attr: Option<String>,
    /// Absolute sysfs attribute holding power-good as an integer (1 = ok).
    #[serde(default)]
    pub status_attr: Option<String>,
}

/// Maps a front-panel port to the i2c bus carrying its module EEPROM.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transceiver {
    pub port: String,
    pub bus: BusRef,
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
