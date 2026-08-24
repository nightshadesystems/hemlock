//! Tab completion for the interactive CLI.
//!
//! A rustyline [`Helper`] over the same command tree the mode handlers in
//! `cli.rs` accept. Completion is context-sensitive per mode, resolves
//! EOS-style unique prefixes in already-typed words (`sh int<TAB>` works),
//! and completes interface names from a port cache the CLI refreshes from
//! syncd in the background.

use std::sync::{Arc, Mutex};

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::Helper;

/// Which command tree applies; mirrors `cli::Mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliMode {
    Operational,
    Config,
}

/// Shared with the CLI loop: it updates `mode` on every prompt and a
/// background task keeps `ports` fresh from syncd (and `acls` from the
/// mgmtd candidate, so ACL names complete while being configured).
pub struct State {
    pub mode: CliMode,
    pub ports: Vec<String>,
    pub acls: Vec<String>,
}

pub struct CliHelper {
    pub state: Arc<Mutex<State>>,
}

/// Sentinel in the word tables meaning "an interface name goes here".
const PORT: &str = "\0port";

/// Sentinel meaning "a number goes here" (VLAN ids). Not offered as a
/// completion; it only lets deeper levels key off "a number was given".
const NUM: &str = "\0num";

/// Sentinel meaning "free text goes here" (MAC addresses, VLAN lists).
/// Accepts any token so deeper levels stay completable; offers nothing.
const ANY: &str = "\0any";

/// Sentinel meaning "an ACL name goes here". Offers the names cached
/// from the mgmtd candidate, but accepts any token (a new ACL's name
/// is completable nowhere).
const ACL: &str = "\0acl";

/// The `show interfaces` subcommand words (the interface argument is
/// optional and completes separately via [`PORT`]).
const INTERFACES_SUBCOMMANDS: &[&str] = &[
    "description",
    "status",
    "counters",
    "transceiver",
    "capabilities",
    "flowcontrol",
    "negotiation",
    "phy",
    "mac",
    "switchport",
    "trunk",
    "vlans",
];

const INTERFACES_START: &[&str] = &[
    PORT,
    "description",
    "status",
    "counters",
    "transceiver",
    "capabilities",
    "flowcontrol",
    "negotiation",
    "phy",
    "mac",
    "switchport",
    "trunk",
    "vlans",
];

/// Completion below `show interfaces`, with the optional leading
/// interface argument already normalized to [`PORT`].
fn interfaces_words(path: &[&str]) -> &'static [&'static str] {
    let (had_port, rest) = match path.split_first() {
        Some((&PORT, rest)) => (true, rest),
        _ => (false, path),
    };
    match rest {
        [] if had_port => INTERFACES_SUBCOMMANDS,
        [] => INTERFACES_START,
        ["status"] => &["connected", "notconnect", "errdisabled", "inactive"],
        ["counters"] => &["errors", "discards", "rates", "queue", "bins"],
        ["transceiver"] => &["detail", "properties", "eeprom"],
        ["negotiation" | "phy" | "mac"] => &["detail"],
        _ => &[],
    }
}

