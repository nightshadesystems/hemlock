//! The commit engine and its gRPC surface (`hemlock.v1.Mgmt`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::intents::{self, InterfaceIntent, PortChange};
use crate::store::{RollbackMeta, Store};

pub struct Engine {
    store: Store,
    syncd: IpcEndpoint,
    commit_seq: u64,
    /// Pending commit-confirm: the pre-commit running text to restore if no
    /// confirmation arrives, plus a cancel handle for the timer task.
    pending_confirm: Option<PendingConfirm>,
}

struct PendingConfirm {
    cancel: tokio::sync::oneshot::Sender<()>,
}

pub type SharedEngine = Arc<Mutex<Engine>>;

impl Engine {
    pub fn new(store: Store, syncd: IpcEndpoint) -> Self {
        Self {
            store,
            syncd,
            commit_seq: 0,
            pending_confirm: None,
        }
    }

    async fn syncd_client(
        &self,
    ) -> Result<pb::syncd_client::SyncdClient<tonic::transport::Channel>> {
        let channel = self.syncd.connect().await.context("connecting to syncd")?;
        Ok(pb::syncd_client::SyncdClient::new(channel))
    }

    fn parse_intents(text: &str) -> Result<BTreeMap<String, InterfaceIntent>> {
        let tree = hemlock_config::parse(text).map_err(|e| anyhow!("{e}"))?;
        intents::interfaces(&tree).map_err(|e| anyhow!("{e}"))
    }

    /// Validate candidate text (syntax + intents + port names).
    async fn validate(&self, text: &str) -> Result<()> {
        let wanted = Self::parse_intents(text)?;
        if wanted.is_empty() {
            return Ok(());
        }
        let mut client = self.syncd_client().await?;
        let ports = client
            .list_ports(pb::ListPortsRequest {})
            .await
            .context("listing ports from syncd")?
            .into_inner()
            .ports;
        let known: std::collections::HashSet<_> = ports.into_iter().map(|p| p.name).collect();
        for name in wanted.keys() {
            if !known.contains(name) {
                anyhow::bail!("unknown interface {name:?}");
            }
        }
        Ok(())
    }

    /// Apply the delta between running and `new_text` through syncd, then
    /// persist `new_text` as running. Returns the applied changes.
    async fn apply_and_persist(
        &mut self,
        new_text: &str,
        comment: &str,
    ) -> Result<Vec<PortChange>> {
        let running_intents = Self::parse_intents(&self.store.running()?).unwrap_or_default();
        let wanted_intents = Self::parse_intents(new_text)?;
        let changes = intents::diff(&running_intents, &wanted_intents);

        if !changes.is_empty() {
            let mut client = self.syncd_client().await?;
            for change in &changes {
                client
                    .set_port_attrs(pb::SetPortAttrsRequest {
                        name: change.name.clone(),
                        admin_state: change.admin_up.map(|up| {
                            if up {
                                pb::AdminState::Up as i32
                            } else {
                                pb::AdminState::Down as i32
                            }
                        }),
                        description: change.description.clone(),
                    })
                    .await
                    .with_context(|| format!("applying {}", change.describe()))?;
            }
        }

        self.store.commit(
            new_text,
            &RollbackMeta {
                committed_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                comment: comment.to_string(),
            },
        )?;
        self.commit_seq += 1;
        Ok(changes)
    }
}

impl Engine {
    /// Re-apply the full running config to syncd. Needed at startup:
    /// syncd boots ports to defaults, so the persisted running config must
    /// be replayed onto it (a restart of either daemon converges).
    pub async fn replay_running(&self) -> Result<usize> {
        let running = Self::parse_intents(&self.store.running()?)?;
        if running.is_empty() {
            return Ok(0);
        }
        let mut client = self.syncd_client().await?;
        let mut applied = 0;
        for (name, intent) in &running {
            let request = pb::SetPortAttrsRequest {
                name: name.clone(),
                admin_state: intent.admin_up.map(|up| {
                    if up {
                        pb::AdminState::Up as i32
                    } else {
                        pb::AdminState::Down as i32
                    }
                }),
                description: intent.description.clone(),
            };
            if request.admin_state.is_none() && request.description.is_none() {
                continue;
            }
            client
                .set_port_attrs(request)
                .await
                .with_context(|| format!("replaying config for {name}"))?;
            applied += 1;
        }
        Ok(applied)
    }
}

pub struct MgmtService {
    engine: SharedEngine,
}

impl MgmtService {
    pub fn new(engine: SharedEngine) -> Self {
        Self { engine }
    }
}

fn internal(err: anyhow::Error) -> Status {
    Status::internal(format!("{err:#}"))
}

