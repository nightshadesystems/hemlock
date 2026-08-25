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
        acls: Vec::new(),
        wreds: Vec::new(),
    }));
    let mut rl: rustyline::Editor<CliHelper, rustyline::history::DefaultHistory> =
        rustyline::Editor::new()?;
    rl.set_helper(Some(CliHelper {
        state: helper_state.clone(),
    }));

    // Keep the completer's interface-name cache fresh from syncd; a dead
    // or restarting syncd just means stale/no port completion, never an
    // error at the prompt. The management port is an OS netdev, not a
    // syncd port, so it joins the cache from the manifest. The ACL-name
    // cache rides the same sweep, from the mgmtd candidate (so a
    // just-configured ACL completes before it commits).
    {
        let state = helper_state.clone();
        let syncd = endpoints.syncd.clone();
        let mgmtd = endpoints.mgmtd.clone();
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
                if let Ok(channel) = mgmtd.connect().await {
                    let mut client = pb::mgmt_client::MgmtClient::new(channel);
                    if let Ok(response) = client
                        .get_config(pb::GetConfigRequest {
                            source: pb::ConfigSource::Candidate as i32,
                        })
                        .await
                    {
                        let tree = hemlock_config::parse(&response.into_inner().text).ok();
                        let acls = tree.as_ref().map(candidate_acl_names).unwrap_or_default();
                        let wreds = tree.as_ref().map(candidate_wred_names).unwrap_or_default();
                        if let Ok(mut state) = state.lock() {
                            state.acls = acls;
                            state.wreds = wreds;
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
            let (cli_mode, ports, acls, wreds) = match helper_state.lock() {
                Ok(state) => (
                    state.mode,
                    state.ports.clone(),
                    state.acls.clone(),
                    state.wreds.clone(),
                ),
                Err(_) => (CliMode::Operational, Vec::new(), Vec::new(), Vec::new()),
            };
            let options =
                complete::help_candidates(cli_mode, &tokens, partial, &ports, &acls, &wreds);
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
            let clear_topics: &[&str] = &[
                "counters",
                "mac-table",
                "arp",
                "routing",
                "acl",
                "copp",
                "port-security",
                "dhcp",
                "dot1x",
            ];
            match words.get(1).map(|w| resolve(w, clear_topics)).transpose()? {
                Some("arp") => {
                    crate::routing::cmd::clear_arp(&endpoints.orch, &words[2..]).await?;
                }
                Some("routing") => {
                    crate::routing::cmd::clear_routing(&endpoints.orch, &words[2..]).await?;
                }
                Some("acl") => {
                    crate::security::cmd::clear_acl_counters(&endpoints.syncd, &words[2..]).await?;
                }
                Some("copp") => {
                    crate::security::cmd::clear_copp_counters(&endpoints.syncd, &words[2..])
                        .await?;
                }
                Some("port-security") => {
                    crate::security::cmd::clear_port_security(&endpoints.syncd, &words[2..])
                        .await?;
                }
                Some("dhcp") => {
                    crate::security::cmd::clear_dhcp_binding(&endpoints.orch, &words[2..]).await?;
                }
                Some("dot1x") => {
                    crate::security::cmd::clear_dot1x(&endpoints.orch, &words[2..]).await?;
                }
                _ => crate::switching::cmd::clear(&endpoints.syncd, &words[1..]).await?,
            }
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
            println!(
                "  show mac address-table [...]           MAC table (count, aging-time, filters)"
            );
            println!("  show storm-control                     storm-control levels and drops");
            println!(
                "  show mirror                            mirror sessions (monitor session ok)"
            );
            println!("  show ip route [summary|<prefix>]       IPv4 route table");
            println!("  show ipv6 route [summary|<prefix>]     IPv6 route table");
            println!("  show arp | show ipv6 neighbors         kernel neighbor tables");
            println!("  show routing ospf [neighbor|interface] OSPF process / adjacencies");
            println!("  show routing bgp [summary|neighbors <ip>]  BGP table / peers");
            println!("  show vrrp [brief]                      VRRP group state");
            println!("  show acl [<name>|summary]              access lists + match counters");
            println!("  show copp                              control-plane policing classes");
            println!("  show port-security [interface <port>]  learn limits and violations");
            println!("  show dot1x [interface <port>]          802.1X port authentication");
            println!("  show dhcp snooping [binding|statistics]  DHCP snooping state");
            println!("  show arp inspection [statistics]       dynamic ARP inspection");
            println!("  show qos maps                          global DSCP/CoS/TC maps");
            println!("  show qos wred                          WRED/ECN profiles");
            println!("  show qos interface <port>              one port's classification + queues");
            println!("  show qos interfaces                    per-port QoS summary");
            println!("  clear counters [<interface>]           baseline interface counters");
            println!("  clear arp [<ip>]                       flush dynamic ARP entries");
            println!("  clear routing bgp <neighbor|*>         reset BGP sessions");
            println!("  clear mac-table [vlan <id>] [interface <port>]   flush dynamic MACs");
            println!("  clear acl counters [<name>]            baseline ACL match counters");
            println!("  clear copp counters                    baseline CoPP counters");
            println!("  clear port-security [interface <port>] reset learned MACs / errdisable");
            println!("  clear dhcp snooping binding [<mac>]    drop dynamic snooping bindings");
            println!("  clear dot1x interface <port>           force 802.1X reauthentication");
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

/// The ACL names a config tree defines (`security { acl { <family>
/// <name> ... } }`), sorted — the completer's ACL-name cache.
fn candidate_acl_names(tree: &hemlock_config::ConfigTree) -> Vec<String> {
    let Some((_, items)) = tree.block("security") else {
        return Vec::new();
    };
    let Some((_, acl)) = hemlock_config::ConfigTree::blocks_named(items, "acl").next() else {
        return Vec::new();
    };
    let mut names: Vec<String> = acl
        .iter()
        .filter_map(|item| match item {
            hemlock_config::Item::Block { keys, .. } => keys.first().cloned(),
            _ => None,
        })
        .collect();
    names.sort();
    names
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
        "ip",
        "ipv6",
        "arp",
        "routing",
        "vrrp",
        "acl",
        "copp",
        "port-security",
        "dot1x",
        "dhcp",
        "qos",
    ];
    const USAGE: &str = "show <interfaces|environment|configuration|version|vlan|mac address-table|storm-control|mirror|port-channel|lacp|spanning-tree|igmp snooping|mld snooping|ip route|ipv6 route|arp|ipv6 neighbors|routing ospf|routing bgp|vrrp|acl|copp|port-security|dot1x|dhcp snooping|arp inspection|qos maps|qos wred|qos interfaces>";
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
            family @ ("ip" | "ipv6") => {
                crate::routing::cmd::show_family(endpoints, family == "ipv6", &words[1..]).await
            }
            "arp" => {
                // `show arp inspection ...` belongs to the security
                // suite; anything else stays the neighbor table.
                if let Some(next) = words.get(1) {
                    if resolve(next, &["inspection"]).is_ok() {
                        return crate::security::cmd::show_arp_inspection(
                            &endpoints.orch,
                            &words[2..],
                        )
                        .await;
                    }
                }
                crate::routing::cmd::show_neighbors(&endpoints.orch, false, &words[1..]).await
            }
            "routing" => crate::routing::cmd::show_routing(&endpoints.orch, &words[1..]).await,
            "vrrp" => crate::routing::cmd::show_vrrp(&endpoints.orch, &words[1..]).await,
            "acl" => crate::security::cmd::show_acl(&endpoints.syncd, &words[1..]).await,
            "copp" => crate::security::cmd::show_copp(&endpoints.syncd, &words[1..]).await,
            "port-security" => {
                crate::security::cmd::show_port_security(&endpoints.syncd, &words[1..]).await
            }
            "dot1x" => crate::security::cmd::show_dot1x(&endpoints.orch, &words[1..]).await,
            "dhcp" => crate::security::cmd::show_dhcp(&endpoints.orch, &words[1..]).await,
            "qos" => crate::qos::cmd::show(&endpoints.syncd, &words[1..]).await,
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
            println!(
                "  set protocols spanning-tree [mode <mstp|rstp|none>|priority|hello-time|max-age|"
            );
            println!("                               forward-time|mst name|mst revision|mst instance ...]");
            println!("  set protocols <igmp-snooping|mld-snooping> [disable|robustness <1-3>|vlan <id> ...]");
            println!("  set protocols lacp system-priority <0-65535>");
            println!("  set switching mac-table [aging-time <s> | static <mac> vlan <id> interface <port>|drop]");
            println!("  set switching mirror session <1-4> [source <port> [rx|tx|both] | destination <port>]");
            println!("  set system ssh                                enable the SSH server");
            println!("  set system ssh authentication local           password logins (PAM)");
            println!("  set system http                               web console over HTTP");
            println!("  set system https                              web console over HTTPS (self-signed cert)");
            println!("  set routing static <prefix> <next-hop|drop> [distance <1-255>]");
            println!("                                                repeat a prefix for ECMP");
            println!("  set routing arp <ip> interface <if> mac <mac> static ARP/ND entry");
            println!("  set routing router-id <ipv4>");
            println!("  set routing ospf [area <id> network <prefix> | router-id | passive-interface <if>");
            println!(
                "                    | redistribute <connected|static|bgp> | maximum-paths <1-8>"
            );
            println!("                    | interface <if> <cost|hello-interval|dead-interval|priority> <n>]");
            println!(
                "  set routing bgp [as <asn> | router-id | neighbor <ip> <remote-as|description|"
            );
            println!("                   shutdown|ebgp-multihop|next-hop-self> | network <prefix>");
            println!(
                "                   | redistribute <connected|static|ospf> | maximum-paths <1-8>]"
            );
            println!("  set interfaces <if> vrrp <1-255> [address <vip> | priority <1-254>");
            println!(
                "                                    | advertisement-interval <1-40> | no-preempt]"
            );
            println!("  set security acl <ipv4|ipv6|mac> <name> rule <n> <permit|deny>");
            println!("  set security acl <fam> <name> rule <n> [protocol|source|destination|");
            println!("                    source-port|destination-port|dscp|log|police rate <r> burst <b>|");
            println!("                    source-mac|destination-mac|ethertype ...]");
            println!("  set security copp class <name> [rate <pps> | burst <pkts>]");
            println!(
                "  set security dot1x radius-server <ip> [key <secret>|port|timeout|retransmit]"
            );
            println!("  set security dot1x reauth-interval <0|60-86400>");
            println!("  set security dhcp-snooping [vlan <id> | binding <mac> vlan <id> address <ip> interface <port>]");
            println!("  set security arp-inspection [vlan <id> | validate <src-mac|dst-mac|ip>]");
            println!("  set interfaces <port> access-group <name> <in|out>");
            println!("  set interfaces <port> port-security [maximum <1-1024>|violation <protect|shutdown>]");
            println!("  set interfaces <port> dot1x");
            println!("  set interfaces <port|Po> dhcp-snooping trust | arp-inspection trust");
            println!("  set qos map dscp-to-tc dscp <0-63|list|range> tc <0-7>");
            println!("  set qos map cos-to-tc cos <0-7|list> tc <0-7>");
            println!("  set qos map tc-to-dscp tc <0-7> dscp <0-63>");
            println!("  set qos map tc-to-cos tc <0-7> cos <0-7>");
            println!(
                "  set qos wred-profile <name> [min-threshold <1-4096>|max-threshold <1-4096>|"
            );
            println!(
                "                              drop-probability <1-100>|ecn]      thresholds in KB"
            );
            println!("  set interfaces <port|Po> qos trust <dscp|cos|untrusted>");
            println!("  set interfaces <port|Po> qos default-tc <0-7>");
            println!(
                "  set interfaces <port|Po> qos shape rate <rate>       port shaper, k/m/g bps"
            );
            println!(
                "  set interfaces <port|Po> qos queue <0-7> [priority strict | weight <1-127>"
            );
            println!("                                            | shape rate <rate> | wred-profile <name>]");
            println!(
                "  delete interfaces <port> [description|shutdown|no-shutdown|address|switchport|"
            );
            println!("                            channel-group|lacp|spanning-tree|storm-control|min-links|");
            println!("                            access-group|port-security|dot1x|dhcp-snooping|");
            println!("                            arp-inspection|qos ...]");
            println!("  delete vlans vlan <id> [description|state]");
            println!("  delete system <ssh|http|https> [authentication]");
            println!("  delete routing [static [<prefix> [<next-hop>]] | arp [<ip>]]");
            println!("  delete protocols [spanning-tree|igmp-snooping|mld-snooping|lacp ...]");
            println!("  delete switching [mac-table|mirror ...]");
            println!("  delete security [acl|copp|dot1x|dhcp-snooping|arp-inspection ...]");
            println!("  delete qos [map [<table> [<key> <value>]] | wred-profile <name> [...]]");
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
            "% Usage: {verb} <interfaces|system|routing|vlans|protocols|switching|security|qos> ..."
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
            "security",
            "qos",
        ],
    )? {
        "interfaces" => config_interfaces(endpoints, &words[1..], delete).await,
        "system" => config_system(endpoints, &words[1..], delete).await,
        "routing" => config_routing(endpoints, &words[1..], delete).await,
        "vlans" => config_vlans(endpoints, &words[1..], delete).await,
        "protocols" => config_protocols(endpoints, &words[1..], delete).await,
        "switching" => config_switching(endpoints, &words[1..], delete).await,
        "security" => config_security(endpoints, &words[1..], delete).await,
        "qos" => config_qos(endpoints, &words[1..], delete).await,
        _ => unreachable!(),
    }
}

/// The compiled CoPP class names — completion and validation offer
/// exactly these (the full table with default rates lives in syncd).
const COPP_CLASSES: &[&str] = &[
    "bpdu", "lacp", "eapol", "igmp", "mld", "arp", "dhcp", "ospf", "bgp", "vrrp", "ip2me",
    "acl-log", "default",
];

/// ACL name syntax (letter first, then letters/digits/_/-, max 32) —
/// checked at the prompt for immediate feedback; mgmtd re-validates.
fn valid_acl_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `set interfaces <port> access-group <name> <in|out>` /
/// `delete interfaces <port> access-group [in|out]`.
async fn config_access_group(
    endpoints: &Endpoints,
    port: &str,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    if delete {
        let direction = match words.first() {
            Some(word) => Some(resolve(word, &["in", "out"])?.to_string()),
            None => None,
        };
        if let Some(extra) = words.get(1) {
            return Err(format!("% Invalid input: {extra:?}"));
        }
        return edit_interface(endpoints, port, move |eth| {
            eth.retain(|item| {
                !matches!(item, hemlock_config::Item::Leaf { name, values }
                    if name == "access-group"
                        && direction
                            .as_deref()
                            .map(|d| values.get(1).map(String::as_str) == Some(d))
                            .unwrap_or(true))
            });
        })
        .await
        .map_err(fmt_err);
    }
    let (Some(name), Some(direction)) = (words.first(), words.get(1)) else {
        return Err(format!(
            "% Usage: set interfaces {port} access-group <name> <in|out>"
        ));
    };
    if !valid_acl_name(name) {
        return Err(format!(
            "% bad ACL name {name:?} (letter first, then letters/digits/_/-, max 32)"
        ));
    }
    let direction = resolve(direction, &["in", "out"])?.to_string();
    if let Some(extra) = words.get(2) {
        return Err(format!("% Invalid input: {extra:?}"));
    }
    let name = name.to_string();
    edit_interface(endpoints, port, move |eth| {
        // One binding per direction: replace any previous one.
        eth.retain(|item| {
            !matches!(item, hemlock_config::Item::Leaf { name, values }
                if name == "access-group"
                    && values.get(1).map(String::as_str) == Some(direction.as_str()))
        });
        push_leaf(eth, "access-group", vec![name, direction]);
    })
    .await
    .map_err(fmt_err)
}

/// `set interfaces <port> port-security [maximum <1-1024> |
/// violation <protect|shutdown>]`.
async fn config_port_security(
    endpoints: &Endpoints,
    port: &str,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    if words.is_empty() {
        return edit_interface(endpoints, port, move |eth| {
            if delete {
                ConfigTree::remove_block(eth, "port-security", &[]);
            } else {
                // Enables with the defaults (maximum 1, protect).
                ConfigTree::ensure_block(eth, "port-security", &[]);
            }
        })
        .await
        .map_err(fmt_err);
    }
    match resolve(words[0], &["maximum", "violation", "sticky"])? {
        // Deferred by the security suite.
        "sticky" => Err("% sticky port-security MACs are not supported".into()),
        "maximum" => {
            if delete {
                return edit_interface(endpoints, port, |eth| {
                    if let Some(ps) = block_children_mut(eth, "port-security") {
                        ConfigTree::remove_leaf(ps, "maximum");
                    }
                })
                .await
                .map_err(fmt_err);
            }
            let Some(value) = words.get(1) else {
                return Err(format!(
                    "% Usage: set interfaces {port} port-security maximum <1-1024>"
                ));
            };
            let value = int_arg::<u32>(value, 1..=1024, "maximum")?.to_string();
            edit_interface(endpoints, port, move |eth| {
                let ps = ConfigTree::ensure_block(eth, "port-security", &[]);
                ConfigTree::set_leaf(ps, "maximum", vec![value]);
            })
            .await
            .map_err(fmt_err)
        }
        "violation" => {
            if delete {
                return edit_interface(endpoints, port, |eth| {
                    if let Some(ps) = block_children_mut(eth, "port-security") {
                        ConfigTree::remove_leaf(ps, "violation");
                    }
                })
                .await
                .map_err(fmt_err);
            }
            let Some(value) = words.get(1) else {
                return Err(format!(
                    "% Usage: set interfaces {port} port-security violation <protect|shutdown>"
                ));
            };
            let action = resolve(value, &["protect", "shutdown"])?.to_string();
            edit_interface(endpoints, port, move |eth| {
                let ps = ConfigTree::ensure_block(eth, "port-security", &[]);
                ConfigTree::set_leaf(ps, "violation", vec![action]);
            })
            .await
            .map_err(fmt_err)
        }
        _ => unreachable!(),
    }
}

/// The four global map tables and the key/value words each takes.
const QOS_MAP_TABLES: &[(&str, &str, u8, &str, u8)] = &[
    ("dscp-to-tc", "dscp", 63, "tc", 7),
    ("cos-to-tc", "cos", 7, "tc", 7),
    ("tc-to-dscp", "tc", 7, "dscp", 63),
    ("tc-to-cos", "tc", 7, "cos", 7),
];

/// WRED profile name syntax (letter first, then letters/digits/_/-, max
/// 32) — checked at the prompt for immediate feedback; mgmtd
/// re-validates.
fn valid_wred_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The WRED profile names a config tree defines (`qos { wred-profile
/// <name> ... }`), sorted — the completer's profile-name cache.
fn candidate_wred_names(tree: &hemlock_config::ConfigTree) -> Vec<String> {
    let Some((_, items)) = tree.block("qos") else {
        return Vec::new();
    };
    let mut names: Vec<String> = hemlock_config::ConfigTree::blocks_named(items, "wred-profile")
        .filter_map(|(keys, _)| keys.first().cloned())
        .collect();
    names.sort();
    names
}

/// Replace (or insert) one map entry — a per-value phrase leaf, so
/// `delete qos map dscp-to-tc dscp 46` drops exactly one mapping.
fn set_map_entry(
    items: &mut Vec<hemlock_config::Item>,
    key_word: &str,
    key: u8,
    value_word: &str,
    value: u8,
) {
    let key_text = key.to_string();
    for item in items.iter_mut() {
        if let hemlock_config::Item::Leaf { name, values } = item {
            if name == key_word && values.first().map(String::as_str) == Some(key_text.as_str()) {
                *values = vec![key_text, value_word.to_string(), value.to_string()];
                return;
            }
        }
    }
    items.push(hemlock_config::Item::Leaf {
        name: key_word.to_string(),
        values: vec![key_text, value_word.to_string(), value.to_string()],
    });
}

/// Drop the map entries for `keys` (empty = the whole table).
fn remove_map_entries(items: &mut Vec<hemlock_config::Item>, key_word: &str, keys: &[u8]) {
    let wanted: Vec<String> = keys.iter().map(u8::to_string).collect();
    items.retain(|item| {
        !matches!(item, hemlock_config::Item::Leaf { name, values }
            if name == key_word
                && (wanted.is_empty()
                    || values
                        .first()
                        .is_some_and(|v| wanted.iter().any(|w| w == v))))
    });
}

/// Remove an emptied `qos { <sub> { ... } }` chain bottom-up.
fn prune_qos(tree: &mut hemlock_config::ConfigTree) {
    let qos = tree.block_mut("qos");
    // Only the map tables collapse when emptied; a `wred-profile <name>
    // { }` with no leaves is still a defined profile.
    for item in qos.iter_mut() {
        if let hemlock_config::Item::Block { name, children, .. } = item {
            if name == "map" {
                children.retain(
                    |child| !matches!(child, hemlock_config::Item::Block { children, .. } if children.is_empty()),
                );
            }
        }
    }
    qos.retain(|item| {
        !matches!(item, hemlock_config::Item::Block { name, children, .. }
            if name == "map" && children.is_empty())
    });
    remove_block_if_empty(tree, "qos");
}

/// `set|delete qos <map|wred-profile> ...`.
async fn config_qos(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    if delete && words.is_empty() {
        return edit_config(endpoints, |tree| {
            ConfigTree::remove_block(&mut tree.items, "qos", &[]);
        })
        .await
        .map_err(fmt_err);
    }
    let Some(first) = words.first() else {
        return Err(format!("% Usage: {verb} qos <map|wred-profile> ..."));
    };
    match resolve(
        first,
        &["map", "wred-profile", "priority-flow-control", "buffer"],
    )? {
        // Deferred by the QoS suite.
        "priority-flow-control" => Err("% priority-flow-control is not supported".into()),
        "buffer" => Err("% buffer-pool configuration is not supported".into()),
        "map" => config_qos_map(endpoints, &words[1..], delete).await,
        "wred-profile" => config_wred_profile(endpoints, &words[1..], delete).await,
        _ => unreachable!(),
    }
}

/// `set|delete qos map <table> <key> <value-word> <value>`.
async fn config_qos_map(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    if delete && words.is_empty() {
        return edit_config(endpoints, |tree| {
            let qos = tree.block_mut("qos");
            ConfigTree::remove_block(qos, "map", &[]);
            prune_qos(tree);
        })
        .await
        .map_err(fmt_err);
    }
    let table_words: Vec<&str> = QOS_MAP_TABLES.iter().map(|(name, ..)| *name).collect();
    let Some(table_word) = words.first() else {
        return Err(format!(
            "% Usage: {verb} qos map <{}> ...",
            table_words.join("|")
        ));
    };
    let table = resolve(table_word, &table_words)?;
    let (_, key_word, key_max, value_word, value_max) = *QOS_MAP_TABLES
        .iter()
        .find(|(name, ..)| *name == table)
        .expect("resolved from the same table");
    let usage = format!(
        "% Usage: {verb} qos map {table} {key_word} <0-{key_max}> {value_word} <0-{value_max}>"
    );

    // `delete qos map <table>` drops the whole table.
    if delete && words.len() == 1 {
        let table = table.to_string();
        return edit_config(endpoints, move |tree| {
            let qos = tree.block_mut("qos");
            let map = ConfigTree::ensure_block(qos, "map", &[]);
            ConfigTree::remove_block(map, &table, &[]);
            prune_qos(tree);
        })
        .await
        .map_err(fmt_err);
    }

    let Some(key_keyword) = words.get(1) else {
        return Err(usage);
    };
    resolve(key_keyword, &[key_word])?;
    let Some(key_text) = words.get(2) else {
        return Err(usage);
    };
    // Lists and ranges expand to per-value leaves, matching how VLAN
    // sets expand elsewhere.
    let keys = hemlock_common::net::parse_value_list(key_text, key_max, key_word)
        .map_err(|e| format!("% {e}"))?;

    if delete {
        if let Some(extra) = words.get(3) {
            return Err(format!("% Invalid input: {extra:?}"));
        }
        let table = table.to_string();
        let key_word = key_word.to_string();
        return edit_config(endpoints, move |tree| {
            let qos = tree.block_mut("qos");
            let map = ConfigTree::ensure_block(qos, "map", &[]);
            let entries = ConfigTree::ensure_block(map, &table, &[]);
            remove_map_entries(entries, &key_word, &keys);
            prune_qos(tree);
        })
        .await
        .map_err(fmt_err);
    }

    let Some(value_keyword) = words.get(3) else {
        return Err(usage);
    };
    resolve(value_keyword, &[value_word])?;
    let Some(value_text) = words.get(4) else {
        return Err(usage);
    };
    let value = int_arg::<u8>(value_text, 0..=value_max, value_word)?;
    if let Some(extra) = words.get(5) {
        return Err(format!("% Invalid input: {extra:?}"));
    }
    let table = table.to_string();
    let key_word = key_word.to_string();
    let value_word = value_word.to_string();
    edit_config(endpoints, move |tree| {
        let qos = tree.block_mut("qos");
        let map = ConfigTree::ensure_block(qos, "map", &[]);
        let entries = ConfigTree::ensure_block(map, &table, &[]);
        for key in keys {
            set_map_entry(entries, &key_word, key, &value_word, value);
        }
    })
    .await
    .map_err(fmt_err)
}

/// `set|delete qos wred-profile <name> [min-threshold|max-threshold|
/// drop-probability|ecn]`.
async fn config_wred_profile(
    endpoints: &Endpoints,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let Some(name) = words.first() else {
        return Err(format!(
            "% Usage: {verb} qos wred-profile <name> [min-threshold <1-4096>|max-threshold <1-4096>|drop-probability <1-100>|ecn]"
        ));
    };
    if !valid_wred_name(name) {
        return Err(format!(
            "% bad WRED profile name {name:?} (letter first, then letters/digits/_/-, max 32)"
        ));
    }
    let name = name.to_string();
    let rest = &words[1..];

    if rest.is_empty() {
        let name = name.clone();
        return edit_config(endpoints, move |tree| {
            let qos = tree.block_mut("qos");
            if delete {
                ConfigTree::remove_block(qos, "wred-profile", &[&name]);
                prune_qos(tree);
            } else {
                ConfigTree::ensure_block(qos, "wred-profile", &[&name]);
            }
        })
        .await
        .map_err(fmt_err);
    }

    let knob = resolve(
        rest[0],
        &[
            "min-threshold",
            "max-threshold",
            "drop-probability",
            "ecn",
            "weight",
        ],
    )?;
    // Deferred by the QoS suite: one curve per profile.
    if knob == "weight" {
        return Err("% per-profile WRED weight is not supported".into());
    }
    if knob == "ecn" {
        if let Some(extra) = rest.get(1) {
            return Err(format!("% Invalid input: {extra:?}"));
        }
        let name = name.clone();
        return edit_config(endpoints, move |tree| {
            let qos = tree.block_mut("qos");
            let profile = ConfigTree::ensure_block(qos, "wred-profile", &[&name]);
            if delete {
                ConfigTree::remove_leaf(profile, "ecn");
            } else {
                ConfigTree::set_leaf(profile, "ecn", vec![]);
            }
        })
        .await
        .map_err(fmt_err);
    }
    if delete {
        if let Some(extra) = rest.get(1) {
            return Err(format!("% Invalid input: {extra:?}"));
        }
        let knob = knob.to_string();
        let name = name.clone();
        return edit_config(endpoints, move |tree| {
            let qos = tree.block_mut("qos");
            let profile = ConfigTree::ensure_block(qos, "wred-profile", &[&name]);
            ConfigTree::remove_leaf(profile, &knob);
        })
        .await
        .map_err(fmt_err);
    }
    let range = if knob == "drop-probability" {
        1..=100u32
    } else {
        1..=4096u32
    };
    let Some(value) = rest.get(1) else {
        return Err(format!(
            "% Usage: set qos wred-profile {name} {knob} <{}-{}>",
            range.start(),
            range.end()
        ));
    };
    let value = int_arg::<u32>(value, range, knob)?.to_string();
    if let Some(extra) = rest.get(2) {
        return Err(format!("% Invalid input: {extra:?}"));
    }
    let knob = knob.to_string();
    edit_config(endpoints, move |tree| {
        let qos = tree.block_mut("qos");
        let profile = ConfigTree::ensure_block(qos, "wred-profile", &[&name]);
        ConfigTree::set_leaf(profile, &knob, vec![value]);
    })
    .await
    .map_err(fmt_err)
}

/// `set|delete interfaces <port> qos [trust|default-tc|shape rate|
/// queue <0-7> ...]`.
async fn config_port_qos(
    endpoints: &Endpoints,
    port: &str,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    if words.is_empty() {
        return edit_interface(endpoints, port, move |eth| {
            if delete {
                ConfigTree::remove_block(eth, "qos", &[]);
            } else {
                ConfigTree::ensure_block(eth, "qos", &[]);
            }
        })
        .await
        .map_err(fmt_err);
    }
    match resolve(
        words[0],
        &[
            "trust",
            "default-tc",
            "shape",
            "queue",
            "priority-flow-control",
        ],
    )? {
        // Deferred by the QoS suite.
        "priority-flow-control" => Err("% priority-flow-control is not supported".into()),
        "trust" => {
            if delete {
                return edit_interface(endpoints, port, |eth| {
                    if let Some(qos) = block_children_mut(eth, "qos") {
                        ConfigTree::remove_leaf(qos, "trust");
                    }
                })
                .await
                .map_err(fmt_err);
            }
            let Some(mode) = words.get(1) else {
                return Err(format!(
                    "% Usage: set interfaces {port} qos trust <dscp|cos|untrusted>"
                ));
            };
            let mode = resolve(mode, &["dscp", "cos", "untrusted"])?.to_string();
            if let Some(extra) = words.get(2) {
                return Err(format!("% Invalid input: {extra:?}"));
            }
            edit_interface(endpoints, port, move |eth| {
                let qos = ConfigTree::ensure_block(eth, "qos", &[]);
                ConfigTree::set_leaf(qos, "trust", vec![mode]);
            })
            .await
            .map_err(fmt_err)
        }
        "default-tc" => {
            if delete {
                return edit_interface(endpoints, port, |eth| {
                    if let Some(qos) = block_children_mut(eth, "qos") {
                        ConfigTree::remove_leaf(qos, "default-tc");
                    }
                })
                .await
                .map_err(fmt_err);
            }
            let Some(value) = words.get(1) else {
                return Err(format!(
                    "% Usage: set interfaces {port} qos default-tc <0-7>"
                ));
            };
            let value = int_arg::<u8>(value, 0..=7, "default-tc")?.to_string();
            if let Some(extra) = words.get(2) {
                return Err(format!("% Invalid input: {extra:?}"));
            }
            edit_interface(endpoints, port, move |eth| {
                let qos = ConfigTree::ensure_block(eth, "qos", &[]);
                ConfigTree::set_leaf(qos, "default-tc", vec![value]);
            })
            .await
            .map_err(fmt_err)
        }
        "shape" => {
            let rate = qos_shape_rate(&words[1..], delete, &format!("set interfaces {port} qos"))?;
            edit_interface(endpoints, port, move |eth| {
                let qos = ConfigTree::ensure_block(eth, "qos", &[]);
                match rate {
                    Some(rate) => ConfigTree::set_phrase(qos, "shape", "rate", vec![rate]),
                    None => ConfigTree::remove_leaf(qos, "shape"),
                }
            })
            .await
            .map_err(fmt_err)
        }
        "queue" => config_port_qos_queue(endpoints, port, &words[1..], delete).await,
        _ => unreachable!(),
    }
}

/// The `shape rate <rate>` tail shared by the port and queue forms.
/// `Ok(None)` = the delete form.
fn qos_shape_rate(words: &[&str], delete: bool, prefix: &str) -> Result<Option<String>, String> {
    if delete {
        if let Some(word) = words.first() {
            resolve(word, &["rate"])?;
        }
        if let Some(extra) = words.get(1) {
            return Err(format!("% Invalid input: {extra:?}"));
        }
        return Ok(None);
    }
    let Some(keyword) = words.first() else {
        return Err(format!("% Usage: {prefix} shape rate <rate>"));
    };
    resolve(keyword, &["rate"])?;
    let Some(rate) = words.get(1) else {
        return Err(format!("% Usage: {prefix} shape rate <rate>"));
    };
    let parsed = hemlock_common::net::parse_shape_rate(rate).map_err(|e| format!("% {e}"))?;
    if let Some(extra) = words.get(2) {
        return Err(format!("% Invalid input: {extra:?}"));
    }
    Ok(Some(hemlock_common::net::format_shape_rate(parsed)))
}

/// `set|delete interfaces <port> qos queue <0-7> [priority strict|
/// weight <1-127>|shape rate <rate>|wred-profile <name>]`.
async fn config_port_qos_queue(
    endpoints: &Endpoints,
    port: &str,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let Some(index_text) = words.first() else {
        return Err(format!(
            "% Usage: {verb} interfaces {port} qos queue <0-7> [priority strict|weight <1-127>|shape rate <rate>|wred-profile <name>]"
        ));
    };
    let index = int_arg::<u8>(index_text, 0..=7, "queue")?.to_string();
    let rest = &words[1..];

    if rest.is_empty() {
        let index = index.clone();
        return edit_interface(endpoints, port, move |eth| {
            let qos = ConfigTree::ensure_block(eth, "qos", &[]);
            if delete {
                ConfigTree::remove_block(qos, "queue", &[&index]);
            } else {
                ConfigTree::ensure_block(qos, "queue", &[&index]);
            }
        })
        .await
        .map_err(fmt_err);
    }

    let knob = resolve(
        rest[0],
        &["priority", "weight", "shape", "wred-profile", "bandwidth"],
    )?;
    // Deferred by the QoS suite: DWRR weights, not percentages.
    if knob == "bandwidth" {
        return Err("% queue bandwidth percentages are not supported (use weight)".into());
    }
    let prefix = format!("{verb} interfaces {port} qos queue {index}");
    match knob {
        "priority" => {
            if !delete {
                let Some(word) = rest.get(1) else {
                    return Err(format!("% Usage: {prefix} priority strict"));
                };
                resolve(word, &["strict"])?;
            }
            if let Some(extra) = rest.get(2) {
                return Err(format!("% Invalid input: {extra:?}"));
            }
            edit_interface(endpoints, port, move |eth| {
                let qos = ConfigTree::ensure_block(eth, "qos", &[]);
                let queue = ConfigTree::ensure_block(qos, "queue", &[&index]);
                if delete {
                    ConfigTree::remove_leaf(queue, "priority");
                } else {
                    // Strict priority and a DWRR weight are mutually
                    // exclusive, so setting one clears the other.
                    ConfigTree::remove_leaf(queue, "weight");
                    ConfigTree::set_leaf(queue, "priority", vec!["strict".into()]);
                }
            })
            .await
            .map_err(fmt_err)
        }
        "weight" => {
            if delete {
                if let Some(extra) = rest.get(1) {
                    return Err(format!("% Invalid input: {extra:?}"));
                }
                return edit_interface(endpoints, port, move |eth| {
                    let qos = ConfigTree::ensure_block(eth, "qos", &[]);
                    let queue = ConfigTree::ensure_block(qos, "queue", &[&index]);
                    ConfigTree::remove_leaf(queue, "weight");
                })
                .await
                .map_err(fmt_err);
            }
            let Some(value) = rest.get(1) else {
                return Err(format!("% Usage: {prefix} weight <1-127>"));
            };
            let value = int_arg::<u8>(value, 1..=127, "weight")?.to_string();
            if let Some(extra) = rest.get(2) {
                return Err(format!("% Invalid input: {extra:?}"));
            }
            edit_interface(endpoints, port, move |eth| {
                let qos = ConfigTree::ensure_block(eth, "qos", &[]);
                let queue = ConfigTree::ensure_block(qos, "queue", &[&index]);
                ConfigTree::remove_leaf(queue, "priority");
                ConfigTree::set_leaf(queue, "weight", vec![value]);
            })
            .await
            .map_err(fmt_err)
        }
        "shape" => {
            let rate = qos_shape_rate(&rest[1..], delete, &prefix)?;
            edit_interface(endpoints, port, move |eth| {
                let qos = ConfigTree::ensure_block(eth, "qos", &[]);
                let queue = ConfigTree::ensure_block(qos, "queue", &[&index]);
                match rate {
                    Some(rate) => ConfigTree::set_phrase(queue, "shape", "rate", vec![rate]),
                    None => ConfigTree::remove_leaf(queue, "shape"),
                }
            })
            .await
            .map_err(fmt_err)
        }
        "wred-profile" => {
            if delete {
                if let Some(extra) = rest.get(1) {
                    return Err(format!("% Invalid input: {extra:?}"));
                }
                return edit_interface(endpoints, port, move |eth| {
                    let qos = ConfigTree::ensure_block(eth, "qos", &[]);
                    let queue = ConfigTree::ensure_block(qos, "queue", &[&index]);
                    ConfigTree::remove_leaf(queue, "wred-profile");
                })
                .await
                .map_err(fmt_err);
            }
            let Some(name) = rest.get(1) else {
                return Err(format!("% Usage: {prefix} wred-profile <name>"));
            };
            if !valid_wred_name(name) {
                return Err(format!(
                    "% bad WRED profile name {name:?} (letter first, then letters/digits/_/-, max 32)"
                ));
            }
            let name = name.to_string();
            if let Some(extra) = rest.get(2) {
                return Err(format!("% Invalid input: {extra:?}"));
            }
            edit_interface(endpoints, port, move |eth| {
                let qos = ConfigTree::ensure_block(eth, "qos", &[]);
                let queue = ConfigTree::ensure_block(qos, "queue", &[&index]);
                ConfigTree::set_leaf(queue, "wred-profile", vec![name]);
            })
            .await
            .map_err(fmt_err)
        }
        _ => unreachable!(),
    }
}

/// `set|delete security <acl|copp|dot1x|dhcp-snooping|arp-inspection> ...`.
async fn config_security(
    endpoints: &Endpoints,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    if delete && words.is_empty() {
        return edit_config(endpoints, |tree| {
            ConfigTree::remove_block(&mut tree.items, "security", &[]);
        })
        .await
        .map_err(fmt_err);
    }
    let Some(first) = words.first() else {
        return Err(format!(
            "% Usage: {verb} security <acl|copp|dot1x|dhcp-snooping|arp-inspection> ..."
        ));
    };
    match resolve(
        first,
        &[
            "acl",
            "copp",
            "dot1x",
            "dhcp-snooping",
            "arp-inspection",
            "ra-guard",
            "dhcpv6-snooping",
        ],
    )? {
        // Deferred by the security suite.
        "ra-guard" => Err("% IPv6 RA-guard is not supported".into()),
        "dhcpv6-snooping" => Err("% DHCPv6 snooping is not supported".into()),
        "acl" => config_acl(endpoints, &words[1..], delete).await,
        "copp" => config_copp(endpoints, &words[1..], delete).await,
        "dot1x" => config_dot1x(endpoints, &words[1..], delete).await,
        "dhcp-snooping" => config_dhcp_snooping(endpoints, &words[1..], delete).await,
        "arp-inspection" => config_arp_inspection(endpoints, &words[1..], delete).await,
        _ => unreachable!(),
    }
}

/// Remove an emptied `security { <sub> { ... } }` chain bottom-up.
fn prune_security(tree: &mut hemlock_config::ConfigTree) {
    let security = tree.block_mut("security");
    security.retain(
        |item| !matches!(item, hemlock_config::Item::Block { children, .. } if children.is_empty()),
    );
    remove_block_if_empty(tree, "security");
}

/// `set|delete security acl <ipv4|ipv6|mac> <name> [rule <n> ...]`.
async fn config_acl(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage =
        move || format!("% Usage: {verb} security acl <ipv4|ipv6|mac> <name> [rule <n> ...]");
    let Some(family_word) = words.first() else {
        return Err(usage());
    };
    let family = resolve(family_word, &["ipv4", "ipv6", "mac"])?.to_string();
    let Some(name) = words.get(1) else {
        return Err(usage());
    };
    if !valid_acl_name(name) {
        return Err(format!(
            "% bad ACL name {name:?} (letter first, then letters/digits/_/-, max 32)"
        ));
    }
    let name = name.to_string();
    let rest = &words[2..];

    // A closure editing the ACL's block (created on `set` paths).
    let family_for_edit = family.clone();
    let name_for_edit = name.clone();
    let edit_acl = move |edit: BlockEdit| {
        let family = family_for_edit.clone();
        let name = name_for_edit.clone();
        async move {
            edit_config(endpoints, move |tree| {
                let security = tree.block_mut("security");
                let acl = ConfigTree::ensure_block(security, "acl", &[]);
                let block = ConfigTree::ensure_block(acl, &family, &[&name]);
                edit(block);
            })
            .await
            .map_err(fmt_err)
        }
    };
    // The delete twin navigates without creating and prunes upward.
    let family_for_delete = family.clone();
    let name_for_delete = name.clone();
    let delete_in_acl = move |edit: BlockEdit| {
        let family = family_for_delete.clone();
        let name = name_for_delete.clone();
        async move {
            edit_config(endpoints, move |tree| {
                let Some(security) = block_children_mut(&mut tree.items, "security") else {
                    return;
                };
                let Some(acl) = block_children_mut(security, "acl") else {
                    return;
                };
                if let Some(block) = keyed_block_children_mut(acl, &family, &name) {
                    edit(block);
                }
                ConfigTree::remove_block(acl, &family, &[&name]);
                prune_security(tree);
            })
            .await
            .map_err(fmt_err)
        }
    };

    if rest.is_empty() {
        return if delete {
            delete_in_acl(Box::new(|block| block.clear())).await
        } else {
            edit_acl(Box::new(|_| {})).await
        };
    }
    resolve(rest[0], &["rule"])?;
    let Some(number_text) = rest.get(1) else {
        return Err(usage());
    };
    let number = number_text
        .parse::<u32>()
        .ok()
        .filter(|n| *n >= 1 && !number_text.starts_with('0'))
        .ok_or_else(|| format!("% bad rule number {number_text:?} (1..4294967295)"))?
        .to_string();
    let body = &rest[2..];

    if body.is_empty() {
        return if delete {
            let number = number.clone();
            edit_config(endpoints, move |tree| {
                let Some(security) = block_children_mut(&mut tree.items, "security") else {
                    return;
                };
                let Some(acl) = block_children_mut(security, "acl") else {
                    return;
                };
                if let Some(block) = keyed_block_children_mut(acl, &family, &name) {
                    ConfigTree::remove_block(block, "rule", &[&number]);
                }
                // An emptied ACL block stays (the ACL still exists).
                prune_security_rules_only(tree);
            })
            .await
            .map_err(fmt_err)
        } else {
            let number = number.clone();
            edit_acl(Box::new(move |block| {
                ConfigTree::ensure_block(block, "rule", &[&number]);
            }))
            .await
        };
    }

    // Field grammar, family-gated.
    let ip_family = family != "mac";
    let keywords: &[&str] = if ip_family {
        &[
            "permit",
            "deny",
            "protocol",
            "source",
            "destination",
            "source-port",
            "destination-port",
            "dscp",
            "log",
            "police",
            "time-range",
        ]
    } else {
        &[
            "permit",
            "deny",
            "source-mac",
            "destination-mac",
            "ethertype",
            "time-range",
        ]
    };
    let keyword = resolve(body[0], keywords)?;
    if keyword == "time-range" {
        // Deferred by the security suite.
        return Err("% time-based ACLs are not supported".into());
    }
    let value = body.get(1).map(|v| v.to_string());
    let extra = body.get(2..).unwrap_or_default();
    let usage_family = family.clone();
    let usage_name = name.clone();
    let usage_number = number.clone();
    let rule_usage = move |form: &str| {
        format!("% Usage: set security acl {usage_family} {usage_name} rule {usage_number} {form}")
    };

    // Validate + canonicalize the field at the prompt.
    let (leaf, values): (String, Vec<String>) = match keyword {
        marker @ ("permit" | "deny" | "log") => {
            if value.is_some() {
                return Err(format!("% Invalid input: {:?}", body[1]));
            }
            (marker.to_string(), vec![])
        }
        "protocol" => {
            let Some(value) = value else {
                return Err(rule_usage("protocol <tcp|udp|icmp|0-255>"));
            };
            let canonical = match value.as_str() {
                "tcp" | "udp" | "icmp" => value.clone(),
                other => int_arg::<u8>(other, 0..=255, "protocol")?.to_string(),
            };
            ("protocol".into(), vec![canonical])
        }
        slot @ ("source" | "destination") => {
            let Some(value) = value else {
                return Err(rule_usage(&format!("{slot} <prefix|any>")));
            };
            let canonical = if value == "any" {
                value
            } else {
                let canonical = hemlock_common::net::require_canonical_prefix(&value)
                    .map_err(|e| format!("% {e}"))?;
                if canonical.contains(':') != (family == "ipv6") {
                    return Err(format!(
                        "% {canonical} does not match the ACL family ({family})"
                    ));
                }
                canonical
            };
            (slot.into(), vec![canonical])
        }
        slot @ ("source-port" | "destination-port") => {
            let Some(value) = value else {
                return Err(rule_usage(&format!("{slot} <port|a-b>")));
            };
            hemlock_common::net::parse_port_match(&value).map_err(|e| format!("% {e}"))?;
            (slot.into(), vec![value])
        }
        "dscp" => {
            let Some(value) = value else {
                return Err(rule_usage("dscp <0-63>"));
            };
            (
                "dscp".into(),
                vec![int_arg::<u8>(&value, 0..=63, "dscp")?.to_string()],
            )
        }
        "police" => {
            if delete {
                ("police".into(), vec![])
            } else {
                let [kw_rate, rate, kw_burst, burst] = body.get(1..5).unwrap_or_default() else {
                    return Err(rule_usage("police rate <bps|pps> burst <bytes|pkts>"));
                };
                resolve(kw_rate, &["rate"])?;
                resolve(kw_burst, &["burst"])?;
                let (_, pps) =
                    hemlock_common::net::parse_police_rate(rate).map_err(|e| format!("% {e}"))?;
                let (_, burst_pkts) =
                    hemlock_common::net::parse_police_burst(burst).map_err(|e| format!("% {e}"))?;
                let scaled = burst.to_ascii_lowercase().ends_with(['k', 'm', 'g']);
                if pps && scaled {
                    return Err("% a pps rate takes its burst in packets".into());
                }
                if !pps && burst_pkts {
                    return Err("% a bps rate takes its burst in bytes".into());
                }
                if let Some(extra) = body.get(5) {
                    return Err(format!("% Invalid input: {extra:?}"));
                }
                (
                    "police".into(),
                    vec![
                        "rate".into(),
                        rate.to_string(),
                        "burst".into(),
                        burst.to_string(),
                    ],
                )
            }
        }
        slot @ ("source-mac" | "destination-mac") => {
            let Some(value) = value else {
                return Err(rule_usage(&format!("{slot} <mac>[/<mask>]")));
            };
            let canonical = match value.split_once('/') {
                Some((mac, mask)) => format!(
                    "{}/{}",
                    hemlock_common::net::parse_mac(mac).map_err(|e| format!("% {e}"))?,
                    hemlock_common::net::parse_mac_mask(mask).map_err(|e| format!("% {e}"))?
                ),
                None => hemlock_common::net::parse_mac(&value).map_err(|e| format!("% {e}"))?,
            };
            (slot.into(), vec![canonical])
        }
        "ethertype" => {
            let Some(value) = value else {
                return Err(rule_usage("ethertype <0x0000-0xffff|ipv4|ipv6|arp>"));
            };
            let canonical = match value.as_str() {
                "ipv4" | "ipv6" | "arp" => value.clone(),
                hex => {
                    hex.strip_prefix("0x")
                        .and_then(|h| u16::from_str_radix(h, 16).ok())
                        .ok_or_else(|| {
                            format!("% bad ethertype {hex:?} (0x0000-0xffff|ipv4|ipv6|arp)")
                        })?;
                    hex.to_string()
                }
            };
            ("ethertype".into(), vec![canonical])
        }
        _ => unreachable!(),
    };
    if keyword != "police" {
        if let Some(extra) = extra.first() {
            if !(delete && keyword != "permit" && keyword != "deny") {
                return Err(format!("% Invalid input: {extra:?}"));
            }
        }
    }

    let number = number.clone();
    edit_acl(Box::new(move |block| {
        let rule = ConfigTree::ensure_block(block, "rule", &[&number]);
        if delete {
            ConfigTree::remove_leaf(rule, &leaf);
        } else {
            if leaf == "permit" {
                ConfigTree::remove_leaf(rule, "deny");
            }
            if leaf == "deny" {
                ConfigTree::remove_leaf(rule, "permit");
            }
            ConfigTree::set_leaf(rule, &leaf, values);
        }
    }))
    .await
}

/// Drop emptied `rule` blocks nowhere — rules keep meaning while their
/// block exists — but prune an emptied acl/copp chain after a removal.
fn prune_security_rules_only(tree: &mut hemlock_config::ConfigTree) {
    let security = tree.block_mut("security");
    if let Some(acl) = block_children_mut(security, "acl") {
        // ACL blocks with no rules stay: the ACL exists (implicit deny).
        let _ = acl;
    }
    remove_block_if_empty(tree, "security");
}

/// `set|delete security copp class <name> [rate <pps> | burst <pkts>]`.
async fn config_copp(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage =
        move || format!("% Usage: {verb} security copp class <name> [rate <pps> | burst <pkts>]");
    let Some(first) = words.first() else {
        if delete {
            return edit_config(endpoints, |tree| {
                if let Some(security) = block_children_mut(&mut tree.items, "security") {
                    ConfigTree::remove_block(security, "copp", &[]);
                }
                prune_security(tree);
            })
            .await
            .map_err(fmt_err);
        }
        return Err(usage());
    };
    resolve(first, &["class"])?;
    let Some(class_word) = words.get(1) else {
        return Err(usage());
    };
    let class = resolve(class_word, COPP_CLASSES)?.to_string();
    let rest = &words[2..];
    if rest.is_empty() {
        return if delete {
            edit_config(endpoints, move |tree| {
                let Some(security) = block_children_mut(&mut tree.items, "security") else {
                    return;
                };
                if let Some(copp) = block_children_mut(security, "copp") {
                    ConfigTree::remove_block(copp, "class", &[&class]);
                }
                prune_security(tree);
            })
            .await
            .map_err(fmt_err)
        } else {
            edit_config(endpoints, move |tree| {
                let security = tree.block_mut("security");
                let copp = ConfigTree::ensure_block(security, "copp", &[]);
                ConfigTree::ensure_block(copp, "class", &[&class]);
            })
            .await
            .map_err(fmt_err)
        };
    }
    let knob = resolve(rest[0], &["rate", "burst"])?.to_string();
    if delete {
        return edit_config(endpoints, move |tree| {
            let Some(security) = block_children_mut(&mut tree.items, "security") else {
                return;
            };
            let Some(copp) = block_children_mut(security, "copp") else {
                return;
            };
            if let Some(block) = keyed_block_children_mut(copp, "class", &class) {
                ConfigTree::remove_leaf(block, &knob);
            }
            prune_security(tree);
        })
        .await
        .map_err(fmt_err);
    }
    let Some(value) = rest.get(1) else {
        return Err(format!(
            "% Usage: set security copp class {class} {knob} <n>"
        ));
    };
    let value = if knob == "rate" {
        int_arg::<u32>(value, 1..=10_000_000, "rate")?.to_string()
    } else {
        int_arg::<u32>(value, 1..=1_000_000, "burst")?.to_string()
    };
    edit_config(endpoints, move |tree| {
        let security = tree.block_mut("security");
        let copp = ConfigTree::ensure_block(security, "copp", &[]);
        let block = ConfigTree::ensure_block(copp, "class", &[&class]);
        ConfigTree::set_leaf(block, &knob, vec![value]);
    })
    .await
    .map_err(fmt_err)
}

/// `set|delete security dot1x [radius-server <ip> [...] |
/// reauth-interval <0|60-86400>]`.
async fn config_dot1x(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || {
        format!(
            "% Usage: {verb} security dot1x <radius-server <ip> [key|port|timeout|retransmit ...] | reauth-interval <0|60-86400>>"
        )
    };
    let Some(first) = words.first() else {
        if delete {
            return edit_config(endpoints, |tree| {
                if let Some(security) = block_children_mut(&mut tree.items, "security") {
                    ConfigTree::remove_block(security, "dot1x", &[]);
                }
                prune_security(tree);
            })
            .await
            .map_err(fmt_err);
        }
        return Err(usage());
    };
    match resolve(first, &["radius-server", "reauth-interval"])? {
        "reauth-interval" => {
            if delete {
                return edit_config(endpoints, |tree| {
                    let Some(security) = block_children_mut(&mut tree.items, "security") else {
                        return;
                    };
                    if let Some(dot1x) = block_children_mut(security, "dot1x") {
                        ConfigTree::remove_leaf(dot1x, "reauth-interval");
                    }
                    prune_security(tree);
                })
                .await
                .map_err(fmt_err);
            }
            let Some(value) = words.get(1) else {
                return Err(usage());
            };
            let secs = int_arg::<u32>(value, 0..=86400, "reauth-interval")?;
            if secs != 0 && secs < 60 {
                return Err(format!("% bad reauth-interval {secs} (0|60-86400)"));
            }
            let secs = secs.to_string();
            edit_config(endpoints, move |tree| {
                let security = tree.block_mut("security");
                let dot1x = ConfigTree::ensure_block(security, "dot1x", &[]);
                ConfigTree::set_leaf(dot1x, "reauth-interval", vec![secs]);
            })
            .await
            .map_err(fmt_err)
        }
        "radius-server" => {
            let Some(ip_word) = words.get(1) else {
                return Err(usage());
            };
            let ip: std::net::IpAddr = ip_word
                .parse()
                .map_err(|_| format!("% bad radius-server address {ip_word:?}"))?;
            let ip = ip.to_string();
            let rest = &words[2..];
            if rest.is_empty() {
                return if delete {
                    edit_config(endpoints, move |tree| {
                        let Some(security) = block_children_mut(&mut tree.items, "security") else {
                            return;
                        };
                        if let Some(dot1x) = block_children_mut(security, "dot1x") {
                            ConfigTree::remove_block(dot1x, "radius-server", &[&ip]);
                        }
                        prune_security(tree);
                    })
                    .await
                    .map_err(fmt_err)
                } else {
                    edit_config(endpoints, move |tree| {
                        let security = tree.block_mut("security");
                        let dot1x = ConfigTree::ensure_block(security, "dot1x", &[]);
                        ConfigTree::ensure_block(dot1x, "radius-server", &[&ip]);
                    })
                    .await
                    .map_err(fmt_err)
                };
            }
            let knob = resolve(rest[0], &["key", "port", "timeout", "retransmit"])?.to_string();
            if delete {
                return edit_config(endpoints, move |tree| {
                    let Some(security) = block_children_mut(&mut tree.items, "security") else {
                        return;
                    };
                    let Some(dot1x) = block_children_mut(security, "dot1x") else {
                        return;
                    };
                    if let Some(server) = keyed_block_children_mut(dot1x, "radius-server", &ip) {
                        ConfigTree::remove_leaf(server, &knob);
                    }
                    prune_security(tree);
                })
                .await
                .map_err(fmt_err);
            }
            let Some(value) = rest.get(1) else {
                return Err(format!(
                    "% Usage: set security dot1x radius-server {ip} {knob} <value>"
                ));
            };
            let value = match knob.as_str() {
                "key" => value.to_string(),
                "port" => int_arg::<u16>(value, 1..=65535, "port")?.to_string(),
                "timeout" => int_arg::<u8>(value, 1..=60, "timeout")?.to_string(),
                "retransmit" => int_arg::<u8>(value, 0..=10, "retransmit")?.to_string(),
                _ => unreachable!(),
            };
            edit_config(endpoints, move |tree| {
                let security = tree.block_mut("security");
                let dot1x = ConfigTree::ensure_block(security, "dot1x", &[]);
                let server = ConfigTree::ensure_block(dot1x, "radius-server", &[&ip]);
                ConfigTree::set_leaf(server, &knob, vec![value]);
            })
            .await
            .map_err(fmt_err)
        }
        _ => unreachable!(),
    }
}

/// `set|delete security <dhcp-snooping|arp-inspection> vlan <id>` and
/// friends. The shared vlan-list handling lives here.
fn security_vlan_edit(feature: &'static str, id: String, delete: bool) -> BlockEdit {
    Box::new(move |block| {
        if delete {
            block.retain(|item| {
                !matches!(item, hemlock_config::Item::Leaf { name, values }
                    if name == "vlan" && values.first() == Some(&id))
            });
        } else {
            let present = block.iter().any(|item| {
                matches!(item, hemlock_config::Item::Leaf { name, values }
                    if name == "vlan" && values.first() == Some(&id))
            });
            if !present {
                push_leaf(block, "vlan", vec![id.clone()]);
            }
        }
        let _ = feature;
    })
}

/// Apply a block edit under `security { <feature> { ... } }`, creating
/// on set and pruning on delete.
async fn edit_security_feature(
    endpoints: &Endpoints,
    feature: &'static str,
    delete: bool,
    edit: BlockEdit,
) -> Result<(), String> {
    edit_config(endpoints, move |tree| {
        if delete {
            let Some(security) = block_children_mut(&mut tree.items, "security") else {
                return;
            };
            if let Some(block) = block_children_mut(security, feature) {
                edit(block);
            }
            let security = tree.block_mut("security");
            security.retain(|item| {
                !matches!(item, hemlock_config::Item::Block { name, children, .. }
                    if name == feature && children.is_empty())
            });
            remove_block_if_empty(tree, "security");
        } else {
            let security = tree.block_mut("security");
            let block = ConfigTree::ensure_block(security, feature, &[]);
            edit(block);
        }
    })
    .await
    .map_err(fmt_err)
}

/// `set|delete security dhcp-snooping <vlan <id> | binding <mac> vlan
/// <id> address <ipv4> interface <port>>`.
async fn config_dhcp_snooping(
    endpoints: &Endpoints,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || {
        format!(
            "% Usage: {verb} security dhcp-snooping <vlan <id> | binding <mac> vlan <id> address <ipv4> interface <port>>"
        )
    };
    let Some(first) = words.first() else {
        if delete {
            return edit_security_feature(
                endpoints,
                "dhcp-snooping",
                true,
                Box::new(|b| b.clear()),
            )
            .await;
        }
        return Err(usage());
    };
    match resolve(first, &["vlan", "binding", "option-82", "information"])? {
        // Deferred by the security suite.
        "option-82" | "information" => Err("% DHCP option-82 insertion is not supported".into()),
        "vlan" => {
            let Some(id) = words.get(1) else {
                return Err(usage());
            };
            let id = parse_vlan_arg(id)?.to_string();
            edit_security_feature(
                endpoints,
                "dhcp-snooping",
                delete,
                security_vlan_edit("dhcp-snooping", id, delete),
            )
            .await
        }
        "binding" => {
            let Some(mac_word) = words.get(1) else {
                return Err(usage());
            };
            let mac =
                hemlock_common::net::parse_unicast_mac(mac_word).map_err(|e| format!("% {e}"))?;
            if delete {
                if let Some(extra) = words.get(2) {
                    return Err(format!("% Invalid input: {extra:?}"));
                }
                return edit_security_feature(
                    endpoints,
                    "dhcp-snooping",
                    true,
                    Box::new(move |block| {
                        block.retain(|item| {
                            !matches!(item, hemlock_config::Item::Leaf { name, values }
                                if name == "binding" && values.first() == Some(&mac))
                        });
                    }),
                )
                .await;
            }
            let [kw_vlan, vlan, kw_address, address, kw_interface, port] =
                words.get(2..8).unwrap_or_default()
            else {
                return Err(usage());
            };
            resolve(kw_vlan, &["vlan"])?;
            resolve(kw_address, &["address"])?;
            resolve(kw_interface, &["interface"])?;
            let vlan = parse_vlan_arg(vlan)?.to_string();
            let ip: std::net::Ipv4Addr = address
                .parse()
                .map_err(|_| format!("% bad binding address {address:?} (IPv4)"))?;
            let port = match port_channel_interface(port) {
                Some(name) => name,
                None => {
                    let known = list_port_names(endpoints).await.map_err(fmt_err)?;
                    canonical_port(port, &known)?
                }
            };
            let values = vec![
                mac.clone(),
                "vlan".into(),
                vlan,
                "address".into(),
                ip.to_string(),
                "interface".into(),
                port,
            ];
            edit_security_feature(
                endpoints,
                "dhcp-snooping",
                false,
                Box::new(move |block| {
                    // One binding per (mac, vlan): replace it.
                    block.retain(|item| {
                        !matches!(item, hemlock_config::Item::Leaf { name, values: v }
                            if name == "binding"
                                && v.first() == Some(&mac)
                                && v.get(2) == values.get(2))
                    });
                    push_leaf(block, "binding", values.clone());
                }),
            )
            .await
        }
        _ => unreachable!(),
    }
}

/// `set|delete security arp-inspection <vlan <id> | validate
/// <src-mac|dst-mac|ip>>`.
async fn config_arp_inspection(
    endpoints: &Endpoints,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    let verb = if delete { "delete" } else { "set" };
    let usage = move || {
        format!(
            "% Usage: {verb} security arp-inspection <vlan <id> | validate <src-mac|dst-mac|ip>>"
        )
    };
    let Some(first) = words.first() else {
        if delete {
            return edit_security_feature(
                endpoints,
                "arp-inspection",
                true,
                Box::new(|b| b.clear()),
            )
            .await;
        }
        return Err(usage());
    };
    match resolve(first, &["vlan", "validate"])? {
        "vlan" => {
            let Some(id) = words.get(1) else {
                return Err(usage());
            };
            let id = parse_vlan_arg(id)?.to_string();
            edit_security_feature(
                endpoints,
                "arp-inspection",
                delete,
                security_vlan_edit("arp-inspection", id, delete),
            )
            .await
        }
        "validate" => {
            let Some(check) = words.get(1) else {
                return Err(usage());
            };
            let check = resolve(check, &["src-mac", "dst-mac", "ip"])?.to_string();
            edit_security_feature(
                endpoints,
                "arp-inspection",
                delete,
                Box::new(move |block| {
                    block.retain(|item| {
                        !matches!(item, hemlock_config::Item::Leaf { name, values }
                            if name == "validate" && values.first() == Some(&check))
                    });
                    if !delete {
                        push_leaf(block, "validate", vec![check.clone()]);
                    }
                }),
            )
            .await
        }
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
            "vrrp",
            "access-group",
            "port-security",
            "dot1x",
            "dhcp-snooping",
            "arp-inspection",
            "qos",
        ],
    )?;
    // SVIs carry an address (and, with the routing suite, VRRP groups)
    // and nothing else.
    if port.starts_with("Vlan") {
        // Deferred by the security suite: ACLs bind to ports only.
        if subcommand == "access-group" {
            return Err("% VLAN ACLs are not supported (port bindings only)".into());
        }
        if subcommand == "qos" {
            return Err("% QoS is a front-panel concept; configure it on the physical port".into());
        }
        if !matches!(subcommand, "address" | "vrrp") {
            return Err(format!(
                "% {subcommand} is not supported on VLAN interfaces"
            ));
        }
    }
    if port.starts_with("Management")
        && matches!(
            subcommand,
            "channel-group"
                | "lacp"
                | "spanning-tree"
                | "storm-control"
                | "min-links"
                | "vrrp"
                | "access-group"
                | "port-security"
                | "dot1x"
                | "dhcp-snooping"
                | "arp-inspection"
                | "qos"
        )
    {
        return Err(format!(
            "% {subcommand} is not supported on management ports"
        ));
    }
    let is_lag = port.starts_with("Port-Channel");
    if is_lag
        && matches!(
            subcommand,
            "address" | "channel-group" | "vrrp" | "port-security" | "dot1x"
        )
    {
        return Err(format!(
            "% {subcommand} is not supported on port-channel interfaces"
        ));
    }
    if !is_lag && subcommand == "min-links" {
        return Err("% min-links is only supported on port-channel interfaces".into());
    }
    match subcommand {
        "access-group" => {
            return config_access_group(endpoints, &port, &rest[1..], delete).await;
        }
        "port-security" => {
            return config_port_security(endpoints, &port, &rest[1..], delete).await;
        }
        "qos" => {
            return config_port_qos(endpoints, &port, &rest[1..], delete).await;
        }
        "dot1x" => {
            if let Some(extra) = rest.get(1) {
                // The deferred dot1x extensions fail at parse.
                if resolve(extra, &["vlan"]).is_ok() {
                    return Err("% dot1x dynamic VLAN assignment is not supported".into());
                }
                if resolve(extra, &["mac-auth-bypass"]).is_ok() {
                    return Err("% dot1x MAC-auth-bypass is not supported".into());
                }
                return Err(format!("% Invalid input: {extra:?}"));
            }
            return edit_interface(endpoints, &port, move |eth| {
                if delete {
                    ConfigTree::remove_leaf(eth, "dot1x");
                } else {
                    ConfigTree::set_leaf(eth, "dot1x", vec![]);
                }
            })
            .await
            .map_err(fmt_err);
        }
        feature @ ("dhcp-snooping" | "arp-inspection") => {
            // `dhcp-snooping trust` / `arp-inspection trust` markers.
            match rest.get(1) {
                Some(word) => {
                    resolve(word, &["trust"])?;
                    if let Some(extra) = rest.get(2) {
                        return Err(format!("% Invalid input: {extra:?}"));
                    }
                }
                None if !delete => {
                    return Err(format!("% Usage: set interfaces {port} {feature} trust"));
                }
                None => {}
            }
            let feature = feature.to_string();
            return edit_interface(endpoints, &port, move |eth| {
                if delete {
                    ConfigTree::remove_leaf(eth, &feature);
                } else {
                    ConfigTree::set_phrase(eth, &feature, "trust", vec![]);
                }
            })
            .await
            .map_err(fmt_err);
        }
        _ => {}
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
        "vrrp" => {
            return config_vrrp(endpoints, &port, &rest[1..], delete).await;
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
            return Err(format!("% Usage: delete interfaces {port} channel-group"));
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
    let keyword = resolve(
        words[0],
        &["portfast", "bpduguard", "cost", "port-priority"],
    )?;
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
    // All but the last word carry a trailing comma so the stored config
    // renders as `trunk vlans 10, 20, 30`.
    let ids: Vec<u16> = out.into_iter().collect();
    Ok(ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            if i + 1 < ids.len() {
                format!("{id},")
            } else {
                id.to_string()
            }
        })
        .collect())
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

/// `set routing static <prefix> <next-hop|drop> [distance <1-255>]` and
/// its deletes. Repeating a prefix with more next hops is ECMP; a
/// non-canonical prefix (host bits set) is an error, never a rewrite.
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
            "% Usage: delete routing [static|arp|router-id|ospf|bgp ...]".to_string()
        } else {
            "% Usage: set routing <static|arp|router-id|ospf|bgp> ...".to_string()
        }
    };
    let Some(first) = words.first() else {
        return Err(usage());
    };
    match resolve(first, &["static", "arp", "router-id", "ospf", "bgp"])? {
        "arp" => return config_arp(endpoints, &words[1..], delete).await,
        "router-id" => return config_router_id(endpoints, &words[1..], delete).await,
        "ospf" => return config_ospf(endpoints, &words[1..], delete).await,
        "bgp" => return config_bgp(endpoints, &words[1..], delete).await,
        _ => {}
    }
    let rest = &words[1..];

    if delete {
        match rest {
            [] => edit_config(endpoints, |tree| {
                let routing = tree.block_mut("routing");
                ConfigTree::remove_block(routing, "static", &[]);
                remove_block_if_empty(tree, "routing");
            })
            .await
            .map_err(fmt_err),
            [prefix] => {
                let prefix = canonical_route_prefix(prefix)?;
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
            [prefix, next_hop] => {
                let prefix = canonical_route_prefix(prefix)?;
                let next_hop = next_hop.to_string();
                edit_config(endpoints, |tree| {
                    let routing = tree.block_mut("routing");
                    if let Some(routes) = block_children_mut(routing, "static") {
                        routes.retain(|item| {
                            !matches!(item, hemlock_config::Item::Leaf { name, values }
                                if *name == prefix
                                    && values.first().map(String::as_str) == Some(&next_hop))
                        });
                        if routes.is_empty() {
                            ConfigTree::remove_block(routing, "static", &[]);
                        }
                    }
                    remove_block_if_empty(tree, "routing");
                })
                .await
                .map_err(fmt_err)
            }
            _ => Err(usage()),
        }
    } else {
        let (Some(prefix), Some(target)) = (rest.first(), rest.get(1)) else {
            return Err(usage());
        };
        let prefix = canonical_route_prefix(prefix)?;
        // A next hop always carries a separator; anything else resolves
        // against the keyword so `dr` works and typos error cleanly.
        let values = if target.contains('.') || target.contains(':') {
            hemlock_common::net::validate_next_hop(&prefix, target)
                .map_err(|e| format!("% {e}"))?;
            match &rest[2..] {
                [] => vec![target.to_string()],
                [keyword, value] => {
                    resolve(keyword, &["distance"])?;
                    let distance: u8 = value
                        .parse()
                        .ok()
                        .filter(|d| *d >= 1)
                        .ok_or_else(|| format!("% bad distance {value:?} (1..255)"))?;
                    vec![target.to_string(), "distance".into(), distance.to_string()]
                }
                _ => return Err(usage()),
            }
        } else {
            resolve(target, &["drop"])?;
            if rest.len() > 2 {
                return Err(usage());
            }
            vec!["drop".to_string()]
        };
        edit_config(endpoints, |tree| {
            let routing = tree.block_mut("routing");
            let routes = ConfigTree::ensure_block(routing, "static", &[]);
            set_route_leaf(routes, &prefix, values);
        })
        .await
        .map_err(fmt_err)
    }
}