/// The words that may follow the canonical `path` in `mode`. Empty means
/// nothing completable (free text, or the command is complete).
fn next_words(mode: CliMode, path: &[&str]) -> &'static [&'static str] {
    match (mode, path) {
        (CliMode::Operational, []) => &[
            "show",
            "configure",
            "clear",
            "upgrade",
            "bash",
            "exit",
            "quit",
            "logout",
            "help",
        ],
        (CliMode::Operational, ["show"]) => &[
            "interfaces",
            "environment",
            "configuration",
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
        ],
        (CliMode::Operational, ["show", "acl"]) => &["summary", ACL],
        (CliMode::Operational, ["show", "port-security" | "dot1x"]) => &["interface"],
        (CliMode::Operational, ["show", "port-security" | "dot1x", "interface"]) => &[PORT],
        (CliMode::Operational, ["show", "dhcp"]) => &["snooping"],
        (CliMode::Operational, ["show", "dhcp", "snooping"]) => &["binding", "statistics"],
        (CliMode::Operational, ["show", "arp"]) => &["inspection"],
        (CliMode::Operational, ["show", "arp", "inspection"]) => &["statistics"],
        (CliMode::Operational, ["show", "routing"]) => &["ospf", "bgp"],
        (CliMode::Operational, ["show", "routing", "ospf"]) => &["neighbor", "interface"],
        (CliMode::Operational, ["show", "routing", "bgp"]) => &["summary", "neighbors"],
        (CliMode::Operational, ["show", "vrrp"]) => &["brief"],
        (CliMode::Operational, ["show", "ip"]) => &["route"],
        (CliMode::Operational, ["show", "ipv6"]) => &["route", "neighbors"],
        (CliMode::Operational, ["show", "ip" | "ipv6", "route"]) => &["summary", ANY],
        (CliMode::Operational, ["show", "igmp" | "mld"]) => &["snooping"],
        (CliMode::Operational, ["show", "igmp" | "mld", "snooping"]) => &["groups", "querier"],
        (CliMode::Operational, ["show", "interfaces", rest @ ..]) => interfaces_words(rest),
        (CliMode::Operational, ["show", "spanning-tree"]) => &["detail", "blockedports", "mst"],
        (CliMode::Operational, ["show", "spanning-tree", "mst"]) => &["configuration"],
        (CliMode::Operational, ["show", "vlan"]) => &["id", "summary"],
        (CliMode::Operational, ["show", "port-channel"]) => &["summary", "detail", NUM],
        (CliMode::Operational, ["show", "port-channel", NUM]) => &["summary", "detail"],
        (CliMode::Operational, ["show", "lacp"]) => &["neighbor", "counters", "sys-id"],
        (CliMode::Operational, ["show", "lacp", "neighbor"]) => &["detail"],
        (CliMode::Operational, ["show", "mac"]) => &["address-table"],
        (CliMode::Operational, ["show", "mac", "address-table"]) => &[
            "count",
            "aging-time",
            "vlan",
            "interface",
            "address",
            "static",
            "dynamic",
        ],
        (CliMode::Operational, ["show", "mac", "address-table", "vlan"]) => &[NUM],
        (CliMode::Operational, ["show", "mac", "address-table", "interface"]) => &[PORT],
        (CliMode::Operational, ["show", "monitor"]) => &["session"],
        (CliMode::Operational, ["clear"]) => &[
            "counters",
            "mac-table",
            "arp",
            "routing",
            "acl",
            "copp",
            "port-security",
            "dhcp",
            "dot1x",
        ],
        (CliMode::Operational, ["clear", "acl" | "copp"]) => &["counters"],
        (CliMode::Operational, ["clear", "acl", "counters"]) => &[ACL],
        (CliMode::Operational, ["clear", "port-security"]) => &["interface"],
        (CliMode::Operational, ["clear", "port-security", "interface"]) => &[PORT],
        (CliMode::Operational, ["clear", "dhcp"]) => &["snooping"],
        (CliMode::Operational, ["clear", "dhcp", "snooping"]) => &["binding"],
        (CliMode::Operational, ["clear", "dhcp", "snooping", "binding"]) => &[ANY],
        (CliMode::Operational, ["clear", "dot1x"]) => &["interface"],
        (CliMode::Operational, ["clear", "dot1x", "interface"]) => &[PORT],
        (CliMode::Operational, ["clear", "arp"]) => &[ANY],
        (CliMode::Operational, ["clear", "routing"]) => &["bgp"],
        (CliMode::Operational, ["clear", "routing", "bgp"]) => &[ANY],
        (CliMode::Operational, ["clear", "counters"]) => &[PORT],
        (CliMode::Operational, ["clear", "mac-table"]) => &["vlan", "interface"],
        (CliMode::Operational, ["clear", "mac-table", "vlan"]) => &[NUM],
        (CliMode::Operational, ["clear", "mac-table", "vlan", NUM]) => &["interface"],
        (CliMode::Operational, ["clear", "mac-table", "vlan", NUM, "interface"]) => &[PORT],
        (CliMode::Operational, ["clear", "mac-table", "interface"]) => &[PORT],
        (CliMode::Config, []) => &[
            "set", "delete", "show", "commit", "rollback", "discard", "exit", "help",
        ],
        (CliMode::Config, ["set" | "delete"]) => &[
            "interfaces",
            "system",
            "routing",
            "vlans",
            "protocols",
            "switching",
            "security",
        ],
        (CliMode::Config, ["set" | "delete", "interfaces"]) => &[PORT],
        (CliMode::Config, ["set" | "delete", "interfaces", PORT]) => &[
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
        ],
        (CliMode::Config, ["set", "interfaces", PORT, "access-group"]) => &[ACL],
        (CliMode::Config, ["set", "interfaces", PORT, "access-group", ACL]) => &["in", "out"],
        (CliMode::Config, ["delete", "interfaces", PORT, "access-group"]) => &["in", "out"],
        (CliMode::Config, ["set" | "delete", "interfaces", PORT, "port-security"]) => {
            &["maximum", "violation"]
        }
        (CliMode::Config, ["set", "interfaces", PORT, "port-security", "maximum"]) => &[NUM],
        (CliMode::Config, ["set", "interfaces", PORT, "port-security", "violation"]) => {
            &["protect", "shutdown"]
        }
        (CliMode::Config, ["set", "interfaces", PORT, "dhcp-snooping" | "arp-inspection"]) => {
            &["trust"]
        }
        (CliMode::Config, ["set" | "delete", "interfaces", PORT, "vrrp"]) => &[NUM],
        (CliMode::Config, ["set" | "delete", "interfaces", PORT, "vrrp", NUM]) => &[
            "address",
            "priority",
            "advertisement-interval",
            "no-preempt",
        ],
        (CliMode::Config, ["set" | "delete", "interfaces", PORT, "switchport"]) => {
            &["mode", "access", "trunk"]
        }
        (CliMode::Config, ["set", "interfaces", PORT, "switchport", "mode"]) => {
            &["access", "trunk", "dot1q-tunnel"]
        }
        (CliMode::Config, ["set" | "delete", "interfaces", PORT, "switchport", "access"]) => {
            &["vlan"]
        }
        (CliMode::Config, ["set" | "delete", "interfaces", PORT, "switchport", "trunk"]) => {
            &["vlans", "native"]
        }
        (
            CliMode::Config,
            ["set" | "delete", "interfaces", PORT, "switchport", "trunk", "native"],
        ) => &["vlan"],
        (CliMode::Config, ["set", "interfaces", PORT, "channel-group"]) => &[NUM],
        (CliMode::Config, ["set", "interfaces", PORT, "channel-group", NUM]) => &["mode"],
        (CliMode::Config, ["set", "interfaces", PORT, "channel-group", NUM, "mode"]) => {
            &["active", "passive", "on"]
        }
        // The lacp level offers member and port-channel keywords; the
        // handlers reject the ones that don't fit the interface kind.
        (CliMode::Config, ["set" | "delete", "interfaces", PORT, "lacp"]) => {
            &["rate", "port-priority", "fallback", "fallback-timeout"]
        }
        (CliMode::Config, ["set", "interfaces", PORT, "lacp", "rate"]) => &["normal", "fast"],
        (CliMode::Config, ["set", "interfaces", PORT, "lacp", "fallback"]) => {
            &["static", "individual"]
        }
        (CliMode::Config, ["set" | "delete", "interfaces", PORT, "spanning-tree"]) => {
            &["portfast", "bpduguard", "cost", "port-priority"]
        }
        (CliMode::Config, ["set" | "delete", "interfaces", PORT, "storm-control"]) => {
            &["broadcast", "multicast", "unknown-unicast"]
        }
        (
            CliMode::Config,
            ["set", "interfaces", PORT, "storm-control", "broadcast" | "multicast" | "unknown-unicast"],
        ) => &["level"],
        (CliMode::Config, ["set" | "delete", "vlans"]) => &["vlan"],
        (CliMode::Config, ["set" | "delete", "vlans", "vlan"]) => &[NUM],
        (CliMode::Config, ["set" | "delete", "vlans", "vlan", NUM]) => &["description", "state"],
        (CliMode::Config, ["set", "vlans", "vlan", NUM, "state"]) => &["active", "suspend"],
        (CliMode::Config, ["set" | "delete", "system"]) => &["ssh", "http", "https"],
        (CliMode::Config, ["set" | "delete", "system", "ssh"]) => &["authentication"],
        (CliMode::Config, ["set", "system", "ssh", "authentication"]) => &["local"],
        (CliMode::Config, ["set" | "delete", "routing"]) => {
            &["static", "arp", "router-id", "ospf", "bgp"]
        }
        (CliMode::Config, ["set" | "delete", "routing", "ospf"]) => &[
            "area",
            "router-id",
            "passive-interface",
            "redistribute",
            "maximum-paths",
            "interface",
        ],
        (CliMode::Config, ["set" | "delete", "routing", "ospf", "area"]) => &[ANY],
        (CliMode::Config, ["set" | "delete", "routing", "ospf", "area", ANY]) => &["network"],
        (CliMode::Config, ["set" | "delete", "routing", "ospf", "passive-interface"]) => &[PORT],
        (CliMode::Config, ["set" | "delete", "routing", "ospf", "redistribute"]) => {
            &["connected", "static", "bgp"]
        }
        (CliMode::Config, ["set", "routing", "ospf", "maximum-paths"]) => &[NUM],
        (CliMode::Config, ["set" | "delete", "routing", "ospf", "interface"]) => &[PORT],
        (CliMode::Config, ["set" | "delete", "routing", "ospf", "interface", PORT]) => {
            &["cost", "hello-interval", "dead-interval", "priority"]
        }
        (CliMode::Config, ["set" | "delete", "routing", "bgp"]) => &[
            "as",
            "router-id",
            "neighbor",
            "network",
            "redistribute",
            "maximum-paths",
        ],
        (CliMode::Config, ["set" | "delete", "routing", "bgp", "neighbor"]) => &[ANY],
        (CliMode::Config, ["set" | "delete", "routing", "bgp", "neighbor", ANY]) => &[
            "remote-as",
            "description",
            "shutdown",
            "ebgp-multihop",
            "next-hop-self",
        ],
        (CliMode::Config, ["set" | "delete", "routing", "bgp", "redistribute"]) => {
            &["connected", "static", "ospf"]
        }
        (CliMode::Config, ["set", "routing", "bgp", "as" | "maximum-paths"]) => &[NUM],
        (CliMode::Config, ["set" | "delete", "routing", "arp"]) => &[ANY],
        (CliMode::Config, ["set", "routing", "arp", ANY]) => &["interface"],
        (CliMode::Config, ["set", "routing", "arp", ANY, "interface"]) => &[PORT],
        (CliMode::Config, ["set", "routing", "arp", ANY, "interface", PORT]) => &["mac"],
        (CliMode::Config, ["set" | "delete", "routing", "static"]) => &[ANY],
        (CliMode::Config, ["set", "routing", "static", ANY]) => &["drop", ANY],
        (CliMode::Config, ["set", "routing", "static", ANY, ANY]) => &["distance"],
        (CliMode::Config, ["set", "routing", "static", ANY, ANY, "distance"]) => &[NUM],
        (CliMode::Config, ["delete", "routing", "static", ANY]) => &[ANY],
        (CliMode::Config, ["set" | "delete", "protocols"]) => {
            &["spanning-tree", "igmp-snooping", "mld-snooping", "lacp"]
        }
        (CliMode::Config, ["set" | "delete", "protocols", "spanning-tree"]) => &[
            "mode",
            "priority",
            "hello-time",
            "max-age",
            "forward-time",
            "mst",
        ],
        (CliMode::Config, ["set", "protocols", "spanning-tree", "mode"]) => {
            &["mstp", "rstp", "none"]
        }
        (CliMode::Config, ["set" | "delete", "protocols", "spanning-tree", "mst"]) => {
            &["name", "revision", "instance"]
        }
        (CliMode::Config, ["set" | "delete", "protocols", "spanning-tree", "mst", "instance"]) => {
            &[NUM]
        }
        (CliMode::Config, ["set", "protocols", "spanning-tree", "mst", "instance", NUM]) => {
            &["vlans"]
        }
        (CliMode::Config, ["set" | "delete", "protocols", "igmp-snooping" | "mld-snooping"]) => {
            &["disable", "robustness", "vlan"]
        }
        (
            CliMode::Config,
            ["set" | "delete", "protocols", "igmp-snooping" | "mld-snooping", "vlan"],
        ) => &[NUM],
        (
            CliMode::Config,
            ["set" | "delete", "protocols", "igmp-snooping" | "mld-snooping", "vlan", NUM],
        ) => &["disable", "fast-leave", "querier", "mrouter"],
        (
            CliMode::Config,
            ["set", "protocols", "igmp-snooping" | "mld-snooping", "vlan", NUM, "querier"],
        ) => &["address"],
        (
            CliMode::Config,
            ["set" | "delete", "protocols", "igmp-snooping" | "mld-snooping", "vlan", NUM, "mrouter"],
        ) => &["interface"],
        (
            CliMode::Config,
            ["set" | "delete", "protocols", "igmp-snooping" | "mld-snooping", "vlan", NUM, "mrouter", "interface"],
        ) => &[PORT],
        (CliMode::Config, ["set" | "delete", "protocols", "lacp"]) => &["system-priority"],
        (CliMode::Config, ["set" | "delete", "switching"]) => &["mac-table", "mirror"],
        (CliMode::Config, ["set" | "delete", "switching", "mac-table"]) => {
            &["aging-time", "static"]
        }
        (CliMode::Config, ["set" | "delete", "switching", "mac-table", "static"]) => &[ANY],
        (CliMode::Config, ["set" | "delete", "switching", "mac-table", "static", ANY]) => &["vlan"],
        (CliMode::Config, ["set" | "delete", "switching", "mac-table", "static", ANY, "vlan"]) => {
            &[NUM]
        }
        (CliMode::Config, ["set", "switching", "mac-table", "static", ANY, "vlan", NUM]) => {
            &["interface", "drop"]
        }
        (
            CliMode::Config,
            ["set", "switching", "mac-table", "static", ANY, "vlan", NUM, "interface"],
        ) => &[PORT],
        (CliMode::Config, ["set" | "delete", "switching", "mirror"]) => &["session"],
        (CliMode::Config, ["set" | "delete", "switching", "mirror", "session"]) => &[NUM],
        (CliMode::Config, ["set" | "delete", "switching", "mirror", "session", NUM]) => {
            &["source", "destination"]
        }
        (
            CliMode::Config,
            ["set" | "delete", "switching", "mirror", "session", NUM, "source" | "destination"],
        ) => &[PORT],
        (CliMode::Config, ["set", "switching", "mirror", "session", NUM, "source", PORT]) => {
            &["rx", "tx", "both"]
        }
        (CliMode::Config, ["set" | "delete", "security"]) => {
            &["acl", "copp", "dot1x", "dhcp-snooping", "arp-inspection"]
        }
        (CliMode::Config, ["set" | "delete", "security", "acl"]) => &["ipv4", "ipv6", "mac"],
        (CliMode::Config, ["set" | "delete", "security", "acl", "ipv4" | "ipv6" | "mac"]) => &[ACL],
        (CliMode::Config, ["set" | "delete", "security", "acl", "ipv4" | "ipv6" | "mac", ACL]) => {
            &["rule"]
        }
        (
            CliMode::Config,
            ["set" | "delete", "security", "acl", "ipv4" | "ipv6" | "mac", ACL, "rule"],
        ) => &[NUM],
        (
            CliMode::Config,
            ["set" | "delete", "security", "acl", "ipv4" | "ipv6", ACL, "rule", NUM],
        ) => &[
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
        ],
        (CliMode::Config, ["set" | "delete", "security", "acl", "mac", ACL, "rule", NUM]) => &[
            "permit",
            "deny",
            "source-mac",
            "destination-mac",
            "ethertype",
        ],
        (
            CliMode::Config,
            ["set", "security", "acl", "ipv4" | "ipv6", ACL, "rule", NUM, "protocol"],
        ) => &["tcp", "udp", "icmp", NUM],
        (
            CliMode::Config,
            ["set", "security", "acl", "ipv4" | "ipv6", ACL, "rule", NUM, "police"],
        ) => &["rate"],
        (
            CliMode::Config,
            ["set", "security", "acl", "ipv4" | "ipv6", ACL, "rule", NUM, "police", "rate", ANY],
        ) => &["burst"],
        (CliMode::Config, ["set" | "delete", "security", "copp"]) => &["class"],
        (CliMode::Config, ["set" | "delete", "security", "copp", "class"]) => &[
            "bpdu", "lacp", "eapol", "igmp", "mld", "arp", "dhcp", "ospf", "bgp", "vrrp", "ip2me",
            "acl-log", "default",
        ],
        (CliMode::Config, ["set" | "delete", "security", "copp", "class", _]) => &["rate", "burst"],
        (CliMode::Config, ["set" | "delete", "security", "dot1x"]) => {
            &["radius-server", "reauth-interval"]
        }
        (CliMode::Config, ["set" | "delete", "security", "dot1x", "radius-server"]) => &[ANY],
        (CliMode::Config, ["set" | "delete", "security", "dot1x", "radius-server", ANY]) => {
            &["key", "port", "timeout", "retransmit"]
        }
        (CliMode::Config, ["set" | "delete", "security", "dhcp-snooping"]) => &["vlan", "binding"],
        (CliMode::Config, ["set" | "delete", "security", "dhcp-snooping", "vlan"]) => &[NUM],
        (CliMode::Config, ["set" | "delete", "security", "dhcp-snooping", "binding"]) => &[ANY],
        (CliMode::Config, ["set", "security", "dhcp-snooping", "binding", ANY]) => &["vlan"],
        (CliMode::Config, ["set", "security", "dhcp-snooping", "binding", ANY, "vlan"]) => &[NUM],
        (CliMode::Config, ["set", "security", "dhcp-snooping", "binding", ANY, "vlan", NUM]) => {
            &["address"]
        }
        (
            CliMode::Config,
            ["set", "security", "dhcp-snooping", "binding", ANY, "vlan", NUM, "address"],
        ) => &[ANY],
        (
            CliMode::Config,
            ["set", "security", "dhcp-snooping", "binding", ANY, "vlan", NUM, "address", ANY],
        ) => &["interface"],
        (
            CliMode::Config,
            ["set", "security", "dhcp-snooping", "binding", ANY, "vlan", NUM, "address", ANY, "interface"],
        ) => &[PORT],
        (CliMode::Config, ["set" | "delete", "security", "arp-inspection"]) => {
            &["vlan", "validate"]
        }
        (CliMode::Config, ["set" | "delete", "security", "arp-inspection", "vlan"]) => &[NUM],
        (CliMode::Config, ["set" | "delete", "security", "arp-inspection", "validate"]) => {
            &["src-mac", "dst-mac", "ip"]
        }
        (CliMode::Config, ["commit"]) => &["confirmed"],
        _ => &[],
    }
}

