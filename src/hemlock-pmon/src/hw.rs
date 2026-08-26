//! Hardware access backends for pmon.
//!
//! Everything pmon touches goes through [`HwBackend`], so the daemon logic
//! (poll loop, fan control, gRPC) is identical between real sysfs/i2c
//! hardware and the mock used in CI and development.

use hemlock_platform::schema::{FanDef, Psu, ThermalSensor, Transceiver};
use hemlock_platform::sysinit::BusMap;

#[derive(Debug, thiserror::Error)]
pub enum HwError {
    #[error("sysfs read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("sysfs write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot parse {path}: {value:?}")]
    Parse { path: String, value: String },
}

#[derive(Debug, Clone, Default)]
pub struct TransceiverInfo {
    pub form_factor: String,
    pub vendor: String,
    pub part_number: String,
    pub serial: String,
    /// SFF-8472 vendor date code (YYMMDD), e.g. `210412`.
    pub date_code: String,
    /// Compliance-derived media type, e.g. `10GBASE-SR`.
    pub media_type: String,
    /// Digital diagnostics; `None` when the module lacks DOM.
    pub dom: Option<DomInfo>,
    pub thresholds: Option<DomThresholdsInfo>,
    /// Raw pages for `show interfaces transceiver eeprom`.
    pub eeprom_a0: Vec<u8>,
    pub eeprom_a2: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DomInfo {
    pub temp_c: f64,
    pub voltage_v: f64,
    pub bias_ma: f64,
    pub tx_dbm: f64,
    pub rx_dbm: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DomThresholdInfo {
    pub high_alarm: f64,
    pub high_warn: f64,
    pub low_alarm: f64,
    pub low_warn: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DomThresholdsInfo {
    pub temperature: DomThresholdInfo,
    pub voltage: DomThresholdInfo,
    pub bias: DomThresholdInfo,
    pub tx_power: DomThresholdInfo,
    pub rx_power: DomThresholdInfo,
}

pub trait HwBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn read_temp_c(&self, sensor: &ThermalSensor) -> Result<f64, HwError>;
    fn read_fan_rpm(&self, fan: &FanDef) -> Result<u32, HwError>;
    fn set_fan_pwm(&self, fan: &FanDef, percent: u32) -> Result<(), HwError>;
    /// Tray presence from the fan's `presence_attr`; trays without
    /// presence detect read as present.
    fn fan_present(&self, fan: &FanDef) -> Result<bool, HwError>;
    /// Drive the tray LED (`green` / `amber` / `off`) via the fan's
    /// `led_attr`; a no-op when the fan has none.
    fn set_fan_led(&self, fan: &FanDef, color: &str) -> Result<(), HwError>;
    /// (present, ok)
    fn psu_status(&self, psu: &Psu) -> Result<(bool, bool), HwError>;
    /// `None` = cage empty.
    fn read_transceiver(&self, xcvr: &Transceiver) -> Result<Option<TransceiverInfo>, HwError>;
}

// ---------------------------------------------------------------------------
// Real sysfs/hwmon backend. Plain file I/O, so it compiles everywhere; the
// paths only exist on Linux with the platform's i2c topology instantiated.
// ---------------------------------------------------------------------------

/// Every bus number and `<bus>-<addr>` identity this backend touches comes
/// from the manifest's *declared* numbering; `buses` translates it to what
/// the kernel actually assigned (see [`BusMap`]).
pub struct SysfsBackend {
    buses: BusMap,
}

impl SysfsBackend {
    pub fn new(buses: BusMap) -> Self {
        Self { buses }
    }

    fn read_string(path: &str) -> Result<String, HwError> {
        std::fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .map_err(|source| HwError::Read {
                path: path.to_string(),
                source,
            })
    }

    fn read_u64(path: &str) -> Result<u64, HwError> {
        let value = Self::read_string(path)?;
        value.parse().map_err(|_| HwError::Parse {
            path: path.to_string(),
            value,
        })
    }

    /// Resolve `<bus>-<addr>` + channel to the hwmon attribute file, e.g.
    /// `/sys/bus/i2c/devices/11-001a/hwmon/hwmon3/temp3_input`. The hwmonN
    /// index varies per boot, hence the scan. Old-style hwmon drivers (the
    /// haliburton emc2305 port) create their attributes directly on the
    /// i2c device dir and register an empty hwmon node, so the device dir
    /// itself is the fallback.
    fn hwmon_attr(device: &str, attr: &str) -> Result<String, HwError> {
        let dev_dir = format!("/sys/bus/i2c/devices/{device}");
        let base = format!("{dev_dir}/hwmon");
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let candidate = entry.path().join(attr);
                if candidate.exists() {
                    return Ok(candidate.to_string_lossy().into_owned());
                }
            }
        }
        let direct = format!("{dev_dir}/{attr}");
        if std::path::Path::new(&direct).exists() {
            return Ok(direct);
        }
        Err(HwError::Read {
            path: format!("{base}/*/{attr}"),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        })
    }
}

