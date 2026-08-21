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

    let backend = build_backend(&platform, args.mock, args.auto_mock)?;

    let quirks = platform.quirks()?;
    quirks.pre_asic_init(&platform)?;
    let handle = actor::SaiActor::spawn(backend, &platform).await?;
    quirks.post_asic_init(&platform)?;

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

    let listen: IpcEndpoint = match &args.listen {
        Some(s) => s.parse()?,
        None => Daemon::Syncd.default_endpoint(),
    };
    info!(%listen, "serving gRPC");

    let router = tonic::transport::Server::builder()
        .add_service(SyncdServer::new(service::SyncdService::new(handle)));
    listen
        .serve(router, async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutting down");
        })
        .await?;
    Ok(())
}

fn build_backend(platform: &Platform, mock: bool, auto_mock: bool) -> Result<Box<dyn SaiBackend>> {
    let mock = mock
        || (auto_mock && {
            let no_asic = !hemlock_platform::sysinit::broadcom_asic_present();
            if no_asic {
                tracing::warn!(
                    "--auto-mock: no Broadcom PCI device visible; using the mock SAI backend"
                );
            }
            no_asic
        })
        // A build without the vendor library can only ever mock.
        || (auto_mock && cfg!(not(feature = "real-sai")));
    if mock {
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