/// `set routing router-id <ipv4>` / `delete routing router-id`.
async fn config_router_id(
    endpoints: &Endpoints,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    if delete {
        if !words.is_empty() {
            return Err("% Usage: delete routing router-id".into());
        }
        return edit_config(endpoints, |tree| {
            let routing = tree.block_mut("routing");
            ConfigTree::remove_leaf(routing, "router-id");
            remove_block_if_empty(tree, "routing");
        })
        .await
        .map_err(fmt_err);
    }
    let [id] = words else {
        return Err("% Usage: set routing router-id <ipv4>".into());
    };
    let id: std::net::Ipv4Addr = id
        .parse()
        .map_err(|_| format!("% bad router-id {id:?} (IPv4)"))?;
    edit_config(endpoints, move |tree| {
        let routing = tree.block_mut("routing");
        ConfigTree::set_leaf(routing, "router-id", vec![id.to_string()]);
    })
    .await
    .map_err(fmt_err)
}

/// The canonical dotted form of an OSPF area id (dotted or integer).
fn canonical_area(text: &str) -> Result<String, String> {
    if let Ok(area) = text.parse::<std::net::Ipv4Addr>() {
        return Ok(area.to_string());
    }
    text.parse::<u32>()
        .map(|n| std::net::Ipv4Addr::from(n).to_string())
        .map_err(|_| format!("% bad area {text:?} (dotted or 0..4294967295)"))
}

