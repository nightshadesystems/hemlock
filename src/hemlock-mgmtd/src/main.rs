//! hemlock-mgmtd — the management daemon.
//!
//! Owns the configuration lifecycle: candidate/running separation, commit,
//! commit-confirm with auto-rollback, and the on-disk rollback ring. In
//! phase 1 the only intent family it applies is interface admin-state and
//! description, pushed to syncd; further families slot into
//! `intents.rs` + `service.rs` without changing the lifecycle machinery.

mod intents;
mod service;
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
    info!(state_dir = %args.state_dir.display(), %syncd, "mgmtd starting");

    let engine = Arc::new(Mutex::new(service::Engine::new(store, syncd)));

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
