//! The tech-support bundle: everything someone debugging this switch
//! from somewhere else would otherwise have to ask for, in one file.
//!
//! mgmtd assembles it because mgmtd is the daemon that can reach all of
//! them: it holds the config and the commit ring, it runs as root (so
//! the journal is readable), and it already has clients for syncd and
//! orch. The front-ends only ask for one and are told where it landed.
//!
//! Two rules shape what goes in:
//!
//! * **No secrets.** The configuration is redacted through the same
//!   [`ConfigTree::redact_secrets`] every other reader uses, so a
//!   bundle can be attached to a ticket. A test asserts it.
//! * **Partial beats absent.** A daemon that is down is exactly what
//!   the bundle is being collected about, so an unreachable one is
//!   recorded in the manifest and the rest is still written.
//!
//! State dumps are the RPC responses serialized as JSON — the proto
//! types derive `Serialize` (see hemlock-common's build.rs), so there
//! is no hand-written mirror of each message to go stale.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;

/// Where finished bundles land. Flash-backed on a switch, so a bundle
/// survives the reboot someone is about to do.
pub const BUNDLE_DIR: &str = "/var/lib/hemlock";

/// What one collected item is called and how it went.
struct Collected {
    name: &'static str,
    outcome: Result<(), String>,
}

/// Assemble a bundle and return its path.
///
/// `dir` is the output directory (the constant above in production, a
/// temp dir in tests).
pub async fn build(
    dir: &Path,
    hostname: &str,
    running_config: &str,
    commits: &[crate::store::RollbackMeta],
    syncd: &IpcEndpoint,
    orch: &IpcEndpoint,
    pmon: &IpcEndpoint,
) -> Result<PathBuf> {
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let stem = format!("tech-support-{hostname}-{stamp}");
    let staging = dir.join(&stem);
    std::fs::create_dir_all(staging.join("config"))
        .with_context(|| format!("creating {}", staging.display()))?;
    std::fs::create_dir_all(staging.join("state"))?;
    std::fs::create_dir_all(staging.join("logs"))?;

    let mut collected: Vec<Collected> = Vec::new();
    let mut record = |name: &'static str, outcome: Result<(), String>| {
        collected.push(Collected { name, outcome });
    };

    // --- configuration -------------------------------------------------
    record(
        "config/running.conf",
        redacted_config(running_config)
            .and_then(|text| write(&staging.join("config/running.conf"), &text)),
    );
    record(
        "config/commits.json",
        serde_json::to_string_pretty(&commit_history(commits))
            .map_err(|e| e.to_string())
            .and_then(|text| write(&staging.join("config/commits.json"), &text)),
    );

    // --- daemon state --------------------------------------------------
    for (name, outcome) in syncd_state(syncd, &staging).await {
        record(name, outcome);
    }
    for (name, outcome) in orch_state(orch, &staging).await {
        record(name, outcome);
    }
    for (name, outcome) in pmon_state(pmon, &staging).await {
        record(name, outcome);
    }

    // --- logs ------------------------------------------------------------
    record(
        "logs/journal.txt",
        match crate::journal::tail(JOURNAL_LINES) {
            Some(entries) => write(&staging.join("logs/journal.txt"), &render_journal(&entries)),
            None => Err("the journal could not be read".to_string()),
        },
    );

    let manifest = render_manifest(hostname, &stamp, &collected);
    write(&staging.join("manifest.txt"), &manifest).map_err(|e| anyhow::anyhow!("{e}"))?;

    let archive = dir.join(format!("{stem}.tar.gz"));
    archive_dir(dir, &stem, &archive)?;
    let _ = std::fs::remove_dir_all(&staging);
    tracing::info!(path = %archive.display(), "tech-support bundle written");
    Ok(archive)
}

/// Journal lines a bundle carries. Enough to cover the boot and the
/// incident; past that the answer is the log collector the box forwards
/// to.
const JOURNAL_LINES: u32 = 5000;

/// The running config with every secret replaced. The redaction list
/// lives with the config language, so this bundle hides exactly what
/// `show configuration` hides.
fn redacted_config(text: &str) -> Result<String, String> {
    let mut tree =
        hemlock_config::parse(text).map_err(|e| format!("config does not parse: {e}"))?;
    tree.redact_secrets();
    Ok(tree.to_text())
}

/// The commit ring as JSON: index 0 is the running config.
fn commit_history(commits: &[crate::store::RollbackMeta]) -> serde_json::Value {
    serde_json::json!({
        "commits": commits
            .iter()
            .enumerate()
            .map(|(index, meta)| serde_json::json!({
                "index": index,
                "committed_at": meta.committed_at,
                "user": meta.user,
                "client": meta.client,
                "comment": meta.comment,
            }))
            .collect::<Vec<_>>(),
    })
}

