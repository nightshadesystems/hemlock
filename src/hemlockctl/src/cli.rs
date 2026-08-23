//! The Hemlock CLI: the interactive operator shell (`hemlockctl` with no
//! arguments — and, on a switch, the login shell).
//!
//! VyOS/Juniper-style syntax over Hemlock's candidate/commit engine:
//!
//! ```text
//! root@hemlock> show interfaces status
//! root@hemlock> configure
//! root@hemlock# set interfaces Ethernet1 description "uplink to core-1"
//! root@hemlock# set interfaces Eth1 shutdown
//! root@hemlock# set interfaces Management1 address 10.42.10.9/24
//! root@hemlock# set vlans vlan 10 description "Management"
//! root@hemlock# set interfaces Ethernet1 switchport mode trunk
//! root@hemlock# set interfaces Ethernet1 switchport trunk vlans 10,20
//! root@hemlock# set system ssh authentication local
//! root@hemlock# set routing static 0.0.0.0/0 10.42.10.1
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
    // error at the prompt. The management port is an OS netdev, not a
    // syncd port, so it joins the cache from the manifest.
    {
        let state = helper_state.clone();
        let syncd = endpoints.syncd.clone();
        let management = management_interface();
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
                        names.push(management.clone());
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
            let options = complete::help_candidates(cli_mode, &tokens, partial, &ports);
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
/// but this account may not open it" (not in the hemlock group), and
/// systemd's unit state separates "still initializing" from "dead".
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
                return match unit_active_state(daemon).as_deref() {
                    Some("activating") => format!(
                        "% {daemon} is still initializing — try again in a moment"
                    ),
                    Some("failed") => format!(
                        "% {daemon} failed (see: journalctl -u hemlock-{daemon})"
                    ),
                    Some("inactive") => {
                        format!("% {daemon} is not running (hemlock-{daemon}.service is inactive)")
                    }
                    _ => {
                        format!("% cannot reach {daemon} (is hemlock-{daemon}.service running?)")
                    }
                };
            }
        }
    }
    format!("% {text}")
}

