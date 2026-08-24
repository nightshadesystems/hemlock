//! The FRR applier: renders `/etc/frr/frr.conf` + `/etc/frr/daemons`
//! from the FRR intent families (OSPF, BGP, VRRP) and reloads FRR.
//!
//! Shaped like `osapply`: a pure, deterministic render function
//! (unit-tested against golden outputs) plus a best-effort apply that
//! is inert on hosts without FRR installed (`/etc/frr` absent). mgmtd
//! renders and reloads; orch never renders FRR config, and FRR installs
//! its routes into the *kernel* (zebra), where the RIB pipeline picks
//! them up — one pipeline, no side doors.
//!
//! Reload decision: `frr-reload.py --reload` computes and applies the
//! minimal delta and is the first choice; when the script is absent the
//! fallback is `systemctl reload frr` (a full config re-read). A change
//! to the *daemons* file (a protocol appearing or disappearing) needs
//! `systemctl restart frr` instead — reload cannot start or stop
//! daemons. Apply is idempotent: boot replay and commit-confirm expiry
//! re-render the old config through the same path.

use tracing::warn;

use crate::intents::Intents;

const FRR_DIR: &str = "/etc/frr";
const FRR_CONF: &str = "/etc/frr/frr.conf";
const FRR_DAEMONS: &str = "/etc/frr/daemons";
const FRR_RELOAD: &str = "/usr/lib/frr/frr-reload.py";
const FRR_UNIT: &str = "frr";

/// Render the FRR configuration for one intent set: `(frr.conf,
/// daemons)`. `None` = no FRR family is configured (the service stops).
pub fn render_frr(intents: &Intents) -> Option<(String, String)> {
    let vrrp = !intents.vrrp.is_empty();
    if intents.ospf.is_none() && intents.bgp.is_none() && !vrrp {
        return None;
    }
    let router_id = effective_router_id(intents);
    let mut out = String::new();
    let mut line = |text: &str| {
        out.push_str(text);
        out.push('\n');
    };
    line("! Managed by hemlock-mgmtd; edit via the Hemlock config, not here.");
    line("frr defaults traditional");
    line("!");

    // Interface blocks: OSPF per-interface knobs and VRRP groups,
    // merged per interface, sorted by name.
    let mut interfaces: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    if let Some(ospf) = &intents.ospf {
        for (name, knobs) in &ospf.interfaces {
            let body = interfaces.entry(name.clone()).or_default();
            if let Some(cost) = knobs.cost {
                body.push(format!(" ip ospf cost {cost}"));
            }
            if let Some(hello) = knobs.hello_interval {
                body.push(format!(" ip ospf hello-interval {hello}"));
            }
            if let Some(dead) = knobs.dead_interval {
                body.push(format!(" ip ospf dead-interval {dead}"));
            }
            if let Some(priority) = knobs.priority {
                body.push(format!(" ip ospf priority {priority}"));
            }
        }
    }
    for ((name, group), config) in &intents.vrrp {
        let body = interfaces.entry(name.clone()).or_default();
        body.push(format!(" vrrp {group} priority {}", config.priority));
        // FRR takes the advertisement interval in milliseconds.
        body.push(format!(
            " vrrp {group} advertisement-interval {}",
            u32::from(config.advertisement_interval) * 1000
        ));
        for address in &config.addresses {
            body.push(format!(" vrrp {group} ip {address}"));
        }
        if !config.preempt {
            body.push(format!(" no vrrp {group} preempt"));
        }
    }
    for (name, body) in &interfaces {
        line(&format!("interface {name}"));
        for entry in body {
            line(entry);
        }
        line("exit");
        line("!");
    }

    if let Some(ospf) = &intents.ospf {
        line("router ospf");
        if let Some(id) = ospf.router_id.as_ref().or(router_id.as_ref()) {
            line(&format!(" ospf router-id {id}"));
        }
        line(&format!(" maximum-paths {}", ospf.maximum_paths));
        for (area, networks) in &ospf.areas {
            for network in networks {
                line(&format!(" network {network} area {area}"));
            }
        }
        for interface in &ospf.passive_interfaces {
            line(&format!(" passive-interface {interface}"));
        }
        for source in &ospf.redistribute {
            line(&format!(" redistribute {source}"));
        }
        line("exit");
        line("!");
    }

    if let Some(bgp) = &intents.bgp {
        line(&format!("router bgp {}", bgp.as_number));
        if let Some(id) = bgp.router_id.as_ref().or(router_id.as_ref()) {
            line(&format!(" bgp router-id {id}"));
        }
        for (ip, neighbor) in &bgp.neighbors {
            if let Some(remote_as) = neighbor.remote_as {
                line(&format!(" neighbor {ip} remote-as {remote_as}"));
            }
            if let Some(description) = &neighbor.description {
                line(&format!(" neighbor {ip} description {description}"));
            }
            if let Some(ttl) = neighbor.ebgp_multihop {
                line(&format!(" neighbor {ip} ebgp-multihop {ttl}"));
            }
            if neighbor.shutdown {
                line(&format!(" neighbor {ip} shutdown"));
            }
        }
        line(" address-family ipv4 unicast");
        line(&format!("  maximum-paths {}", bgp.maximum_paths));
        for network in &bgp.networks {
            line(&format!("  network {network}"));
        }
        for source in &bgp.redistribute {
            line(&format!("  redistribute {source}"));
        }
        for (ip, neighbor) in &bgp.neighbors {
            if neighbor.next_hop_self {
                line(&format!("  neighbor {ip} next-hop-self"));
            }
        }
        line(" exit-address-family");
        line("exit");
        line("!");
    }

    let daemon = |enabled: bool| if enabled { "yes" } else { "no" };
    let daemons = format!(
        "# Managed by hemlock-mgmtd — which FRR daemons run.\n\
         zebra=yes\n\
         mgmtd=no\n\
         staticd=no\n\
         bgpd={}\n\
         ospfd={}\n\
         ospf6d=no\n\
         ripd=no\n\
         ripngd=no\n\
         isisd=no\n\
         pimd=no\n\
         pim6d=no\n\
         ldpd=no\n\
         nhrpd=no\n\
         eigrpd=no\n\
         babeld=no\n\
         sharpd=no\n\
         pbrd=no\n\
         bfdd=no\n\
         fabricd=no\n\
         vrrpd={}\n\
         pathd=no\n",
        daemon(intents.bgp.is_some()),
        daemon(intents.ospf.is_some()),
        daemon(vrrp),
    );
    Some((out, daemons))
}

