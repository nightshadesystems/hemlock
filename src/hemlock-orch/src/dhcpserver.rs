//! The DHCP server's live state: dnsmasq's lease file, merged with the
//! pools mgmtd rendered it from.
//!
//! Same split as the other rendered services. mgmtd renders
//! `/etc/dnsmasq.d/hemlock.conf` and owns the unit; orch reads the
//! lease file and answers `show dhcp server` — one place that knows how
//! dnsmasq records a lease.
//!
//! The pools arrive over `SetDhcpServerConfig` rather than being
//! re-parsed out of the rendered config: a reservation and a dynamic
//! lease look nothing alike in the lease file (a reservation only
//! appears once the client has actually asked), and the pool's range is
//! what turns a lease count into a utilisation figure.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};

use tracing::warn;

/// dnsmasq's lease file, as mgmtd's render points it at.
pub const LEASE_FILE: &str = "/var/lib/misc/dnsmasq.leases";

/// One configured pool, as mgmtd pushed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    pub name: String,
    /// Canonical CIDR.
    pub network: String,
    pub range_start: Ipv4Addr,
    pub range_end: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub dns_servers: Vec<Ipv4Addr>,
    pub lease_time: u32,
    pub domain_name: String,
    /// Fixed addresses keyed by canonical MAC.
    pub reservations: BTreeMap<String, Ipv4Addr>,
}

impl Default for Pool {
    fn default() -> Self {
        Self {
            name: String::new(),
            network: String::new(),
            range_start: Ipv4Addr::UNSPECIFIED,
            range_end: Ipv4Addr::UNSPECIFIED,
            gateway: Ipv4Addr::UNSPECIFIED,
            dns_servers: Vec::new(),
            lease_time: 0,
            domain_name: String::new(),
            reservations: BTreeMap::new(),
        }
    }
}

impl Pool {
    /// How many addresses the dynamic range holds.
    pub fn capacity(&self) -> u32 {
        u32::from(self.range_end)
            .checked_sub(u32::from(self.range_start))
            .map(|span| span + 1)
            .unwrap_or(0)
    }

    /// Is `address` inside the dynamic range?
    pub fn in_range(&self, address: Ipv4Addr) -> bool {
        let value = u32::from(address);
        value >= u32::from(self.range_start) && value <= u32::from(self.range_end)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// Pools in name order, as the config lists them.
    pub pools: Vec<Pool>,
    /// The lease file to read; overridable for tests.
    pub lease_file: String,
}

/// One lease or reservation, as `show dhcp server leases` prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub address: Ipv4Addr,
    pub mac: String,
    /// Empty when the client sent no hostname.
    pub hostname: String,
    /// Unix seconds; None for a reservation with no active lease.
    pub expires_at: Option<u64>,
    /// True when the address comes from a `reservation` leaf.
    pub reservation: bool,
    /// The pool that covers this address; empty when none does.
    pub pool: String,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub pools: Vec<Pool>,
    /// In-use dynamic leases per pool name.
    pub in_use: BTreeMap<String, u32>,
    pub leases: Vec<Lease>,
}

struct Inner {
    config: Config,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Engine {
        Engine {
            inner: Arc::new(Mutex::new(Inner {
                config: Config {
                    lease_file: LEASE_FILE.to_string(),
                    ..Config::default()
                },
            })),
        }
    }

    /// Replace the configuration (declarative).
    pub fn set_config(&self, mut config: Config) {
        if config.lease_file.is_empty() {
            config.lease_file = LEASE_FILE.to_string();
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.config = config;
        }
    }

    pub fn config(&self) -> Config {
        self.inner
            .lock()
            .map(|inner| inner.config.clone())
            .unwrap_or_default()
    }

    /// The pools plus every lease and reservation, sorted for display.
    pub fn snapshot(&self) -> Snapshot {
        let config = self.config();
        let text = std::fs::read_to_string(&config.lease_file).unwrap_or_default();
        snapshot_from(&config, &text)
    }

    /// `clear dhcp server lease <ip>`: drop one lease and make dnsmasq
    /// forget it.
    ///
    /// dnsmasq only reads its lease file at startup — a SIGHUP reloads
    /// host files, not leases — so the file is rewritten and the unit
    /// restarted. A restart costs nothing a client notices: leases live
    /// in the file, and the ones left there survive it.
    pub fn clear_lease(&self, address: Ipv4Addr) -> bool {
        let config = self.config();
        let Ok(text) = std::fs::read_to_string(&config.lease_file) else {
            return false;
        };
        let (kept, removed) = without_lease(&text, address);
        if !removed {
            return false;
        }
        if let Err(err) = std::fs::write(&config.lease_file, kept) {
            warn!(%err, path = %config.lease_file, "cannot rewrite the dhcp lease file");
            return false;
        }
        restart_dnsmasq();
        true
    }
}

