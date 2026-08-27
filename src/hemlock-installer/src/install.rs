//! The install plan: what turning a blank disk into a Hemlock system means.
//!
//! Steps are data first (so the TUI can show them and `--dry-run` can print
//! them) and shell commands second. Runs inside the ONIE rescue environment
//! (BusyBox + the tools ONIE ships: sgdisk, mkfs.ext4, grub-install).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use tracing::info;

/// How the target board boots, which decides the whole install shape.
///
/// Read from the payload's `platform/cpu-arch` marker rather than guessed
/// from the running environment: the image knows what it was built for,
/// and the installer runs inside ONIE, not on the installed system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BootStyle {
    /// x86: GPT on a block device, GRUB in the BIOS boot partition.
    #[default]
    Grub,
    /// ARM: U-Boot loads a FIT image; the NOS lives on raw NAND with no
    /// bootloader of its own to install.
    Fit,
}

impl BootStyle {
    /// Derive from the payload's `cpu-arch` marker. Payloads written
    /// before the marker existed are all x86.
    pub fn from_arch(arch: &str) -> Self {
        match arch.trim() {
            "armhf" => BootStyle::Fit,
            _ => BootStyle::Grub,
        }
    }
}

/// Everything an install needs to know, resolved before any step runs.
#[derive(Debug, Clone)]
pub struct InstallPlan {
    /// Target block device, e.g. `/dev/sda`. On a FIT/NAND board this is
    /// the UBI-backed MTD device instead.
    pub disk: PathBuf,
    /// Payload directory (unpacked ONIE self-extractor): rootfs.squashfs,
    /// platform/ overlay, boot assets.
    pub payload: PathBuf,
    /// Platform id baked into the image (e.g. `cel-e1031`).
    pub platform_id: String,
    pub boot_style: BootStyle,
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

    /// The ordered steps for this board's boot style.
    pub fn steps(&self) -> Vec<Step> {
        match self.boot_style {
            BootStyle::Grub => self.grub_steps(),
            BootStyle::Fit => self.fit_steps(),
        }
    }