/// How an interface argument matched the known port names.
#[derive(Debug, PartialEq, Eq)]
pub enum PortMatch {
    One(String),
    NoMatch,
    Ambiguous(Vec<String>),
}

/// Canonicalize an interface argument: an exact name, the `Eth1`/`e1`
/// alias form (letters that case-insensitively prefix the name's letters,
/// plus the exact port number), or a unique name prefix.
pub fn match_port(input: &str, known: &[String]) -> PortMatch {
    if let Some(exact) = known.iter().find(|n| n.as_str() == input) {
        return PortMatch::One(exact.clone());
    }

    let digit_at = |s: &str| s.find(|c: char| c.is_ascii_digit()).unwrap_or(s.len());
    let (alpha, digits) = input.split_at(digit_at(input));
    let mut hits: Vec<&String> = Vec::new();
    if !alpha.is_empty() && !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        hits = known
            .iter()
            .filter(|name| {
                let (name_alpha, name_digits) = name.split_at(digit_at(name));
                name_digits == digits
                    && name_alpha
                        .to_ascii_lowercase()
                        .starts_with(&alpha.to_ascii_lowercase())
            })
            .collect();
    }
    if hits.is_empty() {
        hits = known.iter().filter(|n| n.starts_with(input)).collect();
    }
    match hits.as_slice() {
        [only] => PortMatch::One((*only).clone()),
        [] => PortMatch::NoMatch,
        many => PortMatch::Ambiguous(many.iter().map(|s| (*s).clone()).collect()),
    }
}

