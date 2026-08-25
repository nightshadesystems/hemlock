//! The DHCP-server applier: renders `/etc/dnsmasq.d/hemlock.conf` from
//! the pool intents and starts (or stops) dnsmasq.
//!
//! Shaped like `frrapply` and `snmpapply`: a pure, deterministic render
//! function (unit-tested against golden outputs) plus a best-effort
//! apply that is inert on hosts without dnsmasq installed
//! (`/etc/dnsmasq.d` absent).
//!
//! dnsmasq is a DHCP server here and nothing else. `port=0` turns its
//! DNS half off outright — a switch that answered DNS queries on its
//! management network would be a surprise nobody asked for — and
//! `bind-dynamic` keeps it on the SVIs whose subnets it actually
//! serves rather than every interface on the box.
//!
//! Snooping interaction: a pool serving a snooped VLAN needs no
//! trusted-port configuration. The box's own replies originate on the
//! CPU rather than arriving on a front-panel port, so they never meet
//! the untrusted-port trap that drops rogue servers.

use tracing::warn;

use crate::intents::Intents;

const DNSMASQ_DIR: &str = "/etc/dnsmasq.d";
const DNSMASQ_CONF: &str = "/etc/dnsmasq.d/hemlock.conf";
const DNSMASQ_UNIT: &str = "dnsmasq";

/// Where dnsmasq records its leases; orch reads this for
/// `show dhcp server leases`.
pub const DNSMASQ_LEASES: &str = "/var/lib/misc/dnsmasq.leases";

/// Render the dnsmasq configuration for one intent set. `None` = no
/// pools, so the server stops.
pub fn render_dnsmasq(intents: &Intents) -> Option<String> {
    if intents.dhcp_server.is_empty() {
        return None;
    }
    let mut out = String::new();
    let mut line = |text: &str| {
        out.push_str(text);
        out.push('\n');
    };
    line("# Managed by hemlock-mgmtd; edit via the Hemlock config, not here.");
    line("# DHCP only: the DNS half is off.");
    line("port=0");
    line("bind-dynamic");
    line(&format!("dhcp-leasefile={DNSMASQ_LEASES}"));
    // Answer only what a pool covers: an unmatched request is not this
    // switch's to answer.
    line("dhcp-authoritative");

    for (name, pool) in &intents.dhcp_server {
        let Some((_, prefix_len)) = pool.subnet() else {
            continue;
        };
        let Some((start, end)) = pool.range else {
            continue;
        };
        line("");
        line(&format!(
            "# pool {name} ({})",
            pool.network.clone().unwrap_or_default()
        ));
        let netmask = netmask(prefix_len);
        line(&format!(
            "dhcp-range=set:{name},{start},{end},{netmask},{}",
            pool.lease()
        ));
        if let Some(gateway) = pool.default_gateway {
            line(&format!("dhcp-option=tag:{name},option:router,{gateway}"));
        }
        if !pool.dns_servers.is_empty() {
            let servers: Vec<String> = pool.dns_servers.iter().map(|s| s.to_string()).collect();
            line(&format!(
                "dhcp-option=tag:{name},option:dns-server,{}",
                servers.join(",")
            ));
        }
        if let Some(domain) = &pool.domain_name {
            line(&format!(
                "dhcp-option=tag:{name},option:domain-name,{domain}"
            ));
        }
        for (mac, address) in &pool.reservations {
            // A host entry sits outside the tag system on purpose: a
            // reservation must win wherever the client turns up.
            line(&format!("dhcp-host={mac},{address}"));
        }
    }
    Some(out)
}

/// A prefix length as a dotted netmask (dnsmasq wants the mask form).
fn netmask(prefix_len: u8) -> std::net::Ipv4Addr {
    let bits = u32::from(prefix_len.min(32));
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    std::net::Ipv4Addr::from(mask)
}

pub struct DnsmasqApplier;

impl Default for DnsmasqApplier {
    fn default() -> Self {
        Self::new()
    }
}

impl DnsmasqApplier {
    pub fn new() -> Self {
        Self
    }

    /// Inert on hosts without dnsmasq installed.
    fn active(&self) -> bool {
        std::path::Path::new(DNSMASQ_DIR).is_dir()
    }