impl HwBackend for SysfsBackend {
    fn name(&self) -> &'static str {
        "sysfs"
    }

    fn read_temp_c(&self, sensor: &ThermalSensor) -> Result<f64, HwError> {
        let hwmon = self.buses.resolve_hwmon(&sensor.hwmon);
        let path = Self::hwmon_attr(&hwmon, &format!("{}_input", sensor.input))?;
        Ok(Self::read_u64(&path)? as f64 / 1000.0)
    }

    fn read_fan_rpm(&self, fan: &FanDef) -> Result<u32, HwError> {
        let hwmon = self.buses.resolve_hwmon(&fan.hwmon);
        // Standard hwmon spells the tach `<tach>_input`; drivers that
        // don't (ONL's AS4610 fan driver: `fan1_speed_rpm`) name the
        // attribute outright.
        let attr = match &fan.rpm_attr {
            Some(attr) => attr.clone(),
            None => format!("{}_input", fan.tach),
        };
        let path = Self::hwmon_attr(&hwmon, &attr)?;
        Ok(Self::read_u64(&path)? as u32)
    }

    fn set_fan_pwm(&self, fan: &FanDef, percent: u32) -> Result<(), HwError> {
        let hwmon = self.buses.resolve_hwmon(&fan.hwmon);
        let attr = fan.pwm_attr.as_ref().unwrap_or(&fan.pwm);
        let path = Self::hwmon_attr(&hwmon, attr)?;
        // pwm_max is the attribute's full scale: 255 for hwmon `pwmN`,
        // 100 for a driver that takes a percentage directly.
        let raw = (percent.min(100) * fan.pwm_max / 100).to_string();
        std::fs::write(&path, &raw).map_err(|source| HwError::Write { path, source })
    }

    fn fan_present(&self, fan: &FanDef) -> Result<bool, HwError> {
        let Some(attr) = &fan.presence_attr else {
            return Ok(true);
        };
        let raw = Self::read_u64(attr)?;
        Ok((raw == 0) == fan.presence_active_low)
    }

    fn set_fan_led(&self, fan: &FanDef, color: &str) -> Result<(), HwError> {
        let Some(attr) = &fan.led_attr else {
            return Ok(());
        };
        std::fs::write(attr, color).map_err(|source| HwError::Write {
            path: attr.clone(),
            source,
        })
    }

    fn psu_status(&self, psu: &Psu) -> Result<(bool, bool), HwError> {
        // Prefer the platform's dedicated presence/power-good attributes
        // (CPLD-backed, valid even when the pmbus device never probed).
        if let Some(prs) = &psu.presence_attr {
            let present = Self::read_u64(prs)? != 0;
            if !present {
                return Ok((false, false));
            }
            if let Some(status) = &psu.status_attr {
                return Ok((true, Self::read_u64(status)? != 0));
            }
        }
        // Fallback: pmbus driver bound => device dir exists; a readable
        // status/power attribute means the PSU answers on the bus.
        // An unresolvable bus (a root the manifest never declared) is a
        // lint error; here it just means the PSU cannot be read.
        let Some(bus) = self.buses.resolve_ref(&psu.bus) else {
            return Ok((false, false));
        };
        let device = format!("{bus}-{:04x}", psu.address);
        let dir = format!("/sys/bus/i2c/devices/{device}");
        if !std::path::Path::new(&dir).exists() {
            return Ok((false, false));
        }
        let ok = Self::hwmon_attr(&device, "in2_input")
            .and_then(|p| Self::read_u64(&p))
            .map(|mv| mv > 0)
            .unwrap_or(false);
        Ok((true, ok))
    }

    fn read_transceiver(&self, xcvr: &Transceiver) -> Result<Option<TransceiverInfo>, HwError> {
        let Some(bus) = self.buses.resolve_ref(&xcvr.bus) else {
            return Ok(None);
        };
        let path = format!("/sys/bus/i2c/devices/{bus}-0050/eeprom");
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            // Empty cage: EEPROM reads fail with EIO/ENXIO.
            Err(_) => return Ok(None),
        };
        Ok(Some(parse_sfp_eeprom(&bytes)))
    }
}

