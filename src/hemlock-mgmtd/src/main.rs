//! hemlock-mgmtd — the management daemon.
//!
//! Owns the configuration lifecycle: candidate/running separation, commit,
//! commit-confirm with auto-rollback, and the on-disk rollback ring.
//! Intent families: interface admin-state and description (pushed to
//! syncd) and the OS-side families — management addressing, static
//! routes, the SSH service, and the web console (applied via
//! `osapply`). Further families slot into `intents.rs` + `service.rs`
//! without changing the lifecycle machinery.

mod dnsmasqapply;
mod frrapply;
mod identityapply;
mod intents;
mod osapply;
mod service;
mod sessions;
mod snmpapply;
mod store;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use hemlock_common::ipc::{Daemon, IpcEndpoint};
use hemlock_common::proto::v1::mgmt_server::MgmtServer;
use tokio::sync::Mutex;
use tracing::info;

#[derive(Parser)]
#[command(name = "hemlock-mgmtd", version = hemlock_common::VERSION, about)]
struct Args {
    /// State directory (running/candidate/rollback ring).
    #[arg(long, default_value_os_t = default_state_dir())]
    state_dir: std::path::PathBuf,

    /// syncd endpoint to apply config through.
    #[arg(long)]
    syncd: Option<String>,

    /// orch endpoint the protocol families (LACP/STP/snooping) are
    /// pushed through.
    #[arg(long)]
    orch: Option<String>,

    /// Platform manifest dir (management netdev mapping for OS-side
    /// config apply). OS-side apply is skipped when absent (dev hosts).
    #[arg(long, default_value = "/hemlock/platform")]
    platform: String,

    /// gRPC endpoint to serve (unix:/path or tcp:host:port).
    #[arg(long)]
    listen: Option<String>,
}

fn default_state_dir() -> std::path::PathBuf {
    if cfg!(unix) {
        "/var/lib/hemlock/config".into()
    } else {
        ".hemlock-state".into()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    hemlock_common::logging::init("info");
    let args = Args::parse();

    let store = store::Store::open(&args.state_dir)
        .with_context(|| format!("opening state dir {}", args.state_dir.display()))?;
    let syncd: IpcEndpoint = match &args.syncd {
        Some(s) => s.parse()?,
        None => Daemon::Syncd.default_endpoint(),
    };
    let orch: IpcEndpoint = match &args.orch {
        Some(s) => s.parse()?,
        None => Daemon::Orch.default_endpoint(),
    };
    info!(state_dir = %args.state_dir.display(), %syncd, %orch, "mgmtd starting");

    let management = hemlock_platform::Platform::find("/", &args.platform)
        .ok()
        .and_then(|platform| platform.manifest.management)
        .map(|m| (m.interface, m.os_device));
    if management.is_none() {
        info!("no platform manifest; OS-side config (management address, routes, ssh) will not be applied");
    }
    let engine = Arc::new(Mutex::new(service::Engine::new(
        store,
        syncd,
        orch,
        osapply::OsApplier::new(management),
    )));

    // OS-side families don't depend on syncd; replay them once now.
    if let Err(err) = engine.lock().await.replay_os() {
        tracing::warn!(%err, "cannot replay OS-side running config");
    }

    // Replay the persisted running config onto syncd (which boots with
    // default port state). Retry until syncd is up — daemon start order is
    // not guaranteed.
    {
        let engine = engine.clone();
        tokio::spawn(async move {
            let mut delay = std::time::Duration::from_millis(500);
            loop {
                let result = { engine.lock().await.replay_running().await };
                match result {
                    Ok(0) => break,
                    Ok(n) => {
                        info!(interfaces = n, "running config replayed to syncd");
                        // SVI bridge netdevs exist only now (syncd made
                        // them during the replay); re-run the idempotent
                        // OS replay so their kernel addresses land.
                        if let Err(err) = engine.lock().await.replay_os() {
                            tracing::warn!(%err, "post-replay OS pass failed");
                        }
                        break;
                    }
                    Err(err) => {
                        tracing::warn!(%err, "cannot replay running config yet; retrying");
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(std::time::Duration::from_secs(10));
                    }
                }
            }
        });
    }

    let listen: IpcEndpoint = match &args.listen {
        Some(s) => s.parse()?,
        None => Daemon::Mgmtd.default_endpoint(),
    };
    info!(%listen, "serving gRPC");

    let router = tonic::transport::Server::builder()
        .add_service(MgmtServer::new(service::MgmtService::new(engine)));
    listen
        .serve(router, async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutting down");
        })
        .await?;
    Ok(())
}
