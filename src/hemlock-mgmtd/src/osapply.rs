//! OS-side apply for the non-ASIC intent families: management
//! addressing (iproute2 on the manifest's os_device), static routes,
//! the SSH service (systemd + an sshd_config drop-in), and the web
//! console (hemlock-webd's systemd unit).
//!
//! Shell-outs to `ip` and `systemctl` follow the workspace's
//! established access path (syncd's netdev sampling, sysinit's
//! modprobe). Every operation is idempotent (`addr replace`, `route
//! replace`, `enable --now`), so boot-time replay and commit-time
//! diffs converge to the same state.
//!
//! Apply is deliberately best-effort: a failed OS command is logged
//! and the commit still persists — the config stays the source of
//! truth and the next boot's replay converges. Without a platform
//! manifest (dev hosts) the whole applier is inert, so a
//! workstation's sshd and routing table are never touched.

use tracing::warn;

use crate::intents::{
    ArpChange, Intents, NetdevChange, OsChanges, RouteChange, SshIntent, VrrpMacvlanChange,
    WebIntent,
};

/// mgmtd's sshd drop-in (`10-hemlock-motd.conf` is the image's).
const SSHD_DROPIN: &str = "/etc/ssh/sshd_config.d/20-hemlock.conf";
/// Debian's openssh-server unit name.
const SSH_UNIT: &str = "ssh";
/// The web console daemon's unit. webd reads the running config itself
/// (which listeners, TLS); mgmtd only decides whether it runs.
const WEBD_UNIT: &str = "hemlock-webd";

/// `authentication local`: PAM password logins against the on-box
/// user database, pinned against other drop-ins overriding them.
const LOCAL_AUTH_DROPIN: &str = "\
# Managed by hemlock-mgmtd — `set system ssh authentication local`.
PasswordAuthentication yes
KbdInteractiveAuthentication yes
UsePAM yes
";

pub struct OsApplier {
    /// (CLI name, OS netdev) of the management port, from the platform
    /// manifest. None = off-switch; nothing is ever applied.
    management: Option<(String, String)>,
}

impl OsApplier {
    pub fn new(management: Option<(String, String)>) -> Self {
        Self { management }
    }

    fn active(&self) -> bool {
        self.management.is_some()
    }

    /// The manifest's management interface name, when on-switch.
    pub fn management_interface(&self) -> Option<&str> {
        self.management.as_ref().map(|(name, _)| name.as_str())
    }

    fn os_device(&self, interface: &str) -> Option<&str> {
        self.management
            .as_ref()
            .and_then(|(name, dev)| (name == interface).then_some(dev.as_str()))
    }

    /// Apply one commit's OS-side delta.
    pub fn apply(&self, changes: &OsChanges) {
        if changes.is_empty() {
            return;
        }
        if !self.active() {
            warn!("no platform manifest; OS-side config (management address, routes, ssh) not applied");
            return;
        }
        for change in &changes.management {
            self.apply_management(change);
        }
        for change in &changes.ports {
            // A front-panel port's hostif netdev is named after it.
            apply_netdev(change, &change.name);
        }
        for change in &changes.svis {
            // An SVI's kernel bridge netdev is named after it (VlanN),
            // created by syncd just before this applier runs.
            apply_netdev(change, &change.name);
        }
        for route in &changes.routes {
            apply_route(route);
        }
        for arp in &changes.arp {
            self.apply_arp(arp);
        }
        for macvlan in &changes.vrrp_macvlans {
            self.apply_vrrp_macvlan(macvlan);
        }
        if let Some(ssh) = &changes.ssh {
            apply_ssh(ssh);
        }
        if let Some(web) = &changes.web {
            apply_web(web);
        }
    }

