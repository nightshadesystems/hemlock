//! `ping` and `traceroute`: live passthrough to the system tools.
//!
//! These two are deliberately *not* rendered. An operator reading a
//! ping already knows what iputils prints, the output has to appear
//! line by line as the replies arrive (a buffered render would defeat
//! the point), and Ctrl-C has to stop it the way it always does. So the
//! CLI maps its arguments onto the tool, hands over the terminal, and
//! waits — no pager, no model, no golden.
//!
//! What *is* Hemlock's is the argument mapping: `source <interface>`
//! becomes the tool's own source-selection flag, and a host that is
//! neither an address nor a plausible hostname is refused before a
//! process is spawned.

use std::process::Command;

/// The tools this module drives, and the flag each takes for a source
/// interface. `ping` binds by device; `traceroute` takes the same idea
/// under a different letter.
const PING: &str = "ping";
const TRACEROUTE: &str = "traceroute";

/// A host argument: an IP literal, or a syntactically plausible
/// hostname. Resolution is the tool's problem — this only keeps
/// nonsense (and anything shell-shaped) from reaching an argv.
pub fn valid_host(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// The argv one of the two tools is run with, so the mapping can be
/// asserted without spawning anything.
pub fn argv(tool: &str, host: &str, source: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(source) = source {
        // Both tools bind to an interface; only the flag differs.
        args.push(if tool == PING { "-I" } else { "-i" }.to_string());
        args.push(source.to_string());
    }
    args.push(host.to_string());
    args
}

/// Run `ping`/`traceroute` against `host`, streaming to the terminal.
///
/// The child inherits stdio, so its output arrives as it is produced
/// and Ctrl-C reaches it directly: the terminal delivers SIGINT to the
/// whole foreground process group, the tool prints its own summary and
/// exits, and this function returns normally. A non-zero exit (an
/// unreachable host, say) is the tool's answer, not a CLI error, so it
/// is not reported as one.
fn run(tool: &str, host: &str, source: Option<&str>) -> Result<(), String> {
    if !valid_host(host) {
        return Err(format!("% bad host {host:?}"));
    }
    let args = argv(tool, host, source);
    match Command::new(tool).args(&args).status() {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("% {tool} is not installed on this switch"))
        }
        Err(err) => Err(format!("% cannot run {tool}: {err}")),
    }
}

pub fn ping(host: &str, source: Option<&str>) -> Result<(), String> {
    run(PING, host, source)
}

pub fn traceroute(host: &str, source: Option<&str>) -> Result<(), String> {
    run(TRACEROUTE, host, source)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The whole of what Hemlock contributes here: which arguments the
    /// tool is handed.
    #[test]
    fn arguments_map_onto_the_tools() {
        assert_eq!(argv(PING, "10.42.0.5", None), ["10.42.0.5"]);
        assert_eq!(
            argv(PING, "10.42.0.5", Some("Vlan10")),
            ["-I", "Vlan10", "10.42.0.5"]
        );
        // traceroute binds with its own flag.
        assert_eq!(
            argv(TRACEROUTE, "example.net", Some("Management1")),
            ["-i", "Management1", "example.net"]
        );
        // The host is always last, so a source never shadows it.
        let args = argv(TRACEROUTE, "2001:db8::1", Some("Vlan10"));
        assert_eq!(args.last().map(String::as_str), Some("2001:db8::1"));
    }

    #[test]
    fn hosts_validate_before_anything_is_spawned() {
        assert!(valid_host("10.42.0.5"));
        assert!(valid_host("2001:db8::1"));
        assert!(valid_host("core-sw-01.nightshade.systems"));
        assert!(valid_host("example.net."));
        assert!(!valid_host(""));
        assert!(!valid_host("-leading"));
        assert!(!valid_host("has space"));
        assert!(!valid_host("semi;colon"));
        assert!(!valid_host("$(whoami)"));
    }

    /// A missing tool is reported as such rather than as a mysterious
    /// failure — and nothing is spawned for a bad host.
    #[test]
    fn refuses_a_bad_host_without_spawning() {
        assert_eq!(
            run("definitely-not-a-tool", "bad host", None).unwrap_err(),
            "% bad host \"bad host\""
        );
        assert_eq!(
            run("definitely-not-a-tool", "10.0.0.1", None).unwrap_err(),
            "% definitely-not-a-tool is not installed on this switch"
        );
    }
}
