//! The RIB manager: kernel netlink -> orch -> syncd.
//!
//! The kernel is the single upstream truth — statics (mgmtd's OS
//! applier), connected routes (address config), and FRR (zebra) all
//! install there, and this engine mirrors the result into the ASIC via
//! syncd's FIB RPCs. Kernel state arrives as *full snapshots*: rather
//! than a raw rtnetlink socket (a new unsafe-adjacent dependency), the
//! Linux feed runs `ip monitor route neigh` and re-dumps `ip -j route
//! show` / `ip -j -s neigh show` whenever the monitor reports a change
//! (iproute2 is netlink under the hood and already the workspace's
//! established kernel access path). Full-dump + reconcile is also
//! exactly the resync protocol a syncd or orch restart needs, so there
//! is one convergence path, not two.
//!
//! Translation rules:
//! - only routes out interfaces that map to ASIC L3 interfaces
//!   (front-panel hostifs, SVI bridges) are programmed; Management-only
//!   routes stay kernel-only;
//! - connected/local routes (no `via`) are already handled by syncd's
//!   RIF path — the RIB skips them;
//! - ECMP kernel routes translate to next-hop sets (syncd builds the
//!   groups);
//! - **resolve-via-punt**: a route whose next hops have no resolved
//!   (REACHABLE/PERMANENT) neighbor is programmed to punt to the CPU,
//!   so the kernel resolves ARP/ND; the neighbor event then reprograms
//!   it onto the resolved hops.
//!
//! The engine itself is pure state: snapshots in, a wanted `Program`
//! out, so tests drive it with synthetic kernel state and assert the
//! expected syncd programming without a kernel or a socket.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::watch;

// ---------------------------------------------------------------- kernel

/// One kernel route, as parsed from `ip -j route show`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelRoute {
    /// Canonical CIDR prefix ("default" already expanded).
    pub prefix: String,
    /// iproute2 protocol tag: "kernel", "boot", "static", "ospf", ...
    pub protocol: String,
    pub metric: u32,
    pub blackhole: bool,
    pub hops: Vec<KernelHop>,
}

/// One kernel next hop; `via: None` = directly connected (device route).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelHop {
    pub via: Option<String>,
    pub dev: Option<String>,
}

/// One kernel neighbor entry, as parsed from `ip -j -s neigh show`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelNeighbor {
    pub ip: String,
    pub dev: String,
    /// Colon-separated lowercase; None while unresolved.
    pub mac: Option<String>,
    /// NUD PERMANENT (a configured static) or NOARP.
    pub permanent: bool,
    /// NUD REACHABLE.
    pub reachable: bool,
    /// STALE/DELAY/PROBE: usable by the kernel but not programmed.
    pub stale: bool,
    /// Seconds since last confirmation (`-s` output), when known.
    pub age_secs: Option<u64>,
}

/// Parse `ip -j [-6] route show` output. `v6` picks the default-route
/// expansion and host prefix length.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_routes(json: &str, v6: bool) -> Vec<KernelRoute> {
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    let mut routes = Vec::new();
    for entry in entries {
        let text = |key: &str| entry.get(key).and_then(|v| v.as_str()).map(str::to_string);
        let Some(dst) = text("dst") else { continue };
        let prefix = canonical_dst(&dst, v6);
        let blackhole = text("type").as_deref() == Some("blackhole");
        let mut hops = Vec::new();
        if let Some(serde_json::Value::Array(nexthops)) = entry.get("nexthops") {
            for hop in nexthops {
                hops.push(KernelHop {
                    via: hop
                        .get("gateway")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    dev: hop.get("dev").and_then(|v| v.as_str()).map(str::to_string),
                });
            }
        } else if !blackhole {
            hops.push(KernelHop {
                via: text("gateway"),
                dev: text("dev"),
            });
        }
        routes.push(KernelRoute {
            prefix,
            protocol: text("protocol").unwrap_or_default(),
            metric: entry.get("metric").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            blackhole,
            hops,
        });
    }
    routes
}

