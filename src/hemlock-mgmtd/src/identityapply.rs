//! The identity applier: hostname, time zone, resolver and login
//! banner.
//!
//! Shaped like `frrapply`/`snmpapply` — pure, deterministic render
//! functions (golden-tested) plus a best-effort apply. Four OS surfaces
//! carry one config family:
//!
//! - **hostname**: `hostnamectl set-hostname`, plus the `127.0.1.1`
//!   line in `/etc/hosts` Debian keeps the machine's own name on (sudo
//!   and anything else resolving the hostname reads it there, not from
//!   the resolver).
//! - **timezone**: `timedatectl set-timezone`.
//! - **resolver**: a `systemd-resolved` drop-in with `DNS=` and
//!   `Domains=`.
//! - **login banner**: `/etc/issue.net` plus an sshd drop-in pointing
//!   `Banner` at it, so the text shows *before* authentication.
//!
//! The hostname change reaches the CLI prompt and the MOTD with no
//! further plumbing: both read the OS hostname when they render.
//!
//! Every write is idempotent and the whole applier is inert on hosts
//! without the surface it manages (`/etc/hosts` absent, no
//! `hostnamectl`), so a development workstation's identity is never
//! touched.

use std::fmt::Write as _;

use tracing::warn;

use crate::intents::IdentityIntent;

/// The `/etc/hosts` line Debian keeps the machine's own name on.
const HOSTS_FILE: &str = "/etc/hosts";
const HOSTS_ADDRESS: &str = "127.0.1.1";

/// mgmtd's systemd-resolved drop-in and the unit it configures.
const RESOLVED_DROPIN_DIR: &str = "/etc/systemd/resolved.conf.d";
const RESOLVED_DROPIN: &str = "/etc/systemd/resolved.conf.d/20-hemlock.conf";
const RESOLVED_UNIT: &str = "systemd-resolved";

/// The pre-authentication banner sshd shows, and mgmtd's drop-in
/// pointing at it.
const ISSUE_NET: &str = "/etc/issue.net";
const BANNER_DROPIN: &str = "/etc/ssh/sshd_config.d/21-hemlock-banner.conf";
const SSH_UNIT: &str = "ssh";

/// The header every rendered file carries, so an operator who finds one
/// by hand knows where it came from.
const MANAGED: &str = "# Managed by hemlock-mgmtd; edit via the Hemlock config, not here.";

/// The `127.0.1.1` line for one identity: the FQDN first (Debian's
/// convention), then the short name.
pub fn render_hosts_line(identity: &IdentityIntent) -> String {
    let host = identity.effective_hostname();
    match identity.fqdn() {
        Some(fqdn) => format!("{HOSTS_ADDRESS}\t{fqdn} {host}"),
        None => format!("{HOSTS_ADDRESS}\t{host}"),
    }
}

/// `/etc/hosts` with the machine's own line replaced (or appended).
/// Every other line is left exactly as it was — the file is shared with
/// whatever else the operator put there.
pub fn render_hosts(current: &str, identity: &IdentityIntent) -> String {
    let wanted = render_hosts_line(identity);
    let mut out = String::with_capacity(current.len() + wanted.len() + 1);
    let mut replaced = false;
    for line in current.lines() {
        let is_self = line
            .split_whitespace()
            .next()
            .is_some_and(|address| address == HOSTS_ADDRESS);
        if is_self {
            if !replaced {
                let _ = writeln!(out, "{wanted}");
                replaced = true;
            }
            continue;
        }
        let _ = writeln!(out, "{line}");
    }
    if !replaced {
        let _ = writeln!(out, "{wanted}");
    }
    out
}

/// The systemd-resolved drop-in. `None` = no resolver configuration, so
/// the drop-in is removed and resolved falls back to whatever DHCP or
/// the image gives it.
///
/// `Domains=` carries the search domain; the resolver only searches it
/// when a name has no dot of its own, which is what an operator setting
/// `domain-name` expects.
pub fn render_resolved(identity: &IdentityIntent) -> Option<String> {
    if identity.name_servers.is_empty() && identity.domain_name.is_none() {
        return None;
    }
    let mut out = String::new();
    let _ = writeln!(out, "{MANAGED}");
    let _ = writeln!(out, "[Resolve]");
    if !identity.name_servers.is_empty() {
        let _ = writeln!(out, "DNS={}", identity.name_servers.join(" "));
        // Debian ships a fallback list; a switch told which resolvers
        // to use must not quietly reach a different one.
        let _ = writeln!(out, "FallbackDNS=");
    }
    if let Some(domain) = &identity.domain_name {
        let _ = writeln!(out, "Domains={domain}");
    }
    Some(out)
}