    /// Render and apply the full DHCP-server state. Idempotent; the
    /// caller diffs the render and only invokes on change (or at boot
    /// replay).
    pub fn apply(&self, intents: &Intents) {
        if !self.active() {
            warn!("no /etc/dnsmasq.d; DHCP server config not applied");
            return;
        }
        match render_dnsmasq(intents) {
            Some(conf) => {
                let changed =
                    std::fs::read_to_string(DNSMASQ_CONF).ok().as_deref() != Some(conf.as_str());
                if let Err(err) = std::fs::write(DNSMASQ_CONF, conf) {
                    warn!(%err, path = DNSMASQ_CONF, "cannot write the dnsmasq config");
                    return;
                }
                run("systemctl", &["enable", "--now", DNSMASQ_UNIT]);
                if changed {
                    // dnsmasq re-reads its config only on restart; a
                    // SIGHUP reloads host files, not ranges.
                    run("systemctl", &["restart", DNSMASQ_UNIT]);
                }
            }
            None => {
                // No pools: stop serving, and take the config with it
                // so a later install cannot resurrect stale ranges.
                if let Err(err) = std::fs::remove_file(DNSMASQ_CONF) {
                    if err.kind() != std::io::ErrorKind::NotFound {
                        warn!(%err, path = DNSMASQ_CONF, "cannot remove the dnsmasq config");
                    }
                }
                run("systemctl", &["disable", "--now", DNSMASQ_UNIT]);
            }
        }
    }
}

/// Run one OS command, logging (not failing) on error — apply is
/// best-effort like the other appliers'.
fn run(program: &str, args: &[&str]) {
    let command = || format!("{program} {}", args.join(" "));
    match std::process::Command::new(program).args(args).output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => warn!(
            command = %command(),
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "dnsmasq apply command failed"
        ),
        Err(err) => warn!(command = %command(), %err, "cannot run dnsmasq apply command"),
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

    /// The spec's Part 1.1 seed pool.
    fn seed() -> Intents {
        intents_of(
            r#"
services {
    dhcp-server {
        pool LAN-USERS {
            network 10.0.10.0/24;
            range 10.0.10.100 10.0.10.200;
            default-gateway 10.0.10.1;
            dns-server 10.42.0.5;
            dns-server 10.42.0.6;
            lease-time 86400;
            reservation 00:1c:73:0c:aa:01 address 10.0.10.50;
        }
    }
}
"#,
        )
    }

    #[test]
    fn renders_the_full_seed() {
        let conf = render_dnsmasq(&seed()).unwrap();
        assert_golden(&conf, include_str!("../tests/golden/dnsmasq_seed.conf"));
        // Determinism.
        assert_eq!(render_dnsmasq(&seed()).unwrap(), conf);
    }

    fn two_pools() -> Intents {
        intents_of(
            r#"
services {
    dhcp-server {
        pool GUEST {
            network 10.0.20.0/25;
            range 10.0.20.10 10.0.20.100;
            default-gateway 10.0.20.1;
            domain-name "guest.nightshade.systems";
        }
        pool LAN-USERS {
            network 10.0.10.0/24;
            range 10.0.10.100 10.0.10.200;
            default-gateway 10.0.10.1;
        }
    }
}
"#,
        )
    }

    /// Two pools render in name order, each with its own tag, and the
    /// optional leaves really are optional.
    #[test]
    fn renders_multiple_pools() {
        assert_golden(
            &render_dnsmasq(&two_pools()).unwrap(),
            include_str!("../tests/golden/dnsmasq_two_pools.conf"),
        );
    }

    #[test]
    fn no_pools_render_nothing() {
        assert!(render_dnsmasq(&intents_of("")).is_none());
        assert!(render_dnsmasq(&intents_of("services { lldp { } }")).is_none());
    }

    /// Prefix lengths become the dotted masks dnsmasq wants.
    #[test]
    fn netmasks_render_dotted() {
        assert_eq!(netmask(24).to_string(), "255.255.255.0");
        assert_eq!(netmask(25).to_string(), "255.255.255.128");
        assert_eq!(netmask(16).to_string(), "255.255.0.0");
        assert_eq!(netmask(32).to_string(), "255.255.255.255");
        assert_eq!(netmask(0).to_string(), "0.0.0.0");
    }

    /// The seed pool's shape is what the render (and, downstream,
    /// `show dhcp server`) reads.
    #[test]
    fn the_seed_pool_reads_back() {
        let intents = seed();
        let pool = &intents.dhcp_server["LAN-USERS"];
        assert_eq!(
            pool.subnet(),
            Some((std::net::Ipv4Addr::new(10, 0, 10, 0), 24))
        );
        assert_eq!(pool.lease(), 86400);
        assert_eq!(pool.dns_servers.len(), 2);
        assert_eq!(pool.reservations.len(), 1);
    }
}