/// Parse `ip -j -s [-6] neigh show` output.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_neighbors(json: &str) -> Vec<KernelNeighbor> {
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    let mut neighbors = Vec::new();
    for entry in entries {
        let text = |key: &str| entry.get(key).and_then(|v| v.as_str()).map(str::to_string);
        let (Some(ip), Some(dev)) = (text("dst"), text("dev")) else {
            continue;
        };
        let states: Vec<String> = match entry.get("state") {
            Some(serde_json::Value::Array(states)) => states
                .iter()
                .filter_map(|s| s.as_str())
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        let has = |s: &str| states.iter().any(|state| state == s);
        neighbors.push(KernelNeighbor {
            ip,
            dev,
            mac: text("lladdr"),
            permanent: has("PERMANENT") || has("NOARP"),
            reachable: has("REACHABLE"),
            stale: has("STALE") || has("DELAY") || has("PROBE"),
            age_secs: entry.get("confirmed").and_then(|v| v.as_u64()),
        });
    }
    neighbors
}

/// iproute2 prints "default" and drops full-length suffixes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn canonical_dst(dst: &str, v6: bool) -> String {
    if dst == "default" {
        return if v6 {
            "::/0".into()
        } else {
            "0.0.0.0/0".into()
        };
    }
    if dst.contains('/') {
        return dst.to_string();
    }
    if dst.contains(':') {
        format!("{dst}/128")
    } else {
        format!("{dst}/32")
    }
}

// --------------------------------------------------------------- program

/// What syncd's FIB should hold, derived from the kernel snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Program {
    /// Canonical prefix -> wanted program.
    pub routes: BTreeMap<String, WantedRoute>,
    /// (interface, ip) -> mac.
    pub neighbors: BTreeMap<(String, String), String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WantedRoute {
    /// Punt while no next hop has a resolved neighbor.
    pub cpu: bool,
    /// Null route.
    pub drop: bool,
    /// Resolved (interface, ip) next hops.
    pub hops: Vec<(String, String)>,
}

/// One syncd push, in apply order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FibOp {
    EnsureNeighbor {
        interface: String,
        ip: String,
        mac: String,
    },
    EnsureRoute {
        prefix: String,
        route: WantedRoute,
    },
    RemoveRoute {
        prefix: String,
    },
    RemoveNeighbor {
        interface: String,
        ip: String,
    },
}

/// The pushes that converge `current` (syncd's mirror) onto `wanted`.
/// Neighbors land before the routes that resolve through them; stale
/// routes go before the neighbors they referenced.
pub fn diff_program(current: &Program, wanted: &Program) -> Vec<FibOp> {
    let mut ops = Vec::new();
    for ((interface, ip), mac) in &wanted.neighbors {
        if current.neighbors.get(&(interface.clone(), ip.clone())) != Some(mac) {
            ops.push(FibOp::EnsureNeighbor {
                interface: interface.clone(),
                ip: ip.clone(),
                mac: mac.clone(),
            });
        }
    }
    for (prefix, route) in &wanted.routes {
        if current.routes.get(prefix) != Some(route) {
            ops.push(FibOp::EnsureRoute {
                prefix: prefix.clone(),
                route: route.clone(),
            });
        }
    }
    for prefix in current.routes.keys() {
        if !wanted.routes.contains_key(prefix) {
            ops.push(FibOp::RemoveRoute {
                prefix: prefix.clone(),
            });
        }
    }
    for (interface, ip) in current.neighbors.keys() {
        if !wanted
            .neighbors
            .contains_key(&(interface.clone(), ip.clone()))
        {
            ops.push(FibOp::RemoveNeighbor {
                interface: interface.clone(),
                ip: ip.clone(),
            });
        }
    }
    ops
}

// --------------------------------------------------------------- engine

struct Inner {
    routes: Vec<KernelRoute>,
    neighbors: Vec<KernelNeighbor>,
    /// ASIC L3 interface names (ports/SVIs with addresses), from syncd.
    l3_interfaces: BTreeSet<String>,
    /// First-seen instants for route uptimes, keyed by prefix.
    first_seen: HashMap<String, Instant>,
    /// A kernel snapshot has arrived (false on non-Linux dev hosts —
    /// consumers can fall back to config-derived views).
    have_kernel: bool,
}