/// Journal entries as plain text — the one dump a human reads directly,
/// so it is not JSON.
fn render_journal(entries: &[crate::journal::Entry]) -> String {
    let mut out = String::new();
    for entry in entries {
        let stamp = chrono::DateTime::from_timestamp(entry.time_unix, 0)
            .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "-".into());
        let pid = if entry.pid == 0 {
            String::new()
        } else {
            format!("[{}]", entry.pid)
        };
        out.push_str(&format!(
            "{stamp} {} {}{pid}: {}\n",
            entry.host, entry.tag, entry.message
        ));
    }
    out
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// One RPC response as a JSON file.
fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    write(path, &format!("{text}\n"))
}

async fn syncd_state(
    syncd: &IpcEndpoint,
    staging: &Path,
) -> Vec<(&'static str, Result<(), String>)> {
    let mut out = Vec::new();
    let channel = match syncd.connect().await {
        Ok(channel) => channel,
        Err(err) => {
            // One entry rather than one per dump: the daemon is down,
            // which is a single fact.
            out.push((
                "state/syncd-*.json",
                Err(format!("syncd unreachable: {err}")),
            ));
            return out;
        }
    };
    let mut client = pb::syncd_client::SyncdClient::new(channel);
    macro_rules! dump {
        ($name:expr, $call:expr) => {
            out.push((
                $name,
                match $call.await {
                    Ok(response) => write_json(&staging.join($name), &response.into_inner()),
                    Err(status) => Err(status.message().to_string()),
                },
            ));
        };
    }
    dump!(
        "state/syncd-switch.json",
        client.get_switch_info(pb::GetSwitchInfoRequest {})
    );
    dump!(
        "state/syncd-interfaces.json",
        client.get_interfaces(pb::GetInterfacesRequest { names: vec![] })
    );
    dump!(
        "state/syncd-fib.json",
        client.dump_fib(pb::DumpFibRequest {})
    );
    dump!(
        "state/syncd-fdb.json",
        client.dump_fdb(pb::DumpFdbRequest::default())
    );
    dump!(
        "state/syncd-acl.json",
        client.get_acl_state(pb::GetAclStateRequest {})
    );
    dump!(
        "state/syncd-copp.json",
        client.get_copp_state(pb::GetCoppStateRequest {})
    );
    dump!(
        "state/syncd-qos.json",
        client.get_qos_state(pb::GetQosStateRequest {})
    );
    out
}

async fn orch_state(orch: &IpcEndpoint, staging: &Path) -> Vec<(&'static str, Result<(), String>)> {
    let mut out = Vec::new();
    let channel = match orch.connect().await {
        Ok(channel) => channel,
        Err(err) => {
            out.push(("state/orch-*.json", Err(format!("orch unreachable: {err}"))));
            return out;
        }
    };
    let mut client = pb::orch_client::OrchClient::new(channel);
    macro_rules! dump {
        ($name:expr, $call:expr) => {
            out.push((
                $name,
                match $call.await {
                    Ok(response) => write_json(&staging.join($name), &response.into_inner()),
                    Err(status) => Err(status.message().to_string()),
                },
            ));
        };
    }
    dump!(
        "state/orch-status.json",
        client.get_status(pb::GetOrchStatusRequest {})
    );
    dump!(
        "state/orch-lacp.json",
        client.get_lacp_state(pb::GetLacpStateRequest {})
    );
    dump!(
        "state/orch-stp.json",
        client.get_stp_state(pb::GetStpStateRequest {})
    );
    dump!(
        "state/orch-lldp.json",
        client.get_lldp_state(pb::GetLldpStateRequest {
            port: String::new()
        })
    );
    out
}

async fn pmon_state(pmon: &IpcEndpoint, staging: &Path) -> Vec<(&'static str, Result<(), String>)> {
    let mut out = Vec::new();
    let channel = match pmon.connect().await {
        Ok(channel) => channel,
        Err(err) => {
            out.push(("state/pmon-*.json", Err(format!("pmon unreachable: {err}"))));
            return out;
        }
    };
    let mut client = pb::pmon_client::PmonClient::new(channel);
    out.push((
        "state/pmon-environment.json",
        match client.get_environment(pb::GetEnvironmentRequest {}).await {
            Ok(response) => write_json(
                &staging.join("state/pmon-environment.json"),
                &response.into_inner(),
            ),
            Err(status) => Err(status.message().to_string()),
        },
    ));
    out.push((
        "state/pmon-transceivers.json",
        match client
            .list_transceivers(pb::ListTransceiversRequest {})
            .await
        {
            Ok(response) => write_json(
                &staging.join("state/pmon-transceivers.json"),
                &response.into_inner(),
            ),
            Err(status) => Err(status.message().to_string()),
        },
    ));
    out
}