/// A bounded integer CLI argument with the customary `%` error.
fn int_arg<T>(value: &str, range: std::ops::RangeInclusive<T>, what: &str) -> Result<T, String>
where
    T: std::str::FromStr + PartialOrd + std::fmt::Display + Copy,
{
    value
        .parse::<T>()
        .ok()
        .filter(|n| range.contains(n))
        .ok_or_else(|| {
            format!(
                "% bad {what} {value:?} ({}..{})",
                range.start(),
                range.end()
            )
        })
}

/// An interface argument for the routing families: `Vlan<id>`, a
/// port-channel form, or a (prefix-resolved) port name.
async fn l3_interface_arg(endpoints: &Endpoints, input: &str) -> Result<String, String> {
    if let Some(svi) = vlan_interface(input) {
        return Ok(svi);
    }
    if let Some(lag) = port_channel_interface(input) {
        return Ok(lag);
    }
    let known = list_port_names(endpoints).await.map_err(fmt_err)?;
    canonical_port(input, &known)
}

/// `set|delete routing ospf ...` (Part 1.2's OSPFv2 family).
async fn config_ospf(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    const SET_USAGE: &str = "% Usage: set routing ospf <area <id> network <prefix> | router-id <ipv4> | passive-interface <if> | redistribute <connected|static|bgp> | maximum-paths <1-8> | interface <if> <cost|hello-interval|dead-interval|priority> <n>>";
    const DELETE_USAGE: &str = "% Usage: delete routing ospf [area <id> [network <prefix>] | router-id | passive-interface <if> | redistribute <src> | maximum-paths | interface <if> [<knob>]]";
    let usage = move || {
        if delete {
            DELETE_USAGE.to_string()
        } else {
            SET_USAGE.to_string()
        }
    };
    // The scoped edit: run `edit` on the ospf block's children, then
    // prune emptied blocks.
    let ospf_edit = |edit: BlockEdit| async move {
        edit_config(endpoints, |tree| {
            let routing = tree.block_mut("routing");
            let ospf = ConfigTree::ensure_block(routing, "ospf", &[]);
            edit(ospf);
            if ospf.is_empty() {
                ConfigTree::remove_block(routing, "ospf", &[]);
            }
            remove_block_if_empty(tree, "routing");
        })
        .await
        .map_err(fmt_err)
    };

    if delete && words.is_empty() {
        return edit_config(endpoints, |tree| {
            let routing = tree.block_mut("routing");
            ConfigTree::remove_block(routing, "ospf", &[]);
            remove_block_if_empty(tree, "routing");
        })
        .await
        .map_err(fmt_err);
    }
    let Some(first) = words.first() else {
        return Err(usage());
    };
    let keyword = resolve(
        first,
        &[
            "area",
            "router-id",
            "passive-interface",
            "redistribute",
            "maximum-paths",
            "interface",
        ],
    )?;
    let rest = &words[1..];
    match (keyword, delete) {
        ("area", false) => {
            let [area, keyword_network, prefix] = rest else {
                return Err(usage());
            };
            resolve(keyword_network, &["network"])?;
            let area = canonical_area(area)?;
            let prefix = canonical_route_prefix(prefix)?;
            if prefix.contains(':') {
                return Err("% OSPFv3 is not supported".into());
            }
            ospf_edit(Box::new(move |ospf| {
                let networks = ConfigTree::ensure_block(ospf, "area", &[&area]);
                remove_leaf_matching(networks, "network", &[&prefix]);
                push_leaf(networks, "network", vec![prefix]);
            }))
            .await
        }
        ("area", true) => match rest {
            [area] => {
                let area = canonical_area(area)?;
                ospf_edit(Box::new(move |ospf| {
                    ConfigTree::remove_block(ospf, "area", &[&area]);
                }))
                .await
            }
            [area, keyword_network, prefix] => {
                resolve(keyword_network, &["network"])?;
                let area = canonical_area(area)?;
                let prefix = canonical_route_prefix(prefix)?;
                ospf_edit(Box::new(move |ospf| {
                    if let Some(networks) = keyed_block_children_mut(ospf, "area", &area) {
                        remove_leaf_matching(networks, "network", &[&prefix]);
                        if networks.is_empty() {
                            ConfigTree::remove_block(ospf, "area", &[&area]);
                        }
                    }
                }))
                .await
            }
            _ => Err(usage()),
        },
        ("router-id", false) => {
            let [id] = rest else { return Err(usage()) };
            let id: std::net::Ipv4Addr = id
                .parse()
                .map_err(|_| format!("% bad router-id {id:?} (IPv4)"))?;
            ospf_edit(Box::new(move |ospf| {
                ConfigTree::set_leaf(ospf, "router-id", vec![id.to_string()]);
            }))
            .await
        }
        ("router-id" | "maximum-paths", true) => {
            if !rest.is_empty() {
                return Err(usage());
            }
            let leaf = keyword.to_string();
            ospf_edit(Box::new(move |ospf| {
                ConfigTree::remove_leaf(ospf, &leaf);
            }))
            .await
        }
        ("passive-interface", set_delete) => {
            let [interface] = rest else {
                return Err(usage());
            };
            let interface = l3_interface_arg(endpoints, interface).await?;
            ospf_edit(Box::new(move |ospf| {
                remove_leaf_matching(ospf, "passive-interface", &[&interface]);
                if !set_delete {
                    push_leaf(ospf, "passive-interface", vec![interface]);
                }
            }))
            .await
        }
        ("redistribute", set_delete) => {
            let [source] = rest else { return Err(usage()) };
            let source = resolve(source, &["connected", "static", "bgp"])?.to_string();
            ospf_edit(Box::new(move |ospf| {
                remove_leaf_matching(ospf, "redistribute", &[&source]);
                if !set_delete {
                    push_leaf(ospf, "redistribute", vec![source]);
                }
            }))
            .await
        }
        ("maximum-paths", false) => {
            let [paths] = rest else { return Err(usage()) };
            let paths = int_arg::<u8>(paths, 1..=8, "maximum-paths")?;
            ospf_edit(Box::new(move |ospf| {
                ConfigTree::set_leaf(ospf, "maximum-paths", vec![paths.to_string()]);
            }))
            .await
        }
        ("interface", false) => {
            let [interface, knob, value] = rest else {
                return Err(usage());
            };
            let interface = l3_interface_arg(endpoints, interface).await?;
            let knob = resolve(
                knob,
                &["cost", "hello-interval", "dead-interval", "priority"],
            )?
            .to_string();
            let value = match knob.as_str() {
                "priority" => int_arg::<u16>(value, 0..=255, &knob)?,
                _ => int_arg::<u16>(value, 1..=65535, &knob)?,
            };
            ospf_edit(Box::new(move |ospf| {
                let block = ConfigTree::ensure_block(ospf, "interface", &[&interface]);
                ConfigTree::set_leaf(block, &knob, vec![value.to_string()]);
            }))
            .await
        }
        ("interface", true) => match rest {
            [interface] => {
                let interface = l3_interface_arg(endpoints, interface).await?;
                ospf_edit(Box::new(move |ospf| {
                    ConfigTree::remove_block(ospf, "interface", &[&interface]);
                }))
                .await
            }
            [interface, knob] => {
                let interface = l3_interface_arg(endpoints, interface).await?;
                let knob = resolve(
                    knob,
                    &["cost", "hello-interval", "dead-interval", "priority"],
                )?
                .to_string();
                ospf_edit(Box::new(move |ospf| {
                    if let Some(block) = keyed_block_children_mut(ospf, "interface", &interface) {
                        ConfigTree::remove_leaf(block, &knob);
                        if block.is_empty() {
                            ConfigTree::remove_block(ospf, "interface", &[&interface]);
                        }
                    }
                }))
                .await
            }
            _ => Err(usage()),
        },
        _ => Err(usage()),
    }
}