/// Rewrite a lease file without one address; the flag says whether it
/// was there at all.
fn without_lease(text: &str, address: Ipv4Addr) -> (String, bool) {
    let mut kept = String::with_capacity(text.len());
    let mut removed = false;
    for line in text.lines() {
        if parse_lease_line(line).map(|lease| lease.address) == Some(address) {
            removed = true;
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    (kept, removed)
}

fn restart_dnsmasq() {
    match std::process::Command::new("systemctl")
        .args(["restart", "dnsmasq"])
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => warn!(
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "restarting dnsmasq after a lease clear failed"
        ),
        Err(err) => warn!(%err, "cannot restart dnsmasq after a lease clear"),
    }
}

/// One dnsmasq lease line: `<expiry> <mac> <ip> <hostname> <client-id>`.
/// A hostname of `*` means the client sent none.
fn parse_lease_line(line: &str) -> Option<Lease> {
    let mut fields = line.split_whitespace();
    let expires: u64 = fields.next()?.parse().ok()?;
    let mac = fields.next()?.to_ascii_lowercase();
    let address: Ipv4Addr = fields.next()?.parse().ok()?;
    let hostname = match fields.next() {
        Some("*") | None => String::new(),
        Some(name) => name.to_string(),
    };
    Some(Lease {
        address,
        mac,
        hostname,
        // dnsmasq writes 0 for an infinite lease.
        expires_at: (expires > 0).then_some(expires),
        reservation: false,
        pool: String::new(),
    })
}

/// Merge one lease file with the configured pools. Pure, so the
/// utilisation arithmetic and the reservation merge are testable
/// without touching a filesystem.
pub fn snapshot_from(config: &Config, lease_text: &str) -> Snapshot {
    let pool_of = |address: Ipv4Addr| -> String {
        config
            .pools
            .iter()
            .find(|pool| {
                pool.in_range(address) || pool.reservations.values().any(|held| *held == address)
            })
            .map(|pool| pool.name.clone())
            .unwrap_or_default()
    };

    let mut leases: Vec<Lease> = lease_text
        .lines()
        .filter_map(parse_lease_line)
        .map(|mut lease| {
            lease.pool = pool_of(lease.address);
            lease
        })
        .collect();

    // Reservations are configuration, so they show even before the
    // client has ever asked; one that *has* a lease keeps the lease's
    // hostname and expiry and is simply marked as reserved.
    for pool in &config.pools {
        for (mac, address) in &pool.reservations {
            match leases
                .iter_mut()
                .find(|lease| lease.address == *address || lease.mac == *mac)
            {
                Some(lease) => {
                    lease.reservation = true;
                    lease.pool = pool.name.clone();
                }
                None => leases.push(Lease {
                    address: *address,
                    mac: mac.clone(),
                    hostname: String::new(),
                    expires_at: None,
                    reservation: true,
                    pool: pool.name.clone(),
                }),
            }
        }
    }

    // Utilisation counts the dynamic range only: a reservation outside
    // it is not competing for a pool address.
    let mut in_use: BTreeMap<String, u32> = config
        .pools
        .iter()
        .map(|pool| (pool.name.clone(), 0))
        .collect();
    for lease in &leases {
        if lease.reservation {
            continue;
        }
        if let Some(pool) = config
            .pools
            .iter()
            .find(|pool| pool.in_range(lease.address))
        {
            *in_use.entry(pool.name.clone()).or_default() += 1;
        }
    }

    leases.sort_by_key(|lease| (u32::from(lease.address), lease.mac.clone()));
    Snapshot {
        pools: config.pools.clone(),
        in_use,
        leases,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn pool() -> Pool {
        Pool {
            name: "LAN-USERS".into(),
            network: "10.0.10.0/24".into(),
            range_start: Ipv4Addr::new(10, 0, 10, 100),
            range_end: Ipv4Addr::new(10, 0, 10, 200),
            gateway: Ipv4Addr::new(10, 0, 10, 1),
            dns_servers: vec![Ipv4Addr::new(10, 42, 0, 5)],
            lease_time: 86400,
            domain_name: String::new(),
            reservations: [(
                "00:1c:73:0c:aa:01".to_string(),
                Ipv4Addr::new(10, 0, 10, 50),
            )]
            .into(),
        }
    }

    fn config() -> Config {
        Config {
            pools: vec![pool()],
            lease_file: "/nonexistent".into(),
        }
    }

    const LEASES: &str = "\
1787000047 00:1c:73:0c:aa:02 10.0.10.101 laptop-jchen 01:00:1c:73:0c:aa:02
1786993364 a0:36:9f:44:be:02 10.0.10.102 voip-2214 *
1786993364 00:50:56:be:ef:09 10.0.10.103 * *
";

    #[test]
    fn lease_lines_parse() {
        let lease = parse_lease_line(LEASES.lines().next().unwrap()).unwrap();
        assert_eq!(lease.address, Ipv4Addr::new(10, 0, 10, 101));
        assert_eq!(lease.mac, "00:1c:73:0c:aa:02");
        assert_eq!(lease.hostname, "laptop-jchen");
        assert_eq!(lease.expires_at, Some(1_787_000_047));
        // A `*` hostname means the client sent none.
        let lease = parse_lease_line(LEASES.lines().nth(2).unwrap()).unwrap();
        assert!(lease.hostname.is_empty());
        // An infinite lease writes expiry 0.
        let lease = parse_lease_line("0 00:11:22:33:44:55 10.0.10.9 host *").unwrap();
        assert_eq!(lease.expires_at, None);
        // Junk is skipped rather than guessed at.
        assert!(parse_lease_line("").is_none());
        assert!(parse_lease_line("not a lease").is_none());
        assert!(parse_lease_line("1787000047 00:1c:73:0c:aa:02 notanip host *").is_none());
    }

    /// Reservations merge with the leases, utilisation counts only the
    /// dynamic range, and rows come out in address order.
    #[test]
    fn snapshot_merges_reservations_and_counts_utilisation() {
        let snapshot = snapshot_from(&config(), LEASES);
        assert_eq!(snapshot.in_use["LAN-USERS"], 3);
        let addresses: Vec<String> = snapshot
            .leases
            .iter()
            .map(|lease| lease.address.to_string())
            .collect();
        assert_eq!(
            addresses,
            vec!["10.0.10.50", "10.0.10.101", "10.0.10.102", "10.0.10.103"]
        );
        let reserved = snapshot
            .leases
            .iter()
            .find(|lease| lease.address == Ipv4Addr::new(10, 0, 10, 50))
            .unwrap();
        assert!(reserved.reservation);
        assert_eq!(reserved.expires_at, None);
        assert_eq!(reserved.pool, "LAN-USERS");
        // The reservation sits outside the range, so it is not counted
        // against the pool's capacity.
        assert_eq!(pool().capacity(), 101);
        assert!(!pool().in_range(Ipv4Addr::new(10, 0, 10, 50)));
    }

    /// A reservation the client has actually taken up shows as one row,
    /// with the lease's hostname and expiry.
    #[test]
    fn an_active_reservation_is_one_row() {
        let leases = "1787000047 00:1c:73:0c:aa:01 10.0.10.50 printer-3rdfloor *\n";
        let snapshot = snapshot_from(&config(), leases);
        assert_eq!(snapshot.leases.len(), 1);
        let lease = &snapshot.leases[0];
        assert!(lease.reservation);
        assert_eq!(lease.hostname, "printer-3rdfloor");
        assert_eq!(lease.expires_at, Some(1_787_000_047));
        // ...and it is not counted against the dynamic range.
        assert_eq!(snapshot.in_use["LAN-USERS"], 0);
    }

    /// A lease no pool covers still lists (it is real), but counts
    /// against nothing.
    #[test]
    fn leases_outside_every_pool_still_list() {
        let leases = "1787000047 00:11:22:33:44:55 192.0.2.7 stray *\n";
        let snapshot = snapshot_from(&config(), leases);
        assert_eq!(snapshot.leases.len(), 2, "the reservation plus the stray");
        let stray = snapshot
            .leases
            .iter()
            .find(|lease| lease.address == Ipv4Addr::new(192, 0, 2, 7))
            .unwrap();
        assert!(stray.pool.is_empty());
        assert_eq!(snapshot.in_use["LAN-USERS"], 0);
    }

    /// Clearing rewrites the file without one address and leaves every
    /// other line byte-identical.
    #[test]
    fn clearing_removes_exactly_one_lease() {
        let (kept, removed) = without_lease(LEASES, Ipv4Addr::new(10, 0, 10, 102));
        assert!(removed);
        assert!(!kept.contains("10.0.10.102"));
        assert!(kept.contains("10.0.10.101"));
        assert!(kept.contains("10.0.10.103"));
        assert_eq!(kept.lines().count(), 2);

        // An address with no lease is not an error, just nothing done.
        let (kept, removed) = without_lease(LEASES, Ipv4Addr::new(10, 0, 10, 55));
        assert!(!removed);
        assert_eq!(kept.lines().count(), 3);
    }
}
