//! The remote-syslog applier: renders `/etc/rsyslog.d/20-hemlock.conf`
//! from the logging intent and starts (or stops) rsyslog.
//!
//! Shaped like `snmpapply`: a pure, deterministic render function
//! (golden-tested) plus a best-effort apply that is inert on hosts
//! without rsyslog installed (`/etc/rsyslog.d` absent).
//!
//! The path is journal -> rsyslog -> collector. systemd's journal stays
//! the local log store — nothing here changes what the box keeps or what
//! `show logging` reads — and rsyslog is only a forwarder, running
//! exactly when at least one collector is configured. A forwarder with
//! nowhere to send would cost a daemon and buy nothing.
//!
//! TCP forwarding gets a disk-assisted queue and infinite resume
//! retries, so a collector that goes away stalls the queue instead of
//! blocking the daemons doing the logging. UDP needs neither.

use tracing::warn;

use crate::intents::{log_level_severity, LoggingIntent};

const RSYSLOG_DIR: &str = "/etc/rsyslog.d";
const RSYSLOG_CONF: &str = "/etc/rsyslog.d/20-hemlock.conf";
const RSYSLOG_UNIT: &str = "rsyslog";

/// Render the forwarding config for one intent. `None` = no
/// collectors, so nothing is forwarded and the daemon stops.
pub fn render_rsyslog(logging: &LoggingIntent) -> Option<String> {
    if !logging.enabled() {
        return None;
    }
    // Validation guarantees a known level before this runs; the
    // fallback keeps the render total.
    let severity = log_level_severity(logging.effective_level())?;
    let mut out = String::new();
    let mut line = |text: &str| {
        out.push_str(text);
        out.push('\n');
    };
    line("# Managed by hemlock-mgmtd; edit via the Hemlock config, not here.");
    line("");
    line("# The journal is the local log store; rsyslog only forwards.");
    line("module(load=\"imjournal\" StateFile=\"hemlock-imjournal\")");
    line("");
    line(&format!(
        "# Forwarding at `{}` and above.",
        logging.effective_level()
    ));
    for host in &logging.hosts {
        // rsyslog wants a bare v6 literal in `target`, without the
        // brackets the display form carries.
        let target = &host.address;
        if host.protocol == "tcp" {
            line(&format!("*.{severity} action(type=\"omfwd\""));
            line(&format!("    target=\"{target}\" port=\"{}\"", host.port));
            line("    protocol=\"tcp\"");
            // A collector that goes away must never block the daemons
            // doing the logging: queue on disk and keep retrying.
            line("    action.resumeRetryCount=\"-1\"");
            line("    queue.type=\"linkedList\"");
            line("    queue.size=\"10000\"");
            line(&format!(
                "    queue.filename=\"hemlock-{}\"",
                queue_name(target)
            ));
            line("    queue.saveOnShutdown=\"on\")");
        } else {
            line(&format!(
                "*.{severity} action(type=\"omfwd\" target=\"{target}\" port=\"{}\" protocol=\"udp\")",
                host.port
            ));
        }
    }
    Some(out)
}

/// A queue-file name for one collector: rsyslog wants a filesystem-safe
/// stem, and an address is not one.
fn queue_name(address: &str) -> String {
    address.replace([':', '.'], "-")
}

pub struct RsyslogApplier;

impl Default for RsyslogApplier {
    fn default() -> Self {
        Self::new()
    }
}

impl RsyslogApplier {
    pub fn new() -> Self {
        Self
    }

    /// Inert on hosts without rsyslog installed.
    fn active(&self) -> bool {
        std::path::Path::new(RSYSLOG_DIR).is_dir()
    }

