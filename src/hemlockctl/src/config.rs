//! Config lifecycle verbs: candidate loading, commit, rollback.

use anyhow::{bail, Context, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;

async fn client(endpoint: IpcEndpoint) -> Result<pb::mgmt_client::MgmtClient<tonic::transport::Channel>> {
    let channel = endpoint.connect().await.context("connecting to mgmtd")?;
    Ok(pb::mgmt_client::MgmtClient::new(channel))
}

pub async fn load(endpoint: IpcEndpoint, file: &str) -> Result<()> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let mut client = client(endpoint).await?;
    let response = client
        .set_candidate(pb::ConfigText { text })
        .await?
        .into_inner();
    if response.valid {
        println!("candidate loaded from {file}");
        Ok(())
    } else {
        for err in &response.errors {
            eprintln!("{err}");
        }
        bail!("candidate rejected ({} error(s))", response.errors.len());
    }
}

pub async fn candidate(endpoint: IpcEndpoint) -> Result<()> {
    let mut client = client(endpoint).await?;
    let text = client
        .get_config(pb::GetConfigRequest {
            source: pb::ConfigSource::Candidate as i32,
        })
        .await?
        .into_inner()
        .text;
    print!("{text}");
    Ok(())
}

pub async fn commit(endpoint: IpcEndpoint, comment: &str, confirm: Option<u32>) -> Result<()> {
    let mut client = client(endpoint).await?;
    let response = client
        .commit(pb::CommitRequest {
            comment: comment.to_string(),
            confirm_timeout_secs: confirm.unwrap_or(0),
        })
        .await?
        .into_inner();
    println!("commit {} applied", response.commit_id);
    for change in &response.applied_changes {
        println!("  {change}");
    }
    if let Some(secs) = confirm {
        println!("commit-confirm armed: run `hemlockctl confirm` within {secs}s or the config rolls back");
    }
    Ok(())
}

pub async fn confirm(endpoint: IpcEndpoint) -> Result<()> {
    let mut client = client(endpoint).await?;
    let response = client
        .confirm_commit(pb::ConfirmCommitRequest {})
        .await?
        .into_inner();
    if response.was_pending {
        println!("commit confirmed");
    } else {
        println!("nothing to confirm");
    }
    Ok(())
}

pub async fn rollback(endpoint: IpcEndpoint, revisions_back: u32) -> Result<()> {
    let mut client = client(endpoint).await?;
    client
        .rollback(pb::RollbackRequest { revisions_back })
        .await?;
    let response = client
        .commit(pb::CommitRequest {
            comment: format!("rollback {revisions_back}"),
            confirm_timeout_secs: 0,
        })
        .await?
        .into_inner();
    println!("rolled back {revisions_back} revision(s) (commit {})", response.commit_id);
    for change in &response.applied_changes {
        println!("  {change}");
    }
    Ok(())
}

pub async fn rollbacks(endpoint: IpcEndpoint) -> Result<()> {
    let mut client = client(endpoint).await?;
    let entries = client
        .list_rollbacks(pb::ListRollbacksRequest {})
        .await?
        .into_inner()
        .entries;
    if entries.is_empty() {
        println!("no rollback points");
        return Ok(());
    }
    println!("{:<4} {:<25} {}", "N", "Committed", "Comment");
    for e in entries {
        println!("{:<4} {:<25} {}", e.revisions_back, e.committed_at, e.comment);
    }
    Ok(())
}

pub async fn discard(endpoint: IpcEndpoint) -> Result<()> {
    let mut client = client(endpoint).await?;
    client.discard(pb::DiscardRequest {}).await?;
    println!("candidate discarded");
    Ok(())
}
