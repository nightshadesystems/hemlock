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
#[command(name = "hemlock-syncd", version, about)]
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

    let backend = build_backend(&platform, args.mock)?;

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

fn build_backend(platform: &Platform, mock: bool) -> Result<Box<dyn SaiBackend>> {
    if mock {
        return Ok(Box::new(hemlock_sai::mock::MockSai::new(
            platform.ports.clone(),
        )));
    }

    #[cfg(feature = "real-sai")]
    {
        let init = hemlock_sai::SwitchInit {
            libsai_path: platform.manifest.sai.libsai_path.clone(),
            config_bcm_path: platform.config_bcm_path(),
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
