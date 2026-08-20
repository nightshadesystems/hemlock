//! hemlockctl — the Hemlock operator CLI.
//!
//! Talks to the Hemlock daemons over their gRPC endpoints: syncd/pmon for
//! `show ...` state, mgmtd for the config lifecycle (`load`, `commit`,
//! `rollback`, ...), plus offline platform tooling.

use clap::{Parser, Subcommand};
use hemlock_common::ipc::{Daemon, IpcEndpoint};

mod config;
mod platform;
mod show;

#[derive(Parser)]
#[command(name = "hemlockctl", version, about = "Hemlock operator CLI")]
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

    #[command(subcommand)]
    command: Command,
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
    /// Front-panel interfaces from syncd.
    Interfaces,
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
    match cli.command {
        Command::Show { command } => match command {
            ShowCommand::Interfaces => show::interfaces(endpoint(&cli.syncd, Daemon::Syncd)?).await,
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
