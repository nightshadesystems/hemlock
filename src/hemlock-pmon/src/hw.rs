//! Hardware access backends for pmon.
//!
//! Everything pmon touches goes through [`HwBackend`], so the daemon logic
//! (poll loop, fan control, gRPC) is identical between real sysfs/i2c
//! hardware and the mock used in CI and development.

use hemlock_platform::schema::{FanDef, Psu, ThermalSensor, Transceiver};

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
}

pub trait HwBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn read_temp_c(&self, sensor: &ThermalSensor) -> Result<f64, HwError>;
    fn read_fan_rpm(&self, fan: &FanDef) -> Result<u32, HwError>;
    fn set_fan_pwm(&self, fan: &FanDef, percent: u32) -> Result<(), HwError>;
    /// (present, ok)
    fn psu_status(&self, psu: &Psu) -> Result<(bool, bool), HwError>;
    /// `None` = cage empty.
    fn read_transceiver(&self, xcvr: &Transceiver) -> Result<Option<TransceiverInfo>, HwError>;
}

// ---------------------------------------------------------------------------
// Real sysfs/hwmon backend. Plain file I/O, so it compiles everywhere; the
// paths only exist on Linux with the platform's i2c topology instantiated.
// ---------------------------------------------------------------------------

pub struct SysfsBackend;

impl SysfsBackend {
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
    /// index varies per boot, hence the scan.
    fn hwmon_attr(device: &str, attr: &str) -> Result<String, HwError> {
        let base = format!("/sys/bus/i2c/devices/{device}/hwmon");
        let entries = std::fs::read_dir(&base).map_err(|source| HwError::Read {
            path: base.clone(),
            source,
        })?;
        for entry in entries.flatten() {
            let candidate = entry.path().join(attr);
            if candidate.exists() {
                return Ok(candidate.to_string_lossy().into_owned());
            }
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
        let path = Self::hwmon_attr(&sensor.hwmon, &format!("{}_input", sensor.input))?;
        Ok(Self::read_u64(&path)? as f64 / 1000.0)
    }

    fn read_fan_rpm(&self, fan: &FanDef) -> Result<u32, HwError> {
        let path = Self::hwmon_attr(&fan.hwmon, &format!("{}_input", fan.tach))?;
        Ok(Self::read_u64(&path)? as u32)
    }

    fn set_fan_pwm(&self, fan: &FanDef, percent: u32) -> Result<(), HwError> {
        let path = Self::hwmon_attr(&fan.hwmon, &fan.pwm)?;
        let raw = (percent.min(100) * 255 / 100).to_string();
        std::fs::write(&path, &raw).map_err(|source| HwError::Write { path, source })
    }

    fn psu_status(&self, psu: &Psu) -> Result<(bool, bool), HwError> {
        // pmbus driver bound => device dir exists; a readable status/power
        // attribute means the PSU answers on the bus.
        let dir = format!("/sys/bus/i2c/devices/{}-{:04x}", psu.bus, psu.address);
        if !std::path::Path::new(&dir).exists() {
            return Ok((false, false));
        }
        let ok = Self::hwmon_attr(&format!("{}-{:04x}", psu.bus, psu.address), "in2_input")
            .and_then(|p| Self::read_u64(&p))
            .map(|mv| mv > 0)
            .unwrap_or(false);
        Ok((true, ok))
    }

    fn read_transceiver(&self, xcvr: &Transceiver) -> Result<Option<TransceiverInfo>, HwError> {
        let path = format!("/sys/bus/i2c/devices/{}-0050/eeprom", xcvr.bus);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            // Empty cage: EEPROM reads fail with EIO/ENXIO.
            Err(_) => return Ok(None),
        };
        Ok(Some(parse_sfp_eeprom(&bytes)))
    }
}

/// Minimal SFF-8472 A0h parse: identifier + vendor/part/serial strings.
fn parse_sfp_eeprom(bytes: &[u8]) -> TransceiverInfo {
    fn text(bytes: &[u8], range: std::ops::Range<usize>) -> String {
        bytes
            .get(range)
            .map(|b| String::from_utf8_lossy(b).trim().to_string())
            .unwrap_or_default()
    }
    let form_factor = match bytes.first() {
        Some(0x03) => "SFP",   // SFP/SFP+ per SFF-8024 identifier
        Some(0x0c | 0x0d | 0x11) => "QSFP",
        _ => "?",
    };
    TransceiverInfo {
        form_factor: form_factor.to_string(),
        vendor: text(bytes, 20..36),
        part_number: text(bytes, 40..56),
        serial: text(bytes, 68..84),
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
}

impl MockBackend {
    pub fn new(temp_c: f64) -> Self {
        Self {
            temp_c: std::sync::atomic::AtomicU32::new((temp_c * 1000.0) as u32),
            pwm: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn pwm_for(&self, fan: &str) -> Option<u32> {
        self.pwm.lock().ok()?.get(fan).copied()
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

    fn psu_status(&self, _psu: &Psu) -> Result<(bool, bool), HwError> {
        Ok((true, true))
    }

    fn read_transceiver(&self, xcvr: &Transceiver) -> Result<Option<TransceiverInfo>, HwError> {
        // Populate the first cage; leave the rest empty.
        if xcvr.port.ends_with("48") {
            Ok(Some(TransceiverInfo {
                form_factor: "SFP".into(),
                vendor: "HEMLOCK-MOCK".into(),
                part_number: "MOCK-10G-SR".into(),
                serial: "M0CK0001".into(),
            }))
        } else {
            Ok(None)
        }
    }
}
