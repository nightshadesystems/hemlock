//! hemlock-syncd — the SAI sync daemon.
//!
//! The only process that owns the ASIC. Platform-agnostic by construction:
//! everything board-specific arrives via the platform manifest (libsai path,
//! config.bcm, port table); the daemon itself just drives a [`SaiBackend`].
//!
//! Startup: load manifest -> construct backend (vendor or mock) -> quirks
//! pre-init -> create switch -> correlate ASIC ports with the manifest port
//! table by lane set -> bring ports to their default admin state -> serve
//! gRPC for mgmtd/orch/hemlockctl.

mod actor;
mod ifstats;
mod netdev;
mod security;
mod service;
mod state;

use anyhow::{bail, Context, Result};
use clap::Parser;
use hemlock_common::ipc::{Daemon, IpcEndpoint};
use hemlock_common::proto::v1::syncd_server::SyncdServer;
use hemlock_platform::Platform;
use hemlock_sai::SaiBackend;
use tracing::info;

#[derive(Parser)]
#[command(name = "hemlock-syncd", version = hemlock_common::VERSION, about)]
struct Args {
    /// Platform id (under --platforms-dir) or a platform directory path.
    #[arg(long)]
    platform: String,

    /// Root directory holding platform definitions.
    #[arg(long, default_value = "platforms")]
    platforms_dir: String,

    /// Use the pure-Rust mock backend instead of the vendor library.
    #[arg(long)]
    mock: bool,

    /// Use the mock backend when no Broadcom ASIC is visible on PCI
    /// (QEMU, bench machines); the vendor backend otherwise. The systemd
    /// unit runs with this so one image boots everywhere.
    #[arg(long, conflicts_with = "mock")]
    auto_mock: bool,

    /// Bring-up shakeout: create the switch, print the port table, exit.
    /// No gRPC server; works with both backends.
    #[arg(long)]
    probe: bool,

    /// Bench bring-up: enable the vendor SAI's diagnostic shell on this
    /// process's stdin/stdout (`BCM.0>` prompt for `led`, `setreg`, ...).
    /// Run syncd manually in the foreground; the shell coexists with the
    /// gRPC service. Vendor backend only; ignored with --mock.
    #[arg(long)]
    diag_shell: bool,

    /// gRPC endpoint to serve (unix:/path or tcp:host:port).
    #[arg(long)]
    listen: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    hemlock_common::logging::init("info");
    let args = Args::parse();

    let platform = Platform::find(&args.platforms_dir, &args.platform)
        .with_context(|| format!("loading platform {:?}", args.platform))?;
    info!(
        platform = %platform.manifest.platform.id,
        ports = platform.ports.len(),
        "platform manifest loaded"
    );

    let backend = build_backend(&platform, args.mock, args.auto_mock, args.diag_shell)?;

    // Board quirks touch real hardware (CPLD registers); never run them
    // against the mock backend on a dev machine.
    let real_hardware = backend.name() != "mock";
    let quirks = platform.quirks()?;
    if real_hardware {
        quirks.pre_asic_init(&platform)?;
    }
    let handle = actor::SaiActor::spawn(backend, &platform).await?;
    if real_hardware {
        quirks.post_asic_init(&platform)?;
    }

    info!(
        switch_oid = format_args!("{:#x}", handle.switch.oid),
        ports = handle.initial_ports(),
        backend = %handle.backend_name,
        "switch created, ports up"
    );

    if args.probe {
        // Let initial oper-status notifications drain into the port table.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        print_probe_report(&handle);
        return Ok(());
    }

    let handle = std::sync::Arc::new(handle);

    // The CoPP class table: protocol traps into per-class policed trap
    // groups (or unpoliced ARP/IP2ME punt when the SAI lacks trap
    // groups). Best-effort like the punt path itself.
    if let Err(err) = security::program_copp(&handle).await {
        tracing::warn!(%err, "CoPP class table not programmed; CPU punt may be degraded");
    }
    // Port-security: watch FDB learns and learn-limit violations.
    tokio::spawn(security::port_security_watch(handle.clone()));

