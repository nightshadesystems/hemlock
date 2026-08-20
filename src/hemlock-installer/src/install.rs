//! The install plan: what turning a blank disk into a Hemlock system means.
//!
//! Steps are data first (so the TUI can show them and `--dry-run` can print
//! them) and shell commands second. Runs inside the ONIE rescue environment
//! (BusyBox + the tools ONIE ships: sgdisk, mkfs.ext4, grub-install).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use tracing::info;

/// Everything an install needs to know, resolved before any step runs.
#[derive(Debug, Clone)]
pub struct InstallPlan {
    /// Target block device, e.g. `/dev/sda`.
    pub disk: PathBuf,
    /// Payload directory (unpacked ONIE self-extractor): rootfs.squashfs,
    /// platform/ overlay, boot assets.
    pub payload: PathBuf,
    /// Platform id baked into the image (e.g. `cel-e1031`).
    pub platform_id: String,
    pub dry_run: bool,
}

/// A single install step: description + the commands it runs.
pub struct Step {
    pub title: &'static str,
    commands: Vec<Vec<String>>,
}

const MOUNT_POINT: &str = "/tmp/hemlock-install";

impl InstallPlan {
    fn part(&self, n: u32) -> String {
        // /dev/sda -> /dev/sda3, /dev/nvme0n1 -> /dev/nvme0n1p3
        let disk = self.disk.display().to_string();
        if disk.chars().last().is_some_and(|c| c.is_ascii_digit()) {
            format!("{disk}p{n}")
        } else {
            format!("{disk}{n}")
        }
    }

    /// The ordered steps. Layout: GPT with an EFI system partition (kept
    /// small; Rangeley boxes boot legacy GRUB from the BIOS boot part) and
    /// one root filesystem holding the squashfs + persistent overlay.
    pub fn steps(&self) -> Vec<Step> {
        let disk = self.disk.display().to_string();
        let root = self.part(3);
        let payload = self.payload.display().to_string();

        vec![
            Step {
                title: "Partition disk (GPT: BIOS boot, ESP, root)",
                commands: vec![
                    cmd(["sgdisk", "--zap-all", &disk]),
                    cmd(["sgdisk", "-n", "1:0:+2M", "-t", "1:ef02", "-c", "1:HEMLOCK-BIOS", &disk]),
                    cmd(["sgdisk", "-n", "2:0:+128M", "-t", "2:ef00", "-c", "2:HEMLOCK-ESP", &disk]),
                    cmd(["sgdisk", "-n", "3:0:0", "-t", "3:8300", "-c", "3:HEMLOCK-ROOT", &disk]),
                    cmd(["partprobe", &disk]),
                ],
            },
            Step {
                title: "Create root filesystem",
                commands: vec![cmd(["mkfs.ext4", "-F", "-L", "HEMLOCK", &root])],
            },
            Step {
                title: "Copy system image",
                commands: vec![
                    cmd(["mkdir", "-p", MOUNT_POINT]),
                    cmd(["mount", &root, MOUNT_POINT]),
                    cmd(["mkdir", "-p", &format!("{MOUNT_POINT}/hemlock")]),
                    cmd([
                        "cp",
                        &format!("{payload}/rootfs.squashfs"),
                        &format!("{MOUNT_POINT}/hemlock/rootfs.squashfs"),
                    ]),
                    cmd(["mkdir", "-p", &format!("{MOUNT_POINT}/hemlock/persist")]),
                ],
            },
            Step {
                title: "Place platform overlay",
                commands: vec![cmd([
                    "cp",
                    "-r",
                    &format!("{payload}/platform"),
                    &format!("{MOUNT_POINT}/hemlock/platform"),
                ])],
            },
            Step {
                title: "Install GRUB",
                commands: vec![
                    cmd([
                        "grub-install",
                        "--target=i386-pc",
                        &format!("--boot-directory={MOUNT_POINT}/boot"),
                        &disk,
                    ]),
                    cmd([
                        "cp",
                        &format!("{payload}/boot/grub.cfg"),
                        &format!("{MOUNT_POINT}/boot/grub/grub.cfg"),
                    ]),
                    cmd([
                        "cp",
                        &format!("{payload}/boot/vmlinuz"),
                        &format!("{MOUNT_POINT}/boot/vmlinuz"),
                    ]),
                    cmd([
                        "cp",
                        &format!("{payload}/boot/initrd.img"),
                        &format!("{MOUNT_POINT}/boot/initrd.img"),
                    ]),
                ],
            },
            Step {
                title: "Finish",
                commands: vec![cmd(["umount", MOUNT_POINT])],
            },
        ]
    }

