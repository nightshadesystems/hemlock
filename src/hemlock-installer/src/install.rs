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
    /// ARM: U-Boot loads a FIT image from a GPT block device. There is
    /// no bootloader to install — U-Boot is in SPI-NOR and ONIE owns it —
    /// so the hand-off is one `nos_bootcmd` environment variable.
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
    /// Target block device, e.g. `/dev/sda` — a block device on both
    /// boot styles, though on a FIT board it is USB-attached.
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

    /// ARM / U-Boot.
    ///
    /// Verified against the board (ONIE 2016.05, U-Boot 2012.10):
    ///
    /// * The NOS storage is a **USB-attached block device with GPT
    ///   partitions** (`onie_partition_type=gpt`), not NAND. `/proc/mtd`
    ///   holds only the 8 MB SPI-NOR — uboot, shmoo, uboot-env, onie —
    ///   and `ubinfo` reports zero UBI devices. An earlier draft of this
    ///   function formatted UBI volumes on NAND; there is no such NAND.
    /// * There is no bootloader to install: U-Boot lives in SPI-NOR and
    ///   ONIE owns it. Installing means writing files and then pointing
    ///   `nos_bootcmd` at them.
    /// * `bootcmd` runs `nos_bootcmd` *before* `onie_bootcmd`, so setting
    ///   that one variable is the whole hand-off. This ONIE has no
    ///   `onie-nos-mode`, and does not need one.
    ///
    /// Layout: two GPT partitions. A small **ext2** boot partition holds
    /// the FIT, because `ext2load` is what U-Boot reads it with. This is
    /// now confirmed on the board rather than assumed: the flashed
    /// bootloader exports `usbiddev`, `ext2load` and `bootm`, but has
    /// **no `ext4load`**, so an ext4 boot partition would be unreadable
    /// and the 64 MB spent here is mandatory, not caution. The rest is
    /// ext4 and carries the squashfs, the persist overlay and the
    /// platform directory, exactly as on x86.
    fn fit_steps(&self) -> Vec<Step> {
        let disk = self.disk.display().to_string();
        let boot = self.part(1);
        let root = self.part(2);
        let payload = self.payload.display().to_string();

        // `usbiddev` sets ${usbdev}; the stock nos_bootcmd on this board
        // uses the same idiom. The FIT is loaded to ONIE's scratch
        // address and bootm relocates the kernel to its own load address
        // (0x61008000, from the FIT that mkimage.sh built).
        let nos_bootcmd = "usb start && usbiddev && \
             ext2load usb ${usbdev}:1 0x70000000 /boot/hemlock.itb && \
             bootm 0x70000000";

        vec![
            Step {
                title: "Partition disk (GPT: boot, root)",
                commands: vec![
                    cmd(["sgdisk", "--zap-all", &disk]),
                    cmd([
                        "sgdisk",
                        "-n",
                        "1:0:+64M",
                        "-t",
                        "1:8300",
                        "-c",
                        "1:HEMLOCK-BOOT",
                        &disk,
                    ]),
                    cmd([
                        "sgdisk",
                        "-n",
                        "2:0:0",
                        "-t",
                        "2:8300",
                        "-c",
                        "2:HEMLOCK-ROOT",
                        &disk,
                    ]),
                    cmd(["partprobe", &disk]),
                ],
            },
            Step {
                title: "Create filesystems",
                commands: vec![
                    // ext2 for the one file U-Boot has to read.
                    cmd(["mkfs.ext2", "-F", "-L", "HEMLOCK-BOOT", &boot]),
                    cmd(["mkfs.ext4", "-F", "-L", "HEMLOCK", &root]),
                ],
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
                title: "Write boot image (FIT)",
                commands: vec![
                    cmd(["mkdir", "-p", &format!("{MOUNT_POINT}/bootpart")]),
                    cmd(["mount", &boot, &format!("{MOUNT_POINT}/bootpart")]),
                    cmd(["mkdir", "-p", &format!("{MOUNT_POINT}/bootpart/boot")]),
                    cmd([
                        "cp",
                        &format!("{payload}/boot/hemlock.itb"),
                        &format!("{MOUNT_POINT}/bootpart/boot/hemlock.itb"),
                    ]),
                    cmd(["umount", &format!("{MOUNT_POINT}/bootpart")]),
                ],
            },
            Step {
                title: "Set U-Boot environment",
                commands: vec![
                    // The kernel command line rides inside the FIT
                    // (mkimage.sh renders it), so nothing here repeats it.
                    //
                    // -f is mandatory, not tidiness. This board's ONIE
                    // ships BusyBox's fw_setenv, which asks "Proceed with
                    // update [N/y]?", defaults to *no* when there is no
                    // tty to answer — and still exits 0. Without -f the
                    // step reports success, writes nothing, and the board
                    // silently boots ONIE forever because nos_bootcmd is
                    // still the stock no-op `true`.
                    cmd(["fw_setenv", "-f", "nos_bootcmd", nos_bootcmd]),
                    // ...and because a zero exit proved nothing above,
                    // read it back. An install that cannot set the boot
                    // command has not installed anything bootable.
                    cmd([
                        "sh",
                        "-c",
                        "fw_printenv nos_bootcmd | grep -q /boot/hemlock.itb",
                    ]),
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

/// Block-device names that are never install targets, whatever their
/// size or removable flag: RAM disks, loopbacks, device-mapper nodes,
/// optical/floppy — and above all raw flash. `mtdblockN`/`ubiN` are the
/// block views of the SPI-NOR that holds U-Boot, its environment and
/// ONIE itself (on the AS4610: uboot, shmoo, uboot-env, onie), and they
/// sort ahead of `sda`, so before this filter the picker's default
/// selection was the bootloader flash.
fn never_a_target(name: &str) -> bool {
    [
        "ram", "loop", "dm-", "mtdblock", "ubi", "zram", "md", "sr", "fd", "nbd",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

/// Raw-flash device paths get refused even when named explicitly with
/// `--disk`: the install plan speaks GPT + sgdisk, and running that
/// against the SPI-NOR chews up the bootloader, not a NOS partition.
pub fn is_raw_flash(disk: &Path) -> bool {
    disk.file_name()
        .map(|n| {
            n.to_string_lossy().starts_with("mtdblock") || n.to_string_lossy().starts_with("ubi")
        })
        .unwrap_or(false)
}

pub fn list_disks() -> Vec<Disk> {
    let Ok(entries) = std::fs::read_dir("/sys/block") else {
        return Vec::new();
    };
    let mut disks = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if never_a_target(&name) {
            continue;
        }
        let sys = entry.path();
        // Removable means the USB stick the ONIE installer is running
        // from, which must never be offered as a target.
        //
        // The AS4610 looked like it might need an exception — U-Boot
        // reaches its NOS storage with `usb start && usbiddev`, so the
        // target is USB-attached — but the board says otherwise: `sda`
        // (7.5 GB) reports `removable = 0` like any fixed disk. The
        // filter is right on both boot styles.
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

    fn arm_plan(disk: &str) -> InstallPlan {
        InstallPlan {
            disk: disk.into(),
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

        let mut arm = arm_plan("/dev/sda");
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

    /// The ARM install partitions a block device, writes a FIT, and
    /// points `nos_bootcmd` at it. Shape verified against the board:
    /// GPT on USB-attached storage, no NAND, no UBI, and no
    /// `onie-nos-mode` (this ONIE does not ship one, and `bootcmd`
    /// already runs `nos_bootcmd` before `onie_bootcmd`).
    #[test]
    fn the_arm_install_writes_a_fit_and_sets_the_uboot_environment() {
        let commands = all_commands(&arm_plan("/dev/sda"));
        for expected in [
            "sgdisk --zap-all /dev/sda",
            // The FIT lives on ext2 because U-Boot's ext2load reads it.
            "mkfs.ext2 -F -L HEMLOCK-BOOT /dev/sda1",
            "mkfs.ext4 -F -L HEMLOCK /dev/sda2",
            "hemlock.itb",
            // -f: BusyBox fw_setenv prompts and defaults to "no" without a
            // tty, yet exits 0 — so the unforced form silently no-ops.
            "fw_setenv -f nos_bootcmd",
            // The write is read back, because exit 0 does not prove it took.
            "fw_printenv nos_bootcmd | grep -q /boot/hemlock.itb",
            "ext2load usb ${usbdev}:1 0x70000000 /boot/hemlock.itb",
        ] {
            assert!(
                commands.contains(expected),
                "missing {expected:?}:\n{commands}"
            );
        }
        // Nothing that does not exist on this board, and no GRUB.
        for forbidden in ["ubiformat", "ubiattach", "onie-nos-mode", "grub-install"] {
            assert!(
                !commands.contains(forbidden),
                "{forbidden:?} has no business in this board's install:\n{commands}"
            );
        }
    }

    /// Enumeration reads the real `/sys/block`, so all this can assert
    /// is that it is safe on any host, with or without one.
    #[test]
    fn disk_enumeration_is_safe_everywhere() {
        let _ = list_disks();
    }

    /// The SPI-NOR's block views must never be offered or accepted: on
    /// the AS4610, mtdblock0 is the `uboot` partition itself, and it
    /// sorts ahead of `sda` — before this filter it was the picker's
    /// default selection.
    #[test]
    fn raw_flash_is_never_a_target() {
        for name in [
            "mtdblock0",
            "mtdblock3",
            "ubi0",
            "ram0",
            "loop1",
            "dm-0",
            "sr0",
        ] {
            assert!(never_a_target(name), "{name} must be filtered out");
        }
        for name in ["sda", "sdb", "mmcblk0", "nvme0n1"] {
            assert!(!never_a_target(name), "{name} must stay offered");
        }
        assert!(is_raw_flash(Path::new("/dev/mtdblock0")));
        assert!(is_raw_flash(Path::new("/dev/ubi0")));
        assert!(!is_raw_flash(Path::new("/dev/sda")));
        assert!(!is_raw_flash(Path::new("/dev/mmcblk0")));
    }

    /// ...and the x86 install is unchanged by any of it.
    #[test]
    fn the_x86_install_is_untouched() {
        let commands = all_commands(&plan("/dev/sda"));
        for expected in ["sgdisk --zap-all /dev/sda", "grub-install", "mkfs.ext4"] {
            assert!(commands.contains(expected), "missing {expected:?}");
        }
        for forbidden in ["fw_setenv", "hemlock.itb", "ext2load"] {
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
        for plan in [plan("/dev/sda"), arm_plan("/dev/sda")] {
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
        let p = arm_plan("/dev/sda");
        for step in p.steps() {
            p.run_step(&step).unwrap();
        }
    }
}