    // Interface statistics: counters, rates, link history for
    // `show interfaces ...`. Default load interval 300s (EOS parity).
    let engine = ifstats::Engine::new(300);
    let netdevs: service::SharedNetdevs = std::sync::Arc::default();
    let inventory = service::Inventory {
        platform_model: format!(
            "{} {}",
            platform.manifest.platform.vendor, platform.manifest.platform.model
        ),
        management: platform
            .manifest
            .management
            .as_ref()
            .map(|m| (m.interface.clone(), m.os_device.clone())),
        uc_queues: platform.manifest.ports.uc_queues,
        mc_queues: platform.manifest.ports.mc_queues,
        mac: hemlock_platform::sysinit::Sysfs::real()
            .base_mac(&platform.manifest)
            .map(|b| b.map(|x| format!("{x:02x}")).join(":")),
    };
    tokio::spawn(collect_stats(
        handle.clone(),
        engine.clone(),
        netdevs.clone(),
        inventory.management.clone(),
    ));
    {
        // Count link flaps at event granularity, not the 5s sweep.
        let engine = engine.clone();
        let mut events = handle.events.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        engine.note_link(&event.name, event.oper_up, std::time::Instant::now())
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
    }
    {
        // Storm-control levels are percent of link speed: re-derive the
        // programmed rate whenever a link comes up (autoneg may have
        // settled on a different speed than the last derivation used).
        let handle = handle.clone();
        let mut events = handle.events.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) if event.oper_up => {
                        reprogram_storm_rates(&handle, &event.name).await;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
    }

    let listen: IpcEndpoint = match &args.listen {
        Some(s) => s.parse()?,
        None => Daemon::Syncd.default_endpoint(),
    };
    info!(%listen, "serving gRPC");

    // Type=notify: the ASIC is up and the port table built — units
    // ordered After=hemlock-syncd.service may now start. (The socket
    // binds microseconds later inside serve; clients retry regardless.)
    match hemlock_common::systemd::notify_ready() {
        Ok(true) => info!("systemd notified: READY=1"),
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %e, "sd_notify failed (continuing)"),
    }

    let router = tonic::transport::Server::builder().add_service(SyncdServer::new(
        service::SyncdService::new(handle, engine, netdevs, inventory),
    ));
    listen
        .serve(router, async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutting down");
        })
        .await?;
    Ok(())
}

fn build_backend(
    platform: &Platform,
    mock: bool,
    auto_mock: bool,
    diag_shell: bool,
) -> Result<Box<dyn SaiBackend>> {
    // --auto-mock only ever mocks when the ASIC is demonstrably absent.
    // With the ASIC present, every failure (missing modules, missing
    // real-sai feature, SAI init) stays fatal — mock ports on a real
    // switch would look healthy while forwarding nothing.
    let mock = mock
        || (auto_mock && {
            let no_asic = !hemlock_platform::sysinit::broadcom_asic_present();
            if no_asic {
                tracing::warn!(
                    "--auto-mock: no Broadcom PCI device visible; using the mock SAI backend"
                );
            }
            no_asic
        });
    if mock {
        if diag_shell {
            tracing::warn!("--diag-shell has no effect with the mock backend");
        }
        return Ok(Box::new(hemlock_sai::mock::MockSai::new(
            platform.ports.clone(),
        )));
    }

    #[cfg(feature = "real-sai")]
    {
        // Real hardware prerequisites: kernel modules (BDE pair + platform
        // modules) and their device nodes. Idempotent across restarts.
        hemlock_platform::sysinit::load_kernel_modules(&platform.manifest.kernel)?;
        if platform.manifest.platform.asic_family == "broadcom-xgs" {
            hemlock_platform::sysinit::ensure_bde_dev_nodes()?;
        }

        // Switch source MAC: SAI takes it as SAI_SWITCH_ATTR_SRC_MAC_ADDRESS.
        // Broadcom's SAI has no working fallback on the E1031 (create_switch
        // fails "get local MAC address failed"), so resolve one ourselves.
        let src_mac = hemlock_platform::sysinit::Sysfs::real().base_mac(&platform.manifest);
        match src_mac {
            Some(mac) => info!(
                mac = %mac.map(|b| format!("{b:02x}")).join(":"),
                "switch source MAC resolved"
            ),
            None => tracing::warn!(
                "no base MAC found (syseeprom TLV 0x24 or management netdev); \
                 leaving the switch source MAC to the vendor SAI's default"
            ),
        }

        if diag_shell {
            info!("vendor diag shell enabled — BCM.0> will appear on this terminal");
        }
        let init = hemlock_sai::SwitchInit {
            libsai_path: platform.manifest.sai.libsai_path.clone(),
            config_bcm_path: platform.config_bcm_path(),
            profile: platform
                .manifest
                .sai
                .profile
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            src_mac,
            diag_shell,
        };
        if !init.config_bcm_path.exists() {
            bail!(
                "config.bcm not found at {} — vendor data files are not committed; \
                 run vendor/fetch-vendor.sh {} (or pass --mock)",
                init.config_bcm_path.display(),
                platform.manifest.platform.id
            );
        }
        Ok(Box::new(hemlock_sai::vendor::VendorSai::new(&init)?))
    }
    #[cfg(not(feature = "real-sai"))]
    {
        bail!(
            "this hemlock-syncd was built without the real-sai feature; \
             only --mock is available"
        );
    }
}