/// A config-defined interface form (`Po1`, `port-channel1`, `Vlan10`,
/// `v10`) that fills a port slot without appearing in the syncd cache.
fn virtual_port(token: &str) -> bool {
    let digit_at = token
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(token.len());
    let (alpha, digits) = token.split_at(digit_at);
    if alpha.is_empty() || digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let needle: String = alpha
        .chars()
        .filter(|c| *c != '-')
        .flat_map(char::to_lowercase)
        .collect();
    "portchannel".starts_with(&needle) || "vlan".starts_with(&needle)
}

/// Resolve one already-typed word against `words` the way `cli::resolve`
/// does: exact match wins, then a unique prefix. `None` = no or ambiguous
/// match (no completions downstream of a broken word).
fn resolve_word<'a>(input: &str, words: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut prefix_match = None;
    let mut prefix_hits = 0;
    for w in words {
        if w == input {
            return Some(w);
        }
        if w.starts_with(input) {
            prefix_match = Some(w);
            prefix_hits += 1;
        }
    }
    (prefix_hits == 1).then_some(prefix_match).flatten()
}

/// Candidates for the word being typed: canonicalize the completed
/// `tokens`, then filter the next level by `partial`. The ACL-less
/// convenience form the tests exercise; the live completer goes
/// through [`candidates_with_acls`].
#[cfg_attr(not(test), allow(dead_code))]
pub fn candidates(mode: CliMode, tokens: &[&str], partial: &str, ports: &[String]) -> Vec<String> {
    expand(mode, tokens, partial, ports, &[], false)
}

