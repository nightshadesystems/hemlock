//! hemlock-pmon — platform monitoring daemon.
//!
//! Entirely manifest-driven: sensors, fans, the fan curve, PSUs, and
//! transceiver buses all come from `platform.toml`. Hardware access goes
//! through the [`hw::HwBackend`] trait; `--mock` swaps in the pure-Rust
//! backend so the daemon runs anywhere.
//!
//! Loops: one poll/control loop (read temps -> update state -> drive fans
//! per the curve), one slower transceiver scan. State is served over gRPC
//! (`hemlock.v1.Pmon`) for hemlockctl and future telemetry.

mod control;
mod hw;
mod service;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use hemlock_common::ipc::{Daemon, IpcEndpoint};
use hemlock_common::proto::v1::pmon_server::PmonServer;
use hemlock_platform::Platform;
use tracing::{info, warn};

use hw::HwBackend;

#[derive(Parser)]
#[command(name = "hemlock-pmon", version = hemlock_common::VERSION, about)]
struct Args {
    /// Platform id (under --platforms-dir) or a platform directory path.
    #[arg(long)]
    platform: String,

    /// Root directory holding platform definitions.
    #[arg(long, default_value = "platforms")]
    platforms_dir: String,

    /// Use the mock hardware backend.
    #[arg(long)]
    mock: bool,

    /// Use the mock backend when no Broadcom ASIC is visible on PCI
    /// (QEMU, bench machines). On a switch the real path is taken and
    /// bring-up failures stay fatal: mock environment data must never
    /// masquerade as a healthy switch. The systemd unit runs with this
    /// so one image boots everywhere.
    #[arg(long, conflicts_with = "mock")]
    auto_mock: bool,

    /// gRPC endpoint to serve (unix:/path or tcp:host:port).
    #[arg(long)]
    listen: Option<String>,
}

/// Live environment state, refreshed by the poll loops.
#[derive(Debug, Clone, Default)]
pub struct EnvState {
    pub temperatures: Vec<TempReading>,
    pub fans: Vec<FanReading>,
    pub psus: Vec<PsuReading>,
    pub transceivers: Vec<XcvrReading>,
}

#[derive(Debug, Clone)]
pub struct TempReading {
    pub name: String,
    pub celsius: f64,
    pub warn_c: f64,
    pub crit_c: f64,
}

#[derive(Debug, Clone)]
pub struct FanReading {
    pub name: String,
    pub rpm: u32,
    pub pwm_percent: u32,
    pub ok: bool,
    /// Tray present (always true without presence detect).
    pub present: bool,
}

#[derive(Debug, Clone)]
pub struct PsuReading {
    pub name: String,
    pub present: bool,
    pub ok: bool,
}

#[derive(Debug, Clone)]
pub struct XcvrReading {
    pub port: String,
    pub present: bool,
    pub info: hw::TransceiverInfo,
    /// When the EEPROM was last read; serves `Last Update` in the CLI.
    pub read_at: std::time::Instant,
}

pub type SharedEnv = Arc<RwLock<EnvState>>;

#[tokio::main]
async fn main() -> Result<()> {
    hemlock_common::logging::init("info");
    let args = Args::parse();

    let platform = Platform::find(&args.platforms_dir, &args.platform)
        .with_context(|| format!("loading platform {:?}", args.platform))?;

    let backend: Arc<dyn HwBackend> = if args.mock {
        Arc::new(hw::MockBackend::new(35.0))
    } else if args.auto_mock && !hemlock_platform::sysinit::broadcom_asic_present() {
        warn!("--auto-mock: no Broadcom PCI device visible; using the mock hardware backend");
        Arc::new(hw::MockBackend::new(35.0))
    } else {
        // Real switch: bring-up failures are fatal and crash-loop loudly
        // in the journal rather than degrade to fake sensor data.
        real_hw_init(&platform)?
    };
    info!(
        platform = %platform.manifest.platform.id,
        backend = backend.name(),
        "pmon starting"
    );

    // Board quirks touch real hardware (CPLD registers); never run them
    // against the mock backend on a dev machine.
    if backend.name() != "mock" {
        let quirks = platform.quirks()?;
        quirks.post_hw_init(&platform)?;
    }

    let env: SharedEnv = Arc::new(RwLock::new(EnvState::default()));
    let platform = Arc::new(platform);

    tokio::spawn(poll_loop(platform.clone(), backend.clone(), env.clone()));
    tokio::spawn(xcvr_loop(platform.clone(), backend.clone(), env.clone()));

    let listen: IpcEndpoint = match &args.listen {
        Some(s) => s.parse()?,
        None => Daemon::Pmon.default_endpoint(),
    };
    info!(%listen, "serving gRPC");

    let router = tonic::transport::Server::builder()
        .add_service(PmonServer::new(service::PmonService::new(env)));
    router_serve(listen, router).await
}

