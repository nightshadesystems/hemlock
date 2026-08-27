//! Manifest-driven system bring-up: kernel modules, BDE device nodes, and
//! i2c topology instantiation.
//!
//! This is the Rust replacement for a SONiC platform's init scripts. All
//! actions are idempotent — daemons may restart at any time and must be
//! able to re-run bring-up over an already-initialized system.
//!
//! Sysfs access goes through [`Sysfs`] with an injectable root so the i2c
//! logic is unit-testable against a fake tree.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use crate::schema::{AsicAttach, BusRef, I2cSection, KernelSection, Manifest, DEFAULT_ROOT_NAME};

#[derive(Debug, thiserror::Error)]
pub enum SysinitError {
    #[error("modprobe {module} failed: {detail}")]
    Modprobe { module: String, detail: String },

    #[error("no i2c adapter matching {0:?} found (platform modules loaded?)")]
    RootAdapterNotFound(String),

    #[error("i2c {action} on bus {bus}: {detail}")]
    I2c {
        action: &'static str,
        bus: u32,
        detail: String,
    },

    #[error("spawning {command}: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
}

/// Whether the switch ASIC is actually in this box.
///
/// The cheapest such probe, used by `--auto-mock` to pick the mock
/// backend under QEMU or on a bench machine without touching kernel
/// modules. How to look depends on how the CPU reaches the ASIC, which
/// the manifest declares:
///
/// * [`AsicAttach::Pcie`] — a Broadcom (vendor `0x14e4`) PCI device. A
///   Broadcom NIC also matches, but the boxes Hemlock targets pair those
///   with a Broadcom ASIC anyway.
/// * [`AsicAttach::Soc`] — an on-die CMIC on the SoC bus, which has no
///   PCI device at all: look for the `iproc_cmicd` platform device
///   instead. Probing PCI on such a board reports "no ASIC" on live
///   hardware, and `--auto-mock` would then mock a real switch — which
///   is the exact failure the probe exists to prevent.
pub fn asic_present(attach: AsicAttach) -> bool {
    match attach {
        AsicAttach::Pcie => {
            let Ok(entries) = std::fs::read_dir("/sys/bus/pci/devices") else {
                return false;
            };
            entries.flatten().any(|entry| {
                std::fs::read_to_string(entry.path().join("vendor"))
                    .map(|v| v.trim() == "0x14e4")
                    .unwrap_or(false)
            })
        }
        // Matched by suffix so the board's base address (`48000000` on
        // the AS4610) stays out of Hemlock's code.
        AsicAttach::Soc => {
            let Ok(entries) = std::fs::read_dir("/sys/bus/platform/devices") else {
                return false;
            };
            entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(SOC_CMIC_DEVICE_SUFFIX)
            })
        }
    }
}

/// The platform-device name suffix an XGS iProc CMICd registers under.
const SOC_CMIC_DEVICE_SUFFIX: &str = ".iproc_cmicd";