/// [`candidates`] with the candidate-tree ACL names for the ACL slots.
pub fn candidates_with_acls(
    mode: CliMode,
    tokens: &[&str],
    partial: &str,
    ports: &[String],
    acls: &[String],
) -> Vec<String> {
    expand(mode, tokens, partial, ports, acls, false)
}

/// [`candidates`] for the EOS-style `?` contextual help: where an
/// interface name may go, show one `<interface>` placeholder instead of
/// enumerating every port. A typed partial still lists its matches
/// (`show int Eth?`).
pub fn help_candidates(
    mode: CliMode,
    tokens: &[&str],
    partial: &str,
    ports: &[String],
    acls: &[String],
) -> Vec<String> {
    expand(mode, tokens, partial, ports, acls, partial.is_empty())
}

fn expand(
    mode: CliMode,
    tokens: &[&str],
    partial: &str,
    ports: &[String],
    acls: &[String],
    placeholder_ports: bool,
) -> Vec<String> {
    let mut path: Vec<&str> = Vec::with_capacity(tokens.len());
    for token in tokens {
        let level = next_words(mode, &path);
        // Where an interface name may appear it canonicalizes to the
        // sentinel (so deeper levels key off "a port was given", not its
        // spelling); a level offering both PORT and keywords tries the
        // port first, then the keywords.
        let resolved = if level.contains(&PORT) {
            match match_port(token, ports) {
                PortMatch::One(_) => Some(PORT),
                // Config-defined interfaces (`Po1`, `Vlan10`) are not in
                // the syncd cache but still fill a port slot.
                _ if virtual_port(token) => Some(PORT),
                _ => resolve_word(token, level.iter().copied().filter(|w| *w != PORT)),
            }
        } else if level.contains(&NUM)
            && !token.is_empty()
            && token.chars().all(|c| c.is_ascii_digit())
        {
            Some(NUM)
        } else if level.contains(&ACL) && !token.is_empty() {
            // Keywords sharing the level win over a name they prefix
            // (`show acl s<TAB>` means summary); anything else is the
            // name — completable when cached, accepted regardless.
            resolve_word(token, level.iter().copied().filter(|w| *w != ACL)).or(Some(ACL))
        } else if level.contains(&ANY) && !token.is_empty() {
            Some(ANY)
        } else {
            resolve_word(token, level.iter().copied())
        };
        match resolved {
            Some(word) => path.push(word),
            None => return Vec::new(),
        }
    }
    next_words(mode, &path)
        .iter()
        .flat_map(|w| {
            if *w == PORT {
                if placeholder_ports {
                    vec!["<interface>".to_string()]
                } else {
                    ports.to_vec()
                }
            } else if *w == ACL {
                acls.to_vec()
            } else if *w == NUM || *w == ANY {
                Vec::new() // free-form value; nothing to offer
            } else {
                vec![(*w).to_string()]
            }
        })
        .filter(|w| *w == "<interface>" || w.starts_with(partial))
        .collect()
}