/// Real hardware: load platform kernel modules and instantiate the
/// manifest's i2c topology (idempotent across restarts).
fn real_hw_init(platform: &Platform) -> Result<Arc<dyn HwBackend>> {
    hemlock_platform::sysinit::load_kernel_modules(&platform.manifest.kernel)?;
    let sysfs = hemlock_platform::sysinit::Sysfs::real();
    let mut report = sysfs.instantiate_i2c(&platform.manifest.hardware.i2c)?;
    sysfs.instantiate_psus(&platform.manifest.hardware.psus, &mut report);
    info!(
        created = report.created.len(),
        already_present = report.already_present.len(),
        remapped_buses = report.buses.divergences().len(),
        "i2c topology ready"
    );
    Ok(Arc::new(hw::SysfsBackend::new(report.buses)))
}

async fn router_serve(listen: IpcEndpoint, router: tonic::transport::server::Router) -> Result<()> {
    listen
        .serve(router, async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutting down");
        })
        .await?;
    Ok(())
}

/// Read temps + fans + PSUs, then drive fans from the manifest curve.
async fn poll_loop(platform: Arc<Platform>, backend: Arc<dyn HwBackend>, env: SharedEnv) {
    let thermal = &platform.manifest.hardware.thermal;
    let interval = thermal
        .fan_control
        .as_ref()
        .map(|fc| fc.interval_secs)
        .unwrap_or(5)
        .max(1);
    let mut ticker = tokio::time::interval(Duration::from_secs(interval.into()));
    // Last LED color written per fan, to skip redundant writes.
    let mut fan_leds: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    loop {
        ticker.tick().await;

        let mut temperatures = Vec::with_capacity(thermal.sensors.len());
        for sensor in &thermal.sensors {
            match backend.read_temp_c(sensor) {
                Ok(celsius) => {
                    if celsius >= sensor.crit_c {
                        warn!(sensor = %sensor.name, celsius, "CRITICAL temperature");
                    }
                    temperatures.push(TempReading {
                        name: sensor.name.clone(),
                        celsius,
                        warn_c: sensor.warn_c,
                        crit_c: sensor.crit_c,
                    });
                }
                Err(err) => warn!(sensor = %sensor.name, %err, "temperature read failed"),
            }
        }

        // Fan control from the curve's named sensor.
        let mut commanded_pwm = None;
        if let Some(fc) = &thermal.fan_control {
            if let Some(reading) = temperatures.iter().find(|t| t.name == fc.sensor) {
                let pwm = control::target_pwm(fc, reading.celsius);
                commanded_pwm = Some(pwm);
                for fan in &thermal.fans {
                    if let Err(err) = backend.set_fan_pwm(fan, pwm) {
                        warn!(fan = %fan.name, %err, "pwm write failed");
                    }
                }
            }
        }

        let mut fans = Vec::with_capacity(thermal.fans.len());
        for fan in &thermal.fans {
            let present = match backend.fan_present(fan) {
                Ok(present) => present,
                Err(err) => {
                    warn!(fan = %fan.name, %err, "presence read failed");
                    true // fail open: report health from the tach
                }
            };
            let rpm = if present {
                match backend.read_fan_rpm(fan) {
                    Ok(rpm) => rpm,
                    Err(err) => {
                        warn!(fan = %fan.name, %err, "tach read failed");
                        0
                    }
                }
            } else {
                0
            };
            let reading = FanReading {
                name: fan.name.clone(),
                rpm,
                pwm_percent: commanded_pwm.unwrap_or(0),
                ok: present && rpm > 0,
                present,
            };
            // Tray LED follows health: spinning green, stalled amber,
            // empty slot off. Deduplicated so a steady state costs no
            // CPLD writes; cosmetic, so failures only warn.
            let color = match (present, reading.ok) {
                (false, _) => "off",
                (true, true) => "green",
                (true, false) => "amber",
            };
            if fan_leds.get(&fan.name).map(String::as_str) != Some(color) {
                match backend.set_fan_led(fan, color) {
                    Ok(()) => {
                        fan_leds.insert(fan.name.clone(), color.to_string());
                    }
                    Err(err) => warn!(fan = %fan.name, color, %err, "fan LED write failed"),
                }
            }
            fans.push(reading);
        }

        let mut psus = Vec::with_capacity(platform.manifest.hardware.psus.len());
        for psu in &platform.manifest.hardware.psus {
            let (present, ok) = backend.psu_status(psu).unwrap_or((false, false));
            psus.push(PsuReading {
                name: psu.name.clone(),
                present,
                ok,
            });
        }

        if let Ok(mut state) = env.write() {
            state.temperatures = temperatures;
            state.fans = fans;
            state.psus = psus;
        }
    }
}

/// Slow scan of transceiver EEPROMs.
async fn xcvr_loop(platform: Arc<Platform>, backend: Arc<dyn HwBackend>, env: SharedEnv) {
    let mut ticker = tokio::time::interval(Duration::from_secs(10));
    loop {
        ticker.tick().await;
        let mut readings = Vec::new();
        for xcvr in &platform.manifest.hardware.transceivers {
            let info = backend.read_transceiver(xcvr).unwrap_or(None);
            readings.push(XcvrReading {
                port: xcvr.port.clone(),
                present: info.is_some(),
                info: info.unwrap_or_default(),
                read_at: std::time::Instant::now(),
            });
        }
        if let Ok(mut state) = env.write() {
            state.transceivers = readings;
        }
    }
}
