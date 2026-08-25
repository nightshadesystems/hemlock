//! The SNMP applier: renders `/etc/snmp/snmpd.conf` from the SNMP
//! intent and starts (or stops) net-snmp's `snmpd`.
//!
//! Shaped like `frrapply`: a pure, deterministic render function
//! (unit-tested against golden outputs) plus a best-effort apply that
//! is inert on hosts without net-snmp installed (`/etc/snmp` absent).
//!
//! The rendered agent is deliberately small: it owns transport, auth
//! and the system group only. Interface data is **not** snmpd's —
//! kernel netdev counters on the hostifs do not reflect hardware
//! forwarding, so plain `ifTable` output would be wrong. `master
//! agentx` opens the AgentX socket that orch's subagent registers the
//! IF-MIB on — at a higher precedence than snmpd's own built-in
//! handlers, so the ASIC numbers are the ones that answer.
//!
//! v3 users are the one wrinkle: net-snmp consumes `createUser` and
//! persists the derived keys in its own state file, which then shadows
//! a later passphrase change. A changed render therefore drops the
//! persisted USM state before restarting, so the config is always what
//! the agent runs.

use tracing::warn;

use crate::intents::Intents;

const SNMP_DIR: &str = "/etc/snmp";
const SNMPD_CONF: &str = "/etc/snmp/snmpd.conf";
/// net-snmp's persistent state (derived USM keys live here).
const SNMPD_PERSIST: &str = "/var/lib/snmp/snmpd.conf";
const SNMPD_UNIT: &str = "snmpd";

/// The AgentX master socket orch's subagent connects to. Rendered here
/// and pushed to orch, so the two never disagree about the path.
pub const AGENTX_SOCKET: &str = "/var/agentx/master";

/// Render `snmpd.conf` for one intent set. `None` = SNMP is not
/// configured, so the agent stops.
pub fn render_snmpd(intents: &Intents) -> Option<String> {
    let snmp = &intents.snmp;
    if !snmp.enabled {
        return None;
    }
    // Validation guarantees an address before this runs; the fallback
    // keeps the render total (and a bad one binds nothing, not
    // everything).
    let address = crate::intents::management_address(intents)?;
    let mut out = String::new();
    let mut line = |text: &str| {
        out.push_str(text);
        out.push('\n');
    };
    line("# Managed by hemlock-mgmtd; edit via the Hemlock config, not here.");
    line(&format!("agentaddress udp:{address}:161"));
    line("");
    line("# The IF-MIB comes from hemlock-orch's AgentX subagent (ASIC");
    line("# counters, not kernel netdev ones), so snmpd serves transport,");
    line("# auth and the system group only.");
    line("master agentx");
    line(&format!("agentXSocket {AGENTX_SOCKET}"));
    line("agentXPerms 0660 0550");
    line("");
    if let Some(location) = &snmp.location {
        line(&format!("sysLocation {location}"));
    }
    if let Some(contact) = &snmp.contact {
        line(&format!("sysContact {contact}"));
    }
    if snmp.location.is_some() || snmp.contact.is_some() {
        line("");
    }
    if !snmp.communities.is_empty() {
        line("# v2c, read-only. A community with a source only answers there.");
        for community in &snmp.communities {
            let name = &community.name;
            match &community.source {
                Some(source) => line(&format!("rocommunity {name} {source}")),
                None => line(&format!("rocommunity {name}")),
            }
            // The same name answers over IPv6 transport too.
            match &community.source {
                Some(source) if source.contains(':') => {
                    line(&format!("rocommunity6 {name} {source}"));
                }
                Some(_) => {}
                None => line(&format!("rocommunity6 {name}")),
            }
        }
        line("");
    }
    if !snmp.users.is_empty() {
        line("# v3 USM, read-only authPriv (SHA/AES).");
        for (user, config) in &snmp.users {
            line(&format!(
                "createUser {user} SHA \"{}\" AES \"{}\"",
                config.auth_password, config.priv_password
            ));
        }
        for user in snmp.users.keys() {
            line(&format!("rouser {user} authPriv"));
        }
    }
    Some(out)
}

pub struct SnmpApplier;

impl Default for SnmpApplier {
    fn default() -> Self {
        Self::new()
    }
}

impl SnmpApplier {
    pub fn new() -> Self {
        Self
    }

