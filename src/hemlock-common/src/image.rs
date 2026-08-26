//! In-band OS image handling, shared by mgmtd (InstallImage RPC) and
//! webd (staged uploads): header parsing, platform guard, and the
//! in-place install of a `.bin` onto the flash filesystem.
//!
//! Images are the ONIE self-extractors mkimage.sh builds. An install
//! replaces the single system image in place (there is no A/B slot
//! scheme): the payload is unpacked with the image's own
//! HEMLOCK_EXTRACT_ONLY hook, then rootfs.squashfs, the boot assets and
//! the platform overlay are copied onto the flash filesystem the
//! initramfs keeps mounted at /host. Each file lands via copy-to-.new +
//! fsync + rename so a power cut mid-copy leaves the old file intact.
//!
//! Everything here is blocking — callers in async daemons wrap
//! [`install`] in `spawn_blocking`.

use std::path::{Path, PathBuf};

/// Flash root of the running system (docs/architecture.md): the
/// initramfs mounts the flash filesystem at /host, with the squashfs at
/// hemlock/rootfs.squashfs and GRUB under boot/.
const FLASH_ROOT: &str = "/host";
/// The installed platform's ONIE machine string, written at install
/// time (`hemlock-installer`), addressed via the stable /hemlock link.
const INSTALLED_ONIE_MACHINE: &str = "/hemlock/platform/onie-machine";

/// What the running system was installed from, written at install time.
///
/// It lives beside the squashfs on the flash filesystem rather than in
/// the platform overlay, because an install replaces the overlay
/// wholesale and would take the record with it. Same `key=value` shape
/// as the image header, so there is one format to read and no new
/// dependency to carry.
const IMAGE_RECORD: &str = "hemlock/image.env";

/// Where the kernel publishes its own release string.
const KERNEL_RELEASE: &str = "/proc/sys/kernel/osrelease";

/// Marker mgmtd writes when it arms an ONIE rescue boot. `/run` is
/// tmpfs, so the marker is gone after the reboot it applies to — which
/// is exactly the lifetime of the arming.
const ONIE_RESCUE_MARKER: &str = "/run/hemlock/onie-rescue";

#[derive(Debug, PartialEq)]
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

/// Read an image file's header (its first 4 KiB carry the assignments).
pub fn read_header(path: &Path) -> Result<ImageHeader, String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("cannot open image {}: {e}", path.display()))?;
    let mut head = vec![0u8; 4096];
    let n = file
        .read(&mut head)
        .map_err(|e| format!("cannot read image {}: {e}", path.display()))?;
    head.truncate(n);
    parse_image_header(&String::from_utf8_lossy(&head))
}

/// What the running system was installed from — the answer to
/// `show system image`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImageRecord {
    pub version: String,
    pub platform: String,
    /// The `.bin` the install came from, basename only.
    pub file: String,
    /// Unix seconds; 0 = not recorded (an image installed before the
    /// record existed, or a first boot from the installer).
    pub installed_at: u64,
}

/// Render the install record. Same `key=value` shape as the image
/// header the installer already writes.
pub fn render_image_record(record: &ImageRecord) -> String {
    format!(
        "# Written by Hemlock at install time; read by `show system image`.\n\
         hemlock_image_version={}\n\
         hemlock_image_platform={}\n\
         hemlock_image_file={}\n\
         hemlock_image_installed={}\n",
        record.version, record.platform, record.file, record.installed_at
    )
}

/// Parse an install record. Missing keys read as empty/zero, so a
/// record written by an older image still loads.
pub fn parse_image_record(text: &str) -> ImageRecord {
    let value = |key: &str| -> String {
        text.lines()
            .filter_map(|line| line.split_once('='))
            .find(|(name, _)| name.trim() == key)
            .map(|(_, value)| value.trim().trim_matches('"').to_string())
            .unwrap_or_default()
    };
    ImageRecord {
        version: value("hemlock_image_version"),
        platform: value("hemlock_image_platform"),
        file: value("hemlock_image_file"),
        installed_at: value("hemlock_image_installed").parse().unwrap_or(0),
    }
}

/// The install record of the running system, if one was written.
pub fn installed_image() -> Option<ImageRecord> {
    let text = std::fs::read_to_string(Path::new(FLASH_ROOT).join(IMAGE_RECORD)).ok()?;
    Some(parse_image_record(&text))
}