/// The 5s stats sweep: ASIC port counters through the actor, kernel
/// interfaces (management + display-named netdevs) from sysfs. Feeds the
/// engine, which tracks rates, link history, and clear baselines.
async fn collect_stats(
    handle: std::sync::Arc<actor::SaiHandle>,
    engine: ifstats::SharedEngine,
    netdevs: service::SharedNetdevs,
    management: Option<(String, String)>,
) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let now = std::time::Instant::now();

        let ports: Vec<(String, hemlock_sai::PortId)> = handle
            .ports
            .read()
            .map(|table| {
                table
                    .iter()
                    .map(|(name, p)| (name.clone(), p.sai_id))
                    .collect()
            })
            .unwrap_or_default();
        match handle.port_stats(ports).await {
            Ok(samples) => {
                for sample in samples {
                    let queues = sample
                        .queues
                        .iter()
                        .map(|q| ifstats::QueueSample {
                            label: format!("{}{}", if q.unicast { "UC" } else { "MC" }, q.index),
                            pkts: q.pkts,
                            bytes: q.bytes,
                            dropped_pkts: q.dropped_pkts,
                            dropped_bytes: q.dropped_bytes,
                        })
                        .collect();
                    engine.ingest(&sample.name, sample.counters.into(), queues, now);
                }
            }
            Err(e) => tracing::warn!(error = %e, "port stats sweep failed"),
        }
        // Oper-state sync (dedup'd in note_link): seeds initial states
        // and backstops any events lost before the subscription started.
        if let Ok(table) = handle.ports.read() {
            for (name, port) in table.iter() {
                engine.note_link(name, port.oper_up, now);
            }
        }

        let mgmt = management.as_ref().map(|(d, o)| (d.as_str(), o.as_str()));
        let samples = netdev::sample_all(mgmt);
        let mut map = std::collections::HashMap::new();
        for sample in samples {
            engine.ingest(&sample.name, sample.counters, Vec::new(), now);
            engine.note_link(&sample.name, sample.oper_up, now);
            map.insert(sample.name.clone(), sample);
        }
        if let Ok(mut shared) = netdevs.write() {
            *shared = map;
        }
    }
}

/// Re-derive one port's storm-control rates from its current speed
/// (percent levels are relative to link speed, so a renegotiated link
/// wants new absolute rates).
async fn reprogram_storm_rates(handle: &actor::SaiHandle, name: &str) {
    let programs: Vec<(hemlock_sai::PortId, hemlock_sai::StormClass, String, u64)> = {
        let Ok(table) = handle.ports.read() else {
            return;
        };
        let Some(port) = table.get(name) else { return };
        port.storm
            .iter()
            .map(|(class, state)| {
                (
                    port.sai_id,
                    *class,
                    state.level.clone(),
                    u64::from(port.def.speed_mbps),
                )
            })
            .collect()
    };
    for (sai_id, class, level, speed_mbps) in programs {
        let Some((whole, frac)) = level.split_once('.') else {
            continue;
        };
        let hundredths = whole.parse::<u64>().unwrap_or(0) * 100 + frac.parse::<u64>().unwrap_or(0);
        let kbps = speed_mbps * hundredths / 10;
        if let Err(e) = handle.set_port_storm(sai_id, class, Some(kbps)).await {
            tracing::warn!(port = name, error = %e, "storm-control rate re-derivation failed");
        } else if let Ok(mut table) = handle.ports.write() {
            if let Some(port) = table.get_mut(name) {
                if let Some(state) = port.storm.get_mut(&class) {
                    state.kbps = kbps;
                }
            }
        }
    }
}

/// `--probe`: dump the correlated port table for hardware shakeout.
fn print_probe_report(handle: &actor::SaiHandle) {
    println!("platform:   {}", handle.platform_id);
    println!("backend:    {}", handle.backend_name);
    println!("switch oid: {:#x}", handle.switch.oid);
    let Ok(table) = handle.ports.read() else {
        println!("(port table unavailable)");
        return;
    };
    let mut ports: Vec<_> = table.values().collect();
    ports.sort_by_key(|p| p.def.index);
    println!(
        "{:<12} {:>5} {:>8} {:<14} {:>5} {:>4}  SAI OID",
        "Port", "Index", "Speed", "Lanes", "Admin", "Oper"
    );
    for port in ports {
        let lanes = port
            .def
            .lanes
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{:<12} {:>5} {:>8} {:<14} {:>5} {:>4}  {}",
            port.def.name,
            port.def.index,
            format!("{}M", port.def.speed_mbps),
            lanes,
            if port.admin_up { "up" } else { "down" },
            if port.oper_up { "up" } else { "down" },
            port.sai_id,
        );
    }
}