/// `/etc/issue.net`. `None` = no banner, so the file and the sshd
/// drop-in both go away.
///
/// The text is one config leaf; `\n` inside it renders as a real line
/// break, which is how a multi-line legal notice gets written from a
/// single-line CLI.
pub fn render_issue_net(identity: &IdentityIntent) -> Option<String> {
    let banner = identity.banner_login.as_ref()?;
    let mut text = banner.replace("\\n", "\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Some(text)
}

pub struct IdentityApplier;

impl Default for IdentityApplier {
    fn default() -> Self {
        Self::new()
    }
}

impl IdentityApplier {
    pub fn new() -> Self {
        Self
    }

    /// Inert on hosts without the surfaces this applier manages.
    fn active(&self) -> bool {
        std::path::Path::new(HOSTS_FILE).is_file()
    }

    /// Drive the OS to one identity. Idempotent: the commit diff and
    /// the boot replay take the same path.
    pub fn apply(&self, identity: &IdentityIntent) {
        if !self.active() {
            warn!("no /etc/hosts; system identity not applied");
            return;
        }
        self.apply_hostname(identity);
        run("timedatectl", &["set-timezone", identity.effective_timezone()]);
        self.apply_resolver(identity);
        self.apply_banner(identity);
    }

    fn apply_hostname(&self, identity: &IdentityIntent) {
        let host = identity.effective_hostname();
        // /etc/hosts first: a hostname the box cannot resolve makes
        // sudo (and anything else doing a self-lookup) stall.
        match std::fs::read_to_string(HOSTS_FILE) {
            Ok(current) => {
                let wanted = render_hosts(&current, identity);
                if wanted != current {
                    if let Err(err) = std::fs::write(HOSTS_FILE, wanted) {
                        warn!(%err, path = HOSTS_FILE, "cannot write /etc/hosts");
                    }
                }
            }
            Err(err) => warn!(%err, path = HOSTS_FILE, "cannot read /etc/hosts"),
        }
        run("hostnamectl", &["set-hostname", host]);
    }

    fn apply_resolver(&self, identity: &IdentityIntent) {
        match render_resolved(identity) {
            Some(dropin) => {
                if let Err(err) = std::fs::create_dir_all(RESOLVED_DROPIN_DIR) {
                    warn!(%err, path = RESOLVED_DROPIN_DIR, "cannot create the resolved drop-in dir");
                }
                let changed = std::fs::read_to_string(RESOLVED_DROPIN).ok().as_deref()
                    != Some(dropin.as_str());
                if let Err(err) = std::fs::write(RESOLVED_DROPIN, dropin) {
                    warn!(%err, path = RESOLVED_DROPIN, "cannot write the resolved drop-in");
                    return;
                }
                if changed {
                    // resolved re-reads its configuration on restart
                    // only; the running cache goes with it, which is
                    // what a resolver change wants anyway.
                    run("systemctl", &["restart", RESOLVED_UNIT]);
                }
            }
            None => {
                if remove(RESOLVED_DROPIN) {
                    run("systemctl", &["restart", RESOLVED_UNIT]);
                }
            }
        }
    }

    fn apply_banner(&self, identity: &IdentityIntent) {
        let dropin = format!("{MANAGED}\nBanner {ISSUE_NET}\n");
        let changed = match render_issue_net(identity) {
            Some(text) => {
                let text_changed =
                    std::fs::read_to_string(ISSUE_NET).ok().as_deref() != Some(text.as_str());
                if let Err(err) = std::fs::write(ISSUE_NET, text) {
                    warn!(%err, path = ISSUE_NET, "cannot write the login banner");
                    return;
                }
                let dropin_changed = std::fs::read_to_string(BANNER_DROPIN).ok().as_deref()
                    != Some(dropin.as_str());
                if let Err(err) = std::fs::write(BANNER_DROPIN, &dropin) {
                    warn!(%err, path = BANNER_DROPIN, "cannot write the sshd banner drop-in");
                    return;
                }
                text_changed || dropin_changed
            }
            None => {
                // The banner file itself stays: the image ships an
                // /etc/issue.net and removing sshd's Banner directive
                // is what turns the pre-login text off.
                remove(BANNER_DROPIN)
            }
        };
        if changed {
            // Only a running sshd needs telling; `reload` on a stopped
            // unit is a no-op that logs, which is why it is guarded on
            // an actual change.
            run("systemctl", &["reload", SSH_UNIT]);
        }
    }
}

/// Remove a rendered file; true when it was there to remove.
fn remove(path: &str) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!(%err, path, "cannot remove rendered file");
            }
            false
        }
    }
}