/// The running kernel's release string (`uname -r`), from procfs.
pub fn kernel_release() -> String {
    std::fs::read_to_string(KERNEL_RELEASE)
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

/// Is an ONIE rescue boot armed for the next boot?
pub fn onie_rescue_armed() -> bool {
    Path::new(ONIE_RESCUE_MARKER).exists()
}

/// Arm ONIE rescue mode for the next boot only.
///
/// ONIE owns the mechanism: `onie-boot-mode` lives on the ONIE-BOOT
/// partition of every ONIE-installed switch and writes the mode into
/// ONIE's own grub environment. Hemlock does not reimplement that —
/// it calls the tool and records that it did, so `show system image`
/// can say so before the operator commits to the reboot.
pub fn arm_onie_rescue() -> Result<(), String> {
    if !cfg!(unix) {
        return Err("ONIE rescue is only available on the switch".to_string());
    }
    let output = std::process::Command::new("onie-boot-mode")
        .args(["-o", "rescue"])
        .output()
        .map_err(|e| format!("cannot run onie-boot-mode (is this an ONIE install?): {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "onie-boot-mode failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let marker = Path::new(ONIE_RESCUE_MARKER);
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(marker, "rescue\n")
        .map_err(|e| format!("cannot record the ONIE rescue arming: {e}"))?;
    Ok(())
}

/// The ONIE machine string of the installed system, if known (absent on
/// development hosts).
pub fn installed_platform() -> Option<String> {
    let text = std::fs::read_to_string(INSTALLED_ONIE_MACHINE).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn check_platform(
    header: &ImageHeader,
    installed: Option<&str>,
    force: bool,
) -> Result<(), String> {
    match installed {
        Some(installed) if installed == header.platform => Ok(()),
        Some(installed) if !force => Err(format!(
            "image is built for {} but this switch is {installed}",
            header.platform
        )),
        None if !force => Err(format!(
            "cannot read the installed platform ({INSTALLED_ONIE_MACHINE})"
        )),
        _ => {
            tracing::warn!(image = %header.platform, "installing despite platform mismatch (force)");
            Ok(())
        }
    }
}

/// Install an image over the running system: header, platform guard,
/// extract, copy onto flash. Extraction happens in a `.hemlock-extract`
/// directory next to the image (removed afterwards). Blocking; the
/// caller decides about rebooting.
pub fn install(image: &Path, force: bool) -> Result<ImageHeader, String> {
    let header = read_header(image)?;
    if !cfg!(unix) {
        return Err("image install is only available on the switch".to_string());
    }
    check_platform(&header, installed_platform().as_deref(), force)?;

    // Unpack using the image's own extract-only hook (no install logic
    // duplicated here; the payload format stays owned by mkimage.sh).
    let extract = image
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".hemlock-extract");
    let _ = std::fs::remove_dir_all(&extract);
    std::fs::create_dir_all(&extract)
        .map_err(|e| format!("cannot create {}: {e}", extract.display()))?;
    let output = std::process::Command::new("sh")
        .arg(image)
        .env("HEMLOCK_EXTRACT_ONLY", "1")
        .env("HEMLOCK_EXTRACT_DIR", &extract)
        .output()
        .map_err(|e| format!("cannot extract image: {e}"))?;
    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&extract);
        return Err(format!(
            "image extraction failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let result = install_payload(&extract, Path::new(FLASH_ROOT));
    let _ = std::fs::remove_dir_all(&extract);
    result?;
    // Record what this system now runs, for `show system image`. A
    // failure here is not an install failure: the image is on flash
    // either way, and the record only feeds a display.
    let record = ImageRecord {
        version: header.version.clone(),
        platform: header.platform.clone(),
        file: image
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        installed_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or(0),
    };
    if let Err(err) = std::fs::write(
        Path::new(FLASH_ROOT).join(IMAGE_RECORD),
        render_image_record(&record),
    ) {
        tracing::warn!(%err, "cannot write the install record");
    }
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

    /// The install record round-trips, and a record from an older
    /// image (missing keys) still loads.
    #[test]
    fn install_records_round_trip() {
        let record = ImageRecord {
            version: "1.4.0".into(),
            platform: "x86_64-cel_e1031-r0".into(),
            file: "hemlock-e1031-1.4.0.bin".into(),
            installed_at: 1_787_148_131,
        };
        assert_eq!(parse_image_record(&render_image_record(&record)), record);
        // Missing keys read as empty/zero rather than failing.
        let partial = parse_image_record(
            "hemlock_image_version=1.3.0
",
        );
        assert_eq!(partial.version, "1.3.0");
        assert!(partial.file.is_empty());
        assert_eq!(partial.installed_at, 0);
        assert_eq!(parse_image_record(""), ImageRecord::default());
        // A quoted value (the installer writes some) unquotes.
        assert_eq!(
            parse_image_record(
                "hemlock_image_file=\"a b.bin\"
"
            )
            .file,
            "a b.bin"
        );
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

    #[test]
    fn platform_guard_requires_match_or_force() {
        let header = parse_image_header(HEADER).unwrap();
        assert!(check_platform(&header, Some("x86_64-cel_e1031-r0"), false).is_ok());
        assert!(check_platform(&header, Some("x86_64-other-r0"), false).is_err());
        assert!(check_platform(&header, Some("x86_64-other-r0"), true).is_ok());
        assert!(check_platform(&header, None, false).is_err());
        assert!(check_platform(&header, None, true).is_ok());
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
}
