//! Maintenance operations: reboot scheduling and in-band OS image
//! upgrades.
//!
//! Follows the users.rs precedent — webd runs as root on the switch and
//! shells out, with cfg!(unix) guards so development hosts get a clear
//! refusal instead of a broken half-action. Image uploads are staged
//! under the webd state directory (flash-backed on the switch), so an
//! upload survives a webd restart and "install" is a separate,
//! deliberate step.
//!
//! An install replaces the single system image in place (there is no
//! A/B slot scheme yet): the payload is extracted with the image's own
//! HEMLOCK_EXTRACT_ONLY hook, then rootfs.squashfs, the boot assets and
//! the platform overlay are copied onto the flash filesystem the
//! initramfs keeps mounted at /host. Each file lands via write-to-.new
//! + fsync + rename so a power cut mid-copy leaves the old file intact.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Flash locations of the running system (docs/architecture.md): the
/// initramfs mounts the flash filesystem at /host, with the squashfs at
/// hemlock/rootfs.squashfs and GRUB under boot/.
const FLASH_ROOT: &str = "/host";
/// The installed platform's ONIE machine string, written at install
/// time (`hemlock-installer`), addressed via the stable /hemlock link.
const INSTALLED_ONIE_MACHINE: &str = "/hemlock/platform/onie-machine";
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

#[derive(Debug, PartialEq, Serialize)]
pub struct ImageHeader {
    /// ONIE machine string the image targets (e.g. x86_64-cel_e1031-r0).
    pub platform: String,
    pub version: String,
}

/// Parse the `hemlock_image_*` shell assignments mkimage.sh writes into
/// the self-extractor's header (always within the first few lines).
pub fn parse_image_header(head: &str) -> Result<ImageHeader, String> {
    if !head.starts_with("#!/bin/sh") {
        return Err("not a Hemlock image (missing installer shebang)".to_string());
    }
    let mut platform = None;
    let mut version = None;
    for line in head.lines().take(64) {
        if line.starts_with("__HEMLOCK_PAYLOAD__") {
            break;
        }
        if let Some(v) = line.strip_prefix("hemlock_image_platform=") {
            platform = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("hemlock_image_version=") {
            version = Some(v.trim().to_string());
        }
    }
    match (platform, version) {
        (Some(platform), Some(version)) if !platform.is_empty() && !version.is_empty() => {
            Ok(ImageHeader { platform, version })
        }
        _ => Err("not a Hemlock image (missing platform/version header)".to_string()),
    }
}

fn upgrade_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("upgrade")
}

pub fn staged_path(state_dir: &Path) -> PathBuf {
    upgrade_dir(state_dir).join("image.bin")
}

fn read_header(path: &Path) -> Result<ImageHeader, String> {
    use std::io::Read as _;
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("cannot open staged image: {e}"))?;
    let mut head = vec![0u8; 4096];
    let n = file
        .read(&mut head)
        .map_err(|e| format!("cannot read staged image: {e}"))?;
    head.truncate(n);
    parse_image_header(&String::from_utf8_lossy(&head))
}

fn installed_platform() -> Option<String> {
    let text = std::fs::read_to_string(INSTALLED_ONIE_MACHINE).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
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
    let header = read_header(&path).ok()?;
    let platform_ok = installed_platform().map(|installed| installed == header.platform);
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
        let header = read_header(&part)?;
        tokio::fs::rename(&part, staged_path(state_dir))
            .await
            .map_err(|e| format!("cannot stage image: {e}"))?;
        let platform_ok = installed_platform().map(|installed| installed == header.platform);
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
    let _ = tokio::fs::remove_dir_all(dir.join("extract")).await;
}

// -------------------------------------------------------------- install

