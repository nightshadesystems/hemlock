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
    } else {
        Arc::new(hw::SysfsBackend)
    };
    info!(
        platform = %platform.manifest.platform.id,
        backend = backend.name(),
        "pmon starting"
    );

    let quirks = platform.quirks()?;
    quirks.post_hw_init(&platform)?;

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
            match backend.read_fan_rpm(fan) {
                Ok(rpm) => fans.push(FanReading {
                    name: fan.name.clone(),
                    rpm,
                    pwm_percent: commanded_pwm.unwrap_or(0),
                    ok: rpm > 0,
                }),
                Err(err) => {
                    warn!(fan = %fan.name, %err, "tach read failed");
                    fans.push(FanReading {
                        name: fan.name.clone(),
                        rpm: 0,
                        pwm_percent: commanded_pwm.unwrap_or(0),
                        ok: false,
                    });
                }
            }
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
            });
        }
        if let Ok(mut state) = env.write() {
            state.transceivers = readings;
        }
    }
}