/// The RIB engine handle (cloneable, like the other orch engines).
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
    generation: watch::Sender<u64>,
}

impl Engine {
    /// Create the engine plus the change signal its pusher watches.
    pub fn spawn() -> (Self, watch::Receiver<u64>) {
        let (generation, rx) = watch::channel(0);
        (
            Self {
                inner: Arc::new(Mutex::new(Inner {
                    routes: Vec::new(),
                    neighbors: Vec::new(),
                    l3_interfaces: BTreeSet::new(),
                    first_seen: HashMap::new(),
                    have_kernel: false,
                })),
                generation,
            },
            rx,
        )
    }

    fn bump(&self) {
        self.generation.send_modify(|g| *g += 1);
    }

    /// Replace the kernel snapshot (both families together).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn ingest_kernel(&self, routes: Vec<KernelRoute>, neighbors: Vec<KernelNeighbor>) {
        if let Ok(mut inner) = self.inner.lock() {
            let now = Instant::now();
            let live: BTreeSet<&String> = routes.iter().map(|r| &r.prefix).collect();
            inner.first_seen.retain(|prefix, _| live.contains(prefix));
            for route in &routes {
                inner.first_seen.entry(route.prefix.clone()).or_insert(now);
            }
            inner.routes = routes;
            inner.neighbors = neighbors;
            inner.have_kernel = true;
        }
        self.bump();
    }

    /// Which interfaces map to ASIC L3 interfaces (from syncd's
    /// interface table).
    pub fn set_l3_interfaces(&self, interfaces: BTreeSet<String>) {
        if let Ok(mut inner) = self.inner.lock() {
            if inner.l3_interfaces == interfaces {
                return;
            }
            inner.l3_interfaces = interfaces;
        }
        self.bump();
    }

    /// A kernel snapshot has arrived (dev hosts without the Linux feed
    /// never set this; `GetRib` then reports nothing).
    pub fn have_kernel(&self) -> bool {
        self.inner.lock().map(|i| i.have_kernel).unwrap_or(false)
    }

    /// The FIB program the kernel snapshot wants.
    pub fn wanted(&self) -> Program {
        let Ok(inner) = self.inner.lock() else {
            return Program::default();
        };
        derive_program(&inner.routes, &inner.neighbors, &inner.l3_interfaces)
    }

    /// The RIB view for `show ip route` / the web console, one family,
    /// sorted by prefix.
    pub fn snapshot(&self, v6: bool) -> Vec<RouteView> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        let wanted = derive_program(&inner.routes, &inner.neighbors, &inner.l3_interfaces);
        let mut views: Vec<RouteView> = inner
            .routes
            .iter()
            .filter(|r| r.prefix.contains(':') == v6)
            .filter_map(|r| route_view(r, &wanted, &inner.l3_interfaces, &inner.first_seen))
            .collect();
        views.sort_by_key(|view| prefix_key(&view.prefix));
        views
    }

    /// The neighbor view for `show arp` / `show ipv6 neighbors`, one
    /// family, sorted by address.
    pub fn neighbors(&self, v6: bool) -> Vec<NeighborView> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        let mut views: Vec<NeighborView> = inner
            .neighbors
            .iter()
            .filter(|n| n.ip.contains(':') == v6)
            .filter(|n| !n.ip.starts_with("fe80") && !n.ip.starts_with("169.254."))
            .map(|n| NeighborView {
                ip: n.ip.clone(),
                mac: n.mac.clone().unwrap_or_default(),
                interface: n.dev.clone(),
                permanent: n.permanent,
                age_secs: n.age_secs,
            })
            .collect();
        views.sort_by_key(|view| view.ip.parse::<std::net::IpAddr>().ok());
        views
    }
}