    /// Render and apply the full logging state. Idempotent; the caller
    /// diffs the render and only invokes on change (or at boot replay).
    pub fn apply(&self, logging: &LoggingIntent) {
        if !self.active() {
            warn!("no /etc/rsyslog.d; remote logging not applied");
            return;
        }
        match render_rsyslog(logging) {
            Some(conf) => {
                let changed =
                    std::fs::read_to_string(RSYSLOG_CONF).ok().as_deref() != Some(conf.as_str());
                if let Err(err) = std::fs::write(RSYSLOG_CONF, conf) {
                    warn!(%err, path = RSYSLOG_CONF, "cannot write the rsyslog config");
                    return;
                }
                run("systemctl", &["enable", "--now", RSYSLOG_UNIT]);
                if changed {
                    // rsyslog re-reads its configuration on restart only.
                    run("systemctl", &["restart", RSYSLOG_UNIT]);
                }
            }
            None => {
                if let Err(err) = std::fs::remove_file(RSYSLOG_CONF) {
                    if err.kind() != std::io::ErrorKind::NotFound {
                        warn!(%err, path = RSYSLOG_CONF, "cannot remove the rsyslog config");
                    }
                }
                run("systemctl", &["disable", "--now", RSYSLOG_UNIT]);
            }
        }
    }
}

/// Run one OS command, logging (not failing) on error — apply is
/// best-effort like the OS, FRR and SNMP appliers.
fn run(program: &str, args: &[&str]) {
    let command = || format!("{program} {}", args.join(" "));
    match std::process::Command::new(program).args(args).output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => warn!(
            command = %command(),
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "rsyslog apply command failed"
        ),
        Err(err) => warn!(command = %command(), %err, "cannot run rsyslog apply command"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::intents::extract;
    use hemlock_config::parse;

    fn logging_of(text: &str) -> LoggingIntent {
        extract(&parse(text).unwrap()).unwrap().logging
    }

    #[track_caller]
    fn assert_golden(rendered: &str, golden: &str) {
        let golden = golden.replace("\r\n", "\n");
        if rendered != golden {
            for (n, (got, want)) in rendered.lines().zip(golden.lines()).enumerate() {
                assert_eq!(got, want, "first mismatch at line {}", n + 1);
            }
            assert_eq!(rendered, golden);
        }
    }

    /// The spec's Part 1.1 seed: a plain UDP collector and a TCP one on
    /// a non-default port.
    fn seed() -> LoggingIntent {
        logging_of(
            "system {
    logging {
        host 10.42.0.30;
        host 10.42.0.31 port 6514 protocol tcp;
        level informational;
    }
}",
        )
    }

    #[test]
    fn renders_the_full_seed() {
        let conf = render_rsyslog(&seed()).unwrap();
        assert_golden(&conf, include_str!("../tests/golden/rsyslog_seed.conf"));
        // Determinism: config order is the render order.
        assert_eq!(render_rsyslog(&seed()).unwrap(), conf);
    }

    /// The level is a floor, and it reaches the selector.
    #[test]
    fn the_level_selects_the_severity() {
        let warnings = logging_of("system { logging { host 10.0.0.1\nlevel warnings } }");
        let conf = render_rsyslog(&warnings).unwrap();
        assert!(conf.contains("*.warning action("), "{conf}");
        assert!(
            conf.contains("Forwarding at `warnings` and above."),
            "{conf}"
        );

        // An absent level is the default.
        let default = logging_of("system { logging { host 10.0.0.1 } }");
        assert!(render_rsyslog(&default).unwrap().contains("*.info action("));
        assert_eq!(default.effective_level(), "informational");
    }

    /// A v6 collector renders a bare literal in `target` (brackets are
    /// a display convention, not rsyslog syntax) and a safe queue stem.
    #[test]
    fn renders_ipv6_collectors() {
        let v6 = logging_of("system { logging { host 2001:db8::30 protocol tcp } }");
        let conf = render_rsyslog(&v6).unwrap();
        assert!(conf.contains("target=\"2001:db8::30\""), "{conf}");
        assert!(
            conf.contains("queue.filename=\"hemlock-2001-db8--30\""),
            "{conf}"
        );
        assert_eq!(v6.hosts[0].display(), "[2001:db8::30]:514 (tcp)");
    }

    /// No collectors = no config: nothing is forwarded and the unit
    /// stops. The journal still keeps everything locally.
    #[test]
    fn no_hosts_renders_nothing() {
        assert!(render_rsyslog(&LoggingIntent::default()).is_none());
        assert!(render_rsyslog(&logging_of("system { logging { } }")).is_none());
        assert!(render_rsyslog(&logging_of("")).is_none());
        // Even a level on its own forwards nowhere.
        assert!(render_rsyslog(&logging_of("system { logging { level errors } }")).is_none());
    }
}
