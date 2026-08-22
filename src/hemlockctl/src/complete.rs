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
        (CliMode::Operational, ["show", "interfaces"]) => &["status", "transceiver"],
        (CliMode::Config, []) => &[
            "set", "delete", "show", "commit", "rollback", "discard", "exit", "help",
        ],
        (CliMode::Config, ["set" | "delete"]) => &["interfaces"],
        (CliMode::Config, ["set" | "delete", "interfaces"]) => &[PORT],
        (CliMode::Config, ["set" | "delete", "interfaces", PORT]) => {
            &["description", "admin-state"]
        }
        (CliMode::Config, ["set", "interfaces", PORT, "admin-state"]) => &["enabled", "disabled"],
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
/// `tokens`, then filter the next level by `partial`. Also drives the
/// EOS-style `?` contextual help in the CLI loop.
pub fn candidates(mode: CliMode, tokens: &[&str], partial: &str, ports: &[String]) -> Vec<String> {
    let mut path: Vec<&str> = Vec::with_capacity(tokens.len());
    for token in tokens {
        let level = next_words(mode, &path);
        let resolved = if level.contains(&PORT) {
            // An interface name (aliases like Eth1 included): canonicalize
            // to the sentinel so deeper levels key off "a port was given",
            // not its spelling.
            match match_port(token, ports) {
                PortMatch::One(_) => Some(PORT),
                _ => None,
            }
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
                ports.to_vec()
            } else {
                vec![(*w).to_string()]
            }
        })
        .filter(|w| w.starts_with(partial))
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
        let c = candidates(CliMode::Operational, &["sh", "int"], "", &ports());
        assert_eq!(c, vec!["status".to_string(), "transceiver".to_string()]);
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
            vec!["description".to_string(), "admin-state".to_string()]
        );
        let c = candidates(
            CliMode::Config,
            &["set", "interfaces", "e1", "admin-state"],
            "",
            &ports(),
        );
        assert_eq!(c, vec!["enabled".to_string(), "disabled".to_string()]);
        // delete shares the path but has no admin-state values to complete.
        let c = candidates(
            CliMode::Config,
            &["delete", "interfaces", "Eth0"],
            "",
            &ports(),
        );
        assert_eq!(
            c,
            vec!["description".to_string(), "admin-state".to_string()]
        );
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
