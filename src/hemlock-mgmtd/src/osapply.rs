//! OS-side apply for the non-ASIC intent families: management
//! addressing (iproute2 on the manifest's os_device), static routes,
//! the SSH service (systemd + an sshd_config drop-in), the web
//! console (hemlock-webd's systemd unit), and the NTP client
//! (systemd-timesyncd + a timesyncd.conf drop-in).
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

use hemlock_common::role::Role;

use crate::intents::{
    ArpChange, Intents, NetdevChange, NtpIntent, OsChanges, RouteChange, SshIntent, UserChange,
    UserIntent, VrrpMacvlanChange, WebIntent,
};

/// The login shell every Hemlock operator account gets.
pub const OPERATOR_SHELL: &str = "/usr/bin/hemlockctl";
/// Group membership that grants the daemon sockets (and so both
/// consoles); `sudo` is added for administrators on top.
const HEMLOCK_GROUP: &str = "hemlock";
const SUDO_GROUP: &str = "sudo";

/// The uid floor for accounts Hemlock is allowed to manage. Everything
/// below it belongs to the distribution — `root`, the service
/// accounts, the daemons' own users — and the applier never creates,
/// modifies or deletes one, whatever the config says. The intent
/// extractor rejects the reserved names by spelling; this is the
/// backstop that catches a name the distribution happens to use on
/// *this* box.
const MANAGED_UID_MIN: u32 = 1000;
/// Above this range are the nss/nobody accounts.
const MANAGED_UID_MAX: u32 = 60000;

/// mgmtd's sshd drop-in (`10-hemlock-motd.conf` is the image's).
const SSHD_DROPIN: &str = "/etc/ssh/sshd_config.d/20-hemlock.conf";
/// Debian's openssh-server unit name.
const SSH_UNIT: &str = "ssh";
/// The web console daemon's unit. webd reads the running config itself
/// (which listeners, TLS); mgmtd only decides whether it runs.
const WEBD_UNIT: &str = "hemlock-webd";

/// mgmtd's timesyncd drop-in and the unit it configures.
const TIMESYNCD_DROPIN_DIR: &str = "/etc/systemd/timesyncd.conf.d";
const TIMESYNCD_DROPIN: &str = "/etc/systemd/timesyncd.conf.d/20-hemlock.conf";
const TIMESYNCD_UNIT: &str = "systemd-timesyncd";

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
        for user in &changes.users {
            apply_user(user);
        }
        if let Some(ssh) = &changes.ssh {
            apply_ssh(ssh);
        }
        if let Some(web) = &changes.web {
            apply_web(web);
        }
        if let Some(ntp) = &changes.ntp {
            apply_ntp(ntp);
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
        // Login accounts: materialize every configured user. Removals
        // have no replay — the running config is the whole truth, and
        // an account it never named was never ours to delete.
        for (name, user) in &intents.login {
            apply_user(&UserChange {
                name: name.clone(),
                set: Some(user.clone()),
            });
        }
        // Declarative: an absent `system { ssh }` block means disabled.
        apply_ssh(&intents.ssh);
        // Same for the web console (`system { http }` / `{ https }`).
        apply_web(&intents.web);
        // And for the NTP client: no servers means timesyncd stops.
        apply_ntp(&intents.ntp);
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

/// One account's fields in a `/etc/passwd` text: (uid, home, shell).
/// `None` = no such account.
fn passwd_entry_in(passwd: &str, name: &str) -> Option<(u32, String, String)> {
    passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.first() != Some(&name) || fields.len() < 7 {
            return None;
        }
        Some((
            fields[2].trim().parse().ok()?,
            fields[5].to_string(),
            fields[6].to_string(),
        ))
    })
}

fn passwd_entry(name: &str) -> Option<(u32, String, String)> {
    passwd_entry_in(&std::fs::read_to_string("/etc/passwd").ok()?, name)
}

/// May Hemlock manage this account, given a `/etc/passwd` text? An
/// account that does not exist yet is fair game (the applier creates it
/// in the managed range); one that does must sit in the regular-user
/// uid range. See [`MANAGED_UID_MIN`] for why.
pub fn manageable_account_in(passwd: &str, name: &str) -> bool {
    match passwd_entry_in(passwd, name) {
        None => true,
        Some((uid, _, _)) => (MANAGED_UID_MIN..MANAGED_UID_MAX).contains(&uid),
    }
}

