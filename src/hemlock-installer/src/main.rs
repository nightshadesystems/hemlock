//! hemlock-installer — runs inside the ONIE rescue environment.
//!
//! The self-extracting image (`build/mkimage.sh`) unpacks its payload and
//! executes this binary. Flow: verify machine.conf matches the platform the
//! image was built for (refuse otherwise unless --force), pick a disk (TUI
//! or --disk), then partition, copy the system image + platform overlay,
//! and install GRUB.

mod install;
mod machine;
mod tui;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;
use tracing::{info, warn};

use install::InstallPlan;
use machine::MachineConf;

#[derive(Parser)]
#[command(name = "hemlock-installer", version = env!("HEMLOCK_VERSION"), about)]
struct Args {
    /// Payload directory (rootfs.squashfs, platform/, boot/). The
    /// self-extractor passes this automatically.
    #[arg(long, default_value = ".")]
    payload: PathBuf,

    /// ONIE machine.conf path.
    #[arg(long, default_value = "/etc/machine.conf")]
    machine_conf: PathBuf,

    /// Install even if machine.conf does not match this image's platform.
    #[arg(long)]
    force: bool,

    /// Target disk (skips the TUI disk picker), e.g. /dev/sda.
    #[arg(long)]
    disk: Option<PathBuf>,

    /// Print the install commands instead of running them.
    #[arg(long)]
    dry_run: bool,

    /// No TUI at all: requires --disk; logs step progress to stderr.
    #[arg(long)]
    non_interactive: bool,
}

/// Read the platform identity baked into the payload at image build time.
fn payload_platform(payload: &std::path::Path) -> Result<(String, String)> {
    let path = payload.join("platform/onie-machine");
    let onie_machine = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {} (broken payload?)", path.display()))?
        .trim()
        .to_string();
    let id_path = payload.join("platform/platform-id");
    let platform_id = std::fs::read_to_string(&id_path)
        .with_context(|| format!("reading {}", id_path.display()))?
        .trim()
        .to_string();
    Ok((platform_id, onie_machine))
}

/// Which boot style the payload was built for. Absent marker = an older
/// x86-only payload.
fn payload_boot_style(payload: &std::path::Path) -> install::BootStyle {
    let arch = std::fs::read_to_string(payload.join("platform/cpu-arch")).unwrap_or_default();
    install::BootStyle::from_arch(&arch)
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
    if let Err(err) = run() {
        eprintln!("install failed: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();

    let (platform_id, onie_machine) = payload_platform(&args.payload)?;
    info!("Hemlock installer for platform {platform_id} ({onie_machine})");

    // --- Machine verification ---
    match MachineConf::load(&args.machine_conf) {
        Ok(conf) => {
            let found = conf.platform().unwrap_or_else(|_| "<unknown>".into());
            if conf.matches(&onie_machine) {
                info!("machine.conf matches: {found}");
            } else if args.force {
                warn!("machine.conf reports {found}, image is for {onie_machine} — continuing due to --force");
            } else {
                bail!(
                    "this image is for {onie_machine} but the machine reports {found}; \
                     refusing to install (use --force to override)"
                );
            }
        }
        Err(err) => {
            if args.force {
                warn!("cannot read machine.conf ({err}); continuing due to --force");
            } else {
                bail!("cannot verify platform: {err} (use --force to override)");
            }
        }
    }

    // --- Disk selection ---
    let disk = match (&args.disk, args.non_interactive) {
        (Some(disk), _) => disk.clone(),
        (None, true) => bail!("--non-interactive requires --disk"),
        (None, false) => {
            let disks = install::list_disks();
            match tui::select_disk(&disks, &platform_id)? {
                Some(disk) => disk.device,
                None => {
                    info!("aborted by operator");
                    return Ok(());
                }
            }
        }
    };

    let plan = InstallPlan {
        disk,
        payload: args.payload.clone(),
        platform_id,
        boot_style: payload_boot_style(&args.payload),
        dry_run: args.dry_run,
    };
    plan.validate_payload()?;

    // --- Execute ---
    if args.non_interactive {
        for step in plan.steps() {
            plan.run_step(&step)?;
        }
    } else {
        tui::run_install(&plan)?;
    }
    info!("install complete — remove install media and reboot");
    Ok(())
}
