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
/// background task keeps `ports` fresh from syncd.
pub struct State {
    pub mode: CliMode,
    pub ports: Vec<String>,
}

pub struct CliHelper {
    pub state: Arc<Mutex<State>>,
}

/// Sentinel in the word tables meaning "an interface name goes here".
const PORT: &str = "\0port";

/// Sentinel meaning "a number goes here" (VLAN ids). Not offered as a
/// completion; it only lets deeper levels key off "a number was given".
const NUM: &str = "\0num";

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
            "bash",
            "exit",
            "quit",
            "logout",
            "help",
        ],
        (CliMode::Operational, ["show"]) => {
            &["interfaces", "environment", "configuration", "version"]
        }
        (CliMode::Operational, ["show", "interfaces", rest @ ..]) => interfaces_words(rest),
        (CliMode::Config, []) => &[
            "set", "delete", "show", "commit", "rollback", "discard", "exit", "help",
        ],
        (CliMode::Config, ["set" | "delete"]) => &["interfaces", "system", "routing", "vlans"],
        (CliMode::Config, ["set" | "delete", "interfaces"]) => &[PORT],
        (CliMode::Config, ["set" | "delete", "interfaces", PORT]) => &[
            "description",
            "shutdown",
            "no-shutdown",
            "address",
            "switchport",
        ],
        (CliMode::Config, ["set" | "delete", "interfaces", PORT, "switchport"]) => {
            &["mode", "access", "trunk"]
        }
        (CliMode::Config, ["set", "interfaces", PORT, "switchport", "mode"]) => {
            &["access", "trunk"]
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
        (CliMode::Config, ["set" | "delete", "vlans"]) => &["vlan"],
        (CliMode::Config, ["set" | "delete", "vlans", "vlan"]) => &[NUM],
        (CliMode::Config, ["set" | "delete", "vlans", "vlan", NUM]) => &["description"],
        (CliMode::Config, ["set" | "delete", "system"]) => &["ssh", "http", "https"],
        (CliMode::Config, ["set" | "delete", "system", "ssh"]) => &["authentication"],
        (CliMode::Config, ["set", "system", "ssh", "authentication"]) => &["local"],
        (CliMode::Config, ["set" | "delete", "routing"]) => &["static"],
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
/// `tokens`, then filter the next level by `partial`.
pub fn candidates(mode: CliMode, tokens: &[&str], partial: &str, ports: &[String]) -> Vec<String> {
    expand(mode, tokens, partial, ports, false)
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
) -> Vec<String> {
    expand(mode, tokens, partial, ports, partial.is_empty())
}

fn expand(
    mode: CliMode,
    tokens: &[&str],
    partial: &str,
    ports: &[String],
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
                _ => resolve_word(token, level.iter().copied().filter(|w| *w != PORT)),
            }
        } else if level.contains(&NUM)
            && !token.is_empty()
            && token.chars().all(|c| c.is_ascii_digit())
        {
            Some(NUM)
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
            } else if *w == NUM {
                Vec::new() // free-form number; nothing to offer
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
        let pairs = candidates(state.mode, &tokens, partial, &state.ports)
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
                "switchport".to_string()
            ]
        );
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "e1", "switchport", "mode"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["access".to_string(), "trunk".to_string()]);
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
                "vlans".to_string()
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
        assert_eq!(c, vec!["static".to_string()]);
        // The prefix/next-hop slots are free text.
        let c = candidates(CliMode::Config, &["set", "routing", "static"], "", &ports());
        assert!(c.is_empty());
    }

    #[test]
    fn interface_address_completes() {
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "Eth0"],
            "a",
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
        // ...but a typed number leads to description.
        let c = candidates(
            CliMode::Config,
            &["set", "vlans", "vlan", "10"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["description".to_string()]);
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
