//! OS-side apply for the non-ASIC intent families: management
//! addressing (iproute2 on the manifest's os_device), static routes,
//! and the SSH service (systemd + an sshd_config drop-in).
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

use crate::intents::{Intents, NetdevChange, OsChanges, RouteChange, SshIntent};

/// mgmtd's sshd drop-in (`10-hemlock-motd.conf` is the image's).
const SSHD_DROPIN: &str = "/etc/ssh/sshd_config.d/20-hemlock.conf";
/// Debian's openssh-server unit name.
const SSH_UNIT: &str = "ssh";

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
        if let Some(ssh) = &changes.ssh {
            apply_ssh(ssh);
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
            });
        }
        for (name, intent) in &intents.ports {
            if intent.address.is_some() {
                apply_netdev(
                    &NetdevChange {
                        name: name.clone(),
                        admin_up: None,
                        set_address: intent.address.clone(),
                        del_address: None,
                    },
                    name,
                );
            }
        }
        for (name, intent) in &intents.svis {
            if intent.address.is_some() {
                apply_netdev(
                    &NetdevChange {
                        name: name.clone(),
                        admin_up: None,
                        set_address: intent.address.clone(),
                        del_address: None,
                    },
                    name,
                );
            }
        }
        for (prefix, next_hop) in &intents.routes {
            apply_route(&RouteChange {
                prefix: prefix.clone(),
                next_hop: Some(next_hop.clone()),
            });
        }
        // Declarative: an absent `system { ssh }` block means disabled.
        apply_ssh(&intents.ssh);
    }

    fn apply_management(&self, change: &NetdevChange) {
        let Some(dev) = self.os_device(&change.name) else {
            warn!(interface = %change.name, "no OS netdev known; management change skipped");
            return;
        };
        apply_netdev(change, dev);
    }
}

fn apply_netdev(change: &NetdevChange, dev: &str) {
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

fn apply_route(change: &RouteChange) {
    match &change.next_hop {
        Some(next_hop) => run("ip", &["route", "replace", &change.prefix, "via", next_hop]),
        None => run("ip", &["route", "del", &change.prefix]),
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