    /// ARM / U-Boot / NAND.
    ///
    /// There is no bootloader to install: U-Boot is already in SPI-NOR
    /// and ONIE owns it. What an install does here is put a FIT and the
    /// root filesystem into UBI volumes on the NAND, then point U-Boot's
    /// `nos_bootcmd` at the FIT. `onie-nos-mode -s` is what makes the
    /// next boot pick the NOS instead of ONIE.
    ///
    /// **The UBI layout below is unverified.** Open question 2 in
    /// docs/as4610-54-port.md: nobody has yet read `/proc/mtd` on this
    /// board from ONIE, so which MTD partition the NOS gets, and whether
    /// ONIE presents it as raw MTD or an existing UBI device, is still a
    /// guess. It is written out in full precisely so `--dry-run` shows
    /// the whole sequence for checking against the real box before
    /// anything is written. Phase 5 corrects it.
    fn fit_steps(&self) -> Vec<Step> {
        let mtd = self.disk.display().to_string();
        let payload = self.payload.display().to_string();
        // UBI volume names, kept short: UBI caps them at 127 bytes but
        // U-Boot's env is where they have to be typed.
        let boot_vol = "hemlock-boot";
        let root_vol = "hemlock-root";

        vec![
            Step {
                title: "Attach NAND (UBI)",
                commands: vec![
                    // Detach first so a reinstall does not fail on an
                    // already-attached device.
                    cmd(["ubidetach", "-p", &mtd]),
                    cmd(["ubiformat", &mtd, "-y"]),
                    cmd(["ubiattach", "-p", &mtd]),
                ],
            },
            Step {
                title: "Create UBI volumes",
                commands: vec![
                    cmd(["ubimkvol", "/dev/ubi0", "-N", boot_vol, "-s", "32MiB"]),
                    // The rest of the device: the squashfs plus room for
                    // the writable overlay and a future upgrade.
                    cmd(["ubimkvol", "/dev/ubi0", "-N", root_vol, "-m"]),
                ],
            },
            Step {
                title: "Write boot image (FIT)",
                commands: vec![
                    cmd(["mkdir", "-p", &format!("{MOUNT_POINT}/boot")]),
                    cmd([
                        "mount",
                        "-t",
                        "ubifs",
                        &format!("ubi0:{boot_vol}"),
                        &format!("{MOUNT_POINT}/boot"),
                    ]),
                    cmd([
                        "cp",
                        &format!("{payload}/boot/hemlock.itb"),
                        &format!("{MOUNT_POINT}/boot/hemlock.itb"),
                    ]),
                    cmd(["umount", &format!("{MOUNT_POINT}/boot")]),
                ],
            },
            Step {
                title: "Copy system image",
                commands: vec![
                    cmd(["mkdir", "-p", MOUNT_POINT]),
                    cmd([
                        "mount",
                        "-t",
                        "ubifs",
                        &format!("ubi0:{root_vol}"),
                        MOUNT_POINT,
                    ]),
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
                title: "Set U-Boot environment",
                commands: vec![
                    // Load the FIT from the boot volume and boot its
                    // default configuration. The kernel command line
                    // rides in the FIT itself (mkimage.sh renders it),
                    // so nothing here has to repeat it.
                    cmd([
                        "fw_setenv",
                        "nos_bootcmd",
                        &format!(
                            "ubi part {mtd_name}; ubifsmount ubi0:{boot_vol}; \
                             ubifsload 0x61000000 /hemlock.itb; bootm 0x61000000",
                            mtd_name = "nos"
                        ),
                    ]),
                    // Tell ONIE the NOS is installed, so the next boot
                    // runs nos_bootcmd rather than dropping into ONIE.
                    cmd(["onie-nos-mode", "-s"]),
                ],
            },
            Step {
                title: "Finish",
                commands: vec![cmd(["umount", MOUNT_POINT])],
            },
        ]
    }

    /// x86 / GRUB. Layout: GPT with an EFI system partition (kept small;
    /// Rangeley boxes boot legacy GRUB from the BIOS boot part) and one
    /// root filesystem holding the squashfs + persistent overlay.
    fn grub_steps(&self) -> Vec<Step> {
        let disk = self.disk.display().to_string();
        let root = self.part(3);
        let payload = self.payload.display().to_string();

        vec![
            Step {
                title: "Partition disk (GPT: BIOS boot, ESP, root)",
                commands: vec![
                    cmd(["sgdisk", "--zap-all", &disk]),
                    cmd([
                        "sgdisk",
                        "-n",
                        "1:0:+2M",
                        "-t",
                        "1:ef02",
                        "-c",
                        "1:HEMLOCK-BIOS",
                        &disk,
                    ]),
                    cmd([
                        "sgdisk",
                        "-n",
                        "2:0:+128M",
                        "-t",
                        "2:ef00",
                        "-c",
                        "2:HEMLOCK-ESP",
                        &disk,
                    ]),
                    cmd([
                        "sgdisk",
                        "-n",
                        "3:0:0",
                        "-t",
                        "3:8300",
                        "-c",
                        "3:HEMLOCK-ROOT",
                        &disk,
                    ]),
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
        let mut required: Vec<&Path> = vec![
            Path::new("rootfs.squashfs"),
            Path::new("platform/platform.toml"),
        ];
        // The boot artifact this board actually needs. Finding it absent
        // here beats finding it absent after the NAND has been erased.
        match self.boot_style {
            BootStyle::Grub => required.push(Path::new("boot/grub.cfg")),
            BootStyle::Fit => required.push(Path::new("boot/hemlock.itb")),
        }
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
            boot_style: BootStyle::Grub,
            dry_run: true,
        }
    }

    fn arm_plan(mtd: &str) -> InstallPlan {
        InstallPlan {
            disk: mtd.into(),
            payload: "payload".into(),
            platform_id: "accton-as4610-54".into(),
            boot_style: BootStyle::Fit,
            dry_run: true,
        }
    }

    /// Every command a plan would run, for assertions about the shape of
    /// an install rather than its individual steps.
    fn all_commands(plan: &InstallPlan) -> String {
        plan.steps()
            .iter()
            .flat_map(|s| s.commands.iter())
            .map(|argv| argv.join(" "))
            .collect::<Vec<_>>()
            .join("\n")
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
        std::fs::create_dir_all(dir.path().join("boot")).unwrap();
        std::fs::write(dir.path().join("boot/grub.cfg"), b"# g").unwrap();
        assert!(p.validate_payload().is_ok());
    }

    #[test]
    fn boot_style_comes_from_the_payload_marker() {
        assert_eq!(BootStyle::from_arch("armhf"), BootStyle::Fit);
        assert_eq!(BootStyle::from_arch("armhf\n"), BootStyle::Fit);
        assert_eq!(BootStyle::from_arch("amd64"), BootStyle::Grub);
        // An absent or unreadable marker is an older x86-only payload.
        assert_eq!(BootStyle::from_arch(""), BootStyle::Grub);
    }

    /// Each boot style demands its own boot artifact, checked before the
    /// storage is touched — on an ARM board the NAND is erased in the
    /// first step, so "the FIT is missing" must surface before that.
    #[test]
    fn each_boot_style_requires_its_own_boot_artifact() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rootfs.squashfs"), b"squash").unwrap();
        std::fs::create_dir_all(dir.path().join("platform")).unwrap();
        std::fs::write(dir.path().join("platform/platform.toml"), b"# m").unwrap();
        std::fs::create_dir_all(dir.path().join("boot")).unwrap();

        let mut arm = arm_plan("/dev/mtd3");
        arm.payload = dir.path().to_path_buf();
        let mut x86 = plan("/dev/sda");
        x86.payload = dir.path().to_path_buf();
        assert!(arm.validate_payload().is_err(), "no FIT yet");
        assert!(x86.validate_payload().is_err(), "no grub.cfg yet");

        std::fs::write(dir.path().join("boot/hemlock.itb"), b"fit").unwrap();
        assert!(arm.validate_payload().is_ok());
        assert!(x86.validate_payload().is_err(), "a FIT is not a grub.cfg");

        std::fs::write(dir.path().join("boot/grub.cfg"), b"# g").unwrap();
        assert!(x86.validate_payload().is_ok());
    }

    /// The ARM install writes a FIT into UBI and points U-Boot at it.
    /// There is no bootloader to install: ONIE owns U-Boot in SPI-NOR.
    #[test]
    fn the_arm_install_writes_a_fit_and_sets_the_uboot_environment() {
        let commands = all_commands(&arm_plan("/dev/mtd3"));
        for expected in [
            "ubiformat /dev/mtd3",
            "ubiattach -p /dev/mtd3",
            "hemlock.itb",
            "fw_setenv nos_bootcmd",
            "onie-nos-mode -s",
        ] {
            assert!(
                commands.contains(expected),
                "missing {expected:?}:\n{commands}"
            );
        }
        // Nothing x86 leaks into it.
        for forbidden in ["grub-install", "sgdisk", "mkfs.ext4", "vmlinuz"] {
            assert!(
                !commands.contains(forbidden),
                "{forbidden:?} has no business in an ARM install:\n{commands}"
            );
        }
    }

    /// ...and the x86 install is unchanged by any of it.
    #[test]
    fn the_x86_install_is_untouched() {
        let commands = all_commands(&plan("/dev/sda"));
        for expected in ["sgdisk --zap-all /dev/sda", "grub-install", "mkfs.ext4"] {
            assert!(commands.contains(expected), "missing {expected:?}");
        }
        for forbidden in ["ubiformat", "fw_setenv", "onie-nos-mode"] {
            assert!(
                !commands.contains(forbidden),
                "{forbidden:?} leaked into x86"
            );
        }
    }

    /// Both styles place the squashfs, the persist dir and the platform
    /// overlay — that is the boot contract the initramfs relies on, and
    /// it does not vary by architecture.
    #[test]
    fn both_styles_honour_the_boot_contract() {
        for plan in [plan("/dev/sda"), arm_plan("/dev/mtd3")] {
            let commands = all_commands(&plan);
            for expected in ["hemlock/rootfs.squashfs", "hemlock/persist", "platform"] {
                assert!(
                    commands.contains(expected),
                    "{:?} install is missing {expected:?}",
                    plan.boot_style
                );
            }
        }
    }

    #[test]
    fn arm_dry_run_executes_without_tools() {
        let p = arm_plan("/dev/mtd3");
        for step in p.steps() {
            p.run_step(&step).unwrap();
        }
    }
}