/// SFF-8472 parse over the optoe2 EEPROM image: A0h at bytes 0..256,
/// A2h (diagnostics) at 256..512 when the module exposes it.
fn parse_sfp_eeprom(bytes: &[u8]) -> TransceiverInfo {
    fn text(bytes: &[u8], range: std::ops::Range<usize>) -> String {
        bytes
            .get(range)
            .map(|b| String::from_utf8_lossy(b).trim().to_string())
            .unwrap_or_default()
    }
    let byte = |i: usize| bytes.get(i).copied().unwrap_or(0);
    let form_factor = match bytes.first() {
        Some(0x03) => "SFP", // SFP/SFP+ per SFF-8024 identifier
        Some(0x0c | 0x0d | 0x11) => "QSFP",
        _ => "?",
    };
    let a2 = bytes.get(256..512).unwrap_or(&[]);
    // Byte 92: diagnostic monitoring type; bit 6 = DOM implemented.
    let has_dom = byte(92) & 0x40 != 0 && a2.len() >= 106;
    TransceiverInfo {
        form_factor: form_factor.to_string(),
        vendor: text(bytes, 20..36),
        part_number: text(bytes, 40..56),
        serial: text(bytes, 68..84),
        date_code: text(bytes, 84..90),
        media_type: media_type(bytes),
        dom: has_dom.then(|| dom_values(a2)),
        thresholds: has_dom.then(|| dom_thresholds(a2)),
        eeprom_a0: bytes.get(..256).unwrap_or(bytes).to_vec(),
        eeprom_a2: a2.to_vec(),
    }
}

/// Media type from the SFF-8472 transceiver compliance codes
/// (A0h bytes 3 and 6, plus the byte 8 cable bits).
fn media_type(a0: &[u8]) -> String {
    let byte = |i: usize| a0.get(i).copied().unwrap_or(0);
    let b3 = byte(3);
    if b3 & 0x80 != 0 {
        return "10GBASE-ER".into();
    }
    if b3 & 0x40 != 0 {
        return "10GBASE-LRM".into();
    }
    if b3 & 0x20 != 0 {
        return "10GBASE-LR".into();
    }
    if b3 & 0x10 != 0 {
        return "10GBASE-SR".into();
    }
    // Byte 8: passive (bit 2) / active (bit 3) cable = direct attach.
    if byte(8) & 0x0c != 0 {
        return "10GBASE-CR".into();
    }
    let b6 = byte(6);
    if b6 & 0x08 != 0 {
        return "1000BASE-T".into();
    }
    if b6 & 0x02 != 0 {
        return "1000BASE-LX".into();
    }
    if b6 & 0x01 != 0 {
        return "1000BASE-SX".into();
    }
    String::new()
}

fn u16_at(page: &[u8], i: usize) -> u16 {
    let hi = page.get(i).copied().unwrap_or(0);
    let lo = page.get(i + 1).copied().unwrap_or(0);
    u16::from_be_bytes([hi, lo])
}

/// SFF-8472 unit conversions: temp s16/256 C, voltage u16 * 100uV,
/// bias u16 * 2uA, optical power u16 * 0.1uW (reported in dBm; a zero
/// reading floors at -40 dBm rather than -inf).
fn temp_c(raw: u16) -> f64 {
    f64::from(raw as i16) / 256.0
}

fn power_dbm(raw: u16) -> f64 {
    let mw = f64::from(raw) * 0.0001;
    if mw <= 0.0 {
        -40.0
    } else {
        10.0 * mw.log10()
    }
}

/// Live diagnostics: A2h bytes 96..106.
fn dom_values(a2: &[u8]) -> DomInfo {
    DomInfo {
        temp_c: temp_c(u16_at(a2, 96)),
        voltage_v: f64::from(u16_at(a2, 98)) * 1e-4,
        bias_ma: f64::from(u16_at(a2, 100)) * 0.002,
        tx_dbm: power_dbm(u16_at(a2, 102)),
        rx_dbm: power_dbm(u16_at(a2, 104)),
    }
}

/// Alarm/warning thresholds: A2h bytes 0..40, each metric as
/// high-alarm, low-alarm, high-warning, low-warning.
fn dom_thresholds(a2: &[u8]) -> DomThresholdsInfo {
    let quad = |base: usize, convert: &dyn Fn(u16) -> f64| DomThresholdInfo {
        high_alarm: convert(u16_at(a2, base)),
        low_alarm: convert(u16_at(a2, base + 2)),
        high_warn: convert(u16_at(a2, base + 4)),
        low_warn: convert(u16_at(a2, base + 6)),
    };
    DomThresholdsInfo {
        temperature: quad(0, &temp_c),
        voltage: quad(8, &|raw| f64::from(raw) * 1e-4),
        bias: quad(16, &|raw| f64::from(raw) * 0.002),
        tx_power: quad(24, &power_dbm),
        rx_power: quad(32, &power_dbm),
    }
}

