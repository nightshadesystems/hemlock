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
    pub orch: IpcEndpoint,
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
        for daemon in ["syncd", "pmon", "mgmtd", "orch"] {
            if text.contains(&format!("/{daemon}.sock")) {
                if text.contains("Permission denied") {
                    return format!(
                        "% no permission on the {daemon} socket (is this account in the hemlock group?)"
                    );
                }
                return match unit_active_state(daemon).as_deref() {
                    Some("activating") => {
                        format!("% {daemon} is still initializing — try again in a moment")
                    }
                    Some("failed") => {
                        format!("% {daemon} failed (see: journalctl -u hemlock-{daemon})")
                    }
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
        "clear",
        "upgrade",
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
        "clear" => {
            crate::switching::cmd::clear(&endpoints.syncd, &words[1..]).await?;
            stay(Mode::Operational)
        }
        "upgrade" => {
            upgrade_command(endpoints, &words[1..]).await?;
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
            println!("  show vlan [id <set>|summary]           VLAN table");
            println!("  show mac address-table [...]           MAC table (count, aging-time, filters)");
            println!("  show storm-control                     storm-control levels and drops");
            println!("  show mirror                            mirror sessions (monitor session ok)");
            println!("  clear counters [<interface>]           baseline interface counters");
            println!("  clear mac-table [vlan <id>] [interface <port>]   flush dynamic MACs");
            println!("  configure | conf                       enter configuration mode");
            println!("  upgrade <image.bin> [force] [reboot]   install an OS image (via mgmtd)");
            println!("  bash                                   drop to the Linux shell");
            println!("  exit                                   leave the CLI");
            stay(Mode::Operational)
        }
        _ => unreachable!(),
    }
}

/// `upgrade <image.bin> [force] [reboot]` — install an OS image over
/// the running system through mgmtd's InstallImage RPC. Nothing changes
/// until the next reboot unless `reboot` is given, so the plain form is
/// safe to run in production hours.
async fn upgrade_command(endpoints: &Endpoints, words: &[&str]) -> Result<(), String> {
    const USAGE: &str = "upgrade <image.bin> [force] [reboot]";
    let Some((path, rest)) = words.split_first() else {
        return Err(format!("% Incomplete command: {USAGE}"));
    };
    if matches!(*path, "?" | "help") {
        println!("{USAGE}");
        return Ok(());
    }
    let mut force = false;
    let mut reboot = false;
    for word in rest {
        match resolve(word, &["force", "reboot"])? {
            "force" => force = true,
            "reboot" => reboot = true,
            _ => unreachable!(),
        }
    }
    crate::upgrade::run(endpoints.mgmtd.clone(), path, force, reboot)
        .await
        .map_err(fmt_err)
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
        "vlan",
        "mac",
        "storm-control",
        "mirror",
        "monitor",
        "port-channel",
        "lacp",
        "spanning-tree",
        "igmp",
        "mld",
    ];
    const USAGE: &str = "show <interfaces|environment|configuration|version|vlan|mac address-table|storm-control|mirror|port-channel|lacp|spanning-tree|igmp snooping|mld snooping>";
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
            "vlan" => crate::switching::cmd::show_vlan(&endpoints.syncd, &words[1..]).await,
            "mac" => crate::switching::cmd::show_mac(&endpoints.syncd, &words[1..]).await,
            "storm-control" => {
                crate::switching::cmd::show_storm_control(&endpoints.syncd, &words[1..]).await
            }
            "mirror" => crate::switching::cmd::show_mirror(&endpoints.syncd, &words[1..]).await,
            "port-channel" => {
                crate::switching::cmd::show_port_channel(
                    &endpoints.syncd,
                    &endpoints.orch,
                    &words[1..],
                )
                .await
            }
            "lacp" => {
                crate::switching::cmd::show_lacp(&endpoints.syncd, &endpoints.orch, &words[1..])
                    .await
            }
            "spanning-tree" => {
                crate::switching::cmd::show_spanning_tree(&endpoints.orch, &words[1..]).await
            }
            family @ ("igmp" | "mld") => {
                crate::switching::cmd::show_snooping(&endpoints.orch, family, &words[1..]).await
            }
            "monitor" => {
                // `show monitor session` is the EOS-habitual alias.
                let Some(keyword) = words.get(1) else {
                    return Err("% Usage: show monitor session".into());
                };
                resolve(keyword, &["session"])?;
                crate::switching::cmd::show_mirror(&endpoints.syncd, &words[2..]).await
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
            println!("  set interfaces <port> switchport mode <access|trunk|dot1q-tunnel>");
            println!("  set interfaces <port> switchport access vlan <id>");
            println!("  set interfaces <port> switchport trunk vlans <list>   e.g. 10,20,30-32");
            println!("  set interfaces <port> switchport trunk native vlan <id>");
            println!("  set interfaces <port> channel-group <1-64> mode <active|passive|on>");
            println!("  set interfaces <port> lacp rate <normal|fast> | port-priority <n>");
            println!("  set interfaces <port> spanning-tree [portfast|bpduguard|cost <n>|port-priority <n>]");
            println!("  set interfaces <port> storm-control <class> level <0.00-100.00>");
            println!("  set interfaces Port-Channel<n> [min-links <0-8> | lacp fallback <static|individual>");
            println!("                                  | lacp fallback-timeout <1-900> | switchport ...]");
            println!("  set vlans vlan <id> [description <text> | state <active|suspend>]");
            println!("  set protocols spanning-tree [mode <mstp|rstp|none>|priority|hello-time|max-age|");
            println!("                               forward-time|mst name|mst revision|mst instance ...]");
            println!("  set protocols <igmp-snooping|mld-snooping> [disable|robustness <1-3>|vlan <id> ...]");
            println!("  set protocols lacp system-priority <0-65535>");
            println!("  set switching mac-table [aging-time <s> | static <mac> vlan <id> interface <port>|drop]");
            println!("  set switching mirror session <1-4> [source <port> [rx|tx|both] | destination <port>]");
            println!("  set system ssh                                enable the SSH server");
            println!("  set system ssh authentication local           password logins (PAM)");
            println!("  set system http                               web console over HTTP");
            println!("  set system https                              web console over HTTPS (self-signed cert)");
            println!("  set routing static <prefix> <next-hop>        static route");
            println!("  delete interfaces <port> [description|shutdown|no-shutdown|address|switchport|");
            println!("                            channel-group|lacp|spanning-tree|storm-control|min-links ...]");
            println!("  delete vlans vlan <id> [description|state]");
            println!("  delete system <ssh|http|https> [authentication]");
            println!("  delete routing [static [<prefix>]]");
            println!("  delete protocols [spanning-tree|igmp-snooping|mld-snooping|lacp ...]");
            println!("  delete switching [mac-table|mirror ...]");
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

/// A boxed edit of one config block's children — the closure shape the
/// scoped-edit helpers below hand around.
type BlockEdit = Box<dyn FnOnce(&mut Vec<hemlock_config::Item>) + Send>;

/// Shared body of `set` and `delete`: dispatch on the top-level config
/// noun.
async fn config_edit(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let Some(top) = words.first() else {
        return Err(format!(
            "% Usage: {verb} <interfaces|system|routing|vlans|protocols|switching> ..."
        ));
    };
    match resolve(
        top,
        &[
            "interfaces",
            "system",
            "routing",
            "vlans",
            "protocols",
            "switching",
        ],
    )? {
        "interfaces" => config_interfaces(endpoints, &words[1..], delete).await,
        "system" => config_system(endpoints, &words[1..], delete).await,
        "routing" => config_routing(endpoints, &words[1..], delete).await,
        "vlans" => config_vlans(endpoints, &words[1..], delete).await,
        "protocols" => config_protocols(endpoints, &words[1..], delete).await,
        "switching" => config_switching(endpoints, &words[1..], delete).await,
        _ => unreachable!(),
    }
}

/// `set|delete vlans vlan <id> [description <text> | state <active|suspend>]`.
async fn config_vlans(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || {
        format!("% Usage: {verb} vlans vlan <id> [description <text> | state <active|suspend>]")
    };
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

    match resolve(rest[0], &["description", "state"])? {
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
        "state" => {
            if delete {
                edit_config(endpoints, |tree| {
                    let vlans = tree.block_mut("vlans");
                    if let Some(vlan) = keyed_block_children_mut(vlans, "vlan", &id) {
                        ConfigTree::remove_leaf(vlan, "state");
                    }
                })
                .await
            } else {
                let Some(value) = rest.get(1) else {
                    return Err(format!(
                        "% Usage: set vlans vlan {id} state <active|suspend>"
                    ));
                };
                let value = resolve(value, &["active", "suspend"])?.to_string();
                edit_config(endpoints, |tree| {
                    let vlans = tree.block_mut("vlans");
                    let vlan = ConfigTree::ensure_block(vlans, "vlan", &[&id]);
                    ConfigTree::set_leaf(vlan, "state", vec![value]);
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
    let port = match vlan_interface(raw_port).or_else(|| port_channel_interface(raw_port)) {
        // SVIs (`Vlan10`) and port-channels (`Po1`) are config-defined,
        // not syncd ports.
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
            "channel-group",
            "lacp",
            "spanning-tree",
            "storm-control",
            "min-links",
        ],
    )?;
    // Phase-1 SVIs carry an address and nothing else.
    if port.starts_with("Vlan") && subcommand != "address" {
        return Err(format!(
            "% {subcommand} is not supported on VLAN interfaces"
        ));
    }
    if port.starts_with("Management")
        && matches!(
            subcommand,
            "channel-group" | "lacp" | "spanning-tree" | "storm-control" | "min-links"
        )
    {
        return Err(format!(
            "% {subcommand} is not supported on management ports"
        ));
    }
    let is_lag = port.starts_with("Port-Channel");
    if is_lag && matches!(subcommand, "address" | "channel-group") {
        return Err(format!(
            "% {subcommand} is not supported on port-channel interfaces"
        ));
    }
    if !is_lag && subcommand == "min-links" {
        return Err("% min-links is only supported on port-channel interfaces".into());
    }
    match subcommand {
        "channel-group" => {
            return config_channel_group(endpoints, &port, &rest[1..], delete).await;
        }
        "lacp" => {
            return config_lacp(endpoints, &port, &rest[1..], delete, is_lag).await;
        }
        "spanning-tree" => {
            return config_port_stp(endpoints, &port, &rest[1..], delete).await;
        }
        "storm-control" => {
            return config_storm_control(endpoints, &port, &rest[1..], delete).await;
        }
        "min-links" => {
            return config_min_links(endpoints, &port, &rest[1..], delete).await;
        }
        _ => {}
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
            "% Usage: {verb} interfaces <port> switchport [mode <access|trunk|dot1q-tunnel> | access vlan <id> | trunk vlans <list> | trunk native vlan <id>]"
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
                let value = resolve(value, &["access", "trunk", "dot1q-tunnel"])?.to_string();
                let trunk = value == "trunk";
                let tunnel = value == "dot1q-tunnel";
                edit_interface(endpoints, port, move |eth| {
                    let sp = ConfigTree::ensure_block(eth, "switchport", &[]);
                    ConfigTree::set_leaf(sp, "mode", vec![value]);
                    if trunk {
                        // A trunk carries no access VLAN.
                        ConfigTree::remove_leaf(sp, "access");
                    }
                    if tunnel {
                        // A tunnel port keeps its access (S-)VLAN and
                        // carries no trunk config.
                        ConfigTree::remove_leaf(sp, "trunk");
                        ConfigTree::remove_leaf(sp, "native");
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

/// `set|delete interfaces <port> channel-group ...` — LAG membership on
/// an Ethernet member port.
async fn config_channel_group(
    endpoints: &Endpoints,
    port: &str,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    let usage =
        || format!("% Usage: set interfaces {port} channel-group <1-64> mode <active|passive|on>");
    if delete {
        if !words.is_empty() {
            return Err(format!(
                "% Usage: delete interfaces {port} channel-group"
            ));
        }
        return edit_interface(endpoints, port, |eth| {
            ConfigTree::remove_leaf(eth, "channel-group");
        })
        .await
        .map_err(fmt_err);
    }
    let Some(raw_group) = words.first() else {
        return Err(usage());
    };
    let Some(group) = raw_group
        .parse::<u16>()
        .ok()
        .filter(|n| (1..=64).contains(n))
    else {
        return Err(format!("% bad channel-group number {raw_group:?} (1..64)"));
    };
    let Some(keyword) = words.get(1) else {
        return Err(usage());
    };
    resolve(keyword, &["mode"])?;
    let Some(mode) = words.get(2) else {
        return Err(usage());
    };
    let mode = resolve(mode, &["active", "passive", "on"])?.to_string();
    edit_interface(endpoints, port, move |eth| {
        ConfigTree::set_leaf(
            eth,
            "channel-group",
            vec![group.to_string(), "mode".into(), mode],
        );
    })
    .await
    .map_err(fmt_err)
}

/// `set|delete interfaces <port> lacp ...` — per-member tuning on
/// Ethernet ports (`rate`, `port-priority`), fallback behavior on
/// port-channels (`fallback`, `fallback-timeout`).
async fn config_lacp(
    endpoints: &Endpoints,
    port: &str,
    words: &[&str],
    delete: bool,
    is_lag: bool,
) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || {
        if is_lag {
            format!(
                "% Usage: {verb} interfaces {port} lacp [fallback <static|individual> | fallback-timeout <1-900>]"
            )
        } else {
            format!(
                "% Usage: {verb} interfaces {port} lacp [rate <normal|fast> | port-priority <0-65535>]"
            )
        }
    };
    if words.is_empty() {
        if delete {
            return edit_interface(endpoints, port, |eth| {
                ConfigTree::remove_block(eth, "lacp", &[]);
            })
            .await
            .map_err(fmt_err);
        }
        return Err(usage());
    }
    let keywords: &[&str] = if is_lag {
        &["fallback", "fallback-timeout"]
    } else {
        &["rate", "port-priority"]
    };
    let keyword = resolve(words[0], keywords)?;
    if delete {
        if words.len() > 1 {
            return Err(format!("% Invalid input: {:?}", words[1]));
        }
        let keyword = keyword.to_string();
        return edit_interface(endpoints, port, move |eth| {
            if let Some(lacp) = block_children_mut(eth, "lacp") {
                ConfigTree::remove_leaf(lacp, &keyword);
            }
        })
        .await
        .map_err(fmt_err);
    }
    let Some(value) = words.get(1) else {
        return Err(usage());
    };
    let value = match keyword {
        "rate" => resolve(value, &["normal", "fast"])?.to_string(),
        "fallback" => resolve(value, &["static", "individual"])?.to_string(),
        "port-priority" => value
            .parse::<u16>()
            .map(|n| n.to_string())
            .map_err(|_| format!("% bad port-priority {value:?} (0..65535)"))?,
        "fallback-timeout" => value
            .parse::<u16>()
            .ok()
            .filter(|n| (1..=900).contains(n))
            .map(|n| n.to_string())
            .ok_or_else(|| format!("% bad fallback-timeout {value:?} (1..900)"))?,
        _ => unreachable!(),
    };
    let keyword = keyword.to_string();
    edit_interface(endpoints, port, move |eth| {
        let lacp = ConfigTree::ensure_block(eth, "lacp", &[]);
        ConfigTree::set_leaf(lacp, &keyword, vec![value]);
    })
    .await
    .map_err(fmt_err)
}

/// `set|delete interfaces <port> spanning-tree ...` — per-port STP
/// (portfast, bpduguard, cost, port-priority).
async fn config_port_stp(
    endpoints: &Endpoints,
    port: &str,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || {
        format!(
            "% Usage: {verb} interfaces {port} spanning-tree [portfast | bpduguard | cost <1-200000000> | port-priority <0-240>]"
        )
    };
    if words.is_empty() {
        if delete {
            return edit_interface(endpoints, port, |eth| {
                ConfigTree::remove_block(eth, "spanning-tree", &[]);
            })
            .await
            .map_err(fmt_err);
        }
        return Err(usage());
    }
    let keyword = resolve(words[0], &["portfast", "bpduguard", "cost", "port-priority"])?;
    match keyword {
        marker @ ("portfast" | "bpduguard") => {
            if words.len() > 1 {
                return Err(format!("% Invalid input: {:?}", words[1]));
            }
            let marker = marker.to_string();
            edit_interface(endpoints, port, move |eth| {
                if delete {
                    if let Some(stp) = block_children_mut(eth, "spanning-tree") {
                        ConfigTree::remove_leaf(stp, &marker);
                    }
                } else {
                    let stp = ConfigTree::ensure_block(eth, "spanning-tree", &[]);
                    ConfigTree::set_leaf(stp, &marker, vec![]);
                }
            })
            .await
        }
        keyword @ ("cost" | "port-priority") => {
            if delete {
                if words.len() > 1 {
                    return Err(format!("% Invalid input: {:?}", words[1]));
                }
                let keyword = keyword.to_string();
                return edit_interface(endpoints, port, move |eth| {
                    if let Some(stp) = block_children_mut(eth, "spanning-tree") {
                        ConfigTree::remove_leaf(stp, &keyword);
                    }
                })
                .await
                .map_err(fmt_err);
            }
            let Some(value) = words.get(1) else {
                return Err(usage());
            };
            let value = match keyword {
                "cost" => value
                    .parse::<u32>()
                    .ok()
                    .filter(|n| (1..=200_000_000).contains(n))
                    .map(|n| n.to_string())
                    .ok_or_else(|| format!("% bad cost {value:?} (1..200000000)"))?,
                _ => value
                    .parse::<u16>()
                    .ok()
                    .filter(|n| *n <= 240)
                    .map(|n| n.to_string())
                    .ok_or_else(|| format!("% bad port-priority {value:?} (0..240)"))?,
            };
            let keyword = keyword.to_string();
            edit_interface(endpoints, port, move |eth| {
                let stp = ConfigTree::ensure_block(eth, "spanning-tree", &[]);
                ConfigTree::set_leaf(stp, &keyword, vec![value]);
            })
            .await
        }
        _ => unreachable!(),
    }
    .map_err(fmt_err)
}

/// `set|delete interfaces <port> storm-control ...`.
async fn config_storm_control(
    endpoints: &Endpoints,
    port: &str,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || {
        format!(
            "% Usage: {verb} interfaces {port} storm-control <broadcast|multicast|unknown-unicast> level <0.00-100.00>"
        )
    };
    if words.is_empty() {
        if delete {
            return edit_interface(endpoints, port, |eth| {
                ConfigTree::remove_block(eth, "storm-control", &[]);
            })
            .await
            .map_err(fmt_err);
        }
        return Err(usage());
    }
    let kind = resolve(words[0], &["broadcast", "multicast", "unknown-unicast"])?.to_string();
    if delete {
        if words.len() > 1 {
            return Err(format!("% Invalid input: {:?}", words[1]));
        }
        return edit_interface(endpoints, port, move |eth| {
            if let Some(sc) = block_children_mut(eth, "storm-control") {
                ConfigTree::remove_leaf(sc, &kind);
            }
        })
        .await
        .map_err(fmt_err);
    }
    let Some(keyword) = words.get(1) else {
        return Err(usage());
    };
    resolve(keyword, &["level"])?;
    let Some(raw_level) = words.get(2) else {
        return Err(usage());
    };
    let level = hemlock_common::net::parse_storm_level(raw_level).map_err(|e| format!("% {e}"))?;
    edit_interface(endpoints, port, move |eth| {
        let sc = ConfigTree::ensure_block(eth, "storm-control", &[]);
        ConfigTree::set_leaf(sc, &kind, vec!["level".into(), level]);
    })
    .await
    .map_err(fmt_err)
}

/// `set|delete interfaces Port-Channel<n> min-links <0-8>`.
async fn config_min_links(
    endpoints: &Endpoints,
    port: &str,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    if delete {
        if !words.is_empty() {
            return Err(format!("% Invalid input: {:?}", words[0]));
        }
        return edit_interface(endpoints, port, |eth| {
            ConfigTree::remove_leaf(eth, "min-links");
        })
        .await
        .map_err(fmt_err);
    }
    let Some(value) = words.first() else {
        return Err(format!("% Usage: set interfaces {port} min-links <0-8>"));
    };
    let Some(value) = value.parse::<u8>().ok().filter(|n| *n <= 8) else {
        return Err(format!("% bad min-links {value:?} (0..8)"));
    };
    edit_interface(endpoints, port, move |eth| {
        ConfigTree::set_leaf(eth, "min-links", vec![value.to_string()]);
    })
    .await
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

/// Canonical port-channel name from user input: `Po1`, `po1`,
/// `port-channel1`, `p1` all mean `Port-Channel1`. `None` when the input
/// is not a port-channel form.
fn port_channel_interface(input: &str) -> Option<String> {
    let digit_at = input
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(input.len());
    let (alpha, digits) = input.split_at(digit_at);
    if alpha.is_empty() || digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let needle: String = alpha
        .chars()
        .filter(|c| *c != '-')
        .flat_map(char::to_lowercase)
        .collect();
    if !"portchannel".starts_with(&needle) {
        return None;
    }
    digits
        .parse::<u16>()
        .ok()
        .filter(|n| (1..=64).contains(n))
        .map(|n| format!("Port-Channel{n}"))
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

/// `set|delete system <ssh|http|https> ...` — each service is on
/// exactly when its `system { <name> }` block exists; commit applies it.
/// SSH additionally takes `authentication local`; enabling https makes
/// webd generate a self-signed certificate on first start.
async fn config_system(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || format!("% Usage: {verb} system <ssh|http|https> [authentication local]");
    let Some(first) = words.first() else {
        return Err(usage());
    };
    let service = resolve(first, &["ssh", "http", "https"])?;
    let rest = &words[1..];

    if matches!(service, "http" | "https") {
        if !rest.is_empty() {
            return Err(format!("% Usage: {verb} system {service}"));
        }
        let service = service.to_string();
        return if delete {
            edit_config(endpoints, move |tree| {
                let system = tree.block_mut("system");
                ConfigTree::remove_block(system, &service, &[]);
                remove_block_if_empty(tree, "system");
            })
            .await
        } else {
            edit_config(endpoints, move |tree| {
                let system = tree.block_mut("system");
                ConfigTree::ensure_block(system, &service, &[]);
            })
            .await
        }
        .map_err(fmt_err);
    }

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

/// `set|delete protocols <spanning-tree|igmp-snooping|mld-snooping|lacp> ...`.
async fn config_protocols(
    endpoints: &Endpoints,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    if delete && words.is_empty() {
        return edit_config(endpoints, |tree| {
            ConfigTree::remove_block(&mut tree.items, "protocols", &[]);
        })
        .await
        .map_err(fmt_err);
    }
    let Some(first) = words.first() else {
        return Err(format!(
            "% Usage: {verb} protocols <spanning-tree|igmp-snooping|mld-snooping|lacp> ..."
        ));
    };
    match resolve(
        first,
        &["spanning-tree", "igmp-snooping", "mld-snooping", "lacp"],
    )? {
        "spanning-tree" => config_stp(endpoints, &words[1..], delete).await,
        family @ ("igmp-snooping" | "mld-snooping") => {
            config_snooping(endpoints, family, &words[1..], delete).await
        }
        "lacp" => config_lacp_global(endpoints, &words[1..], delete).await,
        _ => unreachable!(),
    }
}

/// `set|delete protocols lacp system-priority <0-65535>`.
async fn config_lacp_global(
    endpoints: &Endpoints,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    if delete && words.is_empty() {
        return edit_config(endpoints, |tree| {
            let protocols = tree.block_mut("protocols");
            ConfigTree::remove_block(protocols, "lacp", &[]);
            remove_block_if_empty(tree, "protocols");
        })
        .await
        .map_err(fmt_err);
    }
    let Some(first) = words.first() else {
        return Err("% Usage: set protocols lacp system-priority <0-65535>".into());
    };
    resolve(first, &["system-priority"])?;
    if delete {
        if words.len() > 1 {
            return Err(format!("% Invalid input: {:?}", words[1]));
        }
        return edit_config(endpoints, |tree| {
            let protocols = tree.block_mut("protocols");
            if let Some(lacp) = block_children_mut(protocols, "lacp") {
                ConfigTree::remove_leaf(lacp, "system-priority");
                if lacp.is_empty() {
                    ConfigTree::remove_block(protocols, "lacp", &[]);
                }
            }
            remove_block_if_empty(tree, "protocols");
        })
        .await
        .map_err(fmt_err);
    }
    let Some(value) = words.get(1) else {
        return Err("% Usage: set protocols lacp system-priority <0-65535>".into());
    };
    let value = value
        .parse::<u16>()
        .map(|n| n.to_string())
        .map_err(|_| format!("% bad system-priority {value:?} (0..65535)"))?;
    edit_config(endpoints, move |tree| {
        let protocols = tree.block_mut("protocols");
        let lacp = ConfigTree::ensure_block(protocols, "lacp", &[]);
        ConfigTree::set_leaf(lacp, "system-priority", vec![value]);
    })
    .await
    .map_err(fmt_err)
}

/// `set|delete protocols spanning-tree ...` — the global bridge config
/// and the MST region.
async fn config_stp(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || {
        format!(
            "% Usage: {verb} protocols spanning-tree [mode <mstp|rstp|none> | priority <0-61440> | hello-time <1-10> | max-age <6-40> | forward-time <4-30> | mst ...]"
        )
    };
    if words.is_empty() {
        if delete {
            return edit_config(endpoints, |tree| {
                let protocols = tree.block_mut("protocols");
                ConfigTree::remove_block(protocols, "spanning-tree", &[]);
                remove_block_if_empty(tree, "protocols");
            })
            .await
            .map_err(fmt_err);
        }
        return Err(usage());
    }
    // Scoped editing of `protocols { spanning-tree { ... } }`.
    let edit_stp = |edit: BlockEdit| async {
        edit_config(endpoints, move |tree| {
            let protocols = tree.block_mut("protocols");
            let stp = ConfigTree::ensure_block(protocols, "spanning-tree", &[]);
            edit(stp);
        })
        .await
        .map_err(fmt_err)
    };
    let delete_in_stp = |edit: BlockEdit| async {
        edit_config(endpoints, move |tree| {
            let protocols = tree.block_mut("protocols");
            if let Some(stp) = block_children_mut(protocols, "spanning-tree") {
                edit(stp);
                if stp.is_empty() {
                    ConfigTree::remove_block(protocols, "spanning-tree", &[]);
                }
            }
            remove_block_if_empty(tree, "protocols");
        })
        .await
        .map_err(fmt_err)
    };

    let keyword = resolve(
        words[0],
        &[
            "mode",
            "priority",
            "hello-time",
            "max-age",
            "forward-time",
            "mst",
        ],
    )?;
    match keyword {
        "mst" => config_stp_mst(endpoints, &words[1..], delete).await,
        keyword if delete => {
            if words.len() > 1 {
                return Err(format!("% Invalid input: {:?}", words[1]));
            }
            let keyword = keyword.to_string();
            delete_in_stp(Box::new(move |stp| {
                ConfigTree::remove_leaf(stp, &keyword);
            }))
            .await
        }
        "mode" => {
            let Some(value) = words.get(1) else {
                return Err(usage());
            };
            if *value == "rapid-pvst" {
                return Err("% rapid-pvst is not supported (use mstp or rstp)".into());
            }
            let value = resolve(value, &["mstp", "rstp", "none"])?.to_string();
            edit_stp(Box::new(move |stp| {
                ConfigTree::set_leaf(stp, "mode", vec![value]);
            }))
            .await
        }
        keyword @ ("priority" | "hello-time" | "max-age" | "forward-time") => {
            let Some(value) = words.get(1) else {
                return Err(usage());
            };
            let (range, what): (std::ops::RangeInclusive<u32>, &str) = match keyword {
                "priority" => (0..=61440, "priority"),
                "hello-time" => (1..=10, "hello-time"),
                "max-age" => (6..=40, "max-age"),
                _ => (4..=30, "forward-time"),
            };
            let value = value
                .parse::<u32>()
                .ok()
                .filter(|n| range.contains(n))
                .map(|n| n.to_string())
                .ok_or_else(|| {
                    format!(
                        "% bad {what} {value:?} ({}..{})",
                        range.start(),
                        range.end()
                    )
                })?;
            let keyword = keyword.to_string();
            edit_stp(Box::new(move |stp| {
                ConfigTree::set_leaf(stp, &keyword, vec![value]);
            }))
            .await
        }
        _ => unreachable!(),
    }
}

/// `set|delete protocols spanning-tree mst ...` — region name/revision
/// and instance-to-VLAN mappings.
async fn config_stp_mst(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || {
        format!(
            "% Usage: {verb} protocols spanning-tree mst [name <text> | revision <0-65535> | instance <1-15> vlans <list>]"
        )
    };
    let edit_mst = |edit: BlockEdit| async {
        edit_config(endpoints, move |tree| {
            let protocols = tree.block_mut("protocols");
            let stp = ConfigTree::ensure_block(protocols, "spanning-tree", &[]);
            let mst = ConfigTree::ensure_block(stp, "mst", &[]);
            edit(mst);
        })
        .await
        .map_err(fmt_err)
    };
    let delete_in_mst = |edit: BlockEdit| async {
        edit_config(endpoints, move |tree| {
            let protocols = tree.block_mut("protocols");
            if let Some(stp) = block_children_mut(protocols, "spanning-tree") {
                if let Some(mst) = block_children_mut(stp, "mst") {
                    edit(mst);
                    if mst.is_empty() {
                        ConfigTree::remove_block(stp, "mst", &[]);
                    }
                }
                if stp.is_empty() {
                    ConfigTree::remove_block(protocols, "spanning-tree", &[]);
                }
            }
            remove_block_if_empty(tree, "protocols");
        })
        .await
        .map_err(fmt_err)
    };

    if words.is_empty() {
        if delete {
            return delete_in_mst(Box::new(|mst| mst.clear())).await;
        }
        return Err(usage());
    }
    match resolve(words[0], &["name", "revision", "instance"])? {
        "name" => {
            if delete {
                if words.len() > 1 {
                    return Err(format!("% Invalid input: {:?}", words[1]));
                }
                return delete_in_mst(Box::new(|mst| ConfigTree::remove_leaf(mst, "name"))).await;
            }
            let text = words[1..].join(" ");
            if text.is_empty() {
                return Err(usage());
            }
            edit_mst(Box::new(move |mst| {
                ConfigTree::set_leaf(mst, "name", vec![text]);
            }))
            .await
        }
        "revision" => {
            if delete {
                if words.len() > 1 {
                    return Err(format!("% Invalid input: {:?}", words[1]));
                }
                return delete_in_mst(Box::new(|mst| ConfigTree::remove_leaf(mst, "revision")))
                    .await;
            }
            let Some(value) = words.get(1) else {
                return Err(usage());
            };
            let value = value
                .parse::<u16>()
                .map(|n| n.to_string())
                .map_err(|_| format!("% bad revision {value:?} (0..65535)"))?;
            edit_mst(Box::new(move |mst| {
                ConfigTree::set_leaf(mst, "revision", vec![value]);
            }))
            .await
        }
        "instance" => {
            let Some(raw_id) = words.get(1) else {
                return Err(usage());
            };
            let id = raw_id
                .parse::<u8>()
                .ok()
                .filter(|n| (1..=15).contains(n))
                .ok_or_else(|| format!("% bad mst instance {raw_id:?} (1..15)"))?
                .to_string();
            if delete {
                if words.len() > 2 {
                    return Err(format!("% Invalid input: {:?}", words[2]));
                }
                return delete_in_mst(Box::new(move |mst| {
                    remove_leaf_matching(mst, "instance", &[&id]);
                }))
                .await;
            }
            let Some(keyword) = words.get(2) else {
                return Err(usage());
            };
            resolve(keyword, &["vlans"])?;
            let Some(list) = words.get(3) else {
                return Err(usage());
            };
            let vlans = parse_vlan_list(list)?;
            edit_mst(Box::new(move |mst| {
                remove_leaf_matching(mst, "instance", &[&id]);
                let mut values = vec![id, "vlans".to_string()];
                values.extend(vlans);
                push_leaf(mst, "instance", values);
            }))
            .await
        }
        _ => unreachable!(),
    }
}

/// `set|delete protocols <igmp-snooping|mld-snooping> ...` — the two
/// families share one grammar.
async fn config_snooping(
    endpoints: &Endpoints,
    family: &str,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || {
        format!(
            "% Usage: {verb} protocols {family} [disable | robustness <1-3> | vlan <id> [disable|fast-leave|querier [address <ip>]|mrouter interface <port>]]"
        )
    };
    let family_name = family.to_string();
    let edit_family = |edit: BlockEdit| {
        let family = family_name.clone();
        async move {
            edit_config(endpoints, move |tree| {
                let protocols = tree.block_mut("protocols");
                let block = ConfigTree::ensure_block(protocols, &family, &[]);
                edit(block);
            })
            .await
            .map_err(fmt_err)
        }
    };
    let delete_in_family = |edit: BlockEdit| {
        let family = family_name.clone();
        async move {
            edit_config(endpoints, move |tree| {
                let protocols = tree.block_mut("protocols");
                if let Some(block) = block_children_mut(protocols, &family) {
                    edit(block);
                    if block.is_empty() {
                        ConfigTree::remove_block(protocols, &family, &[]);
                    }
                }
                remove_block_if_empty(tree, "protocols");
            })
            .await
            .map_err(fmt_err)
        }
    };

    if words.is_empty() {
        if delete {
            return delete_in_family(Box::new(|block| block.clear())).await;
        }
        return Err(usage());
    }
    match resolve(words[0], &["disable", "robustness", "vlan"])? {
        "disable" => {
            if words.len() > 1 {
                return Err(format!("% Invalid input: {:?}", words[1]));
            }
            if delete {
                delete_in_family(Box::new(|block| ConfigTree::remove_leaf(block, "disable"))).await
            } else {
                edit_family(Box::new(|block| {
                    ConfigTree::set_leaf(block, "disable", vec![]);
                }))
                .await
            }
        }
        "robustness" => {
            if delete {
                if words.len() > 1 {
                    return Err(format!("% Invalid input: {:?}", words[1]));
                }
                return delete_in_family(Box::new(|block| {
                    ConfigTree::remove_leaf(block, "robustness");
                }))
                .await;
            }
            let Some(value) = words.get(1) else {
                return Err(usage());
            };
            let value = value
                .parse::<u8>()
                .ok()
                .filter(|n| (1..=3).contains(n))
                .map(|n| n.to_string())
                .ok_or_else(|| format!("% bad robustness {value:?} (1..3)"))?;
            edit_family(Box::new(move |block| {
                ConfigTree::set_leaf(block, "robustness", vec![value]);
            }))
            .await
        }
        "vlan" => {
            let Some(raw_id) = words.get(1) else {
                return Err(usage());
            };
            let id = parse_vlan_arg(raw_id)?;
            let rest = &words[2..];
            if rest.is_empty() {
                return if delete {
                    delete_in_family(Box::new(move |block| {
                        remove_leaf_matching(block, "vlan", &[&id]);
                        ConfigTree::remove_block(block, "vlan", &[&id]);
                    }))
                    .await
                } else {
                    edit_family(Box::new(move |block| {
                        snoop_vlan_block(block, &id);
                    }))
                    .await
                };
            }
            match resolve(rest[0], &["disable", "fast-leave", "querier", "mrouter"])? {
                marker @ ("disable" | "fast-leave") => {
                    if rest.len() > 1 {
                        return Err(format!("% Invalid input: {:?}", rest[1]));
                    }
                    let marker = marker.to_string();
                    if delete {
                        delete_in_family(Box::new(move |block| {
                            if let Some(vlan) = keyed_block_children_mut(block, "vlan", &id) {
                                ConfigTree::remove_leaf(vlan, &marker);
                            }
                        }))
                        .await
                    } else {
                        edit_family(Box::new(move |block| {
                            let vlan = snoop_vlan_block(block, &id);
                            ConfigTree::set_leaf(vlan, &marker, vec![]);
                        }))
                        .await
                    }
                }
                "querier" => {
                    if delete {
                        if rest.len() > 1 {
                            return Err(format!("% Invalid input: {:?}", rest[1]));
                        }
                        return delete_in_family(Box::new(move |block| {
                            if let Some(vlan) = keyed_block_children_mut(block, "vlan", &id) {
                                ConfigTree::remove_leaf(vlan, "querier");
                            }
                        }))
                        .await;
                    }
                    let values = match rest.get(1) {
                        None => vec![],
                        Some(keyword) => {
                            resolve(keyword, &["address"])?;
                            let Some(address) = rest.get(2) else {
                                return Err(format!(
                                    "% Usage: set protocols {family} vlan {id} querier [address <ip>]"
                                ));
                            };
                            let valid = if family == "igmp-snooping" {
                                address.parse::<std::net::Ipv4Addr>().is_ok()
                            } else {
                                address.parse::<std::net::Ipv6Addr>().is_ok()
                            };
                            if !valid {
                                return Err(format!("% bad querier address {address:?}"));
                            }
                            vec!["address".to_string(), (*address).to_string()]
                        }
                    };
                    edit_family(Box::new(move |block| {
                        let vlan = snoop_vlan_block(block, &id);
                        ConfigTree::set_leaf(vlan, "querier", values);
                    }))
                    .await
                }
                "mrouter" => {
                    if delete && rest.len() == 1 {
                        return delete_in_family(Box::new(move |block| {
                            if let Some(vlan) = keyed_block_children_mut(block, "vlan", &id) {
                                ConfigTree::remove_leaf(vlan, "mrouter");
                            }
                        }))
                        .await;
                    }
                    let Some(keyword) = rest.get(1) else {
                        return Err(usage());
                    };
                    resolve(keyword, &["interface"])?;
                    let Some(raw_port) = rest.get(2) else {
                        return Err(usage());
                    };
                    let port = canonical_l2_port(endpoints, raw_port).await?;
                    if delete {
                        delete_in_family(Box::new(move |block| {
                            if let Some(vlan) = keyed_block_children_mut(block, "vlan", &id) {
                                remove_leaf_matching(vlan, "mrouter", &["interface", &port]);
                            }
                        }))
                        .await
                    } else {
                        edit_family(Box::new(move |block| {
                            let vlan = snoop_vlan_block(block, &id);
                            remove_leaf_matching(vlan, "mrouter", &["interface", &port]);
                            push_leaf(vlan, "mrouter", vec!["interface".into(), port]);
                        }))
                        .await
                    }
                }
                _ => unreachable!(),
            }
        }
        _ => unreachable!(),
    }
}

/// `set|delete switching <mac-table|mirror> ...`.
async fn config_switching(
    endpoints: &Endpoints,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    if delete && words.is_empty() {
        return edit_config(endpoints, |tree| {
            ConfigTree::remove_block(&mut tree.items, "switching", &[]);
        })
        .await
        .map_err(fmt_err);
    }
    let Some(first) = words.first() else {
        return Err(format!("% Usage: {verb} switching <mac-table|mirror> ..."));
    };
    match resolve(first, &["mac-table", "mirror"])? {
        "mac-table" => config_mac_table(endpoints, &words[1..], delete).await,
        "mirror" => config_mirror(endpoints, &words[1..], delete).await,
        _ => unreachable!(),
    }
}

/// `set|delete switching mac-table ...`.
async fn config_mac_table(
    endpoints: &Endpoints,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || {
        format!(
            "% Usage: {verb} switching mac-table [aging-time <0|10-1000000> | static <mac> vlan <id> <interface <port>|drop>]"
        )
    };
    let edit_table = |edit: BlockEdit| async {
        edit_config(endpoints, move |tree| {
            let switching = tree.block_mut("switching");
            let table = ConfigTree::ensure_block(switching, "mac-table", &[]);
            edit(table);
        })
        .await
        .map_err(fmt_err)
    };
    let delete_in_table = |edit: BlockEdit| async {
        edit_config(endpoints, move |tree| {
            let switching = tree.block_mut("switching");
            if let Some(table) = block_children_mut(switching, "mac-table") {
                edit(table);
                if table.is_empty() {
                    ConfigTree::remove_block(switching, "mac-table", &[]);
                }
            }
            remove_block_if_empty(tree, "switching");
        })
        .await
        .map_err(fmt_err)
    };

    if words.is_empty() {
        if delete {
            return delete_in_table(Box::new(|table| table.clear())).await;
        }
        return Err(usage());
    }
    match resolve(words[0], &["aging-time", "static"])? {
        "aging-time" => {
            if delete {
                if words.len() > 1 {
                    return Err(format!("% Invalid input: {:?}", words[1]));
                }
                return delete_in_table(Box::new(|table| {
                    ConfigTree::remove_leaf(table, "aging-time");
                }))
                .await;
            }
            let Some(value) = words.get(1) else {
                return Err(usage());
            };
            let value = value
                .parse::<u32>()
                .ok()
                .filter(|n| *n == 0 || (10..=1_000_000).contains(n))
                .map(|n| n.to_string())
                .ok_or_else(|| format!("% bad aging-time {value:?} (0 or 10..1000000)"))?;
            edit_table(Box::new(move |table| {
                ConfigTree::set_leaf(table, "aging-time", vec![value]);
            }))
            .await
        }
        "static" => {
            if delete && words.len() == 1 {
                return delete_in_table(Box::new(|table| {
                    ConfigTree::remove_leaf(table, "static");
                }))
                .await;
            }
            let Some(raw_mac) = words.get(1) else {
                return Err(usage());
            };
            let mac =
                hemlock_common::net::parse_unicast_mac(raw_mac).map_err(|e| format!("% {e}"))?;
            let Some(keyword) = words.get(2) else {
                return Err(usage());
            };
            resolve(keyword, &["vlan"])?;
            let Some(raw_id) = words.get(3) else {
                return Err(usage());
            };
            let id = parse_vlan_arg(raw_id)?;
            if delete {
                if words.len() > 4 {
                    return Err(format!("% Invalid input: {:?}", words[4]));
                }
                return delete_in_table(Box::new(move |table| {
                    remove_leaf_matching(table, "static", &[&mac, "vlan", &id]);
                }))
                .await;
            }
            let Some(target) = words.get(4) else {
                return Err(usage());
            };
            let values = match resolve(target, &["interface", "drop"])? {
                "interface" => {
                    let Some(raw_port) = words.get(5) else {
                        return Err(usage());
                    };
                    let port = canonical_l2_port(endpoints, raw_port).await?;
                    vec![mac.clone(), "vlan".into(), id.clone(), "interface".into(), port]
                }
                "drop" => vec![mac.clone(), "vlan".into(), id.clone(), "drop".into()],
                _ => unreachable!(),
            };
            edit_table(Box::new(move |table| {
                remove_leaf_matching(table, "static", &[&mac, "vlan", &id]);
                push_leaf(table, "static", values);
            }))
            .await
        }
        _ => unreachable!(),
    }
}

/// `set|delete switching mirror session <1-4> ...`.
async fn config_mirror(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || {
        format!(
            "% Usage: {verb} switching mirror session <1-4> [source <port> [rx|tx|both] | destination <port>]"
        )
    };
    if words.is_empty() {
        if delete {
            return edit_config(endpoints, |tree| {
                let switching = tree.block_mut("switching");
                ConfigTree::remove_block(switching, "mirror", &[]);
                remove_block_if_empty(tree, "switching");
            })
            .await
            .map_err(fmt_err);
        }
        return Err(usage());
    }
    resolve(words[0], &["session"])?;
    let Some(raw_session) = words.get(1) else {
        return Err(usage());
    };
    let session = raw_session
        .parse::<u8>()
        .ok()
        .filter(|n| (1..=4).contains(n))
        .ok_or_else(|| format!("% bad mirror session {raw_session:?} (1..4)"))?
        .to_string();
    let rest = &words[2..];

    let edit_session = |edit: BlockEdit| {
        let session = session.clone();
        async move {
            edit_config(endpoints, move |tree| {
                let switching = tree.block_mut("switching");
                let mirror = ConfigTree::ensure_block(switching, "mirror", &[]);
                let block = ConfigTree::ensure_block(mirror, "session", &[&session]);
                edit(block);
            })
            .await
            .map_err(fmt_err)
        }
    };
    let delete_in_session = |edit: BlockEdit| {
        let session = session.clone();
        async move {
            edit_config(endpoints, move |tree| {
                let switching = tree.block_mut("switching");
                if let Some(mirror) = block_children_mut(switching, "mirror") {
                    if let Some(block) = keyed_block_children_mut(mirror, "session", &session) {
                        edit(block);
                        if block.is_empty() {
                            ConfigTree::remove_block(mirror, "session", &[&session]);
                        }
                    }
                    if mirror.is_empty() {
                        ConfigTree::remove_block(switching, "mirror", &[]);
                    }
                }
                remove_block_if_empty(tree, "switching");
            })
            .await
            .map_err(fmt_err)
        }
    };

    if rest.is_empty() {
        return if delete {
            delete_in_session(Box::new(|block| block.clear())).await
        } else {
            edit_session(Box::new(|_| {})).await
        };
    }
    match resolve(rest[0], &["source", "destination"])? {
        "source" => {
            let Some(raw_port) = rest.get(1) else {
                return Err(usage());
            };
            let port = canonical_l2_port(endpoints, raw_port).await?;
            if delete {
                if rest.len() > 2 {
                    return Err(format!("% Invalid input: {:?}", rest[2]));
                }
                return delete_in_session(Box::new(move |block| {
                    remove_leaf_matching(block, "source", &[&port]);
                }))
                .await;
            }
            let direction = match rest.get(2) {
                None => "both",
                Some(word) => resolve(word, &["rx", "tx", "both"])?,
            }
            .to_string();
            edit_session(Box::new(move |block| {
                remove_leaf_matching(block, "source", &[&port]);
                push_leaf(block, "source", vec![port, direction]);
            }))
            .await
        }
        "destination" => {
            if delete {
                if rest.len() > 1 {
                    return Err(format!("% Invalid input: {:?}", rest[1]));
                }
                return delete_in_session(Box::new(|block| {
                    ConfigTree::remove_leaf(block, "destination");
                }))
                .await;
            }
            let Some(raw_port) = rest.get(1) else {
                return Err(usage());
            };
            // A mirror destination is a physical port, never a LAG.
            let known = list_port_names(endpoints).await.map_err(fmt_err)?;
            let port = canonical_port(raw_port, &known)?;
            edit_session(Box::new(move |block| {
                ConfigTree::set_leaf(block, "destination", vec![port]);
            }))
            .await
        }
        _ => unreachable!(),
    }
}

/// Canonicalize an L2 interface reference: a syncd port name (aliases
/// accepted) or a port-channel form (`Po1`).
async fn canonical_l2_port(endpoints: &Endpoints, input: &str) -> Result<String, String> {
    if let Some(po) = port_channel_interface(input) {
        return Ok(po);
    }
    let known = list_port_names(endpoints).await.map_err(fmt_err)?;
    canonical_port(input, &known)
}

/// Append a leaf without replacing same-named siblings (multi-instance
/// leaves: `static ...`, `source ...`, `mrouter ...`, `instance ...`).
fn push_leaf(items: &mut Vec<hemlock_config::Item>, name: &str, values: Vec<String>) {
    items.push(hemlock_config::Item::Leaf {
        name: name.to_string(),
        values,
    });
}

/// Remove leaves named `name` whose leading values match `prefix`
/// exactly (selects one instance among same-named leaves).
fn remove_leaf_matching(items: &mut Vec<hemlock_config::Item>, name: &str, prefix: &[&str]) {
    items.retain(|item| match item {
        hemlock_config::Item::Leaf { name: n, values } if n == name => {
            !(values.len() >= prefix.len()
                && values.iter().zip(prefix).all(|(value, want)| value == want))
        }
        _ => true,
    });
}

/// The per-VLAN block of a snooping family, upgrading the bare
/// `vlan <id>` leaf form to a block when settings land on it.
fn snoop_vlan_block<'a>(
    children: &'a mut Vec<hemlock_config::Item>,
    id: &str,
) -> &'a mut Vec<hemlock_config::Item> {
    remove_leaf_matching(children, "vlan", &[id]);
    ConfigTree::ensure_block(children, "vlan", &[id])
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
    fn port_channel_interface_names_canonicalize() {
        assert_eq!(
            port_channel_interface("Port-Channel1"),
            Some("Port-Channel1".into())
        );
        assert_eq!(
            port_channel_interface("portchannel1"),
            Some("Port-Channel1".into())
        );
        assert_eq!(port_channel_interface("Po1"), Some("Port-Channel1".into()));
        assert_eq!(port_channel_interface("po64"), Some("Port-Channel64".into()));
        assert_eq!(port_channel_interface("p1"), Some("Port-Channel1".into()));
        assert_eq!(port_channel_interface("po0"), None);
        assert_eq!(port_channel_interface("po65"), None);
        assert_eq!(port_channel_interface("Ethernet1"), None);
        assert_eq!(port_channel_interface("v10"), None);
        assert_eq!(port_channel_interface("po"), None);
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