/// The systemd ActiveState of `hemlock-<daemon>.service`, when systemctl
/// is available (always on a switch; None on dev hosts).
fn unit_active_state(daemon: &str) -> Option<String> {
    let output = std::process::Command::new("systemctl")
        .args([
            "show",
            "--property=ActiveState",
            "--value",
            &format!("hemlock-{daemon}.service"),
        ])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
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
                    crate::pager::page(&text);
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
            println!("  set interfaces <port> shutdown | no-shutdown");
            println!("  set interfaces <port> address <ip/prefix>     puts the port in L3 mode");
            println!("  set interfaces Vlan<id> address <ip/prefix>   SVI (in-band management)");
            println!("  set interfaces <port> switchport mode <access|trunk>");
            println!("  set interfaces <port> switchport access vlan <id>");
            println!("  set interfaces <port> switchport trunk vlans <list>   e.g. 10,20,30-32");
            println!("  set interfaces <port> switchport trunk native vlan <id>");
            println!("  set vlans vlan <id> [description <text>]");
            println!("  set system ssh                                enable the SSH server");
            println!("  set system ssh authentication local           password logins (PAM)");
            println!("  set routing static <prefix> <next-hop>        static route");
            println!("  delete interfaces <port> [description|shutdown|no-shutdown|address|switchport ...]");
            println!("  delete vlans vlan <id> [description]");
            println!("  delete system ssh [authentication]");
            println!("  delete routing [static [<prefix>]]");
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

/// Shared body of `set` and `delete`: dispatch on the top-level config
/// noun.
async fn config_edit(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let Some(top) = words.first() else {
        return Err(format!(
            "% Usage: {verb} <interfaces|system|routing|vlans> ..."
        ));
    };
    match resolve(top, &["interfaces", "system", "routing", "vlans"])? {
        "interfaces" => config_interfaces(endpoints, &words[1..], delete).await,
        "system" => config_system(endpoints, &words[1..], delete).await,
        "routing" => config_routing(endpoints, &words[1..], delete).await,
        "vlans" => config_vlans(endpoints, &words[1..], delete).await,
        _ => unreachable!(),
    }
}

/// `set|delete vlans vlan <id> [description <text>]`.
async fn config_vlans(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || format!("% Usage: {verb} vlans vlan <id> [description <text>]");
    if delete && words.is_empty() {
        return edit_config(endpoints, |tree| {
            ConfigTree::remove_block(&mut tree.items, "vlans", &[]);
        })
        .await
        .map_err(fmt_err);
    }
    let Some(first) = words.first() else {
        return Err(usage());
    };
    resolve(first, &["vlan"])?;
    let Some(raw_id) = words.get(1) else {
        return Err(usage());
    };
    let Some(id) = raw_id
        .parse::<u16>()
        .ok()
        .filter(|id| (1..=4094).contains(id))
    else {
        return Err(format!("% bad VLAN id {raw_id:?} (1..4094)"));
    };
    let id = id.to_string();
    let rest = &words[2..];

    if rest.is_empty() {
        return if delete {
            edit_config(endpoints, |tree| {
                let vlans = tree.block_mut("vlans");
                ConfigTree::remove_block(vlans, "vlan", &[&id]);
                remove_block_if_empty(tree, "vlans");
            })
            .await
        } else {
            edit_config(endpoints, |tree| {
                let vlans = tree.block_mut("vlans");
                ConfigTree::ensure_block(vlans, "vlan", &[&id]);
            })
            .await
        }
        .map_err(fmt_err);
    }

    match resolve(rest[0], &["description"])? {
        "description" => {
            if delete {
                edit_config(endpoints, |tree| {
                    let vlans = tree.block_mut("vlans");
                    if let Some(vlan) = keyed_block_children_mut(vlans, "vlan", &id) {
                        ConfigTree::remove_leaf(vlan, "description");
                    }
                })
                .await
            } else {
                let text = rest[1..].join(" ");
                if text.is_empty() {
                    return Err(format!("% Usage: set vlans vlan {id} description <text>"));
                }
                edit_config(endpoints, |tree| {
                    let vlans = tree.block_mut("vlans");
                    let vlan = ConfigTree::ensure_block(vlans, "vlan", &[&id]);
                    ConfigTree::set_leaf(vlan, "description", vec![text]);
                })
                .await
            }
        }
        _ => unreachable!(),
    }
    .map_err(fmt_err)
}

/// `set|delete interfaces <port> ...` — the interface name is
/// canonicalized against syncd's ports plus the manifest's management
/// port (an OS netdev, so it never appears in syncd's list).
async fn config_interfaces(
    endpoints: &Endpoints,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || {
        format!(
            "% Usage: {verb} interfaces <port> [description|shutdown|no-shutdown|address|switchport ...]"
        )
    };
    let Some(raw_port) = words.first() else {
        return Err(usage());
    };
    let port = match vlan_interface(raw_port) {
        // SVIs (`Vlan10`) are config-defined, not syncd ports.
        Some(name) => name,
        None => {
            let mut known = list_port_names(endpoints).await.map_err(fmt_err)?;
            known.push(management_interface());
            canonical_port(raw_port, &known)?
        }
    };
    let rest = &words[1..];

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

    let subcommand = resolve(
        rest[0],
        &[
            "description",
            "shutdown",
            "no-shutdown",
            "address",
            "switchport",
        ],
    )?;
    // Phase-1 SVIs carry an address and nothing else.
    if port.starts_with("Vlan") && subcommand != "address" {
        return Err(format!("% {subcommand} is not supported on VLAN interfaces"));
    }
    match subcommand {
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
        marker @ ("shutdown" | "no-shutdown") => {
            // Stored as `shutdown` / `no shutdown` (the candidate is
            // normalized before editing, so only those forms exist).
            let shutdown = marker == "shutdown";
            edit_interface(endpoints, &port, move |eth| {
                if delete {
                    if shutdown {
                        ConfigTree::remove_leaf(eth, "shutdown");
                    } else {
                        ConfigTree::remove_leaf(eth, "no");
                    }
                } else if shutdown {
                    ConfigTree::set_leaf(eth, "shutdown", vec![]);
                    ConfigTree::remove_leaf(eth, "no");
                } else {
                    ConfigTree::set_phrase(eth, "no", "shutdown", vec![]);
                    ConfigTree::remove_leaf(eth, "shutdown");
                }
            })
            .await
        }
        "switchport" => {
            return config_switchport(endpoints, &port, &rest[1..], delete).await;
        }
        "address" => {
            if delete {
                edit_interface(endpoints, &port, |eth| {
                    ConfigTree::remove_leaf(eth, "address");
                })
                .await
            } else {
                let Some(value) = rest.get(1) else {
                    return Err(format!(
                        "% Usage: set interfaces {port} address <ip/prefix-length>"
                    ));
                };
                if let Err(e) = hemlock_common::net::parse_cidr(value) {
                    return Err(format!("% {e}"));
                }
                let value = value.to_string();
                edit_interface(endpoints, &port, |eth| {
                    ConfigTree::set_leaf(eth, "address", vec![value]);
                })
                .await
            }
        }
        _ => unreachable!(),
    }
    .map_err(fmt_err)
}

/// `set|delete interfaces <port> switchport ...`: mode, access vlan,
/// trunk vlans, trunk native vlan. Setting `mode trunk` auto-deletes
/// the access-vlan entry (a trunk carries no access VLAN).
async fn config_switchport(
    endpoints: &Endpoints,
    port: &str,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || {
        format!(
            "% Usage: {verb} interfaces <port> switchport [mode <access|trunk> | access vlan <id> | trunk vlans <list> | trunk native vlan <id>]"
        )
    };
    if port.starts_with("Management") {
        return Err("% switchport is not supported on management ports".into());
    }

    if words.is_empty() {
        return if delete {
            edit_interface(endpoints, port, |eth| {
                ConfigTree::remove_block(eth, "switchport", &[]);
            })
            .await
        } else {
            // Bare `switchport`: explicit default L2 (access, VLAN 1).
            edit_interface(endpoints, port, |eth| {
                ConfigTree::ensure_block(eth, "switchport", &[]);
            })
            .await
        }
        .map_err(fmt_err);
    }

    match resolve(words[0], &["mode", "access", "trunk"])? {
        "mode" => {
            if delete {
                edit_interface(endpoints, port, |eth| {
                    if let Some(sp) = block_children_mut(eth, "switchport") {
                        ConfigTree::remove_leaf(sp, "mode");
                    }
                })
                .await
            } else {
                let Some(value) = words.get(1) else {
                    return Err(usage());
                };
                let value = resolve(value, &["access", "trunk"])?.to_string();
                let trunk = value == "trunk";
                edit_interface(endpoints, port, move |eth| {
                    let sp = ConfigTree::ensure_block(eth, "switchport", &[]);
                    ConfigTree::set_leaf(sp, "mode", vec![value]);
                    if trunk {
                        // A trunk carries no access VLAN.
                        ConfigTree::remove_leaf(sp, "access");
                    }
                })
                .await
            }
        }
        "access" => {
            if delete {
                edit_interface(endpoints, port, |eth| {
                    if let Some(sp) = block_children_mut(eth, "switchport") {
                        ConfigTree::remove_leaf(sp, "access");
                    }
                })
                .await
            } else {
                let Some(keyword) = words.get(1) else {
                    return Err(usage());
                };
                resolve(keyword, &["vlan"])?;
                let Some(raw) = words.get(2) else {
                    return Err(usage());
                };
                let id = parse_vlan_arg(raw)?;
                edit_interface(endpoints, port, move |eth| {
                    let sp = ConfigTree::ensure_block(eth, "switchport", &[]);
                    ConfigTree::set_phrase(sp, "access", "vlan", vec![id]);
                })
                .await
            }
        }
        "trunk" => {
            let Some(sub) = words.get(1) else {
                return Err(usage());
            };
            match resolve(sub, &["vlans", "native"])? {
                "vlans" => {
                    if delete {
                        edit_interface(endpoints, port, |eth| {
                            if let Some(sp) = block_children_mut(eth, "switchport") {
                                ConfigTree::remove_leaf(sp, "trunk");
                            }
                        })
                        .await
                    } else {
                        let Some(list) = words.get(2) else {
                            return Err(usage());
                        };
                        let vlans = parse_vlan_list(list)?;
                        edit_interface(endpoints, port, move |eth| {
                            let sp = ConfigTree::ensure_block(eth, "switchport", &[]);
                            ConfigTree::set_phrase(sp, "trunk", "vlans", vlans);
                        })
                        .await
                    }
                }
                "native" => {
                    if delete {
                        edit_interface(endpoints, port, |eth| {
                            if let Some(sp) = block_children_mut(eth, "switchport") {
                                ConfigTree::remove_leaf(sp, "native");
                            }
                        })
                        .await
                    } else {
                        let Some(keyword) = words.get(2) else {
                            return Err(usage());
                        };
                        resolve(keyword, &["vlan"])?;
                        let Some(raw) = words.get(3) else {
                            return Err(usage());
                        };
                        let id = parse_vlan_arg(raw)?;
                        edit_interface(endpoints, port, move |eth| {
                            let sp = ConfigTree::ensure_block(eth, "switchport", &[]);
                            ConfigTree::set_phrase(sp, "native", "vlan", vec![id]);
                        })
                        .await
                    }
                }
                _ => unreachable!(),
            }
        }
        _ => unreachable!(),
    }
    .map_err(fmt_err)
}

/// Canonical SVI name from user input: `vlan10`, `Vl10`, `v10` all mean
/// `Vlan10`. `None` when the input is not a VLAN interface form.
fn vlan_interface(input: &str) -> Option<String> {
    let digit_at = input
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(input.len());
    let (alpha, digits) = input.split_at(digit_at);
    if alpha.is_empty() || digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !"vlan".starts_with(&alpha.to_ascii_lowercase()) {
        return None;
    }
    digits
        .parse::<u16>()
        .ok()
        .filter(|id| (1..=4094).contains(id))
        .map(|id| format!("Vlan{id}"))
}

/// A CLI VLAN id argument, validated and canonicalized.
fn parse_vlan_arg(text: &str) -> Result<String, String> {
    text.parse::<u16>()
        .ok()
        .filter(|id| (1..=4094).contains(id))
        .map(|id| id.to_string())
        .ok_or_else(|| format!("% bad VLAN id {text:?} (1..4094)"))
}

/// A trunk VLAN list: comma-separated ids and ranges (`10,20,30-32`),
/// expanded, deduplicated, and sorted.
fn parse_vlan_list(text: &str) -> Result<Vec<String>, String> {
    let one = |t: &str| {
        t.parse::<u16>()
            .ok()
            .filter(|id| (1..=4094).contains(id))
            .ok_or_else(|| format!("% bad VLAN id {t:?} (1..4094)"))
    };
    let mut out = std::collections::BTreeSet::new();
    for part in text.split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((from, to)) => {
                let (from, to) = (one(from)?, one(to)?);
                if from > to {
                    return Err(format!("% bad VLAN range {part:?}"));
                }
                out.extend(from..=to);
            }
            None => {
                out.insert(one(part)?);
            }
        }
    }
    if out.is_empty() {
        return Err("% empty VLAN list".into());
    }
    Ok(out.into_iter().map(|id| id.to_string()).collect())
}

