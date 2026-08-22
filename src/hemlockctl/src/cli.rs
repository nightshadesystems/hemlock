//! The Hemlock CLI: the interactive operator shell (`hemlockctl` with no
//! arguments — and, on a switch, the login shell).
//!
//! VyOS/Juniper-style syntax over Hemlock's candidate/commit engine:
//!
//! ```text
//! root@hemlock> show interfaces status
//! root@hemlock> configure
//! root@hemlock# set interfaces Ethernet1 description "uplink to core-1"
//! root@hemlock# set interfaces Eth1 admin-state disabled
//! root@hemlock# commit
//! root@hemlock# exit
//! ```
//!
//! Interface arguments accept aliases: `Eth1`, `eth1`, `e1` all mean
//! `Ethernet1`.
//!
//! `bash` drops to the Linux shell, so `sh` unambiguously abbreviates
//! `show`.
//!
//! Prompts follow the Nightshade convention: `user@hostname>` in
//! operational mode, `user@hostname#` in configuration mode. Config-mode
//! edits build the mgmtd *candidate*; nothing touches the ASIC until
//! `commit` (with `commit confirmed <secs>` for auto-rollback safety).

use std::sync::{Arc, Mutex};

use anyhow::Result;
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;
use hemlock_config::ConfigTree;
use rustyline::error::ReadlineError;

use crate::complete::{self, CliHelper, CliMode};
use crate::show;

#[derive(Clone)]
pub struct Endpoints {
    pub syncd: IpcEndpoint,
    pub pmon: IpcEndpoint,
    pub mgmtd: IpcEndpoint,
}

enum Mode {
    Operational,
    Config,
}

/// Match `input` against a command word set, EOS-style: unique prefixes
/// are accepted (`sh` -> `show`, `conf` -> `configure`).
pub(crate) fn resolve<'a>(input: &str, words: &[&'a str]) -> Result<&'a str, String> {
    if let Some(exact) = words.iter().find(|w| **w == input) {
        return Ok(exact);
    }
    let matches: Vec<&str> = words
        .iter()
        .copied()
        .filter(|w| w.starts_with(input))
        .collect();
    match matches.as_slice() {
        [only] => Ok(only),
        [] => Err(format!("% Invalid input: {input:?}")),
        many => Err(format!(
            "% Ambiguous command {input:?}: {}",
            many.join(", ")
        )),
    }
}

