//! hemlockctl — the Hemlock operator CLI.
//!
//! Talks to the Hemlock daemons over their gRPC endpoints: syncd/pmon for
//! `show ...` state, mgmtd for the config lifecycle (`load`, `commit`,
//! `rollback`, ...), plus offline platform tooling.

use clap::{Parser, Subcommand};
use hemlock_common::ipc::{Daemon, IpcEndpoint};

mod cli;
mod complete;
mod config;
mod motd;
mod platform;
mod show;

#[derive(Parser)]
#[command(name = "hemlockctl", version = hemlock_common::VERSION, about = "Hemlock operator CLI")]
struct Cli {
    /// Override the syncd endpoint (unix:/path or tcp:host:port).
    #[arg(long, global = true)]
    syncd: Option<String>,

    /// Override the pmon endpoint.
    #[arg(long, global = true)]
    pmon: Option<String>,

    /// Override the mgmtd endpoint.
    #[arg(long, global = true)]
    mgmtd: Option<String>,

    /// Run a POSIX shell command and exit. hemlockctl is the operator
    /// account's login shell; sshd invokes remote commands and the sftp
    /// subsystem as `<shell> -c <command>`, so this keeps `ssh host cmd`
    /// and scp/sftp working.
    #[arg(short = 'c', value_name = "COMMAND", hide = true)]
    shell_command: Option<String>,

    /// With no subcommand, hemlockctl starts the interactive CLI
    /// (operational mode).
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show daemon state.
    Show {
        #[command(subcommand)]
        command: ShowCommand,
    },

    /// Platform manifest tooling.
    Platform {
        #[command(subcommand)]
        command: PlatformCommand,
    },

    /// Print the login MOTD status block (backs /etc/update-motd.d/).
    ///
    /// Prints whatever data sources are reachable and always exits 0, so
    /// a dead daemon degrades the MOTD instead of breaking logins.
    Motd {
        /// Platform overlay directory holding platform.toml.
        #[arg(long, default_value = "/hemlock/platform")]
        platform_dir: String,
    },

    /// Load a config file into the candidate.
    Load { file: String },

    /// Print the candidate configuration as mgmtd holds it.
    Candidate,

    /// Commit the candidate configuration.
    Commit {
        /// Free-form commit comment, recorded in the rollback ring.
        #[arg(short = 'm', long, default_value = "")]
        comment: String,
        /// Auto-rollback unless `hemlockctl confirm` arrives within N seconds.
        #[arg(long)]
        confirm: Option<u32>,
    },

    /// Confirm a pending commit-confirm.
    Confirm,

    /// Load rollback N into the candidate and commit it.
    Rollback { revisions_back: u32 },

    /// List available rollback points.
    Rollbacks,

    /// Discard the candidate (reset to running).
    Discard,
}

#[derive(Subcommand)]
enum ShowCommand {
    /// Front-panel interfaces from syncd. Add `status` for the EOS-style
    /// summary table.
    Interfaces {
        #[arg(value_parser = ["status"])]
        mode: Option<String>,
    },
    /// Switch/ASIC summary from syncd.
    Switch,
    /// Fans, temperatures, PSUs from pmon.
    Environment,
    /// Transceiver inventory from pmon.
    Transceivers,
    /// The running configuration from mgmtd.
    Config,
}

#[derive(Subcommand)]
enum PlatformCommand {
    /// Validate a platform directory's manifest.
    Lint {
        /// Platform directory (e.g. platforms/cel-e1031) or platform id.
        platform: String,
        /// Root directory holding platforms, used when an id is given.
        #[arg(long, default_value = "platforms")]
        platforms_dir: String,
    },
}

fn endpoint(cli_override: &Option<String>, daemon: Daemon) -> anyhow::Result<IpcEndpoint> {
    Ok(match cli_override {
        Some(s) => s.parse()?,
        None => daemon.default_endpoint(),
    })
}

#[tokio::main]
async fn main() {
    hemlock_common::logging::init("warn");
    let cli = Cli::parse();

    let result = run(cli).await;
    if let Err(err) = result {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    if let Some(shell_command) = &cli.shell_command {
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(shell_command)
            .status()
            .map_err(|e| anyhow::anyhow!("running /bin/sh: {e}"))?;
        std::process::exit(status.code().unwrap_or(1));
    }
    let Some(command) = cli.command else {
        // No subcommand: interactive CLI (operational mode).
        return cli::run(cli::Endpoints {
            syncd: endpoint(&cli.syncd, Daemon::Syncd)?,
            pmon: endpoint(&cli.pmon, Daemon::Pmon)?,
            mgmtd: endpoint(&cli.mgmtd, Daemon::Mgmtd)?,
        })
        .await;
    };
    match command {
        Command::Show { command } => match command {
            ShowCommand::Interfaces { mode } => match mode.as_deref() {
                Some("status") => {
                    show::interfaces_status(endpoint(&cli.syncd, Daemon::Syncd)?).await
                }
                _ => show::interfaces(endpoint(&cli.syncd, Daemon::Syncd)?).await,
            },
            ShowCommand::Switch => show::switch(endpoint(&cli.syncd, Daemon::Syncd)?).await,
            ShowCommand::Environment => show::environment(endpoint(&cli.pmon, Daemon::Pmon)?).await,
            ShowCommand::Transceivers => {
                show::transceivers(endpoint(&cli.pmon, Daemon::Pmon)?).await
            }
            ShowCommand::Config => show::config(endpoint(&cli.mgmtd, Daemon::Mgmtd)?).await,
        },
        Command::Platform { command } => match command {
            PlatformCommand::Lint {
                platform,
                platforms_dir,
            } => platform::lint(&platforms_dir, &platform),
        },
        Command::Motd { platform_dir } => {
            motd::run(
                endpoint(&cli.syncd, Daemon::Syncd)?,
                endpoint(&cli.pmon, Daemon::Pmon)?,
                &platform_dir,
            )
            .await
        }
        Command::Load { file } => config::load(endpoint(&cli.mgmtd, Daemon::Mgmtd)?, &file).await,
        Command::Candidate => config::candidate(endpoint(&cli.mgmtd, Daemon::Mgmtd)?).await,
        Command::Commit { comment, confirm } => {
            config::commit(endpoint(&cli.mgmtd, Daemon::Mgmtd)?, &comment, confirm).await
        }
        Command::Confirm => config::confirm(endpoint(&cli.mgmtd, Daemon::Mgmtd)?).await,
        Command::Rollback { revisions_back } => {
            config::rollback(endpoint(&cli.mgmtd, Daemon::Mgmtd)?, revisions_back).await
        }
        Command::Rollbacks => config::rollbacks(endpoint(&cli.mgmtd, Daemon::Mgmtd)?).await,
        Command::Discard => config::discard(endpoint(&cli.mgmtd, Daemon::Mgmtd)?).await,
    }
}