/// `set|delete routing bgp ...` (Part 1.2's BGP IPv4-unicast family).
async fn config_bgp(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    const SET_USAGE: &str = "% Usage: set routing bgp <as <1-4294967295> | router-id <ipv4> | neighbor <ip> <remote-as <asn>|description <text>|shutdown|ebgp-multihop <1-255>|next-hop-self> | network <prefix> | redistribute <connected|static|ospf> | maximum-paths <1-8>>";
    const DELETE_USAGE: &str = "% Usage: delete routing bgp [as | router-id | neighbor <ip> [<knob>] | network <prefix> | redistribute <src> | maximum-paths]";
    let usage = move || {
        if delete {
            DELETE_USAGE.to_string()
        } else {
            SET_USAGE.to_string()
        }
    };
    let bgp_edit = |edit: BlockEdit| async move {
        edit_config(endpoints, |tree| {
            let routing = tree.block_mut("routing");
            let bgp = ConfigTree::ensure_block(routing, "bgp", &[]);
            edit(bgp);
            if bgp.is_empty() {
                ConfigTree::remove_block(routing, "bgp", &[]);
            }
            remove_block_if_empty(tree, "routing");
        })
        .await
        .map_err(fmt_err)
    };

    if delete && words.is_empty() {
        return edit_config(endpoints, |tree| {
            let routing = tree.block_mut("routing");
            ConfigTree::remove_block(routing, "bgp", &[]);
            remove_block_if_empty(tree, "routing");
        })
        .await
        .map_err(fmt_err);
    }
    let Some(first) = words.first() else {
        return Err(usage());
    };
    let keyword = resolve(
        first,
        &[
            "as",
            "router-id",
            "neighbor",
            "network",
            "redistribute",
            "maximum-paths",
        ],
    )?;
    let rest = &words[1..];
    match (keyword, delete) {
        ("as", false) => {
            let [as_number] = rest else {
                return Err(usage());
            };
            let as_number = int_arg::<u32>(as_number, 1..=4294967295, "as")?;
            bgp_edit(Box::new(move |bgp| {
                ConfigTree::set_leaf(bgp, "as", vec![as_number.to_string()]);
            }))
            .await
        }
        ("as" | "router-id" | "maximum-paths", true) => {
            if !rest.is_empty() {
                return Err(usage());
            }
            let leaf = keyword.to_string();
            bgp_edit(Box::new(move |bgp| {
                ConfigTree::remove_leaf(bgp, &leaf);
            }))
            .await
        }
        ("router-id", false) => {
            let [id] = rest else { return Err(usage()) };
            let id: std::net::Ipv4Addr = id
                .parse()
                .map_err(|_| format!("% bad router-id {id:?} (IPv4)"))?;
            bgp_edit(Box::new(move |bgp| {
                ConfigTree::set_leaf(bgp, "router-id", vec![id.to_string()]);
            }))
            .await
        }
        ("neighbor", false) => {
            let (Some(ip), Some(knob)) = (rest.first(), rest.get(1)) else {
                return Err(usage());
            };
            let ip: std::net::IpAddr = ip
                .parse()
                .map_err(|_| format!("% bad neighbor address {ip:?}"))?;
            if ip.is_ipv6() {
                return Err("% the BGP IPv6 address family is not supported".into());
            }
            let ip = ip.to_string();
            let knob = resolve(
                knob,
                &[
                    "remote-as",
                    "description",
                    "shutdown",
                    "ebgp-multihop",
                    "next-hop-self",
                ],
            )?;
            let values = &rest[2..];
            let edit: BlockEdit = match knob {
                "remote-as" => {
                    let [remote] = values else {
                        return Err(usage());
                    };
                    let remote = int_arg::<u32>(remote, 1..=4294967295, "remote-as")?;
                    Box::new(move |neighbor| {
                        ConfigTree::set_leaf(neighbor, "remote-as", vec![remote.to_string()]);
                    })
                }
                "description" => {
                    let text = values.join(" ");
                    if text.is_empty() {
                        return Err(usage());
                    }
                    Box::new(move |neighbor| {
                        ConfigTree::set_leaf(neighbor, "description", vec![text]);
                    })
                }
                "ebgp-multihop" => {
                    let [ttl] = values else { return Err(usage()) };
                    let ttl = int_arg::<u8>(ttl, 1..=255, "ebgp-multihop")?;
                    Box::new(move |neighbor| {
                        ConfigTree::set_leaf(neighbor, "ebgp-multihop", vec![ttl.to_string()]);
                    })
                }
                marker @ ("shutdown" | "next-hop-self") => {
                    if !values.is_empty() {
                        return Err(usage());
                    }
                    let marker = marker.to_string();
                    Box::new(move |neighbor| {
                        ConfigTree::set_leaf(neighbor, &marker, vec![]);
                    })
                }
                _ => unreachable!(),
            };
            bgp_edit(Box::new(move |bgp| {
                let neighbor = ConfigTree::ensure_block(bgp, "neighbor", &[&ip]);
                edit(neighbor);
            }))
            .await
        }
        ("neighbor", true) => {
            let Some(ip) = rest.first() else {
                return Err(usage());
            };
            let ip: std::net::IpAddr = ip
                .parse()
                .map_err(|_| format!("% bad neighbor address {ip:?}"))?;
            let ip = ip.to_string();
            match rest.get(1) {
                None => {
                    bgp_edit(Box::new(move |bgp| {
                        ConfigTree::remove_block(bgp, "neighbor", &[&ip]);
                    }))
                    .await
                }
                Some(knob) => {
                    if rest.len() > 2 {
                        return Err(usage());
                    }
                    let knob = resolve(
                        knob,
                        &[
                            "remote-as",
                            "description",
                            "shutdown",
                            "ebgp-multihop",
                            "next-hop-self",
                        ],
                    )?
                    .to_string();
                    bgp_edit(Box::new(move |bgp| {
                        if let Some(neighbor) = keyed_block_children_mut(bgp, "neighbor", &ip) {
                            ConfigTree::remove_leaf(neighbor, &knob);
                            if neighbor.is_empty() {
                                ConfigTree::remove_block(bgp, "neighbor", &[&ip]);
                            }
                        }
                    }))
                    .await
                }
            }
        }
        ("network", set_delete) => {
            let [prefix] = rest else { return Err(usage()) };
            let prefix = canonical_route_prefix(prefix)?;
            if prefix.contains(':') {
                return Err("% the BGP IPv6 address family is not supported".into());
            }
            bgp_edit(Box::new(move |bgp| {
                remove_leaf_matching(bgp, "network", &[&prefix]);
                if !set_delete {
                    push_leaf(bgp, "network", vec![prefix]);
                }
            }))
            .await
        }
        ("redistribute", set_delete) => {
            let [source] = rest else { return Err(usage()) };
            let source = resolve(source, &["connected", "static", "ospf"])?.to_string();
            bgp_edit(Box::new(move |bgp| {
                remove_leaf_matching(bgp, "redistribute", &[&source]);
                if !set_delete {
                    push_leaf(bgp, "redistribute", vec![source]);
                }
            }))
            .await
        }
        ("maximum-paths", false) => {
            let [paths] = rest else { return Err(usage()) };
            let paths = int_arg::<u8>(paths, 1..=8, "maximum-paths")?;
            bgp_edit(Box::new(move |bgp| {
                ConfigTree::set_leaf(bgp, "maximum-paths", vec![paths.to_string()]);
            }))
            .await
        }
        _ => Err(usage()),
    }
}