pub async fn run(endpoints: Endpoints) -> Result<()> {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "root".into());
    let hostname = read_hostname();

    let helper_state = Arc::new(Mutex::new(complete::State {
        mode: CliMode::Operational,
        ports: Vec::new(),
    }));
    let mut rl: rustyline::Editor<CliHelper, rustyline::history::DefaultHistory> =
        rustyline::Editor::new()?;
    rl.set_helper(Some(CliHelper {
        state: helper_state.clone(),
    }));

    // Keep the completer's interface-name cache fresh from syncd; a dead
    // or restarting syncd just means stale/no port completion, never an
    // error at the prompt.
    {
        let state = helper_state.clone();
        let syncd = endpoints.syncd.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(channel) = syncd.connect().await {
                    let mut client = pb::syncd_client::SyncdClient::new(channel);
                    if let Ok(response) = client.list_ports(pb::ListPortsRequest {}).await {
                        let mut names: Vec<String> = response
                            .into_inner()
                            .ports
                            .into_iter()
                            .map(|p| p.name)
                            .collect();
                        names.sort();
                        if let Ok(mut state) = state.lock() {
                            state.ports = names;
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        });
    }

    let mut mode = Mode::Operational;

    println!("Hemlock {} — type ? for help", hemlock_common::VERSION);
    loop {
        if let Ok(mut state) = helper_state.lock() {
            state.mode = match &mode {
                Mode::Operational => CliMode::Operational,
                Mode::Config => CliMode::Config,
            };
        }
        let prompt = match &mode {
            Mode::Operational => format!("{user}@{hostname}> "),
            Mode::Config => format!("{user}@{hostname}# "),
        };
        let line = tokio::task::block_in_place(|| rl.readline(&prompt));
        let line = match line {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => continue, // ^C clears the line
            Err(ReadlineError::Eof) => break,            // ^D exits
            Err(e) => return Err(e.into()),
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // EOS-style contextual help: a line ending in `?` lists what may
        // follow instead of executing (`show interfaces ?`, `show int?`).
        // A lone `?` still reaches the mode handlers' annotated help.
        if trimmed != "?" && trimmed.ends_with('?') {
            let body = &trimmed[..trimmed.len() - 1];
            let ends_mid_word = !body.is_empty() && !body.ends_with(char::is_whitespace);
            let mut tokens: Vec<&str> = body.split_whitespace().collect();
            let partial = if ends_mid_word {
                tokens.pop().unwrap_or("")
            } else {
                ""
            };
            let (cli_mode, ports) = match helper_state.lock() {
                Ok(state) => (state.mode, state.ports.clone()),
                Err(_) => (CliMode::Operational, Vec::new()),
            };
            let options = complete::candidates(cli_mode, &tokens, partial, &ports);
            if options.is_empty() {
                println!("  <cr>");
            } else {
                for option in options {
                    println!("  {option}");
                }
            }
            continue;
        }
        let _ = rl.add_history_entry(trimmed);
        let words: Vec<&str> = trimmed.split_whitespace().collect();

        let next = match &mode {
            Mode::Operational => operational(&endpoints, &words).await,
            Mode::Config => config(&endpoints, &words).await,
        };
        match next {
            Ok(Some(new_mode)) => mode = new_mode,
            Ok(None) => break,
            Err(message) => println!("{message}"),
        }
    }
    Ok(())
}

pub(crate) fn read_hostname() -> String {
    #[cfg(unix)]
    {
        for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
            if let Ok(name) = std::fs::read_to_string(path) {
                let name = name.trim();
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Ok(name) = std::env::var("COMPUTERNAME") {
            if !name.is_empty() {
                return name.to_lowercase();
            }
        }
    }
    "hemlock".into()
}

/// `Ok(Some(mode))` = keep going in `mode`; `Ok(None)` = leave the CLI;
/// `Err(text)` = print and stay.
type Step = std::result::Result<Option<Mode>, String>;

fn stay(mode: Mode) -> Step {
    Ok(Some(mode))
}

fn fail(err: anyhow::Error) -> Step {
    Err(fmt_err(err))
}

/// Render an error, translating a raw gRPC connect failure into the
/// operator-facing truth. The socket path in the IPC error identifies
/// which daemon; the cause separates "daemon down" from "socket exists
/// but this account may not open it" (not in the hemlock group).
pub(crate) fn fmt_err(err: anyhow::Error) -> String {
    let text = format!("{err:#}");
    if text.contains("ipc failure") {
        for daemon in ["syncd", "pmon", "mgmtd"] {
            if text.contains(&format!("/{daemon}.sock")) {
                if text.contains("Permission denied") {
                    return format!(
                        "% no permission on the {daemon} socket (is this account in the hemlock group?)"
                    );
                }
                return format!("% cannot reach {daemon} (is hemlock-{daemon}.service running?)");
            }
        }
    }
    format!("% {text}")
}

async fn operational(endpoints: &Endpoints, words: &[&str]) -> Step {
    // No separate "conf" entry: it is a unique prefix of "configure", so
    // `conf`, `conf t`, even `c` all resolve without an ambiguity error.
    const COMMANDS: &[&str] = &[
        "show",
        "configure",
        "bash",
        "exit",
        "quit",
        "logout",
        "help",
        "?",
    ];
    match resolve(words[0], COMMANDS)? {
        "show" => {
            show_command(endpoints, &words[1..]).await?;
            stay(Mode::Operational)
        }
        "configure" => {
            // Accept the EOS-habitual `configure terminal` / `conf t`.
            stay(Mode::Config)
        }
        "bash" => {
            spawn_shell();
            stay(Mode::Operational)
        }
        "exit" | "quit" | "logout" => Ok(None),
        "help" | "?" => {
            println!("Operational commands:");
            println!("  show interfaces [<name>] [<subcommand>] [| json]");
            println!("      subcommands: description, status, counters [errors|discards|rates|");
            println!("      queue|bins], transceiver [detail|properties|eeprom], capabilities,");
            println!("      flowcontrol, negotiation [detail], phy [detail], mac [detail],");
            println!("      switchport, trunk, vlans");
            println!("  show environment                       fans / temps / PSUs");
            println!(
                "  show configuration                     running configuration (config/conf ok)"
            );
            println!("  show version                           software / platform");
            println!("  configure | conf                       enter configuration mode");
            println!("  bash                                   drop to the Linux shell");
            println!("  exit                                   leave the CLI");
            stay(Mode::Operational)
        }
        _ => unreachable!(),
    }
}

/// Platform overlay directory for manifest-driven display (management
/// port name); overridable for tests and dev hosts.
fn platform_dir() -> String {
    std::env::var("HEMLOCK_PLATFORM_DIR").unwrap_or_else(|_| "/hemlock/platform".into())
}

async fn show_command(endpoints: &Endpoints, words: &[&str]) -> Result<(), String> {
    // "config" and "conf" are explicit aliases for "configuration". Being
    // list members (not just prefixes) they also make shorter stubs like
    // `show c` ambiguous — deliberately: only the spelled-out aliases work.
    const TOPICS: &[&str] = &[
        "interfaces",
        "environment",
        "configuration",
        "config",
        "conf",
        "version",
    ];
    const USAGE: &str = "show <interfaces|environment|configuration|version>";
    let Some(first) = words.first() else {
        return Err(format!("% Incomplete command: {USAGE}"));
    };
    if matches!(*first, "?" | "help") {
        println!("{USAGE}");
        return Ok(());
    }
    let run = async {
        match resolve(first, TOPICS)? {
            "interfaces" => {
                crate::interfaces::cmd::run(&endpoints.syncd, &endpoints.pmon, &words[1..]).await
            }
            "environment" => show::environment(endpoints.pmon.clone())
                .await
                .map_err(fmt_err),
            "configuration" | "config" | "conf" => show::configuration(
                endpoints.syncd.clone(),
                endpoints.mgmtd.clone(),
                &platform_dir(),
            )
            .await
            .map_err(fmt_err),
            "version" => {
                show::version(endpoints.syncd.clone()).await;
                Ok(())
            }
            _ => unreachable!(),
        }
    };
    run.await
}

async fn config(endpoints: &Endpoints, words: &[&str]) -> Step {
    const COMMANDS: &[&str] = &[
        "set", "delete", "show", "commit", "rollback", "discard", "exit", "help", "?",
    ];
    match resolve(words[0], COMMANDS)? {
        "set" => {
            config_edit(endpoints, &words[1..], /* delete = */ false).await?;
            stay(Mode::Config)
        }
        "delete" => {
            config_edit(endpoints, &words[1..], /* delete = */ true).await?;
            stay(Mode::Config)
        }
        "show" => {
            // The config session's view: the candidate.
            match candidate_text(endpoints).await {
                Ok(text) if text.trim().is_empty() => {
                    println!("% candidate configuration is empty");
                    stay(Mode::Config)
                }
                Ok(text) => {
                    print!("{text}");
                    stay(Mode::Config)
                }
                Err(e) => fail(e),
            }
        }
        "commit" => {
            let confirm = match words.get(1) {
                Some(w) if resolve(w, &["confirmed"]).is_ok() => match words.get(2) {
                    Some(secs) => match secs.parse::<u32>() {
                        Ok(secs) if secs > 0 => Some(secs),
                        _ => return Err("% Usage: commit confirmed <seconds>".into()),
                    },
                    None => return Err("% Usage: commit confirmed <seconds>".into()),
                },
                Some(other) => return Err(format!("% Invalid input: {other:?}")),
                None => None,
            };
            match commit(endpoints, confirm).await {
                Ok(()) => stay(Mode::Config),
                Err(e) => fail(e),
            }
        }
        "rollback" => {
            let Some(Ok(n)) = words.get(1).map(|w| w.parse::<u32>()) else {
                return Err("% Usage: rollback <n>  (1 = previous running config)".into());
            };
            match rollback_to_candidate(endpoints, n).await {
                Ok(()) => {
                    println!("rollback {n} loaded into candidate — review with `show`, apply with `commit`");
                    stay(Mode::Config)
                }
                Err(e) => fail(e),
            }
        }
        "discard" => match discard(endpoints).await {
            Ok(()) => {
                println!("candidate discarded");
                stay(Mode::Config)
            }
            Err(e) => fail(e),
        },
        "exit" => stay(Mode::Operational),
        "help" | "?" => {
            println!("Configuration commands (edit the candidate; `commit` applies):");
            println!("  set interfaces <port> description <text>");
            println!("  set interfaces <port> admin-state <enabled|disabled>");
            println!("  delete interfaces <port> [description|admin-state]");
            println!("  show                      show the candidate configuration");
            println!(
                "  commit [confirmed <s>]    apply the candidate (auto-rollback unless confirmed)"
            );
            println!("  rollback <n>              load rollback n into the candidate");
            println!("  discard                   reset candidate to running");
            println!("  exit                      back to operational mode");
            println!("Ports accept aliases: Eth1 / eth1 / e1 all mean Ethernet1.");
            stay(Mode::Config)
        }
        _ => unreachable!(),
    }
}

/// Shared body of `set` and `delete`: resolve the path, canonicalize the
/// interface name, apply the edit to the candidate.
async fn config_edit(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || format!("% Usage: {verb} interfaces <port> [description|admin-state ...]");
    let Some(top) = words.first() else {
        return Err(usage());
    };
    resolve(top, &["interfaces"])?;
    let Some(raw_port) = words.get(1) else {
        return Err(usage());
    };
    let known = list_port_names(endpoints).await.map_err(fmt_err)?;
    let port = canonical_port(raw_port, &known)?;
    let rest = &words[2..];

    if rest.is_empty() {
        return if delete {
            // Delete the whole interface node; commit reverts the port to
            // defaults.
            edit_config(endpoints, |tree| {
                let interfaces = tree.block_mut("interfaces");
                ConfigTree::remove_block(interfaces, &port, &[]);
            })
            .await
            .map_err(fmt_err)
        } else {
            // Bare `set interfaces <port>`: create the (empty) node.
            edit_interface(endpoints, &port, |_| {})
                .await
                .map_err(fmt_err)
        };
    }

    match resolve(rest[0], &["description", "admin-state"])? {
        "description" => {
            if delete {
                edit_interface(endpoints, &port, |eth| {
                    ConfigTree::remove_leaf(eth, "description");
                })
                .await
            } else {
                let text = rest[1..].join(" ");
                if text.is_empty() {
                    return Err(format!("% Usage: set interfaces {port} description <text>"));
                }
                edit_interface(endpoints, &port, |eth| {
                    ConfigTree::set_leaf(eth, "description", vec![text]);
                })
                .await
            }
        }
        "admin-state" => {
            if delete {
                edit_interface(endpoints, &port, |eth| {
                    ConfigTree::remove_leaf(eth, "admin-state");
                })
                .await
            } else {
                let Some(value) = rest.get(1) else {
                    return Err(format!(
                        "% Usage: set interfaces {port} admin-state <enabled|disabled>"
                    ));
                };
                let value = resolve(value, &["enabled", "disabled"])?.to_string();
                edit_interface(endpoints, &port, |eth| {
                    ConfigTree::set_leaf(eth, "admin-state", vec![value]);
                })
                .await
            }
        }
        _ => unreachable!(),
    }
    .map_err(fmt_err)
}

/// Canonical interface name from user input (exact, alias like `Eth1`,
/// or unique prefix), validated against syncd's port list.
fn canonical_port(input: &str, known: &[String]) -> Result<String, String> {
    match complete::match_port(input, known) {
        complete::PortMatch::One(name) => Ok(name),
        complete::PortMatch::NoMatch => Err(format!("% No such interface {input:?}")),
        complete::PortMatch::Ambiguous(hits) => Err(format!(
            "% Ambiguous interface {input:?}: {}",
            hits.join(", ")
        )),
    }
}

// --- mgmtd/syncd plumbing ---------------------------------------------------

async fn mgmt_client(
    endpoints: &Endpoints,
) -> Result<pb::mgmt_client::MgmtClient<tonic::transport::Channel>> {
    Ok(pb::mgmt_client::MgmtClient::new(
        endpoints.mgmtd.connect().await?,
    ))
}

async fn list_port_names(endpoints: &Endpoints) -> Result<Vec<String>> {
    let mut client = pb::syncd_client::SyncdClient::new(endpoints.syncd.connect().await?);
    Ok(client
        .list_ports(pb::ListPortsRequest {})
        .await?
        .into_inner()
        .ports
        .into_iter()
        .map(|p| p.name)
        .collect())
}

async fn candidate_text(endpoints: &Endpoints) -> Result<String> {
    let mut client = mgmt_client(endpoints).await?;
    Ok(client
        .get_config(pb::GetConfigRequest {
            source: pb::ConfigSource::Candidate as i32,
        })
        .await?
        .into_inner()
        .text)
}

/// Fetch the candidate, apply `edit` to the tree, push it back. Each
/// config command round-trips so the candidate in mgmtd is always the
/// truth. Legacy `ethernet <name>` blocks are normalized to the current
/// name-as-block form on the way through.
async fn edit_config(endpoints: &Endpoints, edit: impl FnOnce(&mut ConfigTree)) -> Result<()> {
    let text = candidate_text(endpoints).await?;
    let mut tree =
        hemlock_config::parse(&text).map_err(|e| anyhow::anyhow!("candidate unparsable: {e}"))?;
    tree.normalize_interfaces();
    edit(&mut tree);
    let mut client = mgmt_client(endpoints).await?;
    let response = client
        .set_candidate(pb::ConfigText {
            text: tree.to_text(),
        })
        .await?
        .into_inner();
    if response.valid {
        Ok(())
    } else {
        anyhow::bail!("candidate rejected: {}", response.errors.join("; "))
    }
}

/// [`edit_config`] scoped to one interface's block (`interfaces {
/// <port> { ... } }`), creating it if absent.
async fn edit_interface(
    endpoints: &Endpoints,
    port: &str,
    edit: impl FnOnce(&mut Vec<hemlock_config::Item>),
) -> Result<()> {
    edit_config(endpoints, |tree| {
        let interfaces = tree.block_mut("interfaces");
        edit(ConfigTree::ensure_block(interfaces, port, &[]));
    })
    .await
}

async fn commit(endpoints: &Endpoints, confirm: Option<u32>) -> Result<()> {
    let mut client = mgmt_client(endpoints).await?;
    let response = client
        .commit(pb::CommitRequest {
            comment: String::new(),
            confirm_timeout_secs: confirm.unwrap_or(0),
        })
        .await?
        .into_inner();
    println!("commit {} applied", response.commit_id);
    for change in &response.applied_changes {
        println!("  {change}");
    }
    if let Some(secs) = confirm {
        println!("commit-confirm armed: `commit` again or confirm within {secs}s or the config rolls back");
        println!("(confirm with: hemlockctl confirm, or another `commit`)");
    }
    Ok(())
}

async fn rollback_to_candidate(endpoints: &Endpoints, n: u32) -> Result<()> {
    let mut client = mgmt_client(endpoints).await?;
    client
        .rollback(pb::RollbackRequest { revisions_back: n })
        .await?;
    Ok(())
}

async fn discard(endpoints: &Endpoints) -> Result<()> {
    let mut client = mgmt_client(endpoints).await?;
    client.discard(pb::DiscardRequest {}).await?;
    Ok(())
}

fn spawn_shell() {
    #[cfg(unix)]
    let (program, args): (String, Vec<&str>) = {
        // On a switch hemlockctl IS the login shell, so $SHELL points
        // back at us; spawning it would just nest another CLI.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        let shell = if shell.ends_with("hemlockctl") {
            "/bin/bash".into()
        } else {
            shell
        };
        (shell, vec!["-l"])
    };
    #[cfg(not(unix))]
    let (program, args): (String, Vec<&str>) = ("powershell".into(), vec![]);

    println!("(type `exit` to return to the Hemlock CLI)");
    match std::process::Command::new(&program).args(&args).status() {
        Ok(_) => {}
        Err(e) => println!("% cannot start {program}: {e}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn prefix_resolution_is_eos_like() {
        let words = &["show", "bash", "configure", "conf", "exit"];
        assert_eq!(resolve("sh", words).unwrap(), "show");
        assert_eq!(resolve("b", words).unwrap(), "bash");
        assert_eq!(resolve("e", words).unwrap(), "exit");
        assert_eq!(resolve("conf", words).unwrap(), "conf"); // exact beats prefix
        assert!(resolve("c", words).is_err()); // ambiguous: configure/conf
        assert!(resolve("zz", words).is_err());
    }

    #[test]
    fn show_topic_aliases_are_explicit_not_prefixes() {
        // Mirrors show_command's TOPICS: config/conf are deliberate
        // aliases, while bare stubs like "c" stay ambiguous errors.
        let topics = &[
            "interfaces",
            "environment",
            "configuration",
            "config",
            "conf",
            "version",
        ];
        assert_eq!(resolve("configuration", topics).unwrap(), "configuration");
        assert_eq!(resolve("config", topics).unwrap(), "config");
        assert_eq!(resolve("conf", topics).unwrap(), "conf");
        assert!(resolve("c", topics).is_err());
        assert!(resolve("co", topics).is_err());
        assert!(resolve("con", topics).is_err());
    }
}