/// One `show ip route` row.
#[derive(Debug, Clone)]
pub struct RouteView {
    pub prefix: String,
    /// "connected" | "static" | "kernel" | "ospf" | "bgp".
    pub protocol: String,
    pub distance: u32,
    pub metric: u32,
    pub uptime_secs: u64,
    pub hops: Vec<HopView>,
    /// "programmed" | "punt" | "drop" | "connected" | "kernel".
    pub fib: &'static str,
    /// The egress interface of a connected route.
    pub interface: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HopView {
    pub via: String,
    pub interface: String,
    pub resolved: bool,
}

/// One `show arp` row.
#[derive(Debug, Clone)]
pub struct NeighborView {
    pub ip: String,
    pub mac: String,
    pub interface: String,
    pub permanent: bool,
    pub age_secs: Option<u64>,
}

/// The pure translation: kernel snapshot -> wanted FIB program.
fn derive_program(
    routes: &[KernelRoute],
    neighbors: &[KernelNeighbor],
    l3: &BTreeSet<String>,
) -> Program {
    let mut program = Program::default();
    for neighbor in neighbors {
        let Some(mac) = &neighbor.mac else { continue };
        if !l3.contains(&neighbor.dev) {
            continue;
        }
        if !(neighbor.reachable || neighbor.permanent) {
            continue;
        }
        program
            .neighbors
            .insert((neighbor.dev.clone(), neighbor.ip.clone()), mac.clone());
    }
    for route in routes {
        if skip_prefix(&route.prefix) {
            continue;
        }
        if route.blackhole {
            program.routes.insert(
                route.prefix.clone(),
                WantedRoute {
                    cpu: false,
                    drop: true,
                    hops: Vec::new(),
                },
            );
            continue;
        }
        // A hop without `via` is a connected/device route: the RIF path
        // owns those.
        let gateway_hops: Vec<(&String, &String)> = route
            .hops
            .iter()
            .filter_map(|hop| Some((hop.via.as_ref()?, hop.dev.as_ref()?)))
            .collect();
        if gateway_hops.is_empty() {
            continue;
        }
        // Only hops out ASIC L3 interfaces program; a route with none
        // (Management-only) stays kernel-only.
        let asic_hops: Vec<(&String, &String)> = gateway_hops
            .iter()
            .filter(|(_, dev)| l3.contains(*dev))
            .copied()
            .collect();
        if asic_hops.is_empty() {
            continue;
        }
        let resolved: Vec<(String, String)> = asic_hops
            .iter()
            .filter(|(via, dev)| {
                program
                    .neighbors
                    .contains_key(&((*dev).clone(), (*via).clone()))
            })
            .map(|(via, dev)| ((*dev).clone(), (*via).clone()))
            .collect();
        program.routes.insert(
            route.prefix.clone(),
            WantedRoute {
                cpu: resolved.is_empty(),
                drop: false,
                hops: resolved,
            },
        );
    }
    program
}

/// Link-local and multicast destinations never reach the ASIC FIB.
fn skip_prefix(prefix: &str) -> bool {
    let Some((addr, _)) = prefix.split_once('/') else {
        return true;
    };
    match addr.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_multicast() || v4.is_link_local(),
        Ok(std::net::IpAddr::V6(v6)) => v6.is_multicast() || (v6.segments()[0] & 0xffc0) == 0xfe80,
        Err(_) => true,
    }
}

