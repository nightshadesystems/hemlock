//! Maintenance operations: reboot scheduling and OS image staging.
//!
//! Follows the users.rs precedent — webd runs as root on the switch and
//! shells out, with cfg!(unix) guards so development hosts get a clear
//! refusal instead of a broken half-action. Image uploads are staged
//! under the webd state directory (flash-backed on the switch), so an
//! upload survives a webd restart and "install" is a separate,
//! deliberate step. The install itself goes through mgmtd's
//! InstallImage RPC (`hemlock_common::image` is the shared engine), the
//! same path `hemlockctl upgrade` uses.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hemlock_common::image;
use serde::Serialize;

/// systemd's marker file for a pending scheduled shutdown.
const SCHEDULED_FILE: &str = "/run/systemd/shutdown/scheduled";

// ---------------------------------------------------------------- shell

async fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

// --------------------------------------------------------------- reboot

#[derive(Debug, PartialEq, Serialize)]
pub struct ScheduledShutdown {
    /// Unix time (seconds) the shutdown fires.
    pub at_unix: u64,
    /// systemd mode, e.g. "reboot" or "poweroff".
    pub mode: String,
}

/// The pending scheduled shutdown, if any (systemd writes the marker
/// when `shutdown -r +N` is issued and removes it on `shutdown -c`).
pub fn scheduled_shutdown() -> Option<ScheduledShutdown> {
    parse_scheduled(&std::fs::read_to_string(SCHEDULED_FILE).ok()?)
}

fn parse_scheduled(text: &str) -> Option<ScheduledShutdown> {
    let mut usec: Option<u64> = None;
    let mut mode: Option<String> = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("USEC=") {
            usec = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("MODE=") {
            mode = Some(v.trim().to_string());
        }
    }
    Some(ScheduledShutdown {
        at_unix: usec? / 1_000_000,
        mode: mode?,
    })
}

/// Schedule a reboot `minutes` from now (`shutdown -r +N`); logind
/// wall-messages logged-in users and writes the scheduled marker.
pub async fn schedule_reboot(minutes: u64) -> Result<u64, String> {
    if !cfg!(unix) {
        return Err("reboot is only available on the switch".to_string());
    }
    run("shutdown", &["-r", &format!("+{minutes}")]).await?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(scheduled_shutdown()
        .map(|s| s.at_unix)
        .unwrap_or(now + minutes * 60))
}

pub async fn cancel_reboot() -> Result<(), String> {
    if !cfg!(unix) {
        return Err("reboot is only available on the switch".to_string());
    }
    run("shutdown", &["-c"]).await
}

/// Reboot after a short grace period so the HTTP response makes it out
/// before the box goes down. Fire-and-forget by design.
pub fn reboot_now() {
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(750)).await;
        tracing::info!("rebooting (web console)");
        let _ = tokio::process::Command::new("systemctl")
            .arg("reboot")
            .status()
            .await;
    });
}

// -------------------------------------------------------------- staging

fn upgrade_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("upgrade")
}

pub fn staged_path(state_dir: &Path) -> PathBuf {
    upgrade_dir(state_dir).join("image.bin")
}

#[derive(Debug, Serialize)]
pub struct StagedImage {
    pub version: String,
    pub platform: String,
    pub size_bytes: u64,
    /// Whether the image's platform matches this switch; null when the
    /// installed platform is unknown (development host).
    pub platform_ok: Option<bool>,
}

/// The staged image waiting to be installed, if any.
pub fn staged_info(state_dir: &Path) -> Option<StagedImage> {
    let path = staged_path(state_dir);
    let size_bytes = std::fs::metadata(&path).ok()?.len();
    let header = image::read_header(&path).ok()?;
    let platform_ok = image::installed_platform().map(|installed| installed == header.platform);
    Some(StagedImage {
        version: header.version,
        platform: header.platform,
        size_bytes,
        platform_ok,
    })
}

/// Stream an uploaded image to disk and validate its header. Lands in
/// image.bin only after the header checks out, so a staged image is
/// always at least structurally a Hemlock installer.
pub async fn stage_upload<S, E>(state_dir: &Path, mut body: S) -> Result<StagedImage, String>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    use futures_util::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;

    let dir = upgrade_dir(state_dir);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let part = dir.join("image.bin.part");

    let write = async {
        let mut file = tokio::fs::File::create(&part)
            .await
            .map_err(|e| format!("cannot create {}: {e}", part.display()))?;
        let mut size: u64 = 0;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| format!("upload interrupted: {e}"))?;
            file.write_all(&chunk)
                .await
                .map_err(|e| format!("cannot write image: {e}"))?;
            size += chunk.len() as u64;
        }
        file.sync_all()
            .await
            .map_err(|e| format!("cannot sync image: {e}"))?;
        Ok::<u64, String>(size)
    };

    let result = async {
        let size_bytes = write.await?;
        let header = image::read_header(&part)?;
        tokio::fs::rename(&part, staged_path(state_dir))
            .await
            .map_err(|e| format!("cannot stage image: {e}"))?;
        let platform_ok = image::installed_platform().map(|installed| installed == header.platform);
        Ok(StagedImage {
            version: header.version,
            platform: header.platform,
            size_bytes,
            platform_ok,
        })
    }
    .await;

    if result.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    result
}

/// Drop the staged image (and any leftover extraction).
pub async fn discard_staged(state_dir: &Path) {
    let dir = upgrade_dir(state_dir);
    let _ = tokio::fs::remove_file(dir.join("image.bin")).await;
    let _ = tokio::fs::remove_file(dir.join("image.bin.part")).await;
    let _ = tokio::fs::remove_dir_all(dir.join(".hemlock-extract")).await;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_scheduled_shutdown_marker() {
        let text = "USEC=1766500000000000\nWARN_WALL=1\nMODE=reboot\n";
        assert_eq!(
            parse_scheduled(text),
            Some(ScheduledShutdown {
                at_unix: 1_766_500_000,
                mode: "reboot".to_string(),
            })
        );
        assert_eq!(parse_scheduled("MODE=reboot\n"), None);
        assert_eq!(parse_scheduled(""), None);
    }

    const HEADER: &str = "#!/bin/sh\n\
        # Hemlock ONIE installer image\n\
        hemlock_image_platform=x86_64-cel_e1031-r0\n\
        hemlock_image_version=0.1.0-202608231030\n\
        set -e\n";

    #[test]
    fn staged_info_reads_header_and_size() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(staged_info(tmp.path()).is_none());
        std::fs::create_dir_all(upgrade_dir(tmp.path())).unwrap();
        std::fs::write(staged_path(tmp.path()), HEADER).unwrap();
        let staged = staged_info(tmp.path()).unwrap();
        assert_eq!(staged.version, "0.1.0-202608231030");
        assert_eq!(staged.platform, "x86_64-cel_e1031-r0");
        assert_eq!(staged.size_bytes, HEADER.len() as u64);
    }
}
