//! hemlock-orch — routing/switching orchestration daemon.
//!
//! Phase 1 seam stub. The eventual shape: subscribe to kernel netlink and
//! FRR (zebra) state, translate to SAI objects (routes, neighbors, VLANs,
//! LAGs), and drive them through syncd's gRPC API. The service surface and
//! this binary exist now so systemd units, IPC endpoints, and hemlockctl
//! integrations don't move when the real logic lands.

use anyhow::Result;
use clap::Parser;
use hemlock_common::ipc::{Daemon, IpcEndpoint};
use hemlock_common::proto::v1 as pb;
use hemlock_common::proto::v1::orch_server::{Orch, OrchServer};
use tonic::{Request, Response, Status};
use tracing::info;

#[derive(Parser)]
#[command(name = "hemlock-orch", version, about)]
struct Args {
    /// gRPC endpoint to serve (unix:/path or tcp:host:port).
    #[arg(long)]
    listen: Option<String>,
}

struct OrchService;

#[tonic::async_trait]
impl Orch for OrchService {
    async fn get_status(
        &self,
        _request: Request<pb::GetOrchStatusRequest>,
    ) -> Result<Response<pb::OrchStatus>, Status> {
        Ok(Response::new(pb::OrchStatus {
            running: true,
            detail: "phase-1 stub: no orchestration families active".into(),
        }))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    hemlock_common::logging::init("info");
    let args = Args::parse();

    let listen: IpcEndpoint = match &args.listen {
        Some(s) => s.parse()?,
        None => Daemon::Orch.default_endpoint(),
    };
    info!(%listen, "hemlock-orch (stub) serving gRPC");

    let router = tonic::transport::Server::builder().add_service(OrchServer::new(OrchService));
    listen
        .serve(router, async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutting down");
        })
        .await?;
    Ok(())
}