/// Run one OS command, logging (not failing) on error — apply is
/// best-effort like the OS, FRR and SNMP appliers'.
fn run(program: &str, args: &[&str]) {
    let command = || format!("{program} {}", args.join(" "));
    match std::process::Command::new(program).args(args).output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => warn!(
            command = %command(),
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "identity apply command failed"
        ),
        Err(err) => warn!(command = %command(), %err, "cannot run identity apply command"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::intents::extract;
    use hemlock_config::parse;

    fn identity_of(text: &str) -> IdentityIntent {
        extract(&parse(text).unwrap()).unwrap().identity
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

    /// The spec's Part 1.1 seed.
    fn seed() -> IdentityIntent {
        identity_of(
            r#"
system {
    hostname hemlock-a1;
    timezone America/Detroit;
    name-server 10.42.0.5;
    name-server 10.42.0.6;
    domain-name nightshade.systems;
    banner login "Authorized access only. All activity is logged.";
}
"#,
        )
    }

    #[test]
    fn renders_the_resolved_dropin() {
        let rendered = render_resolved(&seed()).unwrap();
        assert_golden(
            &rendered,
            include_str!("../tests/golden/resolved_seed.conf"),
        );
        // Determinism: config order is the render order.
        assert_eq!(render_resolved(&seed()).unwrap(), rendered);
    }

    /// A domain with no resolvers still renders (the search domain is
    /// useful on its own); nothing at all renders nothing.
    #[test]
    fn renders_the_partial_resolver_forms() {
        let domain_only = identity_of("system { domain-name nightshade.systems }");
        let rendered = render_resolved(&domain_only).unwrap();
        assert!(!rendered.contains("DNS="), "{rendered}");
        assert!(rendered.contains("Domains=nightshade.systems"), "{rendered}");

        let servers_only = identity_of("system { name-server 10.42.0.5 }");
        let rendered = render_resolved(&servers_only).unwrap();
        assert!(rendered.contains("DNS=10.42.0.5"), "{rendered}");
        assert!(rendered.contains("FallbackDNS="), "{rendered}");
        assert!(!rendered.contains("Domains="), "{rendered}");

        assert!(render_resolved(&identity_of("")).is_none());
        assert!(render_resolved(&identity_of("system { hostname sw1 }")).is_none());
    }

    #[test]
    fn renders_the_login_banner() {
        assert_golden(
            &render_issue_net(&seed()).unwrap(),
            include_str!("../tests/golden/issue_net_seed.txt"),
        );
        // An escaped newline in the single-line CLI form becomes a real
        // line break, and the file always ends in one.
        let multi = identity_of(r#"system { banner login "line one\nline two" }"#);
        assert_eq!(
            render_issue_net(&multi).unwrap(),
            "line one\nline two\n".to_string()
        );
        assert!(render_issue_net(&identity_of("")).is_none());
    }

    /// The `/etc/hosts` line replaces exactly the machine's own entry,
    /// leaves everything else alone, and is idempotent.
    #[test]
    fn rewrites_only_the_machines_own_hosts_line() {
        const BEFORE: &str = "127.0.0.1\tlocalhost\n\
             127.0.1.1\themlock\n\
             10.42.0.20\tsyslog.nightshade.systems\n\
             ::1\tlocalhost ip6-localhost\n";
        let rendered = render_hosts(BEFORE, &seed());
        assert_golden(
            &rendered,
            "127.0.0.1\tlocalhost\n\
             127.0.1.1\themlock-a1.nightshade.systems hemlock-a1\n\
             10.42.0.20\tsyslog.nightshade.systems\n\
             ::1\tlocalhost ip6-localhost\n",
        );
        assert_eq!(render_hosts(&rendered, &seed()), rendered);

        // No existing self line: the entry is appended.
        let appended = render_hosts("127.0.0.1\tlocalhost\n", &identity_of(""));
        assert_eq!(appended, "127.0.0.1\tlocalhost\n127.0.1.1\themlock\n");

        // Duplicated self lines collapse to one.
        let collapsed = render_hosts("127.0.1.1\told\n127.0.1.1\tolder\n", &identity_of(""));
        assert_eq!(collapsed, "127.0.1.1\themlock\n");
    }

    /// With nothing configured the box is `hemlock` in UTC — the
    /// defaults the image ships.
    #[test]
    fn defaults_stand_in_for_absent_leaves() {
        let empty = identity_of("");
        assert_eq!(empty.effective_hostname(), "hemlock");
        assert_eq!(empty.effective_timezone(), "UTC");
        assert_eq!(empty.fqdn(), None);
        assert_eq!(
            seed().fqdn().as_deref(),
            Some("hemlock-a1.nightshade.systems")
        );
    }
}