// ---------------------------------------------------------------------------
// Mock backend: deterministic values, remembers PWM writes so the fan
// control loop is observable in tests and demos.
// ---------------------------------------------------------------------------

pub struct MockBackend {
    /// Simulated inlet temperature, settable for tests.
    pub temp_c: std::sync::atomic::AtomicU32, // millidegrees
    pwm: std::sync::Mutex<std::collections::HashMap<String, u32>>,
    fan_leds: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl MockBackend {
    pub fn new(temp_c: f64) -> Self {
        Self {
            temp_c: std::sync::atomic::AtomicU32::new((temp_c * 1000.0) as u32),
            pwm: std::sync::Mutex::new(std::collections::HashMap::new()),
            fan_leds: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn pwm_for(&self, fan: &str) -> Option<u32> {
        self.pwm.lock().ok()?.get(fan).copied()
    }

    #[cfg(test)]
    pub fn fan_led_for(&self, fan: &str) -> Option<String> {
        self.fan_leds.lock().ok()?.get(fan).cloned()
    }
}

impl HwBackend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn read_temp_c(&self, _sensor: &ThermalSensor) -> Result<f64, HwError> {
        Ok(self.temp_c.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1000.0)
    }

    fn read_fan_rpm(&self, fan: &FanDef) -> Result<u32, HwError> {
        // RPM tracks commanded PWM: full speed ~ 12000 rpm.
        let pwm = self.pwm_for(&fan.name).unwrap_or(40);
        Ok(120 * pwm)
    }

    fn set_fan_pwm(&self, fan: &FanDef, percent: u32) -> Result<(), HwError> {
        if let Ok(mut map) = self.pwm.lock() {
            map.insert(fan.name.clone(), percent.min(100));
        }
        Ok(())
    }

    fn fan_present(&self, _fan: &FanDef) -> Result<bool, HwError> {
        Ok(true)
    }

    fn set_fan_led(&self, fan: &FanDef, color: &str) -> Result<(), HwError> {
        if let Ok(mut map) = self.fan_leds.lock() {
            map.insert(fan.name.clone(), color.to_string());
        }
        Ok(())
    }

    fn psu_status(&self, _psu: &Psu) -> Result<(bool, bool), HwError> {
        Ok((true, true))
    }

    fn read_transceiver(&self, xcvr: &Transceiver) -> Result<Option<TransceiverInfo>, HwError> {
        // Populate the first two SFP+ cages (Ethernet49/50 on the E1031);
        // leave the rest empty. Built from a synthetic EEPROM image so
        // the mock exercises the same SFF-8472 decode as real hardware.
        if xcvr.port.ends_with("49") || xcvr.port.ends_with("50") {
            Ok(Some(parse_sfp_eeprom(&mock_sfp_eeprom())))
        } else {
            Ok(None)
        }
    }
}

/// A plausible 10GBASE-SR SFP+ EEPROM image (A0 + A2) for the mock.
pub fn mock_sfp_eeprom() -> Vec<u8> {
    let mut a0 = vec![0u8; 256];
    a0[0] = 0x03; // SFP
    a0[3] = 0x10; // 10GBASE-SR
    a0[20..36].copy_from_slice(b"HEMLOCK-MOCK    ");
    a0[40..56].copy_from_slice(b"MOCK-10G-SR     ");
    a0[68..84].copy_from_slice(b"M0CK0001        ");
    a0[84..90].copy_from_slice(b"210412");
    a0[92] = 0x40; // DOM implemented
    let mut a2 = vec![0u8; 256];
    let put = |page: &mut [u8], i: usize, value: u16| {
        page[i..i + 2].copy_from_slice(&value.to_be_bytes());
    };
    // Thresholds: 75/-5 C alarms, 70/0 C warnings; 3.63/2.97 V; 11.8/4 mA;
    // representative optical power windows.
    put(&mut a2, 0, (75 * 256) as u16);
    put(&mut a2, 2, (-5i16 * 256) as u16);
    put(&mut a2, 4, (70 * 256) as u16);
    put(&mut a2, 6, 0);
    put(&mut a2, 8, 36300);
    put(&mut a2, 10, 29700);
    put(&mut a2, 12, 34600);
    put(&mut a2, 14, 31300);
    put(&mut a2, 16, 5900); // 11.8 mA
    put(&mut a2, 18, 2000); // 4.0 mA
    put(&mut a2, 20, 5400); // 10.8 mA
    put(&mut a2, 22, 2500); // 5.0 mA
    put(&mut a2, 24, 14791); // ~1.7 dBm
    put(&mut a2, 26, 1122); // ~-9.5 dBm
    put(&mut a2, 28, 7413); // ~-1.3 dBm
    put(&mut a2, 30, 1479); // ~-8.3 dBm
    put(&mut a2, 32, 15849); // ~2.0 dBm
    put(&mut a2, 34, 490); // ~-13.1 dBm
    put(&mut a2, 36, 7943); // ~-1.0 dBm
    put(&mut a2, 38, 617); // ~-12.1 dBm
                           // Live values: 33.45 C, 3.28 V, 6.42 mA, -1.87 dBm tx, -2.35 dBm rx.
    put(&mut a2, 96, (33.45f64 * 256.0) as u16);
    put(&mut a2, 98, 32800);
    put(&mut a2, 100, 3210);
    put(&mut a2, 102, 6501);
    put(&mut a2, 104, 5821);
    a0.extend(a2);
    a0
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn mock_backend_reports_presence_and_records_led_writes() {
        let mock = MockBackend::new(30.0);
        let fan = FanDef {
            name: "FAN-1".into(),
            hwmon: "23-004d".into(),
            tach: "fan4".into(),
            pwm: "pwm4".into(),
            rpm_attr: None,
            pwm_attr: None,
            pwm_max: 255,
            presence_attr: None,
            presence_active_low: false,
            led_attr: None,
        };
        assert!(mock.fan_present(&fan).unwrap());
        assert!(mock.fan_led_for("FAN-1").is_none());
        mock.set_fan_led(&fan, "amber").unwrap();
        assert_eq!(mock.fan_led_for("FAN-1").as_deref(), Some("amber"));
    }

    #[test]
    fn sff8472_decode_reads_identity_dom_and_thresholds() {
        let info = parse_sfp_eeprom(&mock_sfp_eeprom());
        assert_eq!(info.form_factor, "SFP");
        assert_eq!(info.vendor, "HEMLOCK-MOCK");
        assert_eq!(info.part_number, "MOCK-10G-SR");
        assert_eq!(info.serial, "M0CK0001");
        assert_eq!(info.date_code, "210412");
        assert_eq!(info.media_type, "10GBASE-SR");
        assert_eq!(info.eeprom_a0.len(), 256);
        assert_eq!(info.eeprom_a2.len(), 256);

        let dom = info.dom.unwrap();
        assert!((dom.temp_c - 33.45).abs() < 0.01, "temp {}", dom.temp_c);
        assert!((dom.voltage_v - 3.28).abs() < 0.001);
        assert!((dom.bias_ma - 6.42).abs() < 0.01);
        assert!((dom.tx_dbm - -1.87).abs() < 0.01, "tx {}", dom.tx_dbm);
        assert!((dom.rx_dbm - -2.35).abs() < 0.01, "rx {}", dom.rx_dbm);

        let t = info.thresholds.unwrap();
        assert!((t.temperature.high_alarm - 75.0).abs() < 0.01);
        assert!((t.temperature.low_alarm - -5.0).abs() < 0.01);
        assert!((t.temperature.high_warn - 70.0).abs() < 0.01);
        assert!((t.voltage.high_alarm - 3.63).abs() < 0.001);
        assert!((t.bias.high_alarm - 11.8).abs() < 0.01);
        assert!((t.bias.low_warn - 5.0).abs() < 0.01);
        assert!((t.tx_power.high_alarm - 1.7).abs() < 0.01);
        assert!((t.rx_power.low_alarm - -13.1).abs() < 0.01);
    }

    #[test]
    fn modules_without_dom_or_a2_degrade() {
        let mut a0 = vec![0u8; 256];
        a0[0] = 0x03;
        a0[6] = 0x08; // 1000BASE-T
        let info = parse_sfp_eeprom(&a0);
        assert_eq!(info.media_type, "1000BASE-T");
        assert!(info.dom.is_none());
        assert!(info.thresholds.is_none());
        assert!(info.eeprom_a2.is_empty());

        // A truncated (or absent) EEPROM never panics.
        let empty = parse_sfp_eeprom(&[]);
        assert_eq!(empty.form_factor, "?");
        assert!(empty.dom.is_none());
    }

    #[test]
    fn zero_optical_power_floors_at_minus_40_dbm() {
        assert_eq!(power_dbm(0), -40.0);
        assert!((power_dbm(10000) - 0.0).abs() < 0.001); // 1 mW = 0 dBm
    }
}