/// The router identity FRR renders with: the configured `routing
/// router-id`, else the highest SVI IPv4 address, else the Management
/// address. Derived at render time, never persisted.
pub fn effective_router_id(intents: &Intents) -> Option<String> {
    if let Some(id) = &intents.router_id {
        return Some(id.clone());
    }
    let highest_v4 = |addresses: &mut dyn Iterator<Item = &String>| {
        addresses
            .filter_map(|cidr| cidr.split('/').next())
            .filter_map(|addr| addr.parse::<std::net::Ipv4Addr>().ok())
            .max()
    };
    let svi = highest_v4(&mut intents.svis.values().filter_map(|s| s.address.as_ref()));
    let management = highest_v4(
        &mut intents
            .management
            .values()
            .filter_map(|m| m.address.as_ref()),
    );
    svi.or(management).map(|addr| addr.to_string())
}

pub struct FrrApplier;

impl FrrApplier {
    pub fn new() -> Self {
        Self
    }

    /// Inert on hosts without FRR installed — a dev workstation's FRR
    /// (if any) is never touched.
    fn active(&self) -> bool {
        std::path::Path::new(FRR_DIR).is_dir()
    }

    /// Render and apply the full FRR state for one intent set.
    /// Idempotent; the caller diffs and only invokes on change (or at
    /// boot replay).
    pub fn apply(&self, intents: &Intents) {
        if !self.active() {
            warn!("no /etc/frr; FRR config (ospf/bgp/vrrp) not applied");
            return;
        }
        match render_frr(intents) {
            Some((conf, daemons)) => {
                let daemons_changed =
                    std::fs::read_to_string(FRR_DAEMONS).ok().as_deref() != Some(daemons.as_str());
                if let Err(err) = std::fs::write(FRR_DAEMONS, daemons) {
                    warn!(%err, path = FRR_DAEMONS, "cannot write the FRR daemons file");
                }
                if let Err(err) = std::fs::write(FRR_CONF, conf) {
                    warn!(%err, path = FRR_CONF, "cannot write frr.conf");
                }
                run("systemctl", &["enable", "--now", FRR_UNIT]);
                if daemons_changed {
                    // Reload cannot start/stop member daemons.
                    run("systemctl", &["restart", FRR_UNIT]);
                } else if std::path::Path::new(FRR_RELOAD).exists() {
                    run(FRR_RELOAD, &["--reload", FRR_CONF]);
                } else {
                    run("systemctl", &["reload", FRR_UNIT]);
                }
            }
            None => {
                run("systemctl", &["disable", "--now", FRR_UNIT]);
            }
        }
    }
}