/// The manifest: what is in the bundle, and what could not be collected
/// and why. Someone opening the archive should not have to guess
/// whether a missing file means "not applicable" or "the daemon was
/// down".
fn render_manifest(hostname: &str, stamp: &str, collected: &[Collected]) -> String {
    let mut out = String::new();
    out.push_str("Hemlock tech-support bundle\n");
    out.push_str(&format!("host    : {hostname}\n"));
    out.push_str(&format!("taken   : {stamp} UTC\n"));
    out.push_str(&format!("version : {}\n", hemlock_common::VERSION));
    out.push_str("\nThe configuration in this bundle is redacted: passwords, RADIUS\n");
    out.push_str("keys and SNMP passphrases all read `<hidden>`.\n\n");
    out.push_str("contents:\n");
    for item in collected {
        match &item.outcome {
            Ok(()) => out.push_str(&format!("  ok      {}\n", item.name)),
            Err(reason) => out.push_str(&format!("  MISSING {}  ({reason})\n", item.name)),
        }
    }
    out
}

/// Archive the staging directory. `tar` comes from the base rootfs, and
/// shelling out to it is the same access path every other OS-facing
/// applier takes.
fn archive_dir(parent: &Path, stem: &str, archive: &Path) -> Result<()> {
    let output = std::process::Command::new("tar")
        .arg("-czf")
        .arg(archive)
        .arg("-C")
        .arg(parent)
        .arg(stem)
        .output()
        .with_context(|| format!("running tar for {}", archive.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "tar failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const SECRETS: &str = r#"
system {
    login {
        user cody {
            role admin;
            password-hash "$6$rounds=656000$abcdefgh$s3cr3thash";
        }
    }
}
security {
    dot1x {
        radius-server 10.42.0.5 {
            key "radiuss3cret";
        }
    }
}
services {
    snmp {
        user monitor auth sha "authpass1" priv aes "privpass1";
    }
}
"#;

    /// The one rule that makes a bundle attachable to a ticket: no
    /// secret survives it.
    #[test]
    fn the_bundled_config_is_redacted() {
        let text = redacted_config(SECRETS).unwrap();
        for secret in ["s3cr3thash", "radiuss3cret", "authpass1", "privpass1"] {
            assert!(!text.contains(secret), "{secret} leaked into the bundle");
        }
        // What is left still reads as configuration.
        assert!(text.contains("role admin"), "{text}");
        assert!(text.contains(r#"password-hash "<hidden>""#), "{text}");
        assert!(text.contains(r#"key "<hidden>""#), "{text}");
    }

    /// A configuration that will not parse is reported, not silently
    /// omitted.
    #[test]
    fn an_unparsable_config_is_reported() {
        assert!(redacted_config("system {").unwrap_err().contains("parse"));
    }

    /// The manifest names every item, and says why a missing one is
    /// missing.
    #[test]
    fn the_manifest_records_what_was_collected() {
        let collected = vec![
            Collected {
                name: "config/running.conf",
                outcome: Ok(()),
            },
            Collected {
                name: "state/orch-*.json",
                outcome: Err("orch unreachable: connection refused".into()),
            },
        ];
        let manifest = render_manifest("hemlock-a1", "20260825-104230", &collected);
        assert!(manifest.contains("host    : hemlock-a1"), "{manifest}");
        assert!(
            manifest.contains("  ok      config/running.conf"),
            "{manifest}"
        );
        assert!(
            manifest
                .contains("  MISSING state/orch-*.json  (orch unreachable: connection refused)"),
            "{manifest}"
        );
        // The redaction promise is stated where someone will read it.
        assert!(manifest.contains("redacted"), "{manifest}");
    }

    #[test]
    fn the_commit_history_carries_its_metadata() {
        let commits = vec![
            crate::store::RollbackMeta {
                committed_at: "2026-08-25T10:41:12Z".into(),
                comment: String::new(),
                user: "cody".into(),
                client: "cli".into(),
            },
            crate::store::RollbackMeta::default(),
        ];
        let value = commit_history(&commits);
        let commits = value["commits"].as_array().unwrap();
        assert_eq!(commits[0]["index"], 0);
        assert_eq!(commits[0]["user"], "cody");
        // An entry from before the metadata existed carries empties,
        // which the readers render as `-`.
        assert_eq!(commits[1]["user"], "");
    }

    #[test]
    fn journal_entries_render_as_readable_lines() {
        let entries = vec![
            crate::journal::Entry {
                time_unix: 1_787_654_472,
                host: "hemlock-a1".into(),
                tag: "mgmtd".into(),
                pid: 812,
                message: "commit 0 applied by cody (cli)".into(),
                severity: 6,
            },
            crate::journal::Entry {
                time_unix: 1_787_654_473,
                host: "hemlock-a1".into(),
                tag: "kernel".into(),
                pid: 0,
                message: "link up".into(),
                severity: 6,
            },
        ];
        let text = render_journal(&entries);
        assert!(
            text.contains("2026-08-25 10:41:12 hemlock-a1 mgmtd[812]: commit 0 applied"),
            "{text}"
        );
        // No pid recorded means no brackets, not `[0]`.
        assert!(text.contains("hemlock-a1 kernel: link up"), "{text}");
    }
}