    /// Inert on hosts without net-snmp installed.
    fn active(&self) -> bool {
        std::path::Path::new(SNMP_DIR).is_dir()
    }

    /// Render and apply the full SNMP state. Idempotent; the caller
    /// diffs the render and only invokes on change (or at boot replay).
    pub fn apply(&self, intents: &Intents) {
        if !self.active() {
            warn!("no /etc/snmp; SNMP config not applied");
            return;
        }
        match render_snmpd(intents) {
            Some(conf) => {
                let changed =
                    std::fs::read_to_string(SNMPD_CONF).ok().as_deref() != Some(conf.as_str());
                if let Err(err) = std::fs::write(SNMPD_CONF, conf) {
                    warn!(%err, path = SNMPD_CONF, "cannot write snmpd.conf");
                    return;
                }
                if changed {
                    // Stale derived USM keys would shadow a changed
                    // passphrase; drop them so the config wins.
                    if let Err(err) = std::fs::remove_file(SNMPD_PERSIST) {
                        if err.kind() != std::io::ErrorKind::NotFound {
                            warn!(%err, path = SNMPD_PERSIST, "cannot clear persisted USM state");
                        }
                    }
                }
                run("systemctl", &["enable", "--now", SNMPD_UNIT]);
                if changed {
                    // snmpd re-reads its config only on restart.
                    run("systemctl", &["restart", SNMPD_UNIT]);
                }
            }
            None => {
                run("systemctl", &["disable", "--now", SNMPD_UNIT]);
            }
        }
    }
}

/// Run one OS command, logging (not failing) on error — apply is
/// best-effort like the OS and FRR appliers'.
fn run(program: &str, args: &[&str]) {
    let command = || format!("{program} {}", args.join(" "));
    match std::process::Command::new(program).args(args).output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => warn!(
            command = %command(),
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "SNMP apply command failed"
        ),
        Err(err) => warn!(command = %command(), %err, "cannot run SNMP apply command"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::intents::extract;
    use hemlock_config::parse;

    fn intents_of(text: &str) -> Intents {
        extract(&parse(text).unwrap()).unwrap()
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

    /// The spec's Part 1.1 seed: two communities (one source-scoped),
    /// location, contact, and one v3 user.
    fn seed() -> Intents {
        intents_of(
            r#"
interfaces {
    Management1 {
        address 10.42.0.9/24
    }
}
services {
    snmp {
        community public;
        community netops source 10.42.0.0/16;
        location "rack 4, closet B";
        contact "cody@nightshade.systems";
        user monitor auth sha "authpass1" priv aes "privpass1";
    }
}
"#,
        )
    }

    #[test]
    fn renders_the_full_seed() {
        let conf = render_snmpd(&seed()).unwrap();
        assert_golden(&conf, include_str!("../tests/golden/snmpd_seed.conf"));
        // Determinism.
        assert_eq!(render_snmpd(&seed()).unwrap(), conf);
    }

    /// A community-only agent renders no USM block, and a bare
    /// `snmp { }` still starts an agent (with nothing to answer).
    #[test]
    fn renders_the_minimal_forms() {
        let intents = intents_of(
            "interfaces { Management1 { address 10.42.0.9/24 } }\nservices { snmp { community public } }",
        );
        assert_golden(
            &render_snmpd(&intents).unwrap(),
            include_str!("../tests/golden/snmpd_community_only.conf"),
        );
        let intents = intents_of(
            "interfaces { Management1 { address 10.42.0.9/24 } }\nservices { snmp { } }",
        );
        let conf = render_snmpd(&intents).unwrap();
        assert!(conf.contains("agentaddress udp:10.42.0.9:161"));
        assert!(!conf.contains("rocommunity"));
        assert!(!conf.contains("createUser"));
    }

    #[test]
    fn no_snmp_block_renders_nothing() {
        assert!(render_snmpd(&intents_of("")).is_none());
        assert!(render_snmpd(&intents_of(
            "interfaces { Management1 { address 10.42.0.9/24 } }"
        ))
        .is_none());
    }

    /// The agent binds the management address, so a config without one
    /// fails the commit rather than listening everywhere.
    #[test]
    fn snmp_needs_a_management_address() {
        let tree = parse("services { snmp { community public } }").unwrap();
        assert_eq!(
            extract(&tree).unwrap_err().to_string(),
            "services snmp: the management interface must carry an address \
             (the agent listens there only)"
        );
    }
}