/// `set|delete interfaces <name> vrrp <group> ...` (per-interface VRRP
/// groups, IPv4).
async fn config_vrrp(
    endpoints: &Endpoints,
    port: &str,
    words: &[&str],
    delete: bool,
) -> Result<(), String> {
    const SET_USAGE: &str = "% Usage: set interfaces <if> vrrp <1-255> [address <ipv4> | priority <1-254> | advertisement-interval <1-40> | no-preempt]";
    let usage = move || {
        if delete {
            "% Usage: delete interfaces <if> vrrp <group> [address <ip> | priority | advertisement-interval | no-preempt]".to_string()
        } else {
            SET_USAGE.to_string()
        }
    };
    let Some(group) = words.first() else {
        return Err(usage());
    };
    let group = int_arg::<u16>(group, 1..=255, "vrrp group")?.to_string();
    let rest = &words[1..];

    if delete {
        return match rest {
            [] => edit_interface(endpoints, port, move |iface| {
                ConfigTree::remove_block(iface, "vrrp", &[&group]);
            })
            .await
            .map_err(fmt_err),
            [knob, values @ ..] => {
                let knob = resolve(
                    knob,
                    &[
                        "address",
                        "priority",
                        "advertisement-interval",
                        "no-preempt",
                    ],
                )?
                .to_string();
                let address = match (knob.as_str(), values) {
                    ("address", [ip]) => Some(canonical_ip(ip)?),
                    ("address", []) => None,
                    (_, []) => None,
                    _ => return Err(usage()),
                };
                edit_interface(endpoints, port, move |iface| {
                    if let Some(body) = keyed_block_children_mut(iface, "vrrp", &group) {
                        match (&knob[..], &address) {
                            ("address", Some(ip)) => remove_leaf_matching(body, "address", &[ip]),
                            ("address", None) => ConfigTree::remove_leaf(body, "address"),
                            (knob, _) => ConfigTree::remove_leaf(body, knob),
                        }
                    }
                })
                .await
                .map_err(fmt_err)
            }
        };
    }

    match rest {
        [] => edit_interface(endpoints, port, move |iface| {
            ConfigTree::ensure_block(iface, "vrrp", &[&group]);
        })
        .await
        .map_err(fmt_err),
        [knob, values @ ..] => {
            let knob = resolve(
                knob,
                &[
                    "address",
                    "priority",
                    "advertisement-interval",
                    "no-preempt",
                ],
            )?;
            let edit: BlockEdit = match (knob, values) {
                ("address", [ip]) => {
                    let ip = ip
                        .parse::<std::net::Ipv4Addr>()
                        .map_err(|_| format!("% bad address {ip:?} (IPv4)"))?
                        .to_string();
                    Box::new(move |body| {
                        remove_leaf_matching(body, "address", &[&ip]);
                        push_leaf(body, "address", vec![ip]);
                    })
                }
                ("priority", [priority]) => {
                    let priority = int_arg::<u8>(priority, 1..=254, "priority")?;
                    Box::new(move |body| {
                        ConfigTree::set_leaf(body, "priority", vec![priority.to_string()]);
                    })
                }
                ("advertisement-interval", [interval]) => {
                    let interval = int_arg::<u8>(interval, 1..=40, "advertisement-interval")?;
                    Box::new(move |body| {
                        ConfigTree::set_leaf(
                            body,
                            "advertisement-interval",
                            vec![interval.to_string()],
                        );
                    })
                }
                ("no-preempt", []) => Box::new(move |body| {
                    ConfigTree::set_leaf(body, "no-preempt", vec![]);
                }),
                _ => return Err(usage()),
            };
            edit_interface(endpoints, port, move |iface| {
                let body = ConfigTree::ensure_block(iface, "vrrp", &[&group]);
                edit(body);
            })
            .await
            .map_err(fmt_err)
        }
    }
}