/// Install the staged image onto the flash filesystem. Returns the
/// installed image's header; the caller decides about rebooting.
pub async fn apply_staged(state_dir: &Path, force: bool) -> Result<ImageHeader, String> {
    let staged = staged_path(state_dir);
    let header = read_header(&staged)?;
    if !cfg!(unix) {
        return Err("image install is only available on the switch".to_string());
    }
    match installed_platform() {
        Some(installed) if installed == header.platform => {}
        Some(installed) if !force => {
            return Err(format!(
                "image is built for {} but this switch is {installed}",
                header.platform
            ));
        }
        None if !force => {
            return Err(format!(
                "cannot read the installed platform ({INSTALLED_ONIE_MACHINE})"
            ));
        }
        _ => {
            tracing::warn!(image = %header.platform, "installing despite platform mismatch (force)")
        }
    }

    // Unpack using the image's own extract-only hook (no install logic
    // duplicated here; the payload format stays owned by mkimage.sh).
    let extract = upgrade_dir(state_dir).join("extract");
    let _ = tokio::fs::remove_dir_all(&extract).await;
    tokio::fs::create_dir_all(&extract)
        .await
        .map_err(|e| format!("cannot create {}: {e}", extract.display()))?;
    let output = tokio::process::Command::new("sh")
        .arg(&staged)
        .env("HEMLOCK_EXTRACT_ONLY", "1")
        .env("HEMLOCK_EXTRACT_DIR", &extract)
        .output()
        .await
        .map_err(|e| format!("cannot extract image: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "image extraction failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let payload = extract.clone();
    tokio::task::spawn_blocking(move || install_payload(&payload, Path::new(FLASH_ROOT)))
        .await
        .map_err(|e| format!("install task failed: {e}"))??;

    let _ = tokio::fs::remove_dir_all(&extract).await;
    let _ = tokio::fs::remove_file(&staged).await;
    tracing::info!(version = %header.version, "os image installed");
    Ok(header)
}

/// Copy an extracted payload onto a flash root: squashfs first (the
/// biggest and slowest copy), then the boot assets, then the platform
/// overlay. Parameterized on the flash root so tests run in a tempdir.
fn install_payload(payload: &Path, flash: &Path) -> Result<(), String> {
    for rel in [
        "rootfs.squashfs",
        "boot/vmlinuz",
        "boot/initrd.img",
        "boot/grub.cfg",
        "platform",
    ] {
        if !payload.join(rel).exists() {
            return Err(format!("image payload is missing {rel}"));
        }
    }

    install_file(
        &payload.join("rootfs.squashfs"),
        &flash.join("hemlock/rootfs.squashfs"),
    )?;
    install_file(&payload.join("boot/vmlinuz"), &flash.join("boot/vmlinuz"))?;
    install_file(
        &payload.join("boot/initrd.img"),
        &flash.join("boot/initrd.img"),
    )?;
    install_file(
        &payload.join("boot/grub.cfg"),
        &flash.join("boot/grub/grub.cfg"),
    )?;

    // Platform overlay: build the replacement next to the live one and
    // swap by rename, so a failure keeps the old overlay in place.
    let live = flash.join("hemlock/platform");
    let fresh = flash.join("hemlock/platform.new");
    let old = flash.join("hemlock/platform.old");
    let _ = std::fs::remove_dir_all(&fresh);
    let _ = std::fs::remove_dir_all(&old);
    copy_dir(&payload.join("platform"), &fresh)?;
    if live.exists() {
        std::fs::rename(&live, &old).map_err(|e| format!("cannot move platform overlay: {e}"))?;
    }
    std::fs::rename(&fresh, &live).map_err(|e| format!("cannot place platform overlay: {e}"))?;
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// Copy to `<dst>.new`, fsync, rename over — a power cut mid-copy
/// leaves the previous file intact.
fn install_file(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let mut tmp = dst.as_os_str().to_os_string();
    tmp.push(".new");
    let tmp = PathBuf::from(tmp);
    std::fs::copy(src, &tmp)
        .map_err(|e| format!("cannot copy {} to {}: {e}", src.display(), tmp.display()))?;
    // Reopened with write access: FlushFileBuffers on Windows (dev
    // hosts, tests) denies a read-only handle.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&tmp)
        .map_err(|e| format!("cannot reopen {}: {e}", tmp.display()))?;
    file.sync_all()
        .map_err(|e| format!("cannot sync {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, dst).map_err(|e| format!("cannot rename into {}: {e}", dst.display()))?;
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("cannot create {}: {e}", dst.display()))?;
    let entries =
        std::fs::read_dir(src).map_err(|e| format!("cannot read {}: {e}", src.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", src.display()))?;
        let target = dst.join(entry.file_name());
        if entry
            .file_type()
            .map_err(|e| format!("cannot stat {}: {e}", entry.path().display()))?
            .is_dir()
        {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .map_err(|e| format!("cannot copy {}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
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
    fn parses_image_header() {
        let header = parse_image_header(HEADER).unwrap();
        assert_eq!(header.platform, "x86_64-cel_e1031-r0");
        assert_eq!(header.version, "0.1.0-202608231030");
    }

    #[test]
    fn rejects_non_image_files() {
        assert!(parse_image_header("").is_err());
        assert!(parse_image_header("PK\x03\x04zipzip").is_err());
        assert!(parse_image_header("#!/bin/sh\necho hello\n").is_err());
        // Header assignments after the payload marker don't count.
        let hidden =
            "#!/bin/sh\n__HEMLOCK_PAYLOAD__\nhemlock_image_platform=x\nhemlock_image_version=y\n";
        assert!(parse_image_header(hidden).is_err());
    }

    fn fake_payload(dir: &Path) {
        std::fs::create_dir_all(dir.join("boot")).unwrap();
        std::fs::create_dir_all(dir.join("platform")).unwrap();
        std::fs::write(dir.join("rootfs.squashfs"), b"new-squash").unwrap();
        std::fs::write(dir.join("boot/vmlinuz"), b"new-kernel").unwrap();
        std::fs::write(dir.join("boot/initrd.img"), b"new-initrd").unwrap();
        std::fs::write(dir.join("boot/grub.cfg"), b"new-grub").unwrap();
        std::fs::write(dir.join("platform/onie-machine"), b"x86_64-cel_e1031-r0").unwrap();
        std::fs::write(dir.join("platform/platform.toml"), b"# manifest").unwrap();
    }

    #[test]
    fn installs_payload_over_existing_system() {
        let tmp = tempfile::tempdir().unwrap();
        let payload = tmp.path().join("payload");
        let flash = tmp.path().join("flash");
        fake_payload(&payload);
        // A previously installed system, including a stale platform file
        // that the new overlay does not carry.
        std::fs::create_dir_all(flash.join("hemlock/platform")).unwrap();
        std::fs::create_dir_all(flash.join("boot/grub")).unwrap();
        std::fs::write(flash.join("hemlock/rootfs.squashfs"), b"old-squash").unwrap();
        std::fs::write(flash.join("boot/grub/grub.cfg"), b"old-grub").unwrap();
        std::fs::write(flash.join("hemlock/platform/stale.bcm"), b"old").unwrap();

        install_payload(&payload, &flash).unwrap();

        let read = |p: &str| std::fs::read_to_string(flash.join(p)).unwrap();
        assert_eq!(read("hemlock/rootfs.squashfs"), "new-squash");
        assert_eq!(read("boot/vmlinuz"), "new-kernel");
        assert_eq!(read("boot/initrd.img"), "new-initrd");
        assert_eq!(read("boot/grub/grub.cfg"), "new-grub");
        assert_eq!(read("hemlock/platform/onie-machine"), "x86_64-cel_e1031-r0");
        assert!(!flash.join("hemlock/platform/stale.bcm").exists());
        assert!(!flash.join("hemlock/platform.new").exists());
        assert!(!flash.join("hemlock/platform.old").exists());
        assert!(!flash.join("hemlock/rootfs.squashfs.new").exists());
    }

    #[test]
    fn install_refuses_incomplete_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let payload = tmp.path().join("payload");
        fake_payload(&payload);
        std::fs::remove_file(payload.join("boot/initrd.img")).unwrap();
        let err = install_payload(&payload, &tmp.path().join("flash")).unwrap_err();
        assert!(err.contains("boot/initrd.img"));
    }

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