fn route_view(
    route: &KernelRoute,
    wanted: &Program,
    l3: &BTreeSet<String>,
    first_seen: &HashMap<String, Instant>,
) -> Option<RouteView> {
    if skip_prefix(&route.prefix) {
        return None;
    }
    let uptime_secs = first_seen
        .get(&route.prefix)
        .map(|t| t.elapsed().as_secs())
        .unwrap_or(0);
    let connected = !route.blackhole && route.hops.iter().all(|hop| hop.via.is_none());
    let v6 = route.prefix.contains(':');
    let (protocol, distance, metric) = if connected {
        ("connected".to_string(), 0, 0)
    } else {
        match route.protocol.as_str() {
            // The configured distance rides the kernel metric (the OS
            // applier omits it at the default distance of 1; the kernel
            // then reports 0 for v4 and 1024 for v6).
            "static" | "boot" => (
                "static".to_string(),
                match route.metric {
                    0 => 1,
                    1024 if v6 => 1,
                    metric => metric,
                },
                0,
            ),
            "ospf" => ("ospf".to_string(), 110, route.metric),
            "bgp" => ("bgp".to_string(), 200, route.metric),
            _ => ("kernel".to_string(), 0, route.metric),
        }
    };
    let program = wanted.routes.get(&route.prefix);
    let fib = if route.blackhole {
        "drop"
    } else if connected {
        "connected"
    } else {
        match program {
            Some(w) if w.cpu => "punt",
            Some(_) => "programmed",
            None => "kernel",
        }
    };
    let hops = route
        .hops
        .iter()
        .filter_map(|hop| {
            let via = hop.via.clone()?;
            let dev = hop.dev.clone().unwrap_or_default();
            let resolved = wanted.neighbors.contains_key(&(dev.clone(), via.clone()));
            Some(HopView {
                via,
                interface: dev,
                resolved,
            })
        })
        .collect();
    let interface = connected
        .then(|| route.hops.first().and_then(|hop| hop.dev.clone()))
        .flatten();
    // Connected routes on non-ASIC devices (the management netdev) stay
    // in the view — the kernel owns them — but tagged as kernel routes
    // when the device is unknown to syncd.
    let fib = if connected && interface.as_ref().is_some_and(|i| !l3.contains(i)) {
        "kernel"
    } else {
        fib
    };
    Some(RouteView {
        prefix: route.prefix.clone(),
        protocol,
        distance,
        metric,
        uptime_secs,
        hops,
        fib,
        interface,
    })
}

/// Numeric sort key for a canonical prefix.
fn prefix_key(prefix: &str) -> (Option<std::net::IpAddr>, u8) {
    match prefix.split_once('/') {
        Some((addr, len)) => (addr.parse().ok(), len.parse().unwrap_or(u8::MAX)),
        None => (None, u8::MAX),
    }
}

// ------------------------------------------------------- the Linux feed