/// `set routing arp <ip> interface <port|Vlan<id>> mac <mac>` and its
/// deletes. Addresses are canonicalized; MACs canonicalize to colons.
async fn config_arp(endpoints: &Endpoints, words: &[&str], delete: bool) -> Result<(), String> {
    if delete {
        return match words {
            [] => edit_config(endpoints, |tree| {
                let routing = tree.block_mut("routing");
                ConfigTree::remove_block(routing, "arp", &[]);
                remove_block_if_empty(tree, "routing");
            })
            .await
            .map_err(fmt_err),
            [ip] => {
                let ip = canonical_ip(ip)?;
                edit_config(endpoints, |tree| {
                    let routing = tree.block_mut("routing");
                    if let Some(entries) = block_children_mut(routing, "arp") {
                        ConfigTree::remove_leaf(entries, &ip);
                        if entries.is_empty() {
                            ConfigTree::remove_block(routing, "arp", &[]);
                        }
                    }
                    remove_block_if_empty(tree, "routing");
                })
                .await
                .map_err(fmt_err)
            }
            _ => Err("% Usage: delete routing arp [<ip>]".into()),
        };
    }
    const USAGE: &str = "% Usage: set routing arp <ip> interface <port|Vlan<id>> mac <mac>";
    let [ip, keyword_interface, interface, keyword_mac, mac] = words else {
        return Err(USAGE.into());
    };
    resolve(keyword_interface, &["interface"])?;
    resolve(keyword_mac, &["mac"])?;
    let ip = canonical_ip(ip)?;
    let interface = if let Some(svi) = vlan_interface(interface) {
        svi
    } else if let Some(lag) = port_channel_interface(interface) {
        lag
    } else {
        let known = list_port_names(endpoints).await.map_err(fmt_err)?;
        canonical_port(interface, &known)?
    };
    let mac = hemlock_common::net::parse_unicast_mac(mac).map_err(|e| format!("% {e}"))?;
    edit_config(endpoints, |tree| {
        let routing = tree.block_mut("routing");
        let entries = ConfigTree::ensure_block(routing, "arp", &[]);
        ConfigTree::set_leaf(
            entries,
            &ip,
            vec!["interface".into(), interface, "mac".into(), mac],
        );
    })
    .await
    .map_err(fmt_err)
}