    /// Boot-time replay: drive the OS to the full running-config state.
    pub fn replay(&self, intents: &Intents) {
        if !self.active() {
            return;
        }
        // Early boot races udev: the management NIC's netdev may not
        // exist yet when mgmtd starts, and every `ip` command against it
        // would fail (leaving the box unreachable until the next
        // commit). Bounded wait; on timeout apply anyway and let the
        // per-command warnings tell the story.
        if let Some((_, dev)) = &self.management {
            let sysfs = format!("/sys/class/net/{dev}");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            while !std::path::Path::new(&sysfs).exists() {
                if std::time::Instant::now() >= deadline {
                    warn!(netdev = %dev, "management netdev still absent; applying anyway");
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
        for (name, intent) in &intents.management {
            self.apply_management(&NetdevChange {
                name: name.clone(),
                admin_up: intent.admin_up,
                set_address: intent.address.clone(),
                del_address: None,
                set_mtu: intent.mtu,
            });
        }
        for (name, intent) in &intents.ports {
            if intent.address.is_some() || intent.mtu.is_some() {
                apply_netdev(
                    &NetdevChange {
                        name: name.clone(),
                        admin_up: None,
                        set_address: intent.address.clone(),
                        del_address: None,
                        set_mtu: intent.mtu,
                    },
                    name,
                );
            }
        }
        for (name, intent) in &intents.svis {
            if intent.address.is_some() || intent.mtu.is_some() {
                apply_netdev(
                    &NetdevChange {
                        name: name.clone(),
                        admin_up: None,
                        set_address: intent.address.clone(),
                        del_address: None,
                        set_mtu: intent.mtu,
                    },
                    name,
                );
            }
        }
        for (prefix, route) in &intents.routes {
            apply_route(&RouteChange {
                prefix: prefix.clone(),
                old: None,
                new: Some(route.clone()),
            });
        }
        for (ip, entry) in &intents.arp_statics {
            self.apply_arp(&ArpChange {
                ip: ip.clone(),
                old: None,
                new: Some(entry.clone()),
            });
        }
        for (interface, group) in intents.vrrp.keys() {
            self.apply_vrrp_macvlan(&VrrpMacvlanChange {
                interface: interface.clone(),
                group: *group,
                create: true,
            });
        }
        // Declarative: an absent `system { ssh }` block means disabled.
        apply_ssh(&intents.ssh);
        // Same for the web console (`system { http }` / `{ https }`).
        apply_web(&intents.web);
    }

    fn apply_management(&self, change: &NetdevChange) {
        let Some(dev) = self.os_device(&change.name) else {
            warn!(interface = %change.name, "no OS netdev known; management change skipped");
            return;
        };
        apply_netdev(change, dev);
    }

    /// The kernel netdev an interface name maps to: the manifest's
    /// os_device for the management port, the identically named
    /// hostif/bridge netdev otherwise.
    fn netdev_of<'a>(&'a self, interface: &'a str) -> &'a str {
        self.os_device(interface).unwrap_or(interface)
    }

    /// One VRRP macvlan: `ip link add vrrp4-<if>-<group> link
    /// <parent> addr 00:00:5e:00:01:<group> type macvlan mode bridge`.
    /// FRR's vrrpd finds it by parent + virtual MAC; created before the
    /// FRR reload (this applier runs first), removed on delete.
    fn apply_vrrp_macvlan(&self, change: &VrrpMacvlanChange) {
        let name = crate::intents::vrrp_macvlan_name(&change.interface, change.group);
        if change.create {
            run(
                "ip",
                &[
                    "link",
                    "add",
                    &name,
                    "link",
                    self.netdev_of(&change.interface),
                    "addr",
                    &crate::intents::vrrp_virtual_mac(change.group),
                    "type",
                    "macvlan",
                    "mode",
                    "bridge",
                ],
            );
            run("ip", &["link", "set", "dev", &name, "up"]);
        } else {
            run("ip", &["link", "del", &name]);
        }
    }

    /// One static ARP/ND entry: `ip [-6] neigh replace <ip> lladdr
    /// <mac> dev <netdev> nud permanent`. An interface change deletes
    /// the entry on the old netdev first.
    fn apply_arp(&self, change: &ArpChange) {
        let v6 = change.ip.contains(':');
        let ip = |args: Vec<&str>| {
            let mut full = if v6 {
                vec!["-6", "neigh"]
            } else {
                vec!["neigh"]
            };
            full.extend(args);
            run("ip", &full);
        };
        if let Some(old) = &change.old {
            let stale = match &change.new {
                None => true,
                Some(new) => new.interface != old.interface,
            };
            if stale {
                ip(vec![
                    "del",
                    &change.ip,
                    "dev",
                    self.netdev_of(&old.interface),
                ]);
            }
        }
        if let Some(new) = &change.new {
            ip(vec![
                "replace",
                &change.ip,
                "lladdr",
                &new.mac,
                "dev",
                self.netdev_of(&new.interface),
                "nud",
                "permanent",
            ]);
        }
    }
}

fn apply_netdev(change: &NetdevChange, dev: &str) {
    // MTU before the address: a jumbo address wants the jumbo link
    // already, and the kernel refuses an MTU below an existing one's
    // needs on some tunnels.
    if let Some(mtu) = change.set_mtu {
        run("ip", &["link", "set", "dev", dev, "mtu", &mtu.to_string()]);
    }
    if let Some(old) = &change.del_address {
        run("ip", &["addr", "del", old, "dev", dev]);
    }
    if let Some(cidr) = &change.set_address {
        run("ip", &["addr", "replace", cidr, "dev", dev]);
        // An address implies the link should carry traffic, unless the
        // config says disabled outright.
        if change.admin_up != Some(false) {
            run("ip", &["link", "set", "dev", dev, "up"]);
        }
        // Belt and braces: the kernel installs the connected route with
        // the address, but the route can go missing without the address
        // (seen in the field: address present, subnet unreachable —
        // every reply then dies routeless). `route replace` of what
        // should already exist is a no-op in the healthy case. Skipped
        // for shut interfaces (a down device rejects routes).
        if change.admin_up != Some(false) {
            if let Ok(prefix) = hemlock_common::net::canonical_prefix(cidr) {
                if !prefix.ends_with("/32") && !prefix.ends_with("/128") {
                    run("ip", &["route", "replace", &prefix, "dev", dev]);
                }
            }
        }
    }
    if let Some(up) = change.admin_up {
        run(
            "ip",
            &["link", "set", "dev", dev, if up { "up" } else { "down" }],
        );
    }
}

/// One kernel route per prefix: `ip [-6] route replace <prefix>
/// [metric <distance>]` with one `nexthop via <nh>` per ECMP entry, or
/// `blackhole` for drop routes.
fn apply_route(change: &RouteChange) {
    let v6 = change.prefix.contains(':');
    let ip = |args: Vec<String>| {
        let mut full: Vec<&str> = if v6 {
            vec!["-6", "route"]
        } else {
            vec!["route"]
        };
        full.extend(args.iter().map(String::as_str));
        run("ip", &full);
    };
    let metric = |args: &mut Vec<String>, distance: u8| {
        if distance != 1 {
            args.extend(["metric".into(), distance.to_string()]);
        }
    };
    // The metric is part of a kernel route's identity: a replace at a
    // new metric leaves the old route installed, and a blackhole is a
    // different route type, so either change deletes the old route
    // first.
    if let Some(old) = &change.old {
        let stale = match &change.new {
            None => true,
            Some(new) => new.distance != old.distance || new.drop != old.drop,
        };
        if stale {
            let mut args = vec!["del".to_string()];
            if old.drop {
                args.push("blackhole".into());
            }
            args.push(change.prefix.clone());
            metric(&mut args, old.distance);
            ip(args);
        }
    }
    if let Some(new) = &change.new {
        let mut args = vec!["replace".to_string()];
        if new.drop {
            args.push("blackhole".into());
        }
        args.push(change.prefix.clone());
        metric(&mut args, new.distance);
        for next_hop in &new.next_hops {
            args.extend(["nexthop".into(), "via".into(), next_hop.clone()]);
        }
        ip(args);
    }
}

fn apply_ssh(ssh: &SshIntent) {
    if ssh.enabled {
        if ssh.auth_local {
            if let Err(err) = std::fs::write(SSHD_DROPIN, LOCAL_AUTH_DROPIN) {
                warn!(%err, path = SSHD_DROPIN, "cannot write sshd drop-in");
            }
        } else {
            remove_dropin();
        }
        run("systemctl", &["enable", "--now", SSH_UNIT]);
        // Pick up drop-in changes when sshd was already running.
        run("systemctl", &["reload", SSH_UNIT]);
    } else {
        remove_dropin();
        run("systemctl", &["disable", "--now", SSH_UNIT]);
    }
}

fn apply_web(web: &WebIntent) {
    // Stopping or restarting webd must NOT happen synchronously inside
    // the commit: when the commit came from the web console, webd is
    // blocked waiting on this very Commit RPC, and a synchronous
    // `systemctl restart/stop` waits for webd to finish that request —
    // a deadlock that wedges mgmtd (and every later commit) until
    // systemd's stop timeout force-kills webd. Defer the unit change to
    // a detached thread with a short grace delay so the commit returns
    // and webd flushes its response first.
    let enabled = web.enabled();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if enabled {
            run("systemctl", &["enable", WEBD_UNIT]);
            // Restart (not just enable --now): a listener-set change
            // (http added/removed alongside https) needs webd to
            // re-read the running config. On boot replay this simply
            // starts it.
            run("systemctl", &["restart", WEBD_UNIT]);
        } else {
            run("systemctl", &["disable", "--now", WEBD_UNIT]);
        }
    });
}

fn remove_dropin() {
    if let Err(err) = std::fs::remove_file(SSHD_DROPIN) {
        if err.kind() != std::io::ErrorKind::NotFound {
            warn!(%err, path = SSHD_DROPIN, "cannot remove sshd drop-in");
        }
    }
}

/// Run one OS command, logging (not failing) on error — see the module
/// doc for why apply is best-effort.
fn run(program: &str, args: &[&str]) {
    let command = || format!("{program} {}", args.join(" "));
    match std::process::Command::new(program).args(args).output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => warn!(
            command = %command(),
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "OS apply command failed"
        ),
        Err(err) => warn!(command = %command(), %err, "cannot run OS apply command"),
    }
}