impl Completer for CliHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let before = &line[..pos];
        let start = before
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        let partial = &before[start..];
        let tokens: Vec<&str> = before[..start].split_whitespace().collect();

        let Ok(state) = self.state.lock() else {
            return Ok((start, Vec::new()));
        };
        let pairs = candidates_with_acls(state.mode, &tokens, partial, &state.ports, &state.acls)
            .into_iter()
            .map(|w| Pair {
                display: w.clone(),
                replacement: format!("{w} "),
            })
            .collect();
        Ok((start, pairs))
    }
}

impl Hinter for CliHelper {
    type Hint = String;
}
impl Highlighter for CliHelper {}
impl Validator for CliHelper {}
impl Helper for CliHelper {}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ports() -> Vec<String> {
        vec!["Ethernet0".into(), "Ethernet1".into(), "Ethernet10".into()]
    }

    #[test]
    fn operational_first_word() {
        let c = candidates(CliMode::Operational, &[], "s", &ports());
        assert_eq!(c, vec!["show".to_string()]);
    }

    #[test]
    fn security_paths_complete() {
        let acls = vec!["EDGE-IN".to_string(), "MGMT6-IN".to_string()];
        // ACL names complete from the candidate cache; `summary` shares
        // the level.
        let c = candidates_with_acls(CliMode::Operational, &["show", "acl"], "", &ports(), &acls);
        assert_eq!(
            c,
            vec![
                "summary".to_string(),
                "EDGE-IN".to_string(),
                "MGMT6-IN".to_string()
            ]
        );
        let c = candidates_with_acls(CliMode::Operational, &["show", "acl"], "E", &ports(), &acls);
        assert_eq!(c, vec!["EDGE-IN".to_string()]);
        // A binding's name slot completes too, and the direction follows
        // a typed name.
        let c = candidates_with_acls(
            CliMode::Config,
            &["set", "interfaces", "Eth0", "access-group"],
            "",
            &ports(),
            &acls,
        );
        assert_eq!(c, vec!["EDGE-IN".to_string(), "MGMT6-IN".to_string()]);
        let c = candidates_with_acls(
            CliMode::Config,
            &["set", "interfaces", "Eth0", "access-group", "EDGE-IN"],
            "",
            &ports(),
            &acls,
        );
        assert_eq!(c, vec!["in".to_string(), "out".to_string()]);
        // The config-side rule tree keys off the family.
        let c = candidates(
            CliMode::Config,
            &["set", "security", "acl", "ipv4", "EDGE-IN", "rule", "10"],
            "po",
            &ports(),
        );
        assert_eq!(c, vec!["police".to_string()]);
        let c = candidates(
            CliMode::Config,
            &["set", "security", "acl", "mac", "IOT-MAC", "rule", "10"],
            "",
            &ports(),
        );
        assert_eq!(
            c,
            vec![
                "permit".to_string(),
                "deny".to_string(),
                "source-mac".to_string(),
                "destination-mac".to_string(),
                "ethertype".to_string(),
            ]
        );
        // Operational security shows and clears.
        let c = candidates(CliMode::Operational, &["show", "dhcp"], "", &ports());
        assert_eq!(c, vec!["snooping".to_string()]);
        let c = candidates(CliMode::Operational, &["clear", "dot1x"], "", &ports());
        assert_eq!(c, vec!["interface".to_string()]);
    }

    #[test]
    fn prefix_words_resolve_before_completion() {
        // `sh int<TAB>` completes as if `show int` were typed.
        let c = candidates(CliMode::Operational, &["sh"], "int", &ports());
        assert_eq!(c, vec!["interfaces".to_string()]);
        // The interfaces level offers ports and every subcommand.
        let c = candidates(CliMode::Operational, &["sh", "int"], "", &ports());
        assert!(c.contains(&"Ethernet1".to_string()));
        assert!(c.contains(&"status".to_string()));
        assert!(c.contains(&"switchport".to_string()));
        // `sh int st<TAB>` narrows to keywords.
        let c = candidates(CliMode::Operational, &["sh", "int"], "st", &ports());
        assert_eq!(c, vec!["status".to_string()]);
    }

    #[test]
    fn interfaces_tree_completes_through_a_port() {
        // A port argument leads to the subcommands (ports not re-offered).
        let c = candidates(
            CliMode::Operational,
            &["show", "interfaces", "Eth1"],
            "",
            &ports(),
        );
        assert!(c.contains(&"counters".to_string()));
        assert!(!c.contains(&"Ethernet1".to_string()));
        let c = candidates(
            CliMode::Operational,
            &["show", "interfaces", "e0", "counters"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["errors", "discards", "rates", "queue", "bins"]);
        let c = candidates(
            CliMode::Operational,
            &["show", "interfaces", "status"],
            "err",
            &ports(),
        );
        assert_eq!(c, vec!["errdisabled".to_string()]);
        let c = candidates(
            CliMode::Operational,
            &["show", "interfaces", "negotiation"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["detail".to_string()]);
    }

    #[test]
    fn interface_names_complete_from_the_port_cache() {
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces"],
            "Ethernet1",
            &ports(),
        );
        assert_eq!(c, vec!["Ethernet1".to_string(), "Ethernet10".to_string()]);
    }

    #[test]
    fn set_path_completes_through_a_port_alias() {
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "Eth0"],
            "",
            &ports(),
        );
        assert_eq!(
            c,
            vec![
                "description".to_string(),
                "shutdown".to_string(),
                "no-shutdown".to_string(),
                "address".to_string(),
                "switchport".to_string(),
                "channel-group".to_string(),
                "lacp".to_string(),
                "spanning-tree".to_string(),
                "storm-control".to_string(),
                "min-links".to_string(),
                "vrrp".to_string(),
                "access-group".to_string(),
                "port-security".to_string(),
                "dot1x".to_string(),
                "dhcp-snooping".to_string(),
                "arp-inspection".to_string(),
            ]
        );
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "e1", "switchport", "mode"],
            "",
            &ports(),
        );
        assert_eq!(
            c,
            vec![
                "access".to_string(),
                "trunk".to_string(),
                "dot1q-tunnel".to_string()
            ]
        );
        // delete shares the path.
        let c = candidates(
            CliMode::Config,
            &["delete", "interfaces", "Eth0"],
            "sh",
            &ports(),
        );
        assert_eq!(c, vec!["shutdown".to_string()]);
    }

    #[test]
    fn config_nouns_complete() {
        let c = candidates(CliMode::Config, &["set"], "", &ports());
        assert_eq!(
            c,
            vec![
                "interfaces".to_string(),
                "system".to_string(),
                "routing".to_string(),
                "vlans".to_string(),
                "protocols".to_string(),
                "switching".to_string(),
                "security".to_string(),
            ]
        );
        // delete shares the tree.
        let c = candidates(CliMode::Config, &["delete"], "r", &ports());
        assert_eq!(c, vec!["routing".to_string()]);
    }

    #[test]
    fn system_ssh_path_completes() {
        let c = candidates(CliMode::Config, &["set", "system"], "", &ports());
        assert_eq!(
            c,
            vec!["ssh".to_string(), "http".to_string(), "https".to_string()]
        );
        // A shared prefix narrows to the web services.
        let c = candidates(CliMode::Config, &["set", "system"], "ht", &ports());
        assert_eq!(c, vec!["http".to_string(), "https".to_string()]);
        let c = candidates(CliMode::Config, &["set", "system", "ssh"], "", &ports());
        assert_eq!(c, vec!["authentication".to_string()]);
        let c = candidates(
            CliMode::Config,
            &["set", "system", "ssh", "authentication"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["local".to_string()]);
        // delete stops at authentication (no value to complete).
        let c = candidates(
            CliMode::Config,
            &["delete", "system", "ssh", "authentication"],
            "",
            &ports(),
        );
        assert!(c.is_empty());
    }

    #[test]
    fn routing_path_completes() {
        let c = candidates(CliMode::Config, &["set", "routing"], "", &ports());
        assert_eq!(c, ["static", "arp", "router-id", "ospf", "bgp"]);
        // The prefix slot is free text; the next-hop slot offers `drop`.
        let c = candidates(CliMode::Config, &["set", "routing", "static"], "", &ports());
        assert!(c.is_empty());
        let c = candidates(
            CliMode::Config,
            &["set", "routing", "static", "10.0.0.0/8"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["drop".to_string()]);
        let c = candidates(
            CliMode::Config,
            &["set", "routing", "static", "10.0.0.0/8", "10.1.1.1"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["distance".to_string()]);
    }

    #[test]
    fn show_route_completes() {
        let c = candidates(CliMode::Operational, &["show", "ip"], "", &ports());
        assert_eq!(c, vec!["route".to_string()]);
        let c = candidates(
            CliMode::Operational,
            &["show", "ipv6", "route"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["summary".to_string()]);
    }

    #[test]
    fn interface_address_completes() {
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "Eth0"],
            "a",
            &ports(),
        );
        assert_eq!(
            c,
            vec![
                "address".to_string(),
                "access-group".to_string(),
                "arp-inspection".to_string(),
            ]
        );
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "Eth0"],
            "add",
            &ports(),
        );
        assert_eq!(c, vec!["address".to_string()]);
    }

    #[test]
    fn switchport_tree_completes() {
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "Eth1", "switchport"],
            "",
            &ports(),
        );
        assert_eq!(
            c,
            vec![
                "mode".to_string(),
                "access".to_string(),
                "trunk".to_string()
            ]
        );
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "Eth1", "switchport", "trunk"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["vlans".to_string(), "native".to_string()]);
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "Eth1", "switchport", "trunk", "native"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["vlan".to_string()]);
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "Eth1", "switchport", "access"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["vlan".to_string()]);
    }

    #[test]
    fn vlans_path_completes_through_an_id() {
        let c = candidates(CliMode::Config, &["set", "vlans"], "", &ports());
        assert_eq!(c, vec!["vlan".to_string()]);
        // The id slot is free-form...
        let c = candidates(CliMode::Config, &["set", "vlans", "vlan"], "", &ports());
        assert!(c.is_empty());
        // ...but a typed number leads to the vlan settings.
        let c = candidates(
            CliMode::Config,
            &["set", "vlans", "vlan", "10"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["description".to_string(), "state".to_string()]);
        // A non-number in the id slot stops completion.
        let c = candidates(
            CliMode::Config,
            &["set", "vlans", "vlan", "banana"],
            "",
            &ports(),
        );
        assert!(c.is_empty());
    }

    #[test]
    fn switching_suite_interface_paths_complete() {
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "Eth1"],
            "ch",
            &ports(),
        );
        assert_eq!(c, vec!["channel-group".to_string()]);
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "Eth1", "channel-group", "1", "mode"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["active", "passive", "on"]);
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "Eth1", "lacp"],
            "",
            &ports(),
        );
        assert_eq!(
            c,
            vec!["rate", "port-priority", "fallback", "fallback-timeout"]
        );
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "Po1", "lacp", "fallback"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["static", "individual"]);
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "Eth1", "spanning-tree"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["portfast", "bpduguard", "cost", "port-priority"]);
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "Eth1", "storm-control", "broadcast"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["level"]);
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "Eth1", "switchport", "mode"],
            "d",
            &ports(),
        );
        assert_eq!(c, vec!["dot1q-tunnel"]);
    }

    #[test]
    fn protocols_paths_complete() {
        let c = candidates(CliMode::Config, &["set"], "p", &ports());
        assert_eq!(c, vec!["protocols".to_string()]);
        let c = candidates(CliMode::Config, &["set", "protocols"], "", &ports());
        assert_eq!(
            c,
            vec!["spanning-tree", "igmp-snooping", "mld-snooping", "lacp"]
        );
        let c = candidates(
            CliMode::Config,
            &["set", "protocols", "spanning-tree", "mode"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["mstp", "rstp", "none"]);
        let c = candidates(
            CliMode::Config,
            &["set", "protocols", "spanning-tree", "mst", "instance", "1"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["vlans"]);
        let c = candidates(
            CliMode::Config,
            &["set", "protocols", "igmp-snooping", "vlan", "10"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["disable", "fast-leave", "querier", "mrouter"]);
        let c = candidates(
            CliMode::Config,
            &[
                "set",
                "protocols",
                "igmp-snooping",
                "vlan",
                "10",
                "mrouter",
                "interface",
            ],
            "Ethernet1",
            &ports(),
        );
        assert_eq!(c, vec!["Ethernet1", "Ethernet10"]);
        let c = candidates(CliMode::Config, &["set", "protocols", "lacp"], "", &ports());
        assert_eq!(c, vec!["system-priority"]);
    }

    #[test]
    fn switching_paths_complete() {
        let c = candidates(CliMode::Config, &["set", "switching"], "", &ports());
        assert_eq!(c, vec!["mac-table", "mirror"]);
        // The MAC slot is free text; a typed MAC leads to `vlan`.
        let c = candidates(
            CliMode::Config,
            &[
                "set",
                "switching",
                "mac-table",
                "static",
                "00:50:56:be:ef:01",
            ],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["vlan"]);
        let c = candidates(
            CliMode::Config,
            &[
                "set",
                "switching",
                "mac-table",
                "static",
                "00:50:56:be:ef:01",
                "vlan",
                "10",
            ],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["interface", "drop"]);
        let c = candidates(
            CliMode::Config,
            &["set", "switching", "mirror", "session", "1"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["source", "destination"]);
        let c = candidates(
            CliMode::Config,
            &[
                "set",
                "switching",
                "mirror",
                "session",
                "1",
                "source",
                "Eth1",
            ],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["rx", "tx", "both"]);
    }

    #[test]
    fn vlan_state_path_completes() {
        let c = candidates(
            CliMode::Config,
            &["set", "vlans", "vlan", "10"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["description", "state"]);
        let c = candidates(
            CliMode::Config,
            &["set", "vlans", "vlan", "10", "state"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["active", "suspend"]);
    }

    #[test]
    fn port_channel_tokens_fill_port_slots() {
        // Po1 is not in the syncd cache but still resolves the slot.
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "Po1"],
            "min",
            &ports(),
        );
        assert_eq!(c, vec!["min-links"]);
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "port-channel1", "switchport"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["mode", "access", "trunk"]);
        assert!(virtual_port("Po1"));
        assert!(virtual_port("port-channel1"));
        assert!(virtual_port("Vlan10"));
        assert!(!virtual_port("Ethernet1"));
        assert!(!virtual_port("banana"));
    }

    #[test]
    fn broken_or_ambiguous_words_stop_completion() {
        assert!(candidates(CliMode::Operational, &["zz"], "", &ports()).is_empty());
        // "s" is ambiguous in config mode (set / show).
        assert!(candidates(CliMode::Config, &["s"], "", &ports()).is_empty());
    }

    #[test]
    fn port_aliases_resolve() {
        assert_eq!(
            match_port("Eth1", &ports()),
            PortMatch::One("Ethernet1".into())
        );
        assert_eq!(
            match_port("e10", &ports()),
            PortMatch::One("Ethernet10".into())
        );
        assert_eq!(
            match_port("ethernet0", &ports()),
            PortMatch::One("Ethernet0".into())
        );
        assert_eq!(match_port("zz", &ports()), PortMatch::NoMatch);
        assert!(matches!(
            match_port("Ethernet", &ports()),
            PortMatch::Ambiguous(_)
        ));
    }
}