/// `set|delete system ssh [authentication local]` — SSH is on exactly
/// when the `system { ssh }` block exists; commit applies it.
async fn config_system(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || format!("% Usage: {verb} system ssh [authentication local]");
    let Some(first) = words.first() else {
        return Err(usage());
    };
    resolve(first, &["ssh"])?;
    let rest = &words[1..];

    if rest.is_empty() {
        return if delete {
            edit_config(endpoints, |tree| {
                let system = tree.block_mut("system");
                ConfigTree::remove_block(system, "ssh", &[]);
                remove_block_if_empty(tree, "system");
            })
            .await
        } else {
            edit_config(endpoints, |tree| {
                let system = tree.block_mut("system");
                ConfigTree::ensure_block(system, "ssh", &[]);
            })
            .await
        }
        .map_err(fmt_err);
    }

    match resolve(rest[0], &["authentication"])? {
        "authentication" => {
            if delete {
                edit_config(endpoints, |tree| {
                    let system = tree.block_mut("system");
                    if let Some(ssh) = block_children_mut(system, "ssh") {
                        ConfigTree::remove_leaf(ssh, "authentication");
                    }
                })
                .await
            } else {
                let Some(value) = rest.get(1) else {
                    return Err(usage());
                };
                let value = resolve(value, &["local"])?.to_string();
                // Setting authentication implies turning SSH on.
                edit_config(endpoints, |tree| {
                    let system = tree.block_mut("system");
                    let ssh = ConfigTree::ensure_block(system, "ssh", &[]);
                    ConfigTree::set_leaf(ssh, "authentication", vec![value]);
                })
                .await
            }
        }
        _ => unreachable!(),
    }
    .map_err(fmt_err)
}