/// Run one OS command, logging (not failing) on error — apply is
/// best-effort like the OS applier's.
fn run(program: &str, args: &[&str]) {
    let command = || format!("{program} {}", args.join(" "));
    match std::process::Command::new(program).args(args).output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => warn!(
            command = %command(),
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "FRR apply command failed"
        ),
        Err(err) => warn!(command = %command(), %err, "cannot run FRR apply command"),
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

    /// The spec's Part 1.1 seed configuration.
    pub fn seed() -> Intents {
        intents_of(
            r#"
vlans {
    vlan 99 { }
    vlan 100 { }
}
interfaces {
    Vlan99 {
        address 10.42.10.9/24
    }
    Vlan100 {
        address 10.0.100.2/24
        vrrp 10 {
            address 10.0.100.1
            priority 200
            advertisement-interval 1
        }
    }
    Ethernet48 {
        address 10.9.9.1/31
    }
}
routing {
    router-id 10.42.0.1
    static {
        0.0.0.0/0 10.42.10.1
        10.99.0.0/16 10.9.9.0
        10.99.0.0/16 10.42.10.7
        192.0.2.0/24 drop
        172.16.0.0/12 10.42.10.1 distance 250
        2001:db8:99::/48 2001:db8:9::1
    }
    arp {
        10.42.10.200 interface Vlan99 mac 00:50:56:be:ef:99
    }
    ospf {
        area 0.0.0.0 {
            network 10.42.10.0/24
        }
        passive-interface Vlan100
        redistribute static
        maximum-paths 4
    }
    bgp {
        as 65000
        neighbor 10.42.10.1 {
            remote-as 65001
            description "upstream"
        }
        network 10.42.0.0/16
        redistribute connected
        maximum-paths 4
    }
}
"#,
        )
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

    #[test]
    fn renders_the_full_seed() {
        let (conf, daemons) = render_frr(&seed()).unwrap();
        assert_golden(&conf, include_str!("../tests/golden/frr_full_seed.conf"));
        assert!(daemons.contains("zebra=yes"));
        assert!(daemons.contains("ospfd=yes"));
        assert!(daemons.contains("bgpd=yes"));
        assert!(daemons.contains("vrrpd=yes"));
        // Determinism.
        assert_eq!(render_frr(&seed()).unwrap().0, conf);
    }

    #[test]
    fn renders_ospf_only() {
        let intents = intents_of(
            "vlans { vlan 99 { } }\ninterfaces { Vlan99 { address 10.42.10.9/24 } }\nrouting { ospf { area 0 { network 10.42.10.0/24 } interface Vlan99 { cost 10\nhello-interval 10\ndead-interval 40 } } }",
        );
        let (conf, daemons) = render_frr(&intents).unwrap();
        assert_golden(&conf, include_str!("../tests/golden/frr_ospf_only.conf"));
        assert!(daemons.contains("ospfd=yes"));
        assert!(daemons.contains("bgpd=no"));
        assert!(daemons.contains("vrrpd=no"));
    }

    #[test]
    fn renders_bgp_only() {
        let intents = intents_of(
            "routing { bgp { as 65000\nrouter-id 10.42.0.9\nneighbor 10.42.10.1 { remote-as 65001\nshutdown\nebgp-multihop 2\nnext-hop-self } network 10.42.0.0/16 } }",
        );
        let (conf, _) = render_frr(&intents).unwrap();
        assert_golden(&conf, include_str!("../tests/golden/frr_bgp_only.conf"));
    }

    #[test]
    fn renders_vrrp_only() {
        let intents = intents_of(
            "vlans { vlan 100 { } }\ninterfaces { Vlan100 { address 10.0.100.2/24\nvrrp 10 { address 10.0.100.1\nno-preempt } } }",
        );
        let (conf, daemons) = render_frr(&intents).unwrap();
        assert_golden(&conf, include_str!("../tests/golden/frr_vrrp.conf"));
        assert!(daemons.contains("vrrpd=yes"));
        assert!(daemons.contains("ospfd=no"));
    }

    #[test]
    fn router_id_falls_back_to_svi_then_management() {
        // Explicit wins.
        assert_eq!(seed().router_id.as_deref(), Some("10.42.0.1"));
        assert_eq!(effective_router_id(&seed()).as_deref(), Some("10.42.0.1"));
        // Highest SVI beats Management.
        let intents = intents_of(
            "vlans { vlan 99 { } vlan 100 { } }\ninterfaces { Vlan99 { address 10.42.10.9/24 } Vlan100 { address 10.0.100.2/24 } Management1 { address 192.168.0.2/24 } }",
        );
        assert_eq!(effective_router_id(&intents).as_deref(), Some("10.42.10.9"));
        // Management is the last resort.
        let intents = intents_of("interfaces { Management1 { address 192.168.0.2/24 } }");
        assert_eq!(
            effective_router_id(&intents).as_deref(),
            Some("192.168.0.2")
        );
        // No FRR families -> nothing to render.
        assert!(render_frr(&intents).is_none());
    }
}