/// The role an account has by its OS groups: `sudo` membership means
/// administrator. Used only when the config manages no login users, so
/// a box that predates config-managed users keeps working exactly as it
/// did.
pub fn os_role_in(group: &str, passwd: &str, name: &str) -> Role {
    let mut gid: Option<String> = None;
    for line in group.lines() {
        let mut fields = line.split(':');
        if fields.next() != Some(SUDO_GROUP) {
            continue;
        }
        let _password = fields.next();
        gid = fields.next().map(|g| g.trim().to_string());
        if let Some(members) = fields.next() {
            if members.split(',').any(|m| m.trim() == name) {
                return Role::Admin;
            }
        }
        break;
    }
    // Primary-group membership: the passwd entry's gid field.
    let Some(gid) = gid else {
        return Role::Operator;
    };
    let primary = passwd.lines().any(|line| {
        let mut fields = line.split(':');
        fields.next() == Some(name) && fields.nth(2).map(str::trim) == Some(gid.trim())
    });
    if primary {
        Role::Admin
    } else {
        Role::Operator
    }
}

/// [`os_role_in`] against this box's account databases.
pub fn os_role(name: &str) -> Role {
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    os_role_in(&read("/etc/group"), &read("/etc/passwd"), name)
}

/// [`manageable_account_in`] against this box's `/etc/passwd`.
pub fn manageable_account(name: &str) -> bool {
    match std::fs::read_to_string("/etc/passwd") {
        Ok(passwd) => manageable_account_in(&passwd, name),
        // No passwd file at all (a development host): nothing here is
        // ours to manage, and apply is inert anyway.
        Err(_) => true,
    }
}

/// The `authorized_keys` file for one account, rendered whole. The
/// header names the owner so an operator reading it knows not to edit
/// it by hand.
pub fn render_authorized_keys(user: &UserIntent) -> String {
    let mut out =
        String::from("# Managed by hemlock-mgmtd; edit via the Hemlock config, not here.\n");
    for key in &user.ssh_keys {
        out.push_str(key);
        out.push('\n');
    }
    out
}

/// Create, update or remove one login account.
///
/// Idempotent throughout: `useradd` runs only when the account is
/// missing, and every other step is a `usermod`/file write that
/// converges. Accounts outside the managed uid range are never touched
/// — see [`MANAGED_UID_MIN`].
fn apply_user(change: &UserChange) {
    let name = &change.name;
    if !manageable_account(name) {
        warn!(user = %name, "not a Hemlock-managed account (uid out of range); left alone");
        return;
    }
    let Some(user) = &change.set else {
        // Remove: `userdel -r` takes the home directory (and the
        // authorized_keys inside it) with it.
        run("userdel", &["-r", name]);
        return;
    };

    let groups = if user.role.is_admin() {
        format!("{SUDO_GROUP},{HEMLOCK_GROUP}")
    } else {
        HEMLOCK_GROUP.to_string()
    };
    let existing = passwd_entry(name);
    if existing.is_none() {
        run(
            "useradd",
            &["-m", "-s", OPERATOR_SHELL, "-G", &groups, name],
        );
    } else {
        // `-G` replaces the supplementary set, which is what demoting
        // an admin has to do: dropping `sudo` is the whole point.
        run("usermod", &["-s", OPERATOR_SHELL, "-G", &groups, name]);
    }

    match &user.password_hash {
        Some(hash) => set_password_hash(name, hash),
        // Key-only account: lock the password field rather than leave
        // whatever was there before.
        None => run("passwd", &["-l", name]),
    }
    write_authorized_keys(name, user);
}

/// `usermod -p <hash>` — the hash goes straight into `/etc/shadow`,
/// already hashed at the prompt, so no plaintext ever reaches an argv.
fn set_password_hash(name: &str, hash: &str) {
    run("usermod", &["-p", hash, name]);
}