/// Load every module in `[kernel] required_modules`, with any
/// `[kernel.module_args]` parameters. `modprobe` is idempotent (params
/// are ignored on an already-loaded module), so this is safe on every
/// daemon start.
pub fn load_kernel_modules(kernel: &KernelSection) -> Result<(), SysinitError> {
    for module in &kernel.required_modules {
        let mut command = std::process::Command::new("modprobe");
        command.arg(module);
        if let Some(args) = kernel.module_args.get(module) {
            command.args(args.split_whitespace());
        }
        let output = command.output().map_err(|source| SysinitError::Spawn {
            command: format!("modprobe {module}"),
            source,
        })?;
        if output.status.success() {
            debug!(%module, "kernel module loaded");
        } else {
            return Err(SysinitError::Modprobe {
                module: module.clone(),
                detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
    }
    Ok(())
}

/// The Broadcom BDE/KNET modules register char majors but do not create
/// device nodes themselves; create them the way SONiC's start scripts do
/// (majors from the vendor opennsl-modules init script). Without the KNET
/// pair, soc_knet_init fails and SAI create_switch aborts NOT_SUPPORTED.
pub fn ensure_bde_dev_nodes() -> Result<(), SysinitError> {
    for (node, major) in [
        ("/dev/linux-kernel-bde", 127u32),
        ("/dev/linux-user-bde", 126u32),
        ("/dev/linux-bcm-knet", 122u32),
        ("/dev/linux-knet-cb", 121u32),
    ] {
        if Path::new(node).exists() {
            continue;
        }
        let output = std::process::Command::new("mknod")
            .args([node, "c", &major.to_string(), "0"])
            .output()
            .map_err(|source| SysinitError::Spawn {
                command: format!("mknod {node}"),
                source,
            })?;
        if output.status.success() {
            info!(node, major, "created BDE device node");
        } else {
            warn!(
                node,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "mknod failed (continuing; udev may have raced us)"
            );
        }
    }
    Ok(())
}

/// ONIE TlvInfo EEPROM type code for the base MAC address TLV.
const ONIE_TLV_BASE_MAC: u8 = 0x24;

/// Extract the base MAC (TLV 0x24) from an ONIE TlvInfo EEPROM blob.
/// Layout: 8-byte magic `"TlvInfo\0"`, version byte, big-endian u16 total
/// TLV length, then `type, length, value` records.
pub fn parse_onie_base_mac(blob: &[u8]) -> Option<[u8; 6]> {
    if blob.len() < 11 || &blob[..8] != b"TlvInfo\0" {
        return None;
    }
    let total = u16::from_be_bytes([blob[9], blob[10]]) as usize;
    let mut rest = blob.get(11..11 + total)?;
    while let [ty, len, value @ ..] = rest {
        let value = value.get(..*len as usize)?;
        if *ty == ONIE_TLV_BASE_MAC && value.len() == 6 {
            let mut mac = [0u8; 6];
            mac.copy_from_slice(value);
            return Some(mac);
        }
        rest = &rest[2 + value.len()..];
    }
    None
}

/// Parse `aa:bb:cc:dd:ee:ff` (the `/sys/class/net/*/address` format).
fn parse_mac_text(text: &str) -> Option<[u8; 6]> {
    let mut parts = text.trim().split(':');
    let mut mac = [0u8; 6];
    for byte in &mut mac {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

/// Test-visible alias for [`parse_i2c_byte`], so the quirks module can
/// pin the parsing its CPLD read-back depends on.
#[cfg(test)]
pub fn parse_i2c_byte_for_test(text: &str) -> Option<u8> {
    parse_i2c_byte(text)
}

/// `i2cget` prints `0x1a`; accept a bare hex byte too.
fn parse_i2c_byte(text: &str) -> Option<u8> {
    let text = text.trim();
    let digits = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"));
    match digits {
        Some(hex) => u8::from_str_radix(hex, 16).ok(),
        None => u8::from_str_radix(text, 16).ok(),
    }
}

/// A MAC usable as the switch source address: unicast and non-zero. A
/// blank EEPROM reads as zeros; treat that as "not found" so resolution
/// can fall through to the next source.
fn usable_mac(mac: [u8; 6]) -> bool {
    mac != [0u8; 6] && mac[0] & 1 == 0
}

/// Translation from the bus numbers a manifest *declares* to the ones the
/// kernel actually assigned.
///
/// `child_bus_base` says which declared number means which (mux, channel):
/// declared `child_bus_base + N` is channel N of that mux. What number the
/// kernel gives that channel is its business — it depends on probe order
/// and on how many adapters registered first, and a device tree that
/// instantiates part of the topology can shift all of them. So bring-up
/// follows each mux's `channel-N` symlinks and records declared → actual
/// here; every consumer of a bus number resolves through it.
///
/// Unknown declared numbers resolve to themselves, so a platform whose
/// numbering already matches (the E1031, today) is unaffected.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BusMap {
    map: BTreeMap<u32, u32>,
    roots: BTreeMap<String, u32>,
}

impl BusMap {
    pub fn insert(&mut self, declared: u32, actual: u32) {
        self.map.insert(declared, actual);
    }

    /// Record a named root adapter's kernel bus number.
    pub fn insert_root(&mut self, name: impl Into<String>, bus: u32) {
        self.roots.insert(name.into(), bus);
    }

    /// The kernel bus number for a declared one.
    pub fn resolve(&self, declared: u32) -> u32 {
        self.map.get(&declared).copied().unwrap_or(declared)
    }

    /// Resolve a manifest bus reference: a named root adapter, or a
    /// declared number translated through the map. `None` means the
    /// manifest named a root that does not exist.
    pub fn resolve_ref(&self, bus: &BusRef) -> Option<u32> {
        match bus {
            BusRef::Number(n) => Some(self.resolve(*n)),
            BusRef::Named(name) => self.roots.get(name).copied(),
        }
    }

    /// Translate a `<bus>-<addr>` sysfs identity (a manifest `hwmon`
    /// field, e.g. `"23-004d"`), keeping the address half verbatim.
    /// Anything not in that shape is passed through untouched.
    pub fn resolve_hwmon(&self, hwmon: &str) -> String {
        // A platform device has no bus number to translate.
        if hwmon.starts_with(PLATFORM_HWMON_PREFIX) {
            return hwmon.to_string();
        }
        match hwmon.split_once('-') {
            Some((bus, addr)) => match bus.parse::<u32>() {
                Ok(declared) => format!("{}-{addr}", self.resolve(declared)),
                Err(_) => hwmon.to_string(),
            },
            None => hwmon.to_string(),
        }
    }

    /// Declared/actual pairs that differ — what a platform's manifest got
    /// wrong (or what a device tree moved).
    pub fn divergences(&self) -> Vec<(u32, u32)> {
        self.map
            .iter()
            .filter(|(declared, actual)| declared != actual)
            .map(|(declared, actual)| (*declared, *actual))
            .collect()
    }
}

/// Prefix marking a manifest `hwmon` identity as a *platform* device
/// rather than an i2c client: `platform:as4610_fan` addresses
/// `/sys/devices/platform/as4610_fan`.
pub const PLATFORM_HWMON_PREFIX: &str = "platform:";

/// The sysfs directory a manifest `hwmon` identity names.
///
/// The usual form is an i2c client identity (`23-004d` ->
/// `/sys/bus/i2c/devices/23-004d`). Some vendor drivers instead hang
/// their attributes off a platform device with an empty hwmon node --
/// ONL's AS4610 fan driver registers `as4610_fan` that way -- and those
/// are named with the `platform:` prefix. The bus number in the i2c form
/// is the manifest's *declared* one, so resolve through [`BusMap`] first.
pub fn hwmon_device_dir(hwmon: &str) -> String {
    match hwmon.strip_prefix(PLATFORM_HWMON_PREFIX) {
        Some(name) => format!("/sys/devices/platform/{name}"),
        None => format!("/sys/bus/i2c/devices/{hwmon}"),
    }
}

/// What one i2c instantiation pass did, for logs and diagnostics.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct I2cReport {
    pub root_bus: Option<u32>,
    pub created: Vec<String>,
    pub already_present: Vec<String>,
    /// Declared → actual bus numbers, from the muxes' `channel-N` links.
    pub buses: BusMap,
}

/// Sysfs accessor with an injectable root (`/` in production).
pub struct Sysfs {
    root: PathBuf,
}

impl Sysfs {
    pub fn real() -> Self {
        Self { root: "/".into() }
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn i2c_dev_dir(&self) -> PathBuf {
        self.root.join("sys/bus/i2c/devices")
    }

    /// Find the bus number of the adapter whose name starts with `prefix`
    /// (PCI SMBus adapter numbering varies between boots).
    pub fn find_root_adapter(&self, prefix: &str) -> Result<u32, SysinitError> {
        self.find_root_adapter_instance(prefix, 0)
    }

    /// The `instance`-th adapter whose name starts with `prefix`, counting
    /// in bus-number order. SoC i2c controllers share one driver name, so
    /// a prefix alone cannot tell `i2c0` from `i2c1`.
    pub fn find_root_adapter_instance(
        &self,
        prefix: &str,
        instance: u32,
    ) -> Result<u32, SysinitError> {
        let dir = self.i2c_dev_dir();
        let entries = std::fs::read_dir(&dir)
            .map_err(|_| SysinitError::RootAdapterNotFound(prefix.to_string()))?;
        let mut buses: Vec<u32> = entries
            .flatten()
            .filter_map(|entry| {
                let file_name = entry.file_name();
                let name = file_name.to_string_lossy();
                let bus: u32 = name.strip_prefix("i2c-")?.parse().ok()?;
                let adapter_name = std::fs::read_to_string(entry.path().join("name")).ok()?;
                adapter_name.trim().starts_with(prefix).then_some(bus)
            })
            .collect();
        buses.sort_unstable();
        buses
            .get(instance as usize)
            .copied()
            .ok_or_else(|| SysinitError::RootAdapterNotFound(prefix.to_string()))
    }

    /// Locate every declared root adapter. `root_adapter = "..."` is
    /// shorthand for one root named `root`; `[[hardware.i2c.root]]`
    /// entries name theirs and disambiguate same-named adapters by
    /// instance.
    fn find_roots(&self, i2c: &I2cSection) -> Result<BTreeMap<String, u32>, SysinitError> {
        let mut roots = BTreeMap::new();
        if let Some(prefix) = &i2c.root_adapter {
            let bus = self.find_root_adapter(prefix)?;
            info!(bus, adapter = %prefix, name = DEFAULT_ROOT_NAME, "root i2c adapter located");
            roots.insert(DEFAULT_ROOT_NAME.to_string(), bus);
        }
        for root in &i2c.roots {
            let bus = self.find_root_adapter_instance(&root.adapter, root.instance)?;
            info!(bus, adapter = %root.adapter, name = %root.name, instance = root.instance,
                  "root i2c adapter located");
            roots.insert(root.name.clone(), bus);
        }
        Ok(roots)
    }

    fn resolve(&self, bus: &BusRef, buses: &BusMap, what: &str) -> Result<u32, SysinitError> {
        buses.resolve_ref(bus).ok_or_else(|| SysinitError::I2c {
            action: "resolve",
            bus: 0,
            detail: format!("{what} references root {bus} but no such root adapter is declared"),
        })
    }

    /// Does `<bus>-<addr>` already exist (previous run, or udev)?
    fn device_present(&self, bus: u32, address: u32) -> bool {
        self.i2c_dev_dir()
            .join(format!("{bus}-{address:04x}"))
            .exists()
    }

    /// Resolve a symlink, falling back to reading the path as a text file
    /// so the fake-sysfs tests need no symlink privileges (Windows hosts
    /// refuse them without developer mode).
    fn link_target(path: &Path) -> Option<String> {
        let target = std::fs::read_link(path)
            .ok()
            .map(|t| t.to_string_lossy().into_owned())
            .or_else(|| std::fs::read_to_string(path).ok())?;
        Some(target.trim().to_string())
    }

    /// The kernel bus numbers behind a mux's channels, read from the
    /// `channel-N` symlinks `i2c-mux.c` creates on the mux's own device
    /// (`/sys/bus/i2c/devices/<parent>-<addr>/channel-0 -> ../../i2c-7`).
    /// Present for both device-tree and `new_device` instantiation.
    fn mux_child_buses(&self, parent_bus: u32, address: u32) -> Vec<(u32, u32)> {
        let dir = self
            .i2c_dev_dir()
            .join(format!("{parent_bus}-{address:04x}"));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut channels: Vec<(u32, u32)> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let channel: u32 = name.to_str()?.strip_prefix("channel-")?.parse().ok()?;
                let target = Self::link_target(&entry.path())?;
                let bus: u32 = target
                    .rsplit(['/', '\\'])
                    .next()?
                    .strip_prefix("i2c-")?
                    .parse()
                    .ok()?;
                Some((channel, bus))
            })
            .collect();
        channels.sort_unstable();
        channels
    }

    fn new_device(&self, bus: u32, driver: &str, address: u32) -> Result<(), SysinitError> {
        let path = self
            .i2c_dev_dir()
            .join(format!("i2c-{bus}"))
            .join("new_device");
        std::fs::write(&path, format!("{driver} 0x{address:02x}\n")).map_err(|e| {
            SysinitError::I2c {
                action: "new_device",
                bus,
                detail: format!("{driver}@0x{address:02x}: {e}"),
            }
        })
    }

    /// Instantiate the manifest's i2c topology: pre-writes, then muxes in
    /// declaration order, then devices. Idempotent.
    pub fn instantiate_i2c(&self, i2c: &I2cSection) -> Result<I2cReport, SysinitError> {
        let mut report = I2cReport::default();

        for (name, bus) in self.find_roots(i2c)? {
            report.buses.insert_root(name, bus);
        }
        report.root_bus = report
            .buses
            .resolve_ref(&BusRef::Named(crate::schema::DEFAULT_ROOT_NAME.to_string()));

        for write in &i2c.pre_writes {
            let bus = self.resolve(&write.bus, &report.buses, "pre_write")?;
            self.raw_write(bus, write.address, &write.data, &write.purpose)?;
        }

        // Muxes come in declaration order, and a mux may hang off another
        // mux's channel — so each one's parent is resolved through the map
        // built so far, and its own channels are recorded before the next.
        for mux in &i2c.muxes {
            let parent = self.resolve(&mux.parent_bus, &report.buses, &mux.name)?;
            let label = format!(
                "mux {} ({}@{parent}-0x{:02x})",
                mux.name, mux.driver, mux.address
            );
            if self.device_present(parent, mux.address) {
                report.already_present.push(label);
            } else {
                self.new_device(parent, &mux.driver, mux.address)?;
                // Give the kernel a moment to create the child buses
                // before we go looking for them.
                std::thread::sleep(std::time::Duration::from_millis(200));
                report.created.push(label);
            }
            let children = self.mux_child_buses(parent, mux.address);
            if children.is_empty() {
                // Without the links there is nothing to translate, so
                // every device on this mux falls back to the manifest's
                // declared number -- which is a guess, and on this board
                // a wrong one. Loud, because the symptom otherwise is
                // devices that just never appear.
                warn!(
                    mux = %mux.name,
                    parent,
                    address = format_args!("0x{:02x}", mux.address),
                    "mux exposes no channel-N links; bus numbers stay as declared"
                );
            }
            for (channel, actual) in children {
                if channel < mux.channels {
                    report.buses.insert(mux.child_bus_base + channel, actual);
                }
            }
        }

        for (declared, actual) in report.buses.divergences() {
            warn!(
                declared,
                actual, "i2c bus number differs from the manifest's child_bus_base numbering"
            );
        }

        for device in &i2c.devices {
            let bus = self.resolve(&device.bus, &report.buses, &device.purpose)?;
            let label = format!(
                "{} ({}@{bus}-0x{:02x})",
                device.purpose, device.driver, device.address
            );
            if self.device_present(bus, device.address) {
                report.already_present.push(label);
                continue;
            }
            self.new_device(bus, &device.driver, device.address)?;
            report.created.push(label);
        }

        info!(
            created = report.created.len(),
            already_present = report.already_present.len(),
            "i2c topology instantiated"
        );
        Ok(report)
    }

    /// Instantiate the manifest's PSU pmbus devices (they live in
    /// `[[hardware.psu]]`, not the generic i2c device list, so
    /// [`Self::instantiate_i2c`] never sees them). A missing or absent
    /// PSU makes the driver's probe fail; that is logged and skipped, not
    /// fatal — presence is reported separately. Idempotent.
    pub fn instantiate_psus(&self, psus: &[crate::schema::Psu], report: &mut I2cReport) {
        for psu in psus {
            let Some(bus) = report.buses.resolve_ref(&psu.bus) else {
                warn!(psu = %psu.name, bus = %psu.bus, "PSU references an undeclared root adapter");
                continue;
            };
            let label = format!("{} ({}@{bus}-0x{:02x})", psu.name, psu.driver, psu.address);
            if self.device_present(bus, psu.address) {
                report.already_present.push(label);
                continue;
            }
            match self.new_device(bus, &psu.driver, psu.address) {
                Ok(()) => report.created.push(label),
                Err(e) => warn!(psu = %psu.name, error = %e, "PSU device instantiation failed"),
            }
        }
    }

    /// The base MAC for the switch (`SAI_SWITCH_ATTR_SRC_MAC_ADDRESS`):
    /// the ONIE syseeprom's TLV 0x24 when readable, else the management
    /// netdev's address. Some vendor SAIs have no working fallback of
    /// their own — Broadcom's aborts create_switch on the E1031 when the
    /// attribute is absent — so syncd resolves one here and passes it
    /// explicitly.
    pub fn base_mac(&self, manifest: &Manifest) -> Option<[u8; 6]> {
        if let Some(mac) = self
            .syseeprom_base_mac(&manifest.hardware.i2c)
            .filter(|&mac| usable_mac(mac))
        {
            return Some(mac);
        }
        manifest
            .management
            .as_ref()
            .and_then(|mgmt| self.netdev_mac(&mgmt.os_device))
            .filter(|&mac| usable_mac(mac))
    }

    /// Base MAC from the manifest's `syseeprom` i2c device, if that device
    /// exists in sysfs (pmon may not have instantiated the topology yet).
    ///
    /// syncd calls this without a [`BusMap`] — it does not instantiate the
    /// topology — so the declared bus is only the first guess. When the
    /// EEPROM sits behind a mux whose kernel numbering differs, fall back
    /// to any device at the same address whose blob parses as ONIE
    /// TlvInfo; the magic makes a false positive implausible.
    fn syseeprom_base_mac(&self, i2c: &I2cSection) -> Option<[u8; 6]> {
        let dev = i2c.devices.iter().find(|d| d.purpose == "syseeprom")?;
        if let BusRef::Number(bus) = dev.bus {
            let path = self
                .i2c_dev_dir()
                .join(format!("{bus}-{:04x}", dev.address))
                .join("eeprom");
            if let Some(mac) = std::fs::read(path)
                .ok()
                .and_then(|blob| parse_onie_base_mac(&blob))
            {
                return Some(mac);
            }
        }
        let suffix = format!("-{:04x}", dev.address);
        std::fs::read_dir(self.i2c_dev_dir())
            .ok()?
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(&suffix))
            .find_map(|entry| {
                let blob = std::fs::read(entry.path().join("eeprom")).ok()?;
                parse_onie_base_mac(&blob)
            })
    }

    /// MAC of a Linux netdev, e.g. the management port.
    fn netdev_mac(&self, dev: &str) -> Option<[u8; 6]> {
        let path = self.root.join("sys/class/net").join(dev).join("address");
        parse_mac_text(&std::fs::read_to_string(path).ok()?)
    }

    /// Locate a named root adapter declared by a manifest's i2c section,
    /// without instantiating anything. syncd needs this for quirks that
    /// poke a CPLD before the datapath exists — pmon owns topology
    /// instantiation, and it has not run yet at that point.
    pub fn find_manifest_root(&self, i2c: &I2cSection, name: &str) -> Option<u32> {
        if name == DEFAULT_ROOT_NAME {
            if let Some(prefix) = &i2c.root_adapter {
                return self.find_root_adapter(prefix).ok();
            }
        }
        let root = i2c.roots.iter().find(|r| r.name == name)?;
        self.find_root_adapter_instance(&root.adapter, root.instance)
            .ok()
    }

    /// One SMBus byte-data register write (`i2cset -y -f bus addr reg
    /// value`). `-f` because these targets deliberately have no driver
    /// bound: the CPLD is ours to poke, not the kernel's.
    pub fn i2c_write_reg(
        &self,
        bus: u32,
        address: u32,
        register: u8,
        value: u8,
    ) -> Result<(), SysinitError> {
        // Fake-sysfs tests have no bus; record the intent instead, the
        // way raw_write does.
        if self.root != Path::new("/") {
            let path = self
                .i2c_dev_dir()
                .join(format!("i2c-{bus}"))
                .join("reg-writes");
            let mut log = std::fs::read_to_string(&path).unwrap_or_default();
            log.push_str(&format!("0x{address:02x} 0x{register:02x} 0x{value:02x}\n"));
            return std::fs::write(&path, log).map_err(|e| SysinitError::I2c {
                action: "i2c_write_reg",
                bus,
                detail: e.to_string(),
            });
        }
        let output = std::process::Command::new("i2cset")
            .args([
                "-y",
                "-f",
                &bus.to_string(),
                &format!("0x{address:02x}"),
                &format!("0x{register:02x}"),
                &format!("0x{value:02x}"),
            ])
            .output()
            .map_err(|source| SysinitError::Spawn {
                command: format!("i2cset bus {bus}"),
                source,
            })?;
        if !output.status.success() {
            return Err(SysinitError::I2c {
                action: "i2c_write_reg",
                bus,
                detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(())
    }

    /// One SMBus byte-data register read (`i2cget -y -f bus addr reg`).
    pub fn i2c_read_reg(&self, bus: u32, address: u32, register: u8) -> Result<u8, SysinitError> {
        if self.root != Path::new("/") {
            // Tests drive the value through a file per register.
            let path = self
                .i2c_dev_dir()
                .join(format!("i2c-{bus}"))
                .join(format!("reg-{address:02x}-{register:02x}"));
            let text = std::fs::read_to_string(&path).map_err(|e| SysinitError::I2c {
                action: "i2c_read_reg",
                bus,
                detail: e.to_string(),
            })?;
            return parse_i2c_byte(text.trim()).ok_or_else(|| SysinitError::I2c {
                action: "i2c_read_reg",
                bus,
                detail: format!("unparsable {text:?}"),
            });
        }
        let output = std::process::Command::new("i2cget")
            .args([
                "-y",
                "-f",
                &bus.to_string(),
                &format!("0x{address:02x}"),
                &format!("0x{register:02x}"),
            ])
            .output()
            .map_err(|source| SysinitError::Spawn {
                command: format!("i2cget bus {bus}"),
                source,
            })?;
        if !output.status.success() {
            return Err(SysinitError::I2c {
                action: "i2c_read_reg",
                bus,
                detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        parse_i2c_byte(&text).ok_or_else(|| SysinitError::I2c {
            action: "i2c_read_reg",
            bus,
            detail: format!("unparsable i2cget output {text:?}"),
        })
    }

    /// Raw pre-init write via i2cset (block mode, force — the target has no
    /// driver bound yet by design).
    fn raw_write(
        &self,
        bus: u32,
        address: u32,
        data: &[u32],
        purpose: &str,
    ) -> Result<(), SysinitError> {
        // In fake-sysfs tests there is no real bus; record intent instead.
        if self.root != Path::new("/") {
            let path = self
                .i2c_dev_dir()
                .join(format!("i2c-{bus}"))
                .join("raw-writes");
            let rendered: Vec<String> = data.iter().map(|b| format!("0x{b:02x}")).collect();
            std::fs::write(&path, format!("0x{address:02x} {}\n", rendered.join(" "))).map_err(
                |e| SysinitError::I2c {
                    action: "raw_write",
                    bus,
                    detail: e.to_string(),
                },
            )?;
            return Ok(());
        }
        let mut args = vec!["-y".to_string(), "-f".to_string(), bus.to_string()];
        args.push(format!("0x{address:02x}"));
        for byte in data {
            args.push(format!("0x{byte:02x}"));
        }
        args.push("i".to_string()); // block write
        let output = std::process::Command::new("i2cset")
            .args(&args)
            .output()
            .map_err(|source| SysinitError::Spawn {
                command: format!("i2cset bus {bus}"),
                source,
            })?;
        if !output.status.success() {
            return Err(SysinitError::I2c {
                action: "raw_write",
                bus,
                detail: format!(
                    "{purpose}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }
        debug!(
            bus,
            address = format_args!("0x{address:02x}"),
            purpose,
            "i2c pre-write done"
        );
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::schema::{I2cDevice, I2cMux, I2cWrite};

    /// Build a fake sysfs with i2c adapters and writable new_device files.
    fn fake_sysfs(adapters: &[(u32, &str)]) -> (tempfile::TempDir, Sysfs) {
        let dir = tempfile::tempdir().unwrap();
        for (bus, name) in adapters {
            let bus_dir = dir.path().join(format!("sys/bus/i2c/devices/i2c-{bus}"));
            std::fs::create_dir_all(&bus_dir).unwrap();
            std::fs::write(bus_dir.join("name"), format!("{name}\n")).unwrap();
            std::fs::write(bus_dir.join("new_device"), "").unwrap();
            std::fs::write(bus_dir.join("raw-writes"), "").unwrap();
        }
        let sysfs = Sysfs::at(dir.path());
        (dir, sysfs)
    }

    fn e1031_like_topology() -> I2cSection {
        I2cSection {
            root_adapter: Some("SMBus iSMT adapter".into()),
            roots: Vec::new(),
            pre_writes: vec![I2cWrite {
                bus: BusRef::Named("root".into()),
                address: 0x73,
                data: vec![0x10, 0x00, 0x01],
                purpose: "wake cpu-extender".into(),
            }],
            muxes: vec![I2cMux {
                name: "cpu-extender".into(),
                driver: "pca9548".into(),
                parent_bus: BusRef::Named("root".into()),
                address: 0x73,
                child_bus_base: 2,
                channels: 8,
            }],
            devices: vec![I2cDevice {
                driver: "24lc64t".into(),
                bus: BusRef::Number(2),
                address: 0x50,
                purpose: "syseeprom".into(),
            }],
        }
    }

    #[test]
    fn finds_root_adapter_by_name_prefix() {
        let (_dir, sysfs) = fake_sysfs(&[
            (0, "Synopsys DesignWare adapter"),
            (1, "SMBus iSMT adapter at dfff0000"),
        ]);
        assert_eq!(sysfs.find_root_adapter("SMBus iSMT adapter").unwrap(), 1);
        assert!(matches!(
            sysfs.find_root_adapter("Nonexistent"),
            Err(SysinitError::RootAdapterNotFound(_))
        ));
    }

    #[test]
    fn instantiates_topology_in_order() {
        let (dir, sysfs) = fake_sysfs(&[(1, "SMBus iSMT adapter at dfff0000"), (2, "i2c-mux ch0")]);
        let report = sysfs.instantiate_i2c(&e1031_like_topology()).unwrap();
        assert_eq!(report.root_bus, Some(1));
        assert_eq!(report.created.len(), 2);

        // The mux landed on the root adapter's new_device...
        let mux_write =
            std::fs::read_to_string(dir.path().join("sys/bus/i2c/devices/i2c-1/new_device"))
                .unwrap();
        assert_eq!(mux_write, "pca9548 0x73\n");
        // ...the device on bus 2...
        let dev_write =
            std::fs::read_to_string(dir.path().join("sys/bus/i2c/devices/i2c-2/new_device"))
                .unwrap();
        assert_eq!(dev_write, "24lc64t 0x50\n");
        // ...and the pre-write was recorded against the root bus.
        let raw = std::fs::read_to_string(dir.path().join("sys/bus/i2c/devices/i2c-1/raw-writes"))
            .unwrap();
        assert_eq!(raw, "0x73 0x10 0x00 0x01\n");
    }

    /// Record a mux's channel links the way `i2c-mux.c` does, so
    /// `mux_child_buses` can read them back. Plain files rather than
    /// symlinks: `link_target` accepts both, and Windows hosts refuse to
    /// create symlinks without developer mode.
    fn fake_mux_channels(dir: &Path, parent_bus: u32, address: u32, channels: &[(u32, u32)]) {
        let mux_dir = dir.join(format!("sys/bus/i2c/devices/{parent_bus}-{address:04x}"));
        std::fs::create_dir_all(&mux_dir).unwrap();
        for (channel, bus) in channels {
            std::fs::write(
                mux_dir.join(format!("channel-{channel}")),
                format!("../../../i2c-{bus}"),
            )
            .unwrap();
        }
    }

    #[test]
    fn bus_map_resolves_and_passes_unknowns_through() {
        let mut map = BusMap::default();
        map.insert(2, 7);
        assert_eq!(map.resolve(2), 7);
        assert_eq!(map.resolve(3), 3, "unknown declared bus is identity");
        assert_eq!(map.resolve_hwmon("2-004d"), "7-004d");
        assert_eq!(map.resolve_hwmon("3-001a"), "3-001a");
        // Not a <bus>-<addr> identity: untouched rather than mangled.
        assert_eq!(map.resolve_hwmon("hwmon0"), "hwmon0");
        assert_eq!(map.resolve_hwmon("e1031.smc-fan"), "e1031.smc-fan");
        // A platform device has no bus number to translate, and its dir
        // is not under /sys/bus/i2c.
        assert_eq!(
            map.resolve_hwmon("platform:as4610_fan"),
            "platform:as4610_fan"
        );
        assert_eq!(
            hwmon_device_dir("platform:as4610_fan"),
            "/sys/devices/platform/as4610_fan"
        );
        assert_eq!(hwmon_device_dir("7-004d"), "/sys/bus/i2c/devices/7-004d");
        assert_eq!(map.divergences(), vec![(2, 7)]);
    }

    /// The E1031 case: the kernel numbering already matches the manifest,
    /// so the map records no divergence and every path is unchanged.
    /// A mux that exposes no `channel-N` links leaves every bus number
    /// at its declared value, and the first device on that mux then aims
    /// at a bus the kernel never created. Observed for real: the AS4610's
    /// ONIE runs 3.2.69, which predates the symlinks `i2c-mux.c` creates
    /// today, so its `pca954x` bound and enumerated eight child buses
    /// (i2c-10..17) with nothing linking them back to the mux. Both
    /// kernels Hemlock ships do create them, so this is the degraded
    /// path: it warns, then fails naming the bus it could not find,
    /// rather than quietly reporting a switch with no sensors.
    #[test]
    fn a_mux_without_channel_links_cannot_place_its_devices() {
        let (dir, sysfs) = fake_sysfs(&[(1, "SMBus iSMT adapter"), (10, "ch0")]);
        // No fake_mux_channels() call: the mux dir has no channel-N.
        std::fs::create_dir_all(dir.path().join("sys/bus/i2c/devices/1-0073")).unwrap();
        let err = sysfs
            .instantiate_i2c(&e1031_like_topology())
            .expect_err("declared bus 2 does not exist; the kernel used 10");
        assert!(
            matches!(err, SysinitError::I2c { bus: 2, .. }),
            "error should name the declared bus it could not find: {err}"
        );
    }

    #[test]
    fn matching_kernel_numbering_is_a_no_op() {
        let (dir, sysfs) = fake_sysfs(&[(1, "SMBus iSMT adapter"), (2, "ch0")]);
        fake_mux_channels(dir.path(), 1, 0x73, &[(0, 2)]);
        let report = sysfs.instantiate_i2c(&e1031_like_topology()).unwrap();
        assert_eq!(report.buses.resolve(2), 2);
        assert!(
            report.buses.divergences().is_empty(),
            "declared numbering matches the kernel's: {:?}",
            report.buses.divergences()
        );
        // The syseeprom still landed on declared bus 2.
        let dev_write =
            std::fs::read_to_string(dir.path().join("sys/bus/i2c/devices/i2c-2/new_device"))
                .unwrap();
        assert_eq!(dev_write, "24lc64t 0x50\n");
    }

    /// The kernel put the mux's channels somewhere else: devices follow
    /// the actual buses, and the manifest's `hwmon` identities translate.
    #[test]
    fn devices_follow_actual_child_buses() {
        let (dir, sysfs) = fake_sysfs(&[(1, "SMBus iSMT adapter"), (11, "ch0")]);
        fake_mux_channels(dir.path(), 1, 0x73, &[(0, 11)]);
        let report = sysfs.instantiate_i2c(&e1031_like_topology()).unwrap();

        assert_eq!(report.buses.resolve(2), 11);
        assert_eq!(report.buses.divergences(), vec![(2, 11)]);
        assert_eq!(report.buses.resolve_hwmon("2-0050"), "11-0050");
        // The syseeprom was created on i2c-11, not the declared i2c-2.
        let dev_write =
            std::fs::read_to_string(dir.path().join("sys/bus/i2c/devices/i2c-11/new_device"))
                .unwrap();
        assert_eq!(dev_write, "24lc64t 0x50\n");
    }

    /// A mux hanging off another mux's channel resolves its parent
    /// through the map built so far, not through the declared number.
    #[test]
    fn nested_mux_parent_resolves_through_the_map() {
        let (dir, sysfs) = fake_sysfs(&[(1, "SMBus iSMT adapter"), (11, "ch0"), (20, "sub-ch0")]);
        fake_mux_channels(dir.path(), 1, 0x73, &[(0, 11)]);
        fake_mux_channels(dir.path(), 11, 0x72, &[(0, 20)]);

        let mut topology = e1031_like_topology();
        topology.muxes.push(I2cMux {
            name: "main-extender".into(),
            driver: "pca9548".into(),
            // Declared bus 2 = cpu-extender channel 0, actually i2c-11.
            parent_bus: BusRef::Number(2),
            address: 0x72,
            child_bus_base: 18,
            channels: 8,
        });
        // Only the fan controller: the fake `new_device` is a file, so a
        // second device on the same bus would overwrite the mux write
        // this test is checking. (The syseeprom's own remapping is
        // covered by `devices_follow_actual_child_buses`.)
        topology.devices = vec![I2cDevice {
            driver: "emc2305".into(),
            bus: BusRef::Number(18),
            address: 0x4d,
            purpose: "fan-controller".into(),
        }];

        let report = sysfs.instantiate_i2c(&topology).unwrap();

        // Reading the nested mux's channels at all proves its parent
        // resolved 2 -> 11: that is the only directory the links live in.
        assert_eq!(report.buses.resolve(18), 20);
        // Its label names the resolved parent bus, not the declared one.
        // (The harness has to create `11-0072` to hold the channel links,
        // so the mux reads as already present — the second-run case.)
        assert!(
            report
                .already_present
                .iter()
                .any(|label| label.contains("main-extender") && label.contains("11-0x72")),
            "{:?}",
            report.already_present
        );
        // The fan controller landed on i2c-20.
        let dev_write =
            std::fs::read_to_string(dir.path().join("sys/bus/i2c/devices/i2c-20/new_device"))
                .unwrap();
        assert_eq!(dev_write, "emc2305 0x4d\n");
    }

    /// Channels beyond the mux's declared width are ignored, so a wrong
    /// `channels` count cannot claim bus numbers the manifest never
    /// declared.
    #[test]
    fn channels_past_the_declared_width_are_ignored() {
        let (dir, sysfs) = fake_sysfs(&[(1, "SMBus iSMT adapter"), (2, "ch0")]);
        let mut topology = e1031_like_topology();
        topology.muxes[0].channels = 2;
        fake_mux_channels(dir.path(), 1, 0x73, &[(0, 2), (1, 3), (5, 90)]);
        let report = sysfs.instantiate_i2c(&topology).unwrap();
        assert_eq!(report.buses.resolve(3), 3, "channel 1 -> declared 3");
        assert_eq!(report.buses.resolve(7), 7, "channel 5 was out of range");
    }

    #[test]
    fn second_run_is_idempotent() {
        let (dir, sysfs) = fake_sysfs(&[(1, "SMBus iSMT adapter"), (2, "ch0")]);
        // Simulate the kernel having created the device dirs from run one.
        for dev in ["1-0073", "2-0050"] {
            std::fs::create_dir_all(dir.path().join(format!("sys/bus/i2c/devices/{dev}"))).unwrap();
        }
        let report = sysfs.instantiate_i2c(&e1031_like_topology()).unwrap();
        assert!(report.created.is_empty());
        assert_eq!(report.already_present.len(), 2);
    }

    /// A minimal ONIE TlvInfo blob holding the given TLV records.
    fn tlvinfo(records: &[(u8, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (ty, value) in records {
            body.push(*ty);
            body.push(value.len() as u8);
            body.extend_from_slice(value);
        }
        let mut blob = b"TlvInfo\0".to_vec();
        blob.push(1); // version
        blob.extend_from_slice(&(body.len() as u16).to_be_bytes());
        blob.extend_from_slice(&body);
        blob
    }

    #[test]
    fn parses_onie_base_mac_tlv() {
        let mac = [0x00, 0xe0, 0xec, 0x12, 0x34, 0x56];
        let blob = tlvinfo(&[(0x21, b"E1031"), (0x24, &mac), (0x2a, &[52])]);
        assert_eq!(parse_onie_base_mac(&blob), Some(mac));

        // No 0x24 record, bad magic, and truncated records all yield None.
        assert_eq!(parse_onie_base_mac(&tlvinfo(&[(0x21, b"E1031")])), None);
        assert_eq!(parse_onie_base_mac(b"NotTlv\0\0"), None);
        let mut truncated = tlvinfo(&[(0x24, &mac)]);
        truncated.truncate(14);
        assert_eq!(parse_onie_base_mac(&truncated), None);
    }

    #[test]
    fn base_mac_prefers_syseeprom_then_falls_back_to_netdev() {
        let manifest: Manifest = toml::from_str(
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
version_pin = "x"
libsai_path = "/usr/lib/libsai.so.1"
config_bcm = "config.bcm"
[management]
interface = "Management1"
os_device = "eth0"
[[hardware.i2c.device]]
driver = "24lc64t"
bus = 2
address = 0x50
purpose = "syseeprom"
[[ports.port]]
name = "Ethernet1"
index = 1
speed_mbps = 1000
lanes = [1]
"#,
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let sysfs = Sysfs::at(dir.path());

        // Nothing in sysfs yet: no MAC at all.
        assert_eq!(sysfs.base_mac(&manifest), None);

        // Management netdev appears: its address is the fallback.
        let netdir = dir.path().join("sys/class/net/eth0");
        std::fs::create_dir_all(&netdir).unwrap();
        std::fs::write(netdir.join("address"), "00:e0:ec:aa:bb:cc\n").unwrap();
        assert_eq!(
            sysfs.base_mac(&manifest),
            Some([0x00, 0xe0, 0xec, 0xaa, 0xbb, 0xcc])
        );

        // Syseeprom appears: its TLV 0x24 wins over the netdev.
        let mac = [0x00, 0xe0, 0xec, 0x12, 0x34, 0x56];
        let eedir = dir.path().join("sys/bus/i2c/devices/2-0050");
        std::fs::create_dir_all(&eedir).unwrap();
        std::fs::write(eedir.join("eeprom"), tlvinfo(&[(0x24, &mac)])).unwrap();
        assert_eq!(sysfs.base_mac(&manifest), Some(mac));

        // A blank (all-zero) EEPROM MAC falls through to the netdev.
        std::fs::write(eedir.join("eeprom"), tlvinfo(&[(0x24, &[0u8; 6])])).unwrap();
        assert_eq!(
            sysfs.base_mac(&manifest),
            Some([0x00, 0xe0, 0xec, 0xaa, 0xbb, 0xcc])
        );
    }

    #[test]
    fn rejects_multicast_and_malformed_netdev_macs() {
        assert!(usable_mac([0x00, 0xe0, 0xec, 1, 2, 3]));
        assert!(!usable_mac([0u8; 6]));
        assert!(!usable_mac([0x01, 0x00, 0x5e, 0, 0, 1]));
        assert_eq!(parse_mac_text("00:e0:ec:aa:bb"), None);
        assert_eq!(parse_mac_text("zz:e0:ec:aa:bb:cc"), None);
        assert_eq!(
            parse_mac_text(" 00:E0:EC:AA:BB:CC\n"),
            Some([0x00, 0xe0, 0xec, 0xaa, 0xbb, 0xcc])
        );
    }

    #[test]
    fn numbered_parent_bus_needs_no_root() {
        let (_dir, sysfs) = fake_sysfs(&[(5, "some adapter")]);
        let i2c = I2cSection {
            root_adapter: None,
            roots: Vec::new(),
            pre_writes: vec![],
            muxes: vec![],
            devices: vec![I2cDevice {
                driver: "24c02".into(),
                bus: BusRef::Number(5),
                address: 0x52,
                purpose: "psu-eeprom".into(),
            }],
        };
        let report = sysfs.instantiate_i2c(&i2c).unwrap();
        assert_eq!(report.created.len(), 1);
    }
}