/// `set routing static <prefix> <next-hop>` and its deletes. Prefixes
/// are canonicalized (host bits cleared) before they enter the config.
async fn config_routing(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    if delete && words.is_empty() {
        return edit_config(endpoints, |tree| {
            ConfigTree::remove_block(&mut tree.items, "routing", &[]);
        })
        .await
        .map_err(fmt_err);
    }
    let usage = move || {
        if delete {
            "% Usage: delete routing [static [<prefix>]]".to_string()
        } else {
            "% Usage: set routing static <prefix> <next-hop>".to_string()
        }
    };
    let Some(first) = words.first() else {
        return Err(usage());
    };
    resolve(first, &["static"])?;
    let rest = &words[1..];

    if delete {
        match rest.first() {
            None => edit_config(endpoints, |tree| {
                let routing = tree.block_mut("routing");
                ConfigTree::remove_block(routing, "static", &[]);
                remove_block_if_empty(tree, "routing");
            })
            .await
            .map_err(fmt_err),
            Some(prefix) => {
                let prefix =
                    hemlock_common::net::canonical_prefix(prefix).map_err(|e| format!("% {e}"))?;
                edit_config(endpoints, |tree| {
                    let routing = tree.block_mut("routing");
                    if let Some(routes) = block_children_mut(routing, "static") {
                        ConfigTree::remove_leaf(routes, &prefix);
                        if routes.is_empty() {
                            ConfigTree::remove_block(routing, "static", &[]);
                        }
                    }
                    remove_block_if_empty(tree, "routing");
                })
                .await
                .map_err(fmt_err)
            }
        }
    } else {
        let (Some(prefix), Some(next_hop)) = (rest.first(), rest.get(1)) else {
            return Err(usage());
        };
        let prefix =
            hemlock_common::net::validate_route(prefix, next_hop).map_err(|e| format!("% {e}"))?;
        let next_hop = next_hop.to_string();
        edit_config(endpoints, |tree| {
            let routing = tree.block_mut("routing");
            let routes = ConfigTree::ensure_block(routing, "static", &[]);
            ConfigTree::set_leaf(routes, &prefix, vec![next_hop]);
        })
        .await
        .map_err(fmt_err)
    }
}