/// Write `~/.ssh/authorized_keys` for one account, owned by it and
/// mode 0600 — sshd refuses a group- or world-writable file.
fn write_authorized_keys(name: &str, user: &UserIntent) {
    let Some((_, home, _)) = passwd_entry(name) else {
        warn!(user = %name, "account has no home directory; ssh keys not written");
        return;
    };
    if home.is_empty() {
        return;
    }
    let ssh_dir = std::path::Path::new(&home).join(".ssh");
    if user.ssh_keys.is_empty() {
        // No keys: remove the file rather than leave a stale one, but
        // keep ~/.ssh (it may hold the account's own known_hosts).
        let path = ssh_dir.join("authorized_keys");
        if let Err(err) = std::fs::remove_file(&path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!(%err, path = %path.display(), "cannot remove authorized_keys");
            }
        }
        return;
    }
    if let Err(err) = std::fs::create_dir_all(&ssh_dir) {
        warn!(%err, path = %ssh_dir.display(), "cannot create .ssh");
        return;
    }
    let path = ssh_dir.join("authorized_keys");
    if let Err(err) = std::fs::write(&path, render_authorized_keys(user)) {
        warn!(%err, path = %path.display(), "cannot write authorized_keys");
        return;
    }
    // chown/chmod through the OS tools: the applier is already a
    // shell-out layer, and this keeps the unix-permissions handling in
    // one idiom rather than behind a cfg(unix) fs::Permissions branch.
    run(
        "chown",
        &[
            "-R",
            &format!("{name}:{name}"),
            &ssh_dir.display().to_string(),
        ],
    );
    run("chmod", &["700", &ssh_dir.display().to_string()]);
    run("chmod", &["600", &path.display().to_string()]);
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

/// Render the timesyncd drop-in for one NTP intent. `None` = no
/// servers, so the client is off and the drop-in is removed.
///
/// `FallbackNTP=` is written empty on purpose: Debian ships a distro
/// fallback pool, and a switch told to use one clock source must not
/// quietly reach a different one.
pub fn render_timesyncd(ntp: &NtpIntent) -> Option<String> {
    if ntp.servers.is_empty() {
        return None;
    }
    Some(format!(
        "# Managed by hemlock-mgmtd; edit via the Hemlock config, not here.\n\
         [Time]\n\
         NTP={}\n\
         FallbackNTP=\n",
        ntp.servers.join(" ")
    ))
}

/// Write (or remove) the drop-in and start (or stop) timesyncd.
/// Idempotent: boot replay and commit-confirm expiry take the same
/// path. timesyncd has no reload, so a changed server list restarts it.
fn apply_ntp(ntp: &NtpIntent) {
    match render_timesyncd(ntp) {
        Some(dropin) => {
            if let Err(err) = std::fs::create_dir_all(TIMESYNCD_DROPIN_DIR) {
                warn!(%err, path = TIMESYNCD_DROPIN_DIR, "cannot create the timesyncd drop-in dir");
            }
            let changed =
                std::fs::read_to_string(TIMESYNCD_DROPIN).ok().as_deref() != Some(dropin.as_str());
            if let Err(err) = std::fs::write(TIMESYNCD_DROPIN, dropin) {
                warn!(%err, path = TIMESYNCD_DROPIN, "cannot write the timesyncd drop-in");
                return;
            }
            run("systemctl", &["enable", "--now", TIMESYNCD_UNIT]);
            if changed {
                run("systemctl", &["restart", TIMESYNCD_UNIT]);
            }
        }
        None => {
            remove_timesyncd_dropin();
            run("systemctl", &["disable", "--now", TIMESYNCD_UNIT]);
        }
    }
}