    /// Execute one step; in dry-run mode the commands are only logged.
    pub fn run_step(&self, step: &Step) -> Result<()> {
        info!(step = step.title, dry_run = self.dry_run, "install step");
        for argv in &step.commands {
            let rendered = argv.join(" ");
            if self.dry_run {
                info!("  (dry-run) {rendered}");
                continue;
            }
            let status = Command::new(&argv[0])
                .args(&argv[1..])
                .status()
                .with_context(|| format!("spawning {rendered:?}"))?;
            if !status.success() {
                bail!("step {:?} failed: {rendered:?} -> {status}", step.title);
            }
        }
        Ok(())
    }

    /// Sanity-check the payload before touching the disk.
    pub fn validate_payload(&self) -> Result<()> {
        let required: [&Path; 2] = [
            Path::new("rootfs.squashfs"),
            Path::new("platform/platform.toml"),
        ];
        for rel in required {
            let path = self.payload.join(rel);
            if !path.exists() {
                bail!(
                    "payload is missing {} (looked at {})",
                    rel.display(),
                    path.display()
                );
            }
        }
        Ok(())
    }
}

fn cmd<const N: usize>(argv: [&str; N]) -> Vec<String> {
    argv.iter().map(|s| s.to_string()).collect()
}

/// Enumerate installable block devices (Linux sysfs; empty elsewhere).
#[derive(Debug, Clone)]
pub struct Disk {
    pub device: PathBuf,
    pub size_bytes: u64,
    pub model: String,
}

pub fn list_disks() -> Vec<Disk> {
    let Ok(entries) = std::fs::read_dir("/sys/block") else {
        return Vec::new();
    };
    let mut disks = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Skip ram/loop/dm and removable USB sticks (the ONIE installer
        // itself often runs from one).
        if name.starts_with("ram") || name.starts_with("loop") || name.starts_with("dm-") {
            continue;
        }
        let sys = entry.path();
        let removable = std::fs::read_to_string(sys.join("removable"))
            .map(|s| s.trim() == "1")
            .unwrap_or(false);
        if removable {
            continue;
        }
        let sectors: u64 = std::fs::read_to_string(sys.join("size"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        if sectors == 0 {
            continue;
        }
        let model = std::fs::read_to_string(sys.join("device/model"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        disks.push(Disk {
            device: PathBuf::from(format!("/dev/{name}")),
            size_bytes: sectors * 512,
            model,
        });
    }
    disks.sort_by(|a, b| a.device.cmp(&b.device));
    disks
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn plan(disk: &str) -> InstallPlan {
        InstallPlan {
            disk: disk.into(),
            payload: "payload".into(),
            platform_id: "cel-e1031".into(),
            dry_run: true,
        }
    }

    #[test]
    fn partition_naming_handles_nvme() {
        assert_eq!(plan("/dev/sda").part(3), "/dev/sda3");
        assert_eq!(plan("/dev/nvme0n1").part(3), "/dev/nvme0n1p3");
        assert_eq!(plan("/dev/mmcblk0").part(2), "/dev/mmcblk0p2");
    }

    #[test]
    fn steps_cover_the_whole_install() {
        let steps = plan("/dev/sda").steps();
        let titles: Vec<_> = steps.iter().map(|s| s.title).collect();
        assert!(titles.iter().any(|t| t.contains("Partition")));
        assert!(titles.iter().any(|t| t.contains("GRUB")));
        assert!(titles.iter().any(|t| t.contains("platform overlay")));
    }

    #[test]
    fn dry_run_executes_without_tools() {
        let p = plan("/dev/sda");
        for step in p.steps() {
            p.run_step(&step).unwrap();
        }
    }

    #[test]
    fn payload_validation_catches_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = plan("/dev/sda");
        p.payload = dir.path().to_path_buf();
        assert!(p.validate_payload().is_err());

        std::fs::write(dir.path().join("rootfs.squashfs"), b"squash").unwrap();
        std::fs::create_dir_all(dir.path().join("platform")).unwrap();
        std::fs::write(dir.path().join("platform/platform.toml"), b"# m").unwrap();
        assert!(p.validate_payload().is_ok());
    }
}