/// Kernel feed: initial dump, then re-dump on `ip monitor` output
/// (debounced). Restarts the monitor with backoff if it dies.
#[cfg(target_os = "linux")]
pub async fn run_feed(engine: Engine) {
    use tokio::io::AsyncBufReadExt;
    loop {
        dump_into(&engine).await;
        let child = tokio::process::Command::new("ip")
            .args(["-o", "monitor", "route", "neigh"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(err) => {
                tracing::warn!(%err, "cannot spawn ip monitor; RIB feed idle");
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                continue;
            }
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill().await;
            continue;
        };
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        while let Ok(Some(_)) = lines.next_line().await {
            // Debounce: kernel churn (an FRR convergence) arrives in
            // bursts; one re-dump covers them all.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            while let Ok(Ok(Some(_))) =
                tokio::time::timeout(std::time::Duration::from_millis(1), lines.next_line()).await
            {
            }
            dump_into(&engine).await;
        }
        tracing::warn!("ip monitor exited; restarting the RIB feed");
        let _ = child.kill().await;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

#[cfg(target_os = "linux")]
async fn dump_into(engine: &Engine) {
    async fn ip_json(args: &[&str]) -> String {
        match tokio::process::Command::new("ip").args(args).output().await {
            Ok(output) if output.status.success() => {
                String::from_utf8_lossy(&output.stdout).into_owned()
            }
            Ok(output) => {
                tracing::warn!(status = %output.status, args = ?args, "ip dump failed");
                String::new()
            }
            Err(err) => {
                tracing::warn!(%err, args = ?args, "cannot run ip");
                String::new()
            }
        }
    }
    let mut routes = parse_routes(&ip_json(&["-j", "route", "show"]).await, false);
    routes.extend(parse_routes(
        &ip_json(&["-j", "-6", "route", "show"]).await,
        true,
    ));
    let mut neighbors = parse_neighbors(&ip_json(&["-j", "-s", "neigh", "show"]).await);
    neighbors.extend(parse_neighbors(
        &ip_json(&["-j", "-s", "-6", "neigh", "show"]).await,
    ));
    engine.ingest_kernel(routes, neighbors);
}

/// `clear arp [<ip>]`: flush dynamic kernel neighbors (statics are NUD
/// permanent and survive). The change flows back through the monitor.
#[cfg(target_os = "linux")]
pub async fn flush_neighbors(ip: Option<&str>) -> bool {
    let mut args = vec!["neigh", "flush"];
    match ip {
        Some(ip) => args.extend(["to", ip]),
        None => args.push("all"),
    }
    matches!(
        tokio::process::Command::new("ip").args(&args).status().await,
        Ok(status) if status.success()
    )
}

#[cfg(not(target_os = "linux"))]
pub async fn flush_neighbors(_ip: Option<&str>) -> bool {
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn l3(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn hop(via: &str, dev: &str) -> KernelHop {
        KernelHop {
            via: Some(via.into()),
            dev: Some(dev.into()),
        }
    }

    fn route(prefix: &str, protocol: &str, hops: Vec<KernelHop>) -> KernelRoute {
        KernelRoute {
            prefix: prefix.into(),
            protocol: protocol.into(),
            metric: 0,
            blackhole: false,
            hops,
        }
    }

    fn neighbor(ip: &str, dev: &str, mac: &str, reachable: bool) -> KernelNeighbor {
        KernelNeighbor {
            ip: ip.into(),
            dev: dev.into(),
            mac: Some(mac.into()),
            permanent: false,
            reachable,
            stale: !reachable,
            age_secs: Some(12),
        }
    }

    #[test]
    fn parses_iproute2_json() {
        let json = r#"[
            {"dst":"default","gateway":"10.42.10.1","dev":"Vlan99","protocol":"static","flags":[]},
            {"dst":"10.42.10.0/24","dev":"Vlan99","protocol":"kernel","scope":"link","prefsrc":"10.42.10.9","flags":[]},
            {"dst":"10.99.0.0/16","protocol":"static","metric":250,"flags":[],
             "nexthops":[{"gateway":"10.9.9.0","dev":"Ethernet48","weight":1,"flags":[]},
                         {"gateway":"10.42.10.7","dev":"Vlan99","weight":1,"flags":[]}]},
            {"type":"blackhole","dst":"192.0.2.0/24","protocol":"static","flags":[]},
            {"dst":"10.50.99.1","gateway":"10.42.10.7","dev":"Vlan99","protocol":"ospf","metric":20,"flags":[]}
        ]"#;
        let routes = parse_routes(json, false);
        assert_eq!(routes.len(), 5);
        assert_eq!(routes[0].prefix, "0.0.0.0/0");
        assert_eq!(routes[0].hops, vec![hop("10.42.10.1", "Vlan99")]);
        assert_eq!(routes[1].prefix, "10.42.10.0/24");
        assert_eq!(routes[1].hops[0].via, None);
        assert_eq!(routes[2].metric, 250);
        assert_eq!(routes[2].hops.len(), 2);
        assert!(routes[3].blackhole);
        assert_eq!(routes[4].prefix, "10.50.99.1/32");

        let json = r#"[{"dst":"2001:db8:99::/48","gateway":"2001:db8:9::1","dev":"Ethernet48","protocol":"static","metric":1024,"pref":"medium"}]"#;
        let routes = parse_routes(json, true);
        assert_eq!(routes[0].prefix, "2001:db8:99::/48");

        let json = r#"[
            {"dst":"10.42.10.1","dev":"Vlan99","lladdr":"d4:af:f7:12:9c:00","state":["REACHABLE"],"confirmed":142},
            {"dst":"10.42.10.200","dev":"Vlan99","lladdr":"00:50:56:be:ef:99","state":["PERMANENT"]},
            {"dst":"10.42.10.9","dev":"Vlan99","state":["FAILED"]}
        ]"#;
        let neighbors = parse_neighbors(json);
        assert_eq!(neighbors.len(), 3);
        assert!(neighbors[0].reachable);
        assert_eq!(neighbors[0].age_secs, Some(142));
        assert!(neighbors[1].permanent);
        assert_eq!(neighbors[2].mac, None);
    }

    #[test]
    fn derives_programming_with_resolve_via_punt() {
        let l3 = l3(&["Vlan99", "Ethernet48"]);
        let routes = vec![
            // Connected: the RIF path owns it.
            route(
                "10.42.10.0/24",
                "kernel",
                vec![KernelHop {
                    via: None,
                    dev: Some("Vlan99".into()),
                }],
            ),
            // Unresolved next hop: punt.
            route("0.0.0.0/0", "static", vec![hop("10.42.10.1", "Vlan99")]),
            // ECMP, one hop resolved: program the resolved subset.
            route(
                "10.99.0.0/16",
                "static",
                vec![hop("10.9.9.0", "Ethernet48"), hop("10.42.10.7", "Vlan99")],
            ),
            // Management-only: stays kernel.
            route("172.31.0.0/16", "static", vec![hop("192.168.0.1", "eth0")]),
        ];
        let neighbors = vec![
            neighbor("10.9.9.0", "Ethernet48", "a0:36:9f:44:be:09", true),
            // Stale: not programmed.
            neighbor("10.42.10.7", "Vlan99", "00:1c:73:0c:aa:07", false),
            // Off-ASIC: ignored.
            neighbor("192.168.0.1", "eth0", "11:22:33:44:55:66", true),
        ];
        let program = derive_program(&routes, &neighbors, &l3);
        assert_eq!(program.neighbors.len(), 1);
        assert_eq!(
            program.routes["0.0.0.0/0"],
            WantedRoute {
                cpu: true,
                drop: false,
                hops: vec![]
            }
        );
        assert_eq!(
            program.routes["10.99.0.0/16"],
            WantedRoute {
                cpu: false,
                drop: false,
                hops: vec![("Ethernet48".into(), "10.9.9.0".into())]
            }
        );
        assert!(!program.routes.contains_key("10.42.10.0/24"));
        assert!(!program.routes.contains_key("172.31.0.0/16"));

        // The neighbor resolving flips the punted default route.
        let neighbors = vec![
            neighbor("10.9.9.0", "Ethernet48", "a0:36:9f:44:be:09", true),
            neighbor("10.42.10.7", "Vlan99", "00:1c:73:0c:aa:07", true),
            neighbor("10.42.10.1", "Vlan99", "d4:af:f7:12:9c:00", true),
        ];
        let flipped = derive_program(&routes, &neighbors, &l3);
        assert_eq!(
            flipped.routes["0.0.0.0/0"],
            WantedRoute {
                cpu: false,
                drop: false,
                hops: vec![("Vlan99".into(), "10.42.10.1".into())]
            }
        );
        assert_eq!(flipped.routes["10.99.0.0/16"].hops.len(), 2);

        // The reconcile diff: the flip re-ensures both routes and adds
        // the new neighbors; nothing is removed.
        let ops = diff_program(&program, &flipped);
        assert!(ops.iter().any(|op| matches!(op,
            FibOp::EnsureRoute { prefix, route } if prefix == "0.0.0.0/0" && !route.cpu)));
        assert!(!ops.iter().any(|op| matches!(op, FibOp::RemoveRoute { .. })));

        // A route leaving the kernel is removed.
        let empty = derive_program(&[], &[], &BTreeSet::new());
        let ops = diff_program(&flipped, &empty);
        assert!(ops.iter().any(|op| matches!(op,
            FibOp::RemoveRoute { prefix } if prefix == "10.99.0.0/16")));
        assert!(ops
            .iter()
            .any(|op| matches!(op, FibOp::RemoveNeighbor { .. })));
    }

    #[test]
    fn blackhole_and_v6_program_and_link_local_skipped() {
        let l3 = l3(&["Ethernet48"]);
        let mut blackhole = route("192.0.2.0/24", "static", vec![]);
        blackhole.blackhole = true;
        let routes = vec![
            blackhole,
            route(
                "2001:db8:99::/48",
                "static",
                vec![hop("2001:db8:9::1", "Ethernet48")],
            ),
            route("fe80::/64", "kernel", vec![hop("fe80::1", "Ethernet48")]),
        ];
        let neighbors = vec![neighbor(
            "2001:db8:9::1",
            "Ethernet48",
            "a0:36:9f:44:be:09",
            true,
        )];
        let program = derive_program(&routes, &neighbors, &l3);
        assert_eq!(
            program.routes["192.0.2.0/24"],
            WantedRoute {
                cpu: false,
                drop: true,
                hops: vec![]
            }
        );
        assert_eq!(
            program.routes["2001:db8:99::/48"].hops,
            vec![("Ethernet48".to_string(), "2001:db8:9::1".to_string())]
        );
        assert!(!program.routes.contains_key("fe80::/64"));
    }

    #[test]
    fn neighbor_views_sort_numerically_for_arbitrary_input() {
        // xorshift over random addresses: the view must sort by parsed
        // address (not lexically), both families.
        let mut x: u64 = 0x736f_7274_6172_7021;
        let mut next = move || {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let (engine, _rx) = Engine::spawn();
        for _ in 0..50 {
            let neighbors: Vec<KernelNeighbor> = (0..(next() % 8))
                .map(|_| {
                    let ip = if next() % 2 == 0 {
                        std::net::IpAddr::V4(std::net::Ipv4Addr::from(next() as u32)).to_string()
                    } else {
                        std::net::IpAddr::V6(std::net::Ipv6Addr::from(
                            ((next() as u128) << 64) | next() as u128,
                        ))
                        .to_string()
                    };
                    neighbor(&ip, "Vlan99", "00:1c:73:0c:aa:07", true)
                })
                .collect();
            engine.ingest_kernel(Vec::new(), neighbors);
            for v6 in [false, true] {
                let views = engine.neighbors(v6);
                let parsed: Vec<std::net::IpAddr> =
                    views.iter().map(|v| v.ip.parse().unwrap()).collect();
                let mut sorted = parsed.clone();
                sorted.sort();
                assert_eq!(parsed, sorted);
                assert!(views.iter().all(|v| v.ip.contains(':') == v6));
            }
        }
    }

    #[test]
    fn snapshot_views_carry_protocol_distance_and_fib_state() {
        let (engine, _rx) = Engine::spawn();
        engine.set_l3_interfaces(l3(&["Vlan99", "Ethernet48"]));
        let mut ecmp = route(
            "10.99.0.0/16",
            "static",
            vec![hop("10.9.9.0", "Ethernet48"), hop("10.42.10.7", "Vlan99")],
        );
        ecmp.metric = 250;
        engine.ingest_kernel(
            vec![
                route(
                    "10.42.10.0/24",
                    "kernel",
                    vec![KernelHop {
                        via: None,
                        dev: Some("Vlan99".into()),
                    }],
                ),
                ecmp,
                {
                    let mut ospf = route("10.50.0.0/24", "ospf", vec![hop("10.42.10.7", "Vlan99")]);
                    ospf.metric = 20;
                    ospf
                },
            ],
            vec![neighbor(
                "10.9.9.0",
                "Ethernet48",
                "a0:36:9f:44:be:09",
                true,
            )],
        );
        let views = engine.snapshot(false);
        assert_eq!(views.len(), 3);
        assert_eq!(views[0].prefix, "10.42.10.0/24");
        assert_eq!(views[0].protocol, "connected");
        assert_eq!(views[0].fib, "connected");
        assert_eq!(views[0].interface.as_deref(), Some("Vlan99"));
        assert_eq!(views[1].prefix, "10.50.0.0/24");
        assert_eq!((views[1].distance, views[1].metric), (110, 20));
        assert_eq!(views[1].fib, "punt", "no neighbor for 10.42.10.7 yet");
        assert_eq!(views[2].prefix, "10.99.0.0/16");
        assert_eq!(views[2].distance, 250);
        assert_eq!(views[2].fib, "programmed");
        assert!(views[2].hops[0].resolved);
        assert!(!views[2].hops[1].resolved);

        let neighbors = engine.neighbors(false);
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].interface, "Ethernet48");
        assert!(engine.have_kernel());
    }
}