fn remove_timesyncd_dropin() {
    if let Err(err) = std::fs::remove_file(TIMESYNCD_DROPIN) {
        if err.kind() != std::io::ErrorKind::NotFound {
            warn!(%err, path = TIMESYNCD_DROPIN, "cannot remove the timesyncd drop-in");
        }
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::intents::extract;
    use hemlock_config::parse;

    fn ntp_of(text: &str) -> NtpIntent {
        extract(&parse(text).unwrap()).unwrap().ntp
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

    /// The spec seed's NTP block, and a full four-server list.
    #[test]
    fn renders_the_timesyncd_dropin() {
        let seed = ntp_of(
            "services { ntp { server 10.42.0.5
server pool.ntp.org } }",
        );
        let rendered = render_timesyncd(&seed).unwrap();
        assert_golden(
            &rendered,
            include_str!("../tests/golden/timesyncd_seed.conf"),
        );
        // Determinism: config order is the render order.
        assert_eq!(render_timesyncd(&seed).unwrap(), rendered);

        let four = ntp_of(
            "services { ntp { server 2001:db8::123
server ntp1.example.net
server ntp2.example.net
server 10.0.0.1 } }",
        );
        assert_golden(
            &render_timesyncd(&four).unwrap(),
            include_str!("../tests/golden/timesyncd_four_servers.conf"),
        );
    }

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n\
        frr:x:105:110::/nonexistent:/usr/sbin/nologin\n\
        admin:x:1000:1000::/home/admin:/usr/bin/hemlockctl\n\
        noc:x:1001:1001::/home/noc:/usr/bin/hemlockctl\n\
        nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n";

    /// Accounts outside the regular-user uid range are never Hemlock's,
    /// whatever a config says — the backstop behind the reserved-name
    /// list in the intent extractor.
    #[test]
    fn only_regular_user_accounts_are_managed() {
        assert!(manageable_account_in(PASSWD, "admin"));
        assert!(manageable_account_in(PASSWD, "noc"));
        // Never: root, a service account, the nss placeholder.
        assert!(!manageable_account_in(PASSWD, "root"));
        assert!(!manageable_account_in(PASSWD, "frr"));
        assert!(!manageable_account_in(PASSWD, "nobody"));
        // An account that does not exist yet is created in range.
        assert!(manageable_account_in(PASSWD, "cody"));
    }

    /// Without config-managed users the role comes from the OS: `sudo`
    /// membership, as a supplementary or primary group.
    #[test]
    fn os_roles_follow_sudo_membership() {
        const GROUP: &str = "sudo:x:27:admin\nhemlock:x:990:admin,noc\n";
        assert_eq!(os_role_in(GROUP, PASSWD, "admin"), Role::Admin);
        assert_eq!(os_role_in(GROUP, PASSWD, "noc"), Role::Operator);
        assert_eq!(os_role_in(GROUP, PASSWD, "nobody-here"), Role::Operator);
        // Primary-group membership counts too.
        const PRIMARY: &str = "sudo:x:1000:\n";
        assert_eq!(os_role_in(PRIMARY, PASSWD, "admin"), Role::Admin);
        assert_eq!(os_role_in(PRIMARY, PASSWD, "noc"), Role::Operator);
        // No sudo group at all: everyone is an operator.
        assert_eq!(
            os_role_in("hemlock:x:990:admin\n", PASSWD, "admin"),
            Role::Operator
        );
    }

    /// `authorized_keys` is rendered whole, in config order, with the
    /// managed-by header — so a removed key really disappears.
    #[test]
    fn renders_authorized_keys() {
        let user = UserIntent {
            role: hemlock_common::role::Role::Admin,
            password_hash: Some("$6$a$b".into()),
            ssh_keys: vec![
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIN0ex4mpl3 cody@mars".into(),
                "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQ cody@phobos".into(),
            ],
        };
        assert_eq!(
            render_authorized_keys(&user),
            "# Managed by hemlock-mgmtd; edit via the Hemlock config, not here.\n\
             ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIN0ex4mpl3 cody@mars\n\
             ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQ cody@phobos\n"
        );
        let keyless = UserIntent {
            ssh_keys: vec![],
            ..user
        };
        assert_eq!(
            render_authorized_keys(&keyless),
            "# Managed by hemlock-mgmtd; edit via the Hemlock config, not here.\n"
        );
    }

    /// No servers = no drop-in: the client is off and the unit stops.
    #[test]
    fn no_servers_renders_nothing() {
        assert!(render_timesyncd(&NtpIntent::default()).is_none());
        assert!(render_timesyncd(&ntp_of("services { ntp { } }")).is_none());
        assert!(render_timesyncd(&ntp_of("")).is_none());
    }
}