#[tonic::async_trait]
impl pb::mgmt_server::Mgmt for MgmtService {
    async fn get_config(
        &self,
        request: Request<pb::GetConfigRequest>,
    ) -> Result<Response<pb::ConfigText>, Status> {
        let engine = self.engine.lock().await;
        let text = match request.into_inner().source {
            s if s == pb::ConfigSource::Candidate as i32 => engine.store.candidate(),
            _ => engine.store.running(),
        }
        .map_err(internal)?;
        Ok(Response::new(pb::ConfigText { text }))
    }

    async fn set_candidate(
        &self,
        request: Request<pb::ConfigText>,
    ) -> Result<Response<pb::SetCandidateResponse>, Status> {
        let text = request.into_inner().text;
        let engine = self.engine.lock().await;
        match engine.validate(&text).await {
            Ok(()) => {
                engine.store.set_candidate(&text).map_err(internal)?;
                Ok(Response::new(pb::SetCandidateResponse {
                    valid: true,
                    errors: vec![],
                }))
            }
            Err(err) => Ok(Response::new(pb::SetCandidateResponse {
                valid: false,
                errors: vec![format!("{err:#}")],
            })),
        }
    }

    async fn commit(
        &self,
        request: Request<pb::CommitRequest>,
    ) -> Result<Response<pb::CommitResponse>, Status> {
        let req = request.into_inner();
        let mut engine = self.engine.lock().await;

        // A new commit while a confirm is pending supersedes it.
        if let Some(pending) = engine.pending_confirm.take() {
            let _ = pending.cancel.send(());
            warn!("pending commit-confirm superseded by a new commit");
        }

        let candidate = engine.store.candidate().map_err(internal)?;
        engine
            .validate(&candidate)
            .await
            .map_err(|e| Status::failed_precondition(format!("candidate invalid: {e:#}")))?;

        let pre_commit_running = engine.store.running().map_err(internal)?;
        let changes = engine
            .apply_and_persist(&candidate, &req.comment)
            .await
            .map_err(internal)?;
        let commit_id = engine.commit_seq;
        info!(commit_id, changes = changes.len(), "committed");

        if req.confirm_timeout_secs > 0 {
            let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
            engine.pending_confirm = Some(PendingConfirm { cancel: cancel_tx });
            let shared = self.engine.clone();
            let timeout = Duration::from_secs(req.confirm_timeout_secs.into());
            tokio::spawn(async move {
                tokio::select! {
                    _ = cancel_rx => {} // confirmed or superseded
                    _ = tokio::time::sleep(timeout) => {
                        let mut engine = shared.lock().await;
                        if engine.pending_confirm.take().is_none() {
                            return; // raced with a confirm
                        }
                        warn!("commit-confirm window expired; rolling back");
                        match engine
                            .apply_and_persist(&pre_commit_running, "auto-rollback (commit-confirm expired)")
                            .await
                        {
                            Ok(_) => info!("auto-rollback complete"),
                            Err(err) => warn!(%err, "auto-rollback FAILED"),
                        }
                    }
                }
            });
        }

        Ok(Response::new(pb::CommitResponse {
            commit_id,
            applied_changes: changes.iter().map(PortChange::describe).collect(),
        }))
    }

    async fn confirm_commit(
        &self,
        _request: Request<pb::ConfirmCommitRequest>,
    ) -> Result<Response<pb::ConfirmCommitResponse>, Status> {
        let mut engine = self.engine.lock().await;
        let was_pending = match engine.pending_confirm.take() {
            Some(pending) => {
                let _ = pending.cancel.send(());
                true
            }
            None => false,
        };
        Ok(Response::new(pb::ConfirmCommitResponse { was_pending }))
    }

    async fn rollback(
        &self,
        request: Request<pb::RollbackRequest>,
    ) -> Result<Response<pb::RollbackResponse>, Status> {
        let n = request.into_inner().revisions_back;
        let engine = self.engine.lock().await;
        let text = engine
            .store
            .rollback(n)
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no rollback point {n}")))?;
        engine.store.set_candidate(&text).map_err(internal)?;
        Ok(Response::new(pb::RollbackResponse { loaded_text: text }))
    }

    async fn list_rollbacks(
        &self,
        _request: Request<pb::ListRollbacksRequest>,
    ) -> Result<Response<pb::ListRollbacksResponse>, Status> {
        let engine = self.engine.lock().await;
        Ok(Response::new(pb::ListRollbacksResponse {
            entries: engine
                .store
                .list_rollbacks()
                .into_iter()
                .map(|(n, meta)| pb::RollbackEntry {
                    revisions_back: n,
                    committed_at: meta.committed_at,
                    comment: meta.comment,
                })
                .collect(),
        }))
    }

    async fn discard(
        &self,
        _request: Request<pb::DiscardRequest>,
    ) -> Result<Response<pb::DiscardResponse>, Status> {
        let engine = self.engine.lock().await;
        engine.store.discard_candidate().map_err(internal)?;
        Ok(Response::new(pb::DiscardResponse {}))
    }
}
