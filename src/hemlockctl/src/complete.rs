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

/// Which command tree applies; mirrors `cli::Mode` (which carries data and
/// so cannot be shared with the completer directly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliMode {
    Operational,
    Config,
    ConfigIf,
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
            "interface",
            "show",
            "commit",
            "rollback",
            "discard",
            "abort",
            "exit",
            "end",
            "help",
        ],
        (CliMode::Config, ["interface"]) => &[PORT],
        (CliMode::Config, ["commit"]) => &["confirmed"],
        (CliMode::ConfigIf, []) => &["description", "shutdown", "no", "exit", "end", "help"],
        (CliMode::ConfigIf, ["no"]) => &["shutdown", "description"],
        _ => &[],
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
fn candidates(mode: CliMode, tokens: &[&str], partial: &str, ports: &[String]) -> Vec<String> {
    let mut path: Vec<&str> = Vec::with_capacity(tokens.len());
    for token in tokens {
        let level = next_words(mode, &path);
        let resolved = if level.contains(&PORT) {
            // An interface name: canonicalize to the sentinel so deeper
            // levels key off "a port was given", not its spelling.
            resolve_word(token, ports.iter().map(String::as_str)).map(|_| PORT)
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
        let c = candidates(CliMode::Config, &["interface"], "Ethernet1", &ports());
        assert_eq!(c, vec!["Ethernet1".to_string(), "Ethernet10".to_string()]);
    }

    #[test]
    fn broken_or_ambiguous_words_stop_completion() {
        assert!(candidates(CliMode::Operational, &["zz"], "", &ports()).is_empty());
        // "e" is ambiguous in operational mode (exit / ... nothing else
        // starts with e — use config mode's exit/end instead).
        assert!(candidates(CliMode::Config, &["e"], "", &ports()).is_empty());
    }

    #[test]
    fn config_if_no_subtree() {
        let c = candidates(CliMode::ConfigIf, &["no"], "", &ports());
        assert_eq!(c, vec!["shutdown".to_string(), "description".to_string()]);
    }
}
