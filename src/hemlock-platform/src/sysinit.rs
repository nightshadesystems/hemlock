//! Manifest-driven system bring-up: kernel modules, BDE device nodes, and
//! i2c topology instantiation.
//!
//! This is the Rust replacement for a SONiC platform's init scripts. All
//! actions are idempotent — daemons may restart at any time and must be
//! able to re-run bring-up over an already-initialized system.
//!
//! Sysfs access goes through [`Sysfs`] with an injectable root so the i2c
//! logic is unit-testable against a fake tree.

use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use crate::schema::{BusRef, I2cSection, KernelSection, Manifest};

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

/// Whether a Broadcom (PCI vendor 0x14e4) device is visible on the PCI
/// bus. The cheapest "is the switch ASIC actually in this box" probe:
/// used by `--auto-mock` to pick the mock backend under QEMU or on bench
/// machines without touching kernel modules. A Broadcom NIC also matches,
/// but the boxes Hemlock targets pair those with a Broadcom ASIC anyway.
pub fn broadcom_asic_present() -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/bus/pci/devices") else {
        return false;
    };
    entries.flatten().any(|entry| {
        std::fs::read_to_string(entry.path().join("vendor"))
            .map(|v| v.trim() == "0x14e4")
            .unwrap_or(false)
    })
}

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

/// A MAC usable as the switch source address: unicast and non-zero. A
/// blank EEPROM reads as zeros; treat that as "not found" so resolution
/// can fall through to the next source.
fn usable_mac(mac: [u8; 6]) -> bool {
    mac != [0u8; 6] && mac[0] & 1 == 0
}

/// What one i2c instantiation pass did, for logs and diagnostics.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct I2cReport {
    pub root_bus: Option<u32>,
    pub created: Vec<String>,
    pub already_present: Vec<String>,
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
            .first()
            .copied()
            .ok_or_else(|| SysinitError::RootAdapterNotFound(prefix.to_string()))
    }

    fn resolve(&self, bus: &BusRef, root: Option<u32>, what: &str) -> Result<u32, SysinitError> {
        match bus {
            BusRef::Number(n) => Ok(*n),
            BusRef::Named(_) => root.ok_or_else(|| SysinitError::I2c {
                action: "resolve",
                bus: 0,
                detail: format!("{what} references \"root\" but no root_adapter is declared"),
            }),
        }
    }

    /// Does `<bus>-<addr>` already exist (previous run, or udev)?
    fn device_present(&self, bus: u32, address: u32) -> bool {
        self.i2c_dev_dir()
            .join(format!("{bus}-{address:04x}"))
            .exists()
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

        let root = match &i2c.root_adapter {
            Some(prefix) => {
                let bus = self.find_root_adapter(prefix)?;
                info!(bus, adapter = %prefix, "root i2c adapter located");
                Some(bus)
            }
            None => None,
        };
        report.root_bus = root;

        for write in &i2c.pre_writes {
            let bus = self.resolve(&write.bus, root, "pre_write")?;
            self.raw_write(bus, write.address, &write.data, &write.purpose)?;
        }

        for mux in &i2c.muxes {
            let parent = self.resolve(&mux.parent_bus, root, &mux.name)?;
            let label = format!(
                "mux {} ({}@{parent}-0x{:02x})",
                mux.name, mux.driver, mux.address
            );
            if self.device_present(parent, mux.address) {
                report.already_present.push(label);
                continue;
            }
            self.new_device(parent, &mux.driver, mux.address)?;
            // Give the kernel a moment to create the child buses before the
            // next mux (which may hang off one of them) is instantiated.
            std::thread::sleep(std::time::Duration::from_millis(200));
            report.created.push(label);
        }

        for device in &i2c.devices {
            let label = format!(
                "{} ({}@{}-0x{:02x})",
                device.purpose, device.driver, device.bus, device.address
            );
            if self.device_present(device.bus, device.address) {
                report.already_present.push(label);
                continue;
            }
            self.new_device(device.bus, &device.driver, device.address)?;
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
            let label = format!("{} ({}@{}-0x{:02x})", psu.name, psu.driver, psu.bus, psu.address);
            if self.device_present(psu.bus, psu.address) {
                report.already_present.push(label);
                continue;
            }
            match self.new_device(psu.bus, &psu.driver, psu.address) {
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
    fn syseeprom_base_mac(&self, i2c: &I2cSection) -> Option<[u8; 6]> {
        let dev = i2c.devices.iter().find(|d| d.purpose == "syseeprom")?;
        let path = self
            .i2c_dev_dir()
            .join(format!("{}-{:04x}", dev.bus, dev.address))
            .join("eeprom");
        parse_onie_base_mac(&std::fs::read(path).ok()?)
    }

    /// MAC of a Linux netdev, e.g. the management port.
    fn netdev_mac(&self, dev: &str) -> Option<[u8; 6]> {
        let path = self.root.join("sys/class/net").join(dev).join("address");
        parse_mac_text(&std::fs::read_to_string(path).ok()?)
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
                bus: 2,
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
            pre_writes: vec![],
            muxes: vec![],
            devices: vec![I2cDevice {
                driver: "24c02".into(),
                bus: 5,
                address: 0x52,
                purpose: "psu-eeprom".into(),
            }],
        };
        let report = sysfs.instantiate_i2c(&i2c).unwrap();
        assert_eq!(report.created.len(), 1);
    }
}