/// A canonical IP address argument (v4 or v6).
fn canonical_ip(text: &str) -> Result<String, String> {
    text.parse::<std::net::IpAddr>()
        .map(|ip| ip.to_string())
        .map_err(|_| format!("% bad IP address {text:?}"))
}

/// The canonical form of a route prefix argument, erroring (not
/// rewriting) when host bits are set.
fn canonical_route_prefix(prefix: &str) -> Result<String, String> {
    hemlock_common::net::require_canonical_prefix(prefix).map_err(|e| format!("% {prefix}: {e}"))
}

/// Insert or update one static-route line. Route leaves repeat per
/// prefix (ECMP), so the line's identity is (prefix, first value) and a
/// new line lands next to the prefix's existing ones.
fn set_route_leaf(routes: &mut Vec<hemlock_config::Item>, prefix: &str, values: Vec<String>) {
    let target = values.first().cloned();
    for item in routes.iter_mut() {
        if let hemlock_config::Item::Leaf {
            name,
            values: existing,
        } = item
        {
            if name == prefix && existing.first() == target.as_ref() {
                *existing = values;
                return;
            }
        }
    }
    let insert_at = routes
        .iter()
        .rposition(|item| item.name() == prefix)
        .map(|i| i + 1)
        .unwrap_or(routes.len());
    routes.insert(
        insert_at,
        hemlock_config::Item::Leaf {
            name: prefix.to_string(),
            values,
        },
    );
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
                    vec![
                        mac.clone(),
                        "vlan".into(),
                        id.clone(),
                        "interface".into(),
                        port,
                    ]
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
    fn acl_names_validate_at_the_prompt() {
        assert!(valid_acl_name("EDGE-IN"));
        assert!(valid_acl_name("a"));
        assert!(valid_acl_name("A2345678901234567890123456789012")); // 32
        assert!(!valid_acl_name("A23456789012345678901234567890123")); // 33
        assert!(!valid_acl_name("9BAD"));
        assert!(!valid_acl_name("-BAD"));
        assert!(!valid_acl_name(""));
        assert!(!valid_acl_name("has space"));
    }

    #[test]
    fn candidate_acl_names_come_from_the_security_block() {
        let tree = hemlock_config::parse(
            "security { acl { ipv4 EDGE-IN { } mac IOT-MAC { } ipv6 MGMT6-IN { } } }",
        )
        .unwrap();
        assert_eq!(
            candidate_acl_names(&tree),
            vec!["EDGE-IN", "IOT-MAC", "MGMT6-IN"]
        );
        assert!(candidate_acl_names(&hemlock_config::ConfigTree::default()).is_empty());
    }

    #[test]
    fn wred_names_validate_at_the_prompt() {
        assert!(valid_wred_name("BULK"));
        assert!(valid_wred_name("a"));
        assert!(valid_wred_name("A2345678901234567890123456789012")); // 32
        assert!(!valid_wred_name("A23456789012345678901234567890123")); // 33
        assert!(!valid_wred_name("9BAD"));
        assert!(!valid_wred_name(""));
        assert!(!valid_wred_name("has space"));
    }

    #[test]
    fn candidate_wred_names_come_from_the_qos_block() {
        let tree = hemlock_config::parse(
            "qos { wred-profile VOICE { } wred-profile BULK { } map { dscp-to-tc { } } }",
        )
        .unwrap();
        assert_eq!(candidate_wred_names(&tree), vec!["BULK", "VOICE"]);
        assert!(candidate_wred_names(&hemlock_config::ConfigTree::default()).is_empty());
    }

    /// Map entries are per-value phrase leaves, so a list expands and a
    /// single value deletes on its own.
    #[test]
    fn qos_map_entries_are_per_value_leaves() {
        let mut items: Vec<hemlock_config::Item> = Vec::new();
        for dscp in [40, 41, 42, 48] {
            set_map_entry(&mut items, "dscp", dscp, "tc", 5);
        }
        assert_eq!(items.len(), 4);
        // A later set for the same value replaces it.
        set_map_entry(&mut items, "dscp", 42, "tc", 3);
        assert_eq!(items.len(), 4);
        let entry = items
            .iter()
            .find(|item| {
                matches!(item, hemlock_config::Item::Leaf { values, .. }
                    if values.first().map(String::as_str) == Some("42"))
            })
            .unwrap();
        assert_eq!(
            entry,
            &hemlock_config::Item::Leaf {
                name: "dscp".into(),
                values: vec!["42".into(), "tc".into(), "3".into()],
            }
        );
        // One value out, then the whole table.
        remove_map_entries(&mut items, "dscp", &[41]);
        assert_eq!(items.len(), 3);
        remove_map_entries(&mut items, "dscp", &[]);
        assert!(items.is_empty());
    }

    /// The `qos { map { <table> { } } }` scaffolding collapses when its
    /// last entry goes; a named profile block does not.
    #[test]
    fn prune_qos_collapses_emptied_map_tables() {
        let mut tree =
            hemlock_config::parse("qos { map { dscp-to-tc { } cos-to-tc { } } }").unwrap();
        prune_qos(&mut tree);
        assert_eq!(tree.to_text().trim(), "");

        let mut tree = hemlock_config::parse("qos { map { } wred-profile BULK { } }").unwrap();
        prune_qos(&mut tree);
        assert!(tree.to_text().contains("wred-profile BULK"));
        assert!(!tree.to_text().contains("map"));
    }

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
        assert_eq!(
            port_channel_interface("po64"),
            Some("Port-Channel64".into())
        );
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