/// Mutable children of an existing block among `items` (no creation —
/// the delete paths must not conjure the block they are deleting from).
fn block_children_mut<'a>(
    items: &'a mut [hemlock_config::Item],
    name: &str,
) -> Option<&'a mut Vec<hemlock_config::Item>> {
    items.iter_mut().find_map(|item| match item {
        hemlock_config::Item::Block {
            name: n, children, ..
        } if n == name => Some(children),
        _ => None,
    })
}

/// [`block_children_mut`] for a keyed block (`vlan 10 { ... }`).
fn keyed_block_children_mut<'a>(
    items: &'a mut [hemlock_config::Item],
    name: &str,
    key: &str,
) -> Option<&'a mut Vec<hemlock_config::Item>> {
    items.iter_mut().find_map(|item| match item {
        hemlock_config::Item::Block {
            name: n,
            keys,
            children,
        } if n == name && keys.len() == 1 && keys[0] == key => Some(children),
        _ => None,
    })
}

/// Drop a top-level block that a delete left empty, so the stored
/// config doesn't accumulate `system { }` / `routing { }` husks.
fn remove_block_if_empty(tree: &mut ConfigTree, name: &str) {
    if tree
        .block(name)
        .is_some_and(|(_, children)| children.is_empty())
    {
        ConfigTree::remove_block(&mut tree.items, name, &[]);
    }
}

/// The management interface name from the platform manifest
/// ("Management1" off-manifest): an OS netdev, so it never appears in
/// syncd's port list but is configurable like a port.
fn management_interface() -> String {
    hemlock_platform::Platform::find("/", &platform_dir())
        .ok()
        .and_then(|p| p.manifest.management.map(|m| m.interface))
        .unwrap_or_else(|| "Management1".into())
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
    fn vlan_interface_names_canonicalize() {
        assert_eq!(vlan_interface("Vlan10"), Some("Vlan10".into()));
        assert_eq!(vlan_interface("vlan1"), Some("Vlan1".into()));
        assert_eq!(vlan_interface("Vl10"), Some("Vlan10".into()));
        assert_eq!(vlan_interface("v4094"), Some("Vlan4094".into()));
        assert_eq!(vlan_interface("v0"), None);
        assert_eq!(vlan_interface("v5000"), None);
        assert_eq!(vlan_interface("Ethernet1"), None);
        assert_eq!(vlan_interface("e1"), None);
        assert_eq!(vlan_interface("ma1"), None);
        assert_eq!(vlan_interface("vlan"), None);
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
