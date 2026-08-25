//! DHCP snooping + dynamic ARP inspection.
//!
//! DHCP on snooped VLANs traps to the CPU from untrusted ports
//! (hardware drops it) and copies from trusted ports (hardware still
//! forwards; the copy feeds binding learn) — expressed as syncd
//! internal ACL entries via `SetSnoopRedirects`, recomputed here from
//! the config plus the L2 view (which ports carry which VLAN, with
//! Port-Channels expanded to members). The engine validates trapped
//! client messages (server messages from untrusted ports drop and
//! count; chaddr must match the L2 source MAC), maintains the
//! lease-tracked binding table (persisted across restarts in
//! `/var/lib/hemlock/dhcp-bindings.json`), and re-injects valid frames
//! toward the trusted ports.
//!
//! DAI: ARP on inspected VLANs traps from untrusted ports, validates
//! against the binding table + statics + the configured `validate`
//! checks (default src-mac), then re-injects (flooded within the VLAN)
//! or drops with a counted reason and a syslog line. The CPU load of
//! both paths is bounded by the CoPP dhcp/arp classes.

//!
//! The relay lives here too, deliberately: it is a capability of this
//! engine rather than a daemon of its own, so there is exactly one DHCP
//! packet path on the box. A relayed request rides the same
//! trap/validate/re-inject pipeline as a snooped one — the chaddr check
//! runs before anything is forwarded — and the lease in the reply lands
//! in the same binding table, so DAI protects relayed clients without
//! any extra configuration.
//!
//! The server side is a UDP socket rather than the packet path: a
//! relayed request is unicast to an address the kernel routes and ARPs
//! for, which is exactly what a socket bound to the SVI address does.
//! The client side stays on the packet path, because a reply has to
//! reach a station that has no address yet.
use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::Duration;
use tracing::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Validate {
    SrcMac,
    DstMac,
    Ip,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    pub dhcp_vlans: BTreeSet<u16>,
    pub arp_vlans: BTreeSet<u16>,
    /// Empty = default (src-mac).
    pub validate: BTreeSet<Validate>,
    /// Trusted interfaces — ports or Port-Channels (expanded through
    /// the L2 view).
    pub dhcp_trusted: BTreeSet<String>,
    pub arp_trusted: BTreeSet<String>,
    /// Static bindings keyed by (mac, vlan).
    pub statics: BTreeMap<(String, u16), StaticBinding>,
    /// Relay-enabled SVIs, keyed by VLAN.
    pub relay: BTreeMap<u16, RelayVlan>,
}

/// One VLAN's relay configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayVlan {
    /// Servers a request is relayed to, in config order.
    pub servers: Vec<Ipv4Addr>,
    /// The SVI's address: the giaddr a server sees, and the address
    /// replies come back to.
    pub giaddr: Ipv4Addr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticBinding {
    pub ip: Ipv4Addr,
    pub interface: String,
}

/// The L2 facts the redirects need, from syncd's interface view.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct L2View {
    /// VLAN -> physical ports carrying it (Port-Channel members
    /// expanded).
    pub vlan_ports: BTreeMap<u16, BTreeSet<String>>,
    /// Port-Channel display name -> member ports.
    pub po_members: BTreeMap<String, BTreeSet<String>>,
}

/// The per-VLAN redirect program pushed to syncd.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RedirectProgram {
    /// (vlan, untrusted ports, trusted ports).
    pub dhcp: Vec<(u16, Vec<String>, Vec<String>)>,
    /// (vlan, untrusted ports).
    pub arp: Vec<(u16, Vec<String>)>,
}

/// One dynamic binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DynamicBinding {
    ip: Ipv4Addr,
    interface: String,
    /// Unix seconds; the lease-tracked expiry.
    expires_at: u64,
}

/// The persisted shape of the binding table.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedBindings {
    bindings: BTreeMap<String, DynamicBinding>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DhcpVlanStats {
    pub packets: u64,
    pub dropped: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelayVlanStats {
    /// Client requests relayed on to the servers.
    pub to_server: u64,
    /// Server replies relayed back to the client.
    pub to_client: u64,
    pub dropped: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DaiVlanStats {
    pub forwarded: u64,
    pub dropped: u64,
    pub bad_binding: u64,
    pub bad_src_mac: u64,
}

#[derive(Debug, Clone)]
pub struct BindingSnapshot {
    pub mac: String,
    pub vlan: u16,
    pub ip: Ipv4Addr,
    pub interface: String,
    /// Remaining lease; None = static.
    pub lease_secs: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub dhcp_vlans: Vec<u16>,
    pub arp_vlans: Vec<u16>,
    pub validate: Vec<Validate>,
    pub dhcp_trusted: Vec<String>,
    pub arp_trusted: Vec<String>,
    pub bindings: Vec<BindingSnapshot>,
    pub dhcp_stats: BTreeMap<u16, DhcpVlanStats>,
    pub untrusted_server_drops: u64,
    pub arp_stats: BTreeMap<u16, DaiVlanStats>,
    /// Per-VLAN relay state: servers, giaddr and counters.
    pub relay: BTreeMap<u16, (RelayVlan, RelayVlanStats)>,
}

struct Inner {
    config: Config,
    view: L2View,
    /// Dynamic bindings keyed by (mac, vlan).
    dynamics: BTreeMap<(String, u16), DynamicBinding>,
    /// Client port memory: the port a client message last arrived on,
    /// so the ACK's binding lands on the right interface.
    pending: BTreeMap<(String, u16), String>,
    dhcp_stats: BTreeMap<u16, DhcpVlanStats>,
    untrusted_server_drops: u64,
    arp_stats: BTreeMap<u16, DaiVlanStats>,
    pushed_redirects: Option<RedirectProgram>,
    store: Option<std::path::PathBuf>,
    relay_stats: BTreeMap<u16, RelayVlanStats>,
    /// The switch MAC, used as the source of relayed replies.
    system_mac: [u8; 6],
}

/// One BOOTP payload on its way to a relay server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayDatagram {
    pub server: Ipv4Addr,
    /// The source address to send from — the SVI's, so the server
    /// replies to the right relay.
    pub giaddr: Ipv4Addr,
    pub payload: Vec<u8>,
}

pub struct EngineIo {
    /// Trapped frames: (ingress port, VLAN, frame).
    pub packet_in: mpsc::UnboundedSender<(String, u16, Vec<u8>)>,
    /// Re-injections: (egress port, frame).
    pub packet_out: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    /// Redirect programs -> syncd SetSnoopRedirects.
    pub redirects: mpsc::UnboundedReceiver<RedirectProgram>,
    /// Relayed requests, for the UDP runtime to send on.
    pub relay_out: mpsc::UnboundedReceiver<RelayDatagram>,
    /// Server replies the UDP runtime received, by giaddr.
    pub relay_in: mpsc::UnboundedSender<(Ipv4Addr, Vec<u8>)>,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
    packet_out: mpsc::UnboundedSender<(String, Vec<u8>)>,
    redirects_tx: mpsc::UnboundedSender<RedirectProgram>,
    relay_out: mpsc::UnboundedSender<RelayDatagram>,
}

impl Engine {
    /// `store` = the binding persistence file (None in tests that don't
    /// exercise it). Existing bindings load immediately.
    pub fn spawn(store: Option<std::path::PathBuf>, system_mac: [u8; 6]) -> (Engine, EngineIo) {
        let (packet_in_tx, mut packet_in_rx) = mpsc::unbounded_channel::<(String, u16, Vec<u8>)>();
        let (packet_out_tx, packet_out_rx) = mpsc::unbounded_channel();
        let (redirects_tx, redirects_rx) = mpsc::unbounded_channel();
        let (relay_out_tx, relay_out_rx) = mpsc::unbounded_channel();
        let (relay_in_tx, mut relay_in_rx) = mpsc::unbounded_channel::<(Ipv4Addr, Vec<u8>)>();
        let dynamics = store.as_deref().map(load_bindings).unwrap_or_default();
        let inner = Arc::new(Mutex::new(Inner {
            config: Config::default(),
            view: L2View::default(),
            dynamics,
            pending: BTreeMap::new(),
            dhcp_stats: BTreeMap::new(),
            untrusted_server_drops: 0,
            arp_stats: BTreeMap::new(),
            pushed_redirects: None,
            store,
            relay_stats: BTreeMap::new(),
            system_mac,
        }));
        let engine = Engine {
            inner: inner.clone(),
            packet_out: packet_out_tx,
            redirects_tx,
            relay_out: relay_out_tx,
        };
        {
            let engine = engine.clone();
            tokio::spawn(async move {
                while let Some((port, vlan, frame)) = packet_in_rx.recv().await {
                    engine.handle_frame(&port, vlan, &frame);
                }
            });
        }
        {
            // Server replies arriving on the relay socket.
            let engine = engine.clone();
            tokio::spawn(async move {
                while let Some((giaddr, payload)) = relay_in_rx.recv().await {
                    engine.handle_relay_reply(giaddr, &payload);
                }
            });
        }
        {
            // Lease expiry sweep.
            let engine = engine.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    engine.expire_leases();
                }
            });
        }
        (
            engine,
            EngineIo {
                packet_in: packet_in_tx,
                packet_out: packet_out_rx,
                redirects: redirects_rx,
                relay_out: relay_out_rx,
                relay_in: relay_in_tx,
            },
        )
    }

    pub fn set_config(&self, config: Config) {
        {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            if inner.config == config {
                return;
            }
            // Bindings on VLANs no longer snooped age out naturally;
            // stats for dropped VLANs reset.
            inner.config = config;
        }
        self.push_redirects();
    }

    pub fn set_l2_view(&self, view: L2View) {
        {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            if inner.view == view {
                return;
            }
            inner.view = view;
        }
        self.push_redirects();
    }

    /// Recompute and (change-gated) emit the redirect program.
    fn push_redirects(&self) {
        let program = {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            let program = compute_redirects(&inner.config, &inner.view);
            if inner.pushed_redirects.as_ref() == Some(&program) {
                return;
            }
            inner.pushed_redirects = Some(program.clone());
            program
        };
        let _ = self.redirects_tx.send(program);
    }

    /// The trusted physical set for a feature: named ports plus every
    /// member of a named Port-Channel.
    fn expand_trusted(view: &L2View, trusted: &BTreeSet<String>) -> BTreeSet<String> {
        let mut ports = BTreeSet::new();
        for name in trusted {
            match view.po_members.get(name) {
                Some(members) => ports.extend(members.iter().cloned()),
                None => {
                    ports.insert(name.clone());
                }
            }
        }
        ports
    }

    fn handle_frame(&self, port: &str, vlan: u16, frame: &[u8]) {
        if frame.len() < 14 {
            return;
        }
        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
        match ethertype {
            0x0806 => self.handle_arp(port, vlan, frame),
            0x0800 => self.handle_dhcp(port, vlan, frame),
            _ => {}
        }
    }

    fn handle_dhcp(&self, port: &str, vlan: u16, frame: &[u8]) {
        let Some(dhcp) = parse_dhcp(frame) else {
            return;
        };
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if !inner.config.dhcp_vlans.contains(&vlan) {
            return;
        }
        let trusted = Self::expand_trusted(&inner.view, &inner.config.dhcp_trusted).contains(port)
            || inner.config.dhcp_trusted.contains(port);
        inner.dhcp_stats.entry(vlan).or_default().packets += 1;
        let src_mac = mac_text(&frame[6..12]);
        if dhcp.is_reply {
            if !trusted {
                // A rogue server on an access port: dropped in the
                // dataplane (the trap took it), counted here.
                inner.dhcp_stats.entry(vlan).or_default().dropped += 1;
                inner.untrusted_server_drops += 1;
                warn!(%port, vlan, "DHCP server message from untrusted port dropped");
                return;
            }
            // A copied server message from a trusted port: hardware
            // already forwarded it — only the binding learn happens
            // here.
            let client_mac = mac_text(&dhcp.chaddr);
            match dhcp.message_type {
                Some(5) => {
                    // ACK: bind yiaddr to the client.
                    if dhcp.yiaddr != Ipv4Addr::UNSPECIFIED {
                        let interface = inner
                            .pending
                            .remove(&(client_mac.clone(), vlan))
                            .or_else(|| {
                                inner
                                    .dynamics
                                    .get(&(client_mac.clone(), vlan))
                                    .map(|b| b.interface.clone())
                            })
                            .unwrap_or_default();
                        let lease = dhcp.lease_secs.unwrap_or(86400);
                        inner.dynamics.insert(
                            (client_mac, vlan),
                            DynamicBinding {
                                ip: dhcp.yiaddr,
                                interface,
                                expires_at: unix_now() + u64::from(lease),
                            },
                        );
                        save_bindings(&inner);
                    }
                }
                Some(6) => {
                    // NAK: forget the transaction.
                    inner.pending.remove(&(client_mac, vlan));
                }
                _ => {}
            }
            return;
        }
        // A trapped client message from an untrusted port (or a copied
        // one from a trusted port). The chaddr/src-mac check catches
        // spoofed requests.
        if dhcp.chaddr != frame[6..12] {
            inner.dhcp_stats.entry(vlan).or_default().dropped += 1;
            warn!(%port, vlan, %src_mac, "DHCP client message with mismatched chaddr dropped");
            return;
        }
        let client_mac = mac_text(&dhcp.chaddr);
        match dhcp.message_type {
            // RELEASE / DECLINE drop the binding.
            Some(7) | Some(4) => {
                inner.dynamics.remove(&(client_mac.clone(), vlan));
                save_bindings(&inner);
            }
            _ => {
                inner.pending.insert((client_mac, vlan), port.to_string());
            }
        }
        // A relay-enabled VLAN sends the validated request on to the
        // servers rather than (only) flooding it toward a trusted port:
        // the whole point is that there is no server in this broadcast
        // domain. The chaddr check above has already run, so nothing
        // unvalidated is ever relayed.
        if let Some(relay) = inner.config.relay.get(&vlan).cloned() {
            let stats = inner.relay_stats.entry(vlan).or_default();
            if relay.giaddr.is_unspecified() || relay.servers.is_empty() {
                // Config that cannot relay: counted, not silently lost.
                stats.dropped += 1;
                warn!(
                    vlan,
                    "dhcp-relay has no giaddr or no server; request dropped"
                );
                return;
            }
            let Some(payload) = bootp_payload(frame) else {
                stats.dropped += 1;
                return;
            };
            let relayed = with_giaddr(&payload, relay.giaddr);
            stats.to_server += 1;
            let servers = relay.servers.clone();
            drop(inner);
            for server in servers {
                let _ = self.relay_out.send(RelayDatagram {
                    server,
                    giaddr: relay.giaddr,
                    payload: relayed.clone(),
                });
            }
            return;
        }
        if trusted {
            // Copies from trusted ports already forwarded in hardware.
            return;
        }
        // Re-inject the validated request toward the trusted side.
        let targets: Vec<String> = Self::expand_trusted(&inner.view, &inner.config.dhcp_trusted)
            .into_iter()
            .filter(|p| {
                p != port
                    && inner
                        .view
                        .vlan_ports
                        .get(&vlan)
                        .map(|ports| ports.contains(p))
                        .unwrap_or(false)
            })
            .collect();
        drop(inner);
        for target in targets {
            let _ = self.packet_out.send((target, frame.to_vec()));
        }
    }

    fn handle_arp(&self, port: &str, vlan: u16, frame: &[u8]) {
        let Some(arp) = parse_arp(frame) else { return };
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if !inner.config.arp_vlans.contains(&vlan) {
            return;
        }
        let src_mac = mac_text(&frame[6..12]);
        let sender_mac = mac_text(&arp.sha);
        let checks: BTreeSet<Validate> = if inner.config.validate.is_empty() {
            [Validate::SrcMac].into()
        } else {
            inner.config.validate.clone()
        };
        let stats = inner.arp_stats.entry(vlan).or_default();
        if checks.contains(&Validate::SrcMac) && sender_mac != src_mac {
            stats.dropped += 1;
            stats.bad_src_mac += 1;
            warn!(%port, vlan, %src_mac, %sender_mac, "ARP with mismatched sender MAC dropped");
            return;
        }
        if checks.contains(&Validate::DstMac) && arp.is_reply {
            let target_mac = mac_text(&arp.tha);
            let dst_mac = mac_text(&frame[0..6]);
            if target_mac != dst_mac {
                stats.dropped += 1;
                warn!(%port, vlan, "ARP reply with mismatched target MAC dropped");
                return;
            }
        }
        if checks.contains(&Validate::Ip) {
            let bad_ip = arp.spa.is_broadcast()
                || arp.spa.is_multicast()
                || arp.spa.is_loopback()
                || (arp.is_reply && arp.spa.is_unspecified());
            if bad_ip {
                stats.dropped += 1;
                warn!(%port, vlan, spa = %arp.spa, "ARP with invalid sender IP dropped");
                return;
            }
        }
        // The binding check: the sender (MAC, VLAN) must hold sender IP
        // per the snooping table or a static.
        let bound = inner
            .dynamics
            .get(&(sender_mac.clone(), vlan))
            .map(|b| b.ip == arp.spa)
            .or_else(|| {
                inner
                    .config
                    .statics
                    .get(&(sender_mac.clone(), vlan))
                    .map(|b| b.ip == arp.spa)
            })
            .unwrap_or(false);
        let stats = inner.arp_stats.entry(vlan).or_default();
        if !bound {
            stats.dropped += 1;
            stats.bad_binding += 1;
            warn!(%port, vlan, %sender_mac, spa = %arp.spa, "ARP with no matching snooping binding dropped");
            return;
        }
        stats.forwarded += 1;
        // Valid: re-inject within the VLAN (flooded; the FDB delivers
        // unicast replies to everyone carrying the VLAN, which is what
        // a flood does anyway at this rate).
        let targets: Vec<String> = inner
            .view
            .vlan_ports
            .get(&vlan)
            .map(|ports| ports.iter().filter(|p| *p != port).cloned().collect())
            .unwrap_or_default();
        drop(inner);
        for target in targets {
            let _ = self.packet_out.send((target, frame.to_vec()));
        }
    }

    /// One server reply arriving on the relay socket, addressed to a
    /// giaddr. The VLAN is whichever relay-enabled SVI owns that
    /// address; anything else is a reply for a relay we do not run.
    fn handle_relay_reply(&self, giaddr: Ipv4Addr, payload: &[u8]) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let Some((&vlan, relay)) = inner
            .config
            .relay
            .iter()
            .find(|(_, relay)| relay.giaddr == giaddr)
        else {
            return;
        };
        let servers = relay.servers.clone();
        let _ = servers;
        let Some(reply) = parse_bootp(payload) else {
            inner.relay_stats.entry(vlan).or_default().dropped += 1;
            return;
        };
        if !reply.is_reply {
            inner.relay_stats.entry(vlan).or_default().dropped += 1;
            return;
        }
        let client_mac = mac_text(&reply.chaddr);

        // Relayed leases are ordinary snooping bindings: DAI protects a
        // relayed client exactly as it protects a locally served one.
        match reply.message_type {
            Some(5) if reply.yiaddr != Ipv4Addr::UNSPECIFIED => {
                let interface = inner
                    .pending
                    .remove(&(client_mac.clone(), vlan))
                    .or_else(|| {
                        inner
                            .dynamics
                            .get(&(client_mac.clone(), vlan))
                            .map(|b| b.interface.clone())
                    })
                    .unwrap_or_default();
                let lease = reply.lease_secs.unwrap_or(86400);
                inner.dynamics.insert(
                    (client_mac.clone(), vlan),
                    DynamicBinding {
                        ip: reply.yiaddr,
                        interface,
                        expires_at: unix_now() + u64::from(lease),
                    },
                );
                save_bindings(&inner);
            }
            Some(6) => {
                inner.pending.remove(&(client_mac.clone(), vlan));
            }
            _ => {}
        }

        // Toward the client: unicast when it already has an address and
        // did not ask for a broadcast, else flood the VLAN — a station
        // that has no address cannot receive a unicast.
        let frame = build_client_reply(inner.system_mac, giaddr, &reply, payload);
        let unicast_port = (!reply.broadcast_flag)
            .then(|| {
                inner
                    .dynamics
                    .get(&(client_mac, vlan))
                    .map(|b| b.interface.clone())
            })
            .flatten()
            .filter(|port| !port.is_empty());
        let targets: Vec<String> = match unicast_port {
            Some(port) => vec![port],
            None => inner
                .view
                .vlan_ports
                .get(&vlan)
                .map(|ports| ports.iter().cloned().collect())
                .unwrap_or_default(),
        };
        inner.relay_stats.entry(vlan).or_default().to_client += 1;
        drop(inner);
        for target in targets {
            let _ = self.packet_out.send((target, frame.clone()));
        }
    }

    fn expire_leases(&self) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let now = unix_now();
        let before = inner.dynamics.len();
        inner.dynamics.retain(|_, b| b.expires_at > now);
        if inner.dynamics.len() != before {
            save_bindings(&inner);
        }
    }

    /// `clear dhcp snooping binding [<mac>]` — dynamics only.
    pub fn clear_bindings(&self, mac: Option<&str>) -> u32 {
        let Ok(mut inner) = self.inner.lock() else {
            return 0;
        };
        let before = inner.dynamics.len();
        match mac {
            Some(mac) => inner.dynamics.retain(|(m, _), _| m != mac),
            None => inner.dynamics.clear(),
        }
        let cleared = (before - inner.dynamics.len()) as u32;
        if cleared > 0 {
            save_bindings(&inner);
        }
        cleared
    }

    pub fn snapshot(&self) -> Snapshot {
        let Ok(inner) = self.inner.lock() else {
            return Snapshot::default();
        };
        let now = unix_now();
        let mut bindings: Vec<BindingSnapshot> = inner
            .dynamics
            .iter()
            .map(|((mac, vlan), b)| BindingSnapshot {
                mac: mac.clone(),
                vlan: *vlan,
                ip: b.ip,
                interface: b.interface.clone(),
                lease_secs: Some(b.expires_at.saturating_sub(now)),
            })
            .chain(
                inner
                    .config
                    .statics
                    .iter()
                    .map(|((mac, vlan), b)| BindingSnapshot {
                        mac: mac.clone(),
                        vlan: *vlan,
                        ip: b.ip,
                        interface: b.interface.clone(),
                        lease_secs: None,
                    }),
            )
            .collect();
        bindings.sort_by(|a, b| (a.vlan, &a.ip).cmp(&(b.vlan, &b.ip)));
        Snapshot {
            dhcp_vlans: inner.config.dhcp_vlans.iter().copied().collect(),
            arp_vlans: inner.config.arp_vlans.iter().copied().collect(),
            validate: if inner.config.validate.is_empty() {
                vec![Validate::SrcMac]
            } else {
                inner.config.validate.iter().copied().collect()
            },
            dhcp_trusted: inner.config.dhcp_trusted.iter().cloned().collect(),
            arp_trusted: inner.config.arp_trusted.iter().cloned().collect(),
            bindings,
            dhcp_stats: inner.dhcp_stats.clone(),
            untrusted_server_drops: inner.untrusted_server_drops,
            arp_stats: inner.arp_stats.clone(),
            relay: inner
                .config
                .relay
                .iter()
                .map(|(vlan, relay)| {
                    let stats = inner.relay_stats.get(vlan).copied().unwrap_or_default();
                    (*vlan, (relay.clone(), stats))
                })
                .collect(),
        }
    }
}

/// The redirect program: per snooped VLAN, its untrusted carriers trap
/// and trusted carriers copy; per inspected VLAN, untrusted carriers
/// trap ARP.
fn compute_redirects(config: &Config, view: &L2View) -> RedirectProgram {
    let dhcp_trusted = Engine::expand_trusted(view, &config.dhcp_trusted);
    let arp_trusted = Engine::expand_trusted(view, &config.arp_trusted);
    let mut program = RedirectProgram::default();
    for vlan in &config.dhcp_vlans {
        let carriers = view.vlan_ports.get(vlan).cloned().unwrap_or_default();
        let untrusted: Vec<String> = carriers
            .iter()
            .filter(|p| !dhcp_trusted.contains(*p))
            .cloned()
            .collect();
        let trusted: Vec<String> = carriers
            .iter()
            .filter(|p| dhcp_trusted.contains(*p))
            .cloned()
            .collect();
        program.dhcp.push((*vlan, untrusted, trusted));
    }
    for vlan in &config.arp_vlans {
        let carriers = view.vlan_ports.get(vlan).cloned().unwrap_or_default();
        let untrusted: Vec<String> = carriers
            .iter()
            .filter(|p| !arp_trusted.contains(*p))
            .cloned()
            .collect();
        program.arp.push((*vlan, untrusted));
    }
    program
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mac_text(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn load_bindings(path: &std::path::Path) -> BTreeMap<(String, u16), DynamicBinding> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(persisted) = serde_json::from_str::<PersistedBindings>(&text) else {
        warn!(path = %path.display(), "unreadable binding store; starting empty");
        return BTreeMap::new();
    };
    let now = unix_now();
    persisted
        .bindings
        .into_iter()
        .filter(|(_, b)| b.expires_at > now)
        .filter_map(|(key, binding)| {
            let (mac, vlan) = key.rsplit_once('@')?;
            Some(((mac.to_string(), vlan.parse().ok()?), binding))
        })
        .collect()
}

fn save_bindings(inner: &Inner) {
    let Some(path) = &inner.store else { return };
    let persisted = PersistedBindings {
        bindings: inner
            .dynamics
            .iter()
            .map(|((mac, vlan), b)| (format!("{mac}@{vlan}"), b.clone()))
            .collect(),
    };
    let Ok(text) = serde_json::to_string_pretty(&persisted) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(err) = std::fs::write(path, text) {
        warn!(path = %path.display(), %err, "cannot persist DHCP bindings");
    }
}

struct DhcpMessage {
    is_reply: bool,
    chaddr: [u8; 6],
    yiaddr: Ipv4Addr,
    /// Option 53.
    message_type: Option<u8>,
    /// Option 51.
    lease_secs: Option<u32>,
}

/// A DHCP message out of an untagged IPv4/UDP frame; None = not DHCP.
fn parse_dhcp(frame: &[u8]) -> Option<DhcpMessage> {
    if frame.len() < 14 + 20 + 8 {
        return None;
    }
    let ip = &frame[14..];
    if ip[0] >> 4 != 4 || ip[9] != 17 {
        return None;
    }
    let ihl = usize::from(ip[0] & 0xf) * 4;
    let udp = ip.get(ihl..)?;
    let dport = u16::from_be_bytes([*udp.get(2)?, *udp.get(3)?]);
    if !matches!(dport, 67 | 68) {
        return None;
    }
    let dhcp = udp.get(8..)?;
    if dhcp.len() < 240 {
        return None;
    }
    let is_reply = match dhcp[0] {
        1 => false,
        2 => true,
        _ => return None,
    };
    let mut chaddr = [0u8; 6];
    chaddr.copy_from_slice(&dhcp[28..34]);
    let yiaddr = Ipv4Addr::new(dhcp[16], dhcp[17], dhcp[18], dhcp[19]);
    // Options after the magic cookie.
    if dhcp[236..240] != [99, 130, 83, 99] {
        return None;
    }
    let mut message_type = None;
    let mut lease_secs = None;
    let mut rest = &dhcp[240..];
    while let [code, rest_after @ ..] = rest {
        match code {
            0 => {
                rest = rest_after;
                continue;
            }
            255 => break,
            _ => {}
        }
        let [len, value @ ..] = rest_after else { break };
        let len = usize::from(*len);
        if value.len() < len {
            break;
        }
        match code {
            53 if len == 1 => message_type = Some(value[0]),
            51 if len == 4 => {
                lease_secs = Some(u32::from_be_bytes([value[0], value[1], value[2], value[3]]));
            }
            _ => {}
        }
        rest = &value[len..];
    }
    Some(DhcpMessage {
        is_reply,
        chaddr,
        yiaddr,
        message_type,
        lease_secs,
    })
}

struct ArpMessage {
    is_reply: bool,
    sha: [u8; 6],
    spa: Ipv4Addr,
    tha: [u8; 6],
}

/// An Ethernet/IPv4 ARP out of an untagged frame; None = not ARP.
fn parse_arp(frame: &[u8]) -> Option<ArpMessage> {
    let arp = frame.get(14..42)?;
    // Ethernet/IPv4, 6-byte hw and 4-byte proto addresses.
    if arp[0..2] != [0, 1] || arp[2..4] != [8, 0] || arp[4] != 6 || arp[5] != 4 {
        return None;
    }
    let oper = u16::from_be_bytes([arp[6], arp[7]]);
    let mut sha = [0u8; 6];
    sha.copy_from_slice(&arp[8..14]);
    let spa = Ipv4Addr::new(arp[14], arp[15], arp[16], arp[17]);
    let mut tha = [0u8; 6];
    tha.copy_from_slice(&arp[18..24]);
    Some(ArpMessage {
        is_reply: oper == 2,
        sha,
        spa,
        tha,
    })
}

// ------------------------------------------------------ relay framing

/// The BOOTP payload of a trapped DHCP frame (everything after the UDP
/// header). None = not a well-formed DHCP frame.
fn bootp_payload(frame: &[u8]) -> Option<Vec<u8>> {
    let ip = frame.get(14..)?;
    if ip.len() < 20 || ip[0] >> 4 != 4 || ip[9] != 17 {
        return None;
    }
    let ihl = usize::from(ip[0] & 0xf) * 4;
    // The IP total length bounds the payload: an Ethernet frame is
    // padded to 60 bytes, and relaying the padding would corrupt the
    // options a server reads.
    let total = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
    let end = total.clamp(ihl + 8, ip.len());
    Some(ip.get(ihl + 8..end)?.to_vec())
}

/// The relayed copy of a request: hop count bumped and giaddr filled in
/// (a request that already carries one keeps it — that relay owns the
/// reply path).
fn with_giaddr(payload: &[u8], giaddr: Ipv4Addr) -> Vec<u8> {
    let mut out = payload.to_vec();
    if out.len() < 28 {
        return out;
    }
    out[3] = out[3].saturating_add(1);
    if out[24..28] == [0, 0, 0, 0] {
        out[24..28].copy_from_slice(&giaddr.octets());
    }
    out
}

/// The BOOTP fields the relay reads back out of a reply.
struct BootpReply {
    is_reply: bool,
    chaddr: [u8; 6],
    yiaddr: Ipv4Addr,
    broadcast_flag: bool,
    message_type: Option<u8>,
    lease_secs: Option<u32>,
}

/// A bare BOOTP payload (no IP/UDP headers) — what arrives on the relay
/// socket.
fn parse_bootp(payload: &[u8]) -> Option<BootpReply> {
    if payload.len() < 240 || payload[236..240] != [99, 130, 83, 99] {
        return None;
    }
    let mut chaddr = [0u8; 6];
    chaddr.copy_from_slice(&payload[28..34]);
    let mut message_type = None;
    let mut lease_secs = None;
    let mut rest = &payload[240..];
    while let [code, rest_after @ ..] = rest {
        match code {
            0 => {
                rest = rest_after;
                continue;
            }
            255 => break,
            _ => {}
        }
        let [len, value @ ..] = rest_after else { break };
        let len = usize::from(*len);
        if value.len() < len {
            break;
        }
        match code {
            53 if len == 1 => message_type = Some(value[0]),
            51 if len == 4 => {
                lease_secs = Some(u32::from_be_bytes([value[0], value[1], value[2], value[3]]));
            }
            _ => {}
        }
        rest = &value[len..];
    }
    Some(BootpReply {
        is_reply: payload[0] == 2,
        chaddr,
        yiaddr: Ipv4Addr::new(payload[16], payload[17], payload[18], payload[19]),
        broadcast_flag: payload[10] & 0x80 != 0,
        message_type,
        lease_secs,
    })
}

/// The RFC 1071 one's-complement checksum of an IPv4 header.
fn ip_checksum(header: &[u8]) -> u16 {
    let mut sum = 0u32;
    for pair in header.chunks(2) {
        let word = match pair {
            [high, low] => u16::from_be_bytes([*high, *low]),
            [high] => u16::from_be_bytes([*high, 0]),
            _ => 0,
        };
        sum += u32::from(word);
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Wrap a relayed reply in Ethernet/IPv4/UDP for the client side.
///
/// A client that set the broadcast flag (or has no address yet) is sent
/// to the broadcast addresses; anything else goes to its own MAC and
/// yiaddr. The UDP checksum is left zero, which IPv4 allows and every
/// DHCP client accepts.
fn build_client_reply(
    system_mac: [u8; 6],
    giaddr: Ipv4Addr,
    reply: &BootpReply,
    payload: &[u8],
) -> Vec<u8> {
    let broadcast = reply.broadcast_flag || reply.yiaddr == Ipv4Addr::UNSPECIFIED;
    let (dst_mac, dst_ip) = if broadcast {
        ([0xff; 6], Ipv4Addr::BROADCAST)
    } else {
        (reply.chaddr, reply.yiaddr)
    };
    let udp_len = 8 + payload.len();
    let total_len = 20 + udp_len;

    let mut frame = Vec::with_capacity(14 + total_len);
    frame.extend_from_slice(&dst_mac);
    frame.extend_from_slice(&system_mac);
    frame.extend_from_slice(&[0x08, 0x00]);

    let mut ip = Vec::with_capacity(20);
    ip.extend_from_slice(&[0x45, 0x00]);
    ip.extend_from_slice(&(total_len as u16).to_be_bytes());
    ip.extend_from_slice(&[0, 0, 0x40, 0x00]); // id 0, don't fragment
    ip.extend_from_slice(&[64, 17]); // ttl, UDP
    ip.extend_from_slice(&[0, 0]); // checksum placeholder
    ip.extend_from_slice(&giaddr.octets());
    ip.extend_from_slice(&dst_ip.octets());
    let checksum = ip_checksum(&ip);
    ip[10..12].copy_from_slice(&checksum.to_be_bytes());
    frame.extend_from_slice(&ip);

    frame.extend_from_slice(&67u16.to_be_bytes());
    frame.extend_from_slice(&68u16.to_be_bytes());
    frame.extend_from_slice(&(udp_len as u16).to_be_bytes());
    frame.extend_from_slice(&[0, 0]); // checksum: optional over IPv4
    frame.extend_from_slice(payload);
    // Pad to the 60-byte Ethernet minimum (the FCS follows on the wire).
    if frame.len() < 60 {
        frame.resize(60, 0);
    }
    frame
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const CLIENT_MAC: [u8; 6] = [0x00, 0x1c, 0x73, 0x0c, 0xaa, 0x01];
    const SWITCH_MAC: [u8; 6] = [0x2c, 0xdd, 0xe9, 0x4a, 0x1b, 0x00];
    const SERVER_MAC: [u8; 6] = [0x00, 0x50, 0x56, 0xbe, 0xef, 0x01];

    fn dhcp_frame(
        src_mac: [u8; 6],
        op: u8,
        chaddr: [u8; 6],
        yiaddr: Ipv4Addr,
        message_type: u8,
        lease: Option<u32>,
    ) -> Vec<u8> {
        let mut dhcp = vec![0u8; 240];
        dhcp[0] = op;
        dhcp[1] = 1;
        dhcp[2] = 6;
        dhcp[16..20].copy_from_slice(&yiaddr.octets());
        dhcp[28..34].copy_from_slice(&chaddr);
        dhcp[236..240].copy_from_slice(&[99, 130, 83, 99]);
        dhcp.extend_from_slice(&[53, 1, message_type]);
        if let Some(lease) = lease {
            dhcp.extend_from_slice(&[51, 4]);
            dhcp.extend_from_slice(&lease.to_be_bytes());
        }
        dhcp.push(255);

        let mut udp = Vec::new();
        udp.extend_from_slice(&if op == 1 { 68u16 } else { 67u16 }.to_be_bytes());
        udp.extend_from_slice(&if op == 1 { 67u16 } else { 68u16 }.to_be_bytes());
        udp.extend_from_slice(&((8 + dhcp.len()) as u16).to_be_bytes());
        udp.extend_from_slice(&[0, 0]);
        udp.extend_from_slice(&dhcp);

        let mut ip = vec![0x45, 0];
        ip.extend_from_slice(&((20 + udp.len()) as u16).to_be_bytes());
        ip.extend_from_slice(&[0, 0, 0, 0, 64, 17, 0, 0]);
        ip.extend_from_slice(&[0, 0, 0, 0, 255, 255, 255, 255]);
        ip.extend_from_slice(&udp);

        let mut frame = Vec::new();
        frame.extend_from_slice(&[0xff; 6]);
        frame.extend_from_slice(&src_mac);
        frame.extend_from_slice(&0x0800u16.to_be_bytes());
        frame.extend_from_slice(&ip);
        frame
    }

    fn arp_frame(src_mac: [u8; 6], sha: [u8; 6], spa: Ipv4Addr, reply: bool) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.extend_from_slice(&[0xff; 6]);
        frame.extend_from_slice(&src_mac);
        frame.extend_from_slice(&0x0806u16.to_be_bytes());
        frame.extend_from_slice(&[0, 1, 8, 0, 6, 4]);
        frame.extend_from_slice(&if reply { 2u16 } else { 1u16 }.to_be_bytes());
        frame.extend_from_slice(&sha);
        frame.extend_from_slice(&spa.octets());
        frame.extend_from_slice(&[0u8; 6]);
        frame.extend_from_slice(&[0u8; 4]);
        frame
    }

    fn seed_config() -> Config {
        Config {
            dhcp_vlans: [10].into(),
            arp_vlans: [10].into(),
            validate: BTreeSet::new(),
            dhcp_trusted: ["Port-Channel1".to_string()].into(),
            arp_trusted: ["Port-Channel1".to_string()].into(),
            statics: BTreeMap::new(),
            relay: BTreeMap::new(),
        }
    }

    fn seed_view() -> L2View {
        L2View {
            vlan_ports: [(
                10u16,
                ["Ethernet1", "Ethernet2", "Ethernet49", "Ethernet50"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            )]
            .into(),
            po_members: [(
                "Port-Channel1".to_string(),
                ["Ethernet49", "Ethernet50"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            )]
            .into(),
        }
    }

    async fn drain_redirects(io: &mut EngineIo) -> RedirectProgram {
        let mut last = io.redirects.recv().await.unwrap();
        while let Ok(next) = io.redirects.try_recv() {
            last = next;
        }
        last
    }

    /// A bare BOOTP reply, as it arrives on the relay socket (no
    /// Ethernet/IP/UDP headers — the server sent it to giaddr:67).
    fn bootp_reply(
        chaddr: [u8; 6],
        yiaddr: Ipv4Addr,
        giaddr: Ipv4Addr,
        message_type: u8,
        broadcast: bool,
    ) -> Vec<u8> {
        let mut payload = vec![0u8; 240];
        payload[0] = 2;
        payload[1] = 1;
        payload[2] = 6;
        if broadcast {
            payload[10] = 0x80;
        }
        payload[16..20].copy_from_slice(&yiaddr.octets());
        payload[24..28].copy_from_slice(&giaddr.octets());
        payload[28..34].copy_from_slice(&chaddr);
        payload[236..240].copy_from_slice(&[99, 130, 83, 99]);
        payload.extend_from_slice(&[53, 1, message_type]);
        payload.extend_from_slice(&[51, 4]);
        payload.extend_from_slice(&3600u32.to_be_bytes());
        payload.push(255);
        payload
    }

    fn relay_config() -> Config {
        let mut config = seed_config();
        config.relay.insert(
            10,
            RelayVlan {
                servers: vec![Ipv4Addr::new(10, 42, 0, 5), Ipv4Addr::new(10, 42, 0, 6)],
                giaddr: Ipv4Addr::new(10, 0, 10, 1),
            },
        );
        config
    }

    /// The full relay round trip: a client broadcast is validated, has
    /// giaddr stamped in and goes to every server; the reply comes back
    /// on the socket, learns a binding, and re-injects toward the
    /// client.
    #[tokio::test]
    async fn relay_forwards_requests_and_returns_replies() {
        let (engine, mut io) = Engine::spawn(None, SWITCH_MAC);
        engine.set_l2_view(seed_view());
        engine.set_config(relay_config());

        // A DISCOVER on an untrusted access port.
        let request = dhcp_frame(CLIENT_MAC, 1, CLIENT_MAC, Ipv4Addr::UNSPECIFIED, 1, None);
        io.packet_in
            .send(("Ethernet1".into(), 10, request))
            .unwrap();

        // One datagram per server, each carrying the giaddr.
        let mut relayed = Vec::new();
        for _ in 0..2 {
            relayed.push(io.relay_out.recv().await.unwrap());
        }
        assert_eq!(
            relayed.iter().map(|d| d.server).collect::<Vec<_>>(),
            vec![Ipv4Addr::new(10, 42, 0, 5), Ipv4Addr::new(10, 42, 0, 6)]
        );
        for datagram in &relayed {
            assert_eq!(datagram.giaddr, Ipv4Addr::new(10, 0, 10, 1));
            // giaddr stamped into the BOOTP header, hop count bumped.
            assert_eq!(&datagram.payload[24..28], &[10, 0, 10, 1]);
            assert_eq!(datagram.payload[3], 1);
            // It is the client's request, not a rewrite of one.
            assert_eq!(&datagram.payload[28..34], &CLIENT_MAC);
        }
        let snapshot = engine.snapshot();
        let (_, stats) = &snapshot.relay[&10];
        assert_eq!(stats.to_server, 1);

        // The OFFER comes back on the socket, addressed to giaddr.
        io.relay_in
            .send((
                Ipv4Addr::new(10, 0, 10, 1),
                bootp_reply(
                    CLIENT_MAC,
                    Ipv4Addr::new(10, 0, 10, 55),
                    Ipv4Addr::new(10, 0, 10, 1),
                    2,
                    true,
                ),
            ))
            .unwrap();

        // A broadcast reply floods the VLAN.
        let mut targets = Vec::new();
        let mut frame = Vec::new();
        for _ in 0..4 {
            let (port, bytes) = io.packet_out.recv().await.unwrap();
            targets.push(port);
            frame = bytes;
        }
        targets.sort();
        assert_eq!(
            targets,
            vec!["Ethernet1", "Ethernet2", "Ethernet49", "Ethernet50"]
        );
        // A well-formed broadcast reply from the SVI's address.
        assert_eq!(&frame[0..6], &[0xff; 6]);
        assert_eq!(&frame[6..12], &SWITCH_MAC);
        assert_eq!(u16::from_be_bytes([frame[12], frame[13]]), 0x0800);
        assert_eq!(&frame[26..30], &[10, 0, 10, 1], "source is the giaddr");
        assert_eq!(&frame[30..34], &[255, 255, 255, 255]);
        assert_eq!(u16::from_be_bytes([frame[34], frame[35]]), 67);
        assert_eq!(u16::from_be_bytes([frame[36], frame[37]]), 68);
        // The IPv4 header checksum is real, not zero.
        assert_eq!(ip_checksum(&frame[14..34]), 0);

        let snapshot = engine.snapshot();
        let (_, stats) = &snapshot.relay[&10];
        assert_eq!((stats.to_server, stats.to_client, stats.dropped), (1, 1, 0));
    }

    /// A relayed lease is an ordinary snooping binding, so DAI accepts
    /// the client's ARP afterwards — the whole reason the relay lives
    /// inside this engine.
    #[tokio::test]
    async fn relayed_leases_feed_the_binding_table() {
        let (engine, mut io) = Engine::spawn(None, SWITCH_MAC);
        engine.set_l2_view(seed_view());
        engine.set_config(relay_config());

        // The request remembers which port the client is on...
        io.packet_in
            .send((
                "Ethernet1".into(),
                10,
                dhcp_frame(CLIENT_MAC, 1, CLIENT_MAC, Ipv4Addr::UNSPECIFIED, 3, None),
            ))
            .unwrap();
        io.relay_out.recv().await.unwrap();
        io.relay_out.recv().await.unwrap();

        // ...and the ACK binds yiaddr to it.
        io.relay_in
            .send((
                Ipv4Addr::new(10, 0, 10, 1),
                bootp_reply(
                    CLIENT_MAC,
                    Ipv4Addr::new(10, 0, 10, 55),
                    Ipv4Addr::new(10, 0, 10, 1),
                    5,
                    false,
                ),
            ))
            .unwrap();
        let (port, _) = io.packet_out.recv().await.unwrap();
        // Not a broadcast: it goes straight to the client's port.
        assert_eq!(port, "Ethernet1");

        let snapshot = engine.snapshot();
        let binding = snapshot
            .bindings
            .iter()
            .find(|b| b.mac == "00:1c:73:0c:aa:01")
            .expect("relayed lease should bind");
        assert_eq!(binding.ip, Ipv4Addr::new(10, 0, 10, 55));
        assert_eq!(binding.interface, "Ethernet1");
        assert_eq!(binding.vlan, 10);

        // DAI now accepts that client's ARP.
        io.packet_in
            .send((
                "Ethernet1".into(),
                10,
                arp_frame(CLIENT_MAC, CLIENT_MAC, Ipv4Addr::new(10, 0, 10, 55), false),
            ))
            .unwrap();
        let (port, _) = io.packet_out.recv().await.unwrap();
        assert_ne!(port, "Ethernet1", "the ARP re-injects to the other ports");
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.arp_stats[&10].forwarded, 1);
        assert_eq!(snapshot.arp_stats[&10].dropped, 0);
    }

    /// The relay validates before it forwards: a spoofed chaddr never
    /// reaches a server, and a VLAN with no giaddr counts the drop
    /// rather than losing the request silently.
    #[tokio::test]
    async fn relay_drops_what_snooping_would_drop() {
        let (engine, mut io) = Engine::spawn(None, SWITCH_MAC);
        engine.set_l2_view(seed_view());
        engine.set_config(relay_config());

        // chaddr does not match the source MAC.
        io.packet_in
            .send((
                "Ethernet1".into(),
                10,
                dhcp_frame(
                    CLIENT_MAC,
                    1,
                    [0xde, 0xad, 0xbe, 0xef, 0x00, 0x01],
                    Ipv4Addr::UNSPECIFIED,
                    1,
                    None,
                ),
            ))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(io.relay_out.try_recv().is_err(), "spoofed request relayed");
        assert_eq!(engine.snapshot().dhcp_stats[&10].dropped, 1);

        // A relay with no giaddr cannot forward; the drop is counted.
        let mut config = relay_config();
        config.relay.insert(
            10,
            RelayVlan {
                servers: vec![Ipv4Addr::new(10, 42, 0, 5)],
                giaddr: Ipv4Addr::UNSPECIFIED,
            },
        );
        engine.set_config(config);
        io.packet_in
            .send((
                "Ethernet1".into(),
                10,
                dhcp_frame(CLIENT_MAC, 1, CLIENT_MAC, Ipv4Addr::UNSPECIFIED, 1, None),
            ))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(io.relay_out.try_recv().is_err());
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.relay[&10].1.dropped, 1);

        // A reply for a giaddr no relay owns is ignored entirely.
        io.relay_in
            .send((
                Ipv4Addr::new(192, 0, 2, 1),
                bootp_reply(
                    CLIENT_MAC,
                    Ipv4Addr::new(10, 0, 10, 55),
                    Ipv4Addr::new(192, 0, 2, 1),
                    5,
                    true,
                ),
            ))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(io.packet_out.try_recv().is_err());
    }

    /// The BOOTP payload is bounded by the IP header's total length, so
    /// Ethernet padding never reaches a server as option bytes.
    #[test]
    fn relayed_payloads_exclude_ethernet_padding() {
        let mut frame = dhcp_frame(CLIENT_MAC, 1, CLIENT_MAC, Ipv4Addr::UNSPECIFIED, 1, None);
        let real = bootp_payload(&frame).unwrap();
        frame.extend_from_slice(&[0u8; 18]);
        assert_eq!(bootp_payload(&frame).unwrap(), real);
        // The payload really is the BOOTP message.
        assert_eq!(real[0], 1);
        assert_eq!(&real[236..240], &[99, 130, 83, 99]);
    }

    /// A request that already carries a giaddr keeps it: that relay
    /// owns the reply path, and overwriting it would strand the client.
    #[test]
    fn an_existing_giaddr_is_left_alone() {
        let mut payload = vec![0u8; 240];
        payload[24..28].copy_from_slice(&[10, 9, 9, 9]);
        let relayed = with_giaddr(&payload, Ipv4Addr::new(10, 0, 10, 1));
        assert_eq!(&relayed[24..28], &[10, 9, 9, 9]);
        assert_eq!(relayed[3], 1, "the hop count still counts this relay");

        let relayed = with_giaddr(&vec![0u8; 240], Ipv4Addr::new(10, 0, 10, 1));
        assert_eq!(&relayed[24..28], &[10, 0, 10, 1]);
    }

    #[tokio::test]
    async fn redirects_expand_port_channels_and_split_trust() {
        let (engine, mut io) = Engine::spawn(None, SWITCH_MAC);
        engine.set_config(seed_config());
        engine.set_l2_view(seed_view());
        let program = drain_redirects(&mut io).await;
        let (vlan, untrusted, trusted) = &program.dhcp[0];
        assert_eq!(*vlan, 10);
        assert_eq!(untrusted, &["Ethernet1", "Ethernet2"]);
        assert_eq!(trusted, &["Ethernet49", "Ethernet50"]);
        let (vlan, untrusted) = &program.arp[0];
        assert_eq!(*vlan, 10);
        assert_eq!(untrusted, &["Ethernet1", "Ethernet2"]);
    }

    #[tokio::test]
    async fn dhcp_flow_learns_bindings_and_blocks_rogue_servers() {
        let (engine, mut io) = Engine::spawn(None, SWITCH_MAC);
        engine.set_config(seed_config());
        engine.set_l2_view(seed_view());
        let _ = drain_redirects(&mut io).await;

        // A valid DISCOVER from an untrusted port re-injects toward
        // the trusted members.
        io.packet_in
            .send((
                "Ethernet1".into(),
                10,
                dhcp_frame(CLIENT_MAC, 1, CLIENT_MAC, Ipv4Addr::UNSPECIFIED, 1, None),
            ))
            .unwrap();
        let (target, _) = io.packet_out.recv().await.unwrap();
        assert!(["Ethernet49", "Ethernet50"].contains(&target.as_str()));
        let (target2, _) = io.packet_out.recv().await.unwrap();
        assert_ne!(target, target2);

        // The server ACK (copied from a trusted port) creates the
        // lease-tracked binding on the client's port.
        io.packet_in
            .send((
                "Ethernet49".into(),
                10,
                dhcp_frame(
                    SERVER_MAC,
                    2,
                    CLIENT_MAC,
                    "10.0.10.101".parse().unwrap(),
                    5,
                    Some(86400),
                ),
            ))
            .unwrap();
        for _ in 0..100 {
            if !engine.snapshot().bindings.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.bindings.len(), 1);
        assert_eq!(snapshot.bindings[0].mac, "00:1c:73:0c:aa:01");
        assert_eq!(snapshot.bindings[0].interface, "Ethernet1");
        assert!(snapshot.bindings[0].lease_secs.unwrap() > 86000);

        // A server message from an untrusted port drops and counts.
        io.packet_in
            .send((
                "Ethernet2".into(),
                10,
                dhcp_frame(
                    SERVER_MAC,
                    2,
                    CLIENT_MAC,
                    "10.0.10.200".parse().unwrap(),
                    5,
                    Some(60),
                ),
            ))
            .unwrap();
        for _ in 0..100 {
            if engine.snapshot().untrusted_server_drops == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.untrusted_server_drops, 1);
        assert_eq!(snapshot.dhcp_stats[&10].dropped, 1);
        // The rogue offer never became a binding.
        assert_eq!(
            snapshot.bindings[0].ip,
            "10.0.10.101".parse::<Ipv4Addr>().unwrap()
        );

        // A spoofed chaddr drops.
        io.packet_in
            .send((
                "Ethernet2".into(),
                10,
                dhcp_frame(SERVER_MAC, 1, CLIENT_MAC, Ipv4Addr::UNSPECIFIED, 3, None),
            ))
            .unwrap();
        for _ in 0..100 {
            if engine.snapshot().dhcp_stats[&10].dropped == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(engine.snapshot().dhcp_stats[&10].dropped, 2);
    }

    #[tokio::test]
    async fn dai_validates_against_bindings_and_flags() {
        let (engine, mut io) = Engine::spawn(None, SWITCH_MAC);
        let mut config = seed_config();
        config.statics.insert(
            ("00:50:56:be:ef:99".into(), 10),
            StaticBinding {
                ip: "10.0.10.50".parse().unwrap(),
                interface: "Ethernet2".into(),
            },
        );
        engine.set_config(config.clone());
        engine.set_l2_view(seed_view());
        let _ = drain_redirects(&mut io).await;

        // Learn a dynamic binding via a DHCP ACK.
        io.packet_in
            .send((
                "Ethernet1".into(),
                10,
                dhcp_frame(CLIENT_MAC, 1, CLIENT_MAC, Ipv4Addr::UNSPECIFIED, 3, None),
            ))
            .unwrap();
        io.packet_in
            .send((
                "Ethernet49".into(),
                10,
                dhcp_frame(
                    SERVER_MAC,
                    2,
                    CLIENT_MAC,
                    "10.0.10.101".parse().unwrap(),
                    5,
                    Some(3600),
                ),
            ))
            .unwrap();
        for _ in 0..100 {
            if !engine.snapshot().bindings.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // A valid ARP matching the binding forwards (re-injected to
        // the VLAN's other carriers).
        io.packet_in
            .send((
                "Ethernet1".into(),
                10,
                arp_frame(
                    CLIENT_MAC,
                    CLIENT_MAC,
                    "10.0.10.101".parse().unwrap(),
                    false,
                ),
            ))
            .unwrap();
        let (target, _) = io.packet_out.recv().await.unwrap();
        assert_ne!(target, "Ethernet1");

        // A static binding validates too.
        let static_mac = [0x00, 0x50, 0x56, 0xbe, 0xef, 0x99];
        io.packet_in
            .send((
                "Ethernet2".into(),
                10,
                arp_frame(static_mac, static_mac, "10.0.10.50".parse().unwrap(), false),
            ))
            .unwrap();
        for _ in 0..100 {
            if engine.snapshot().arp_stats[&10].forwarded == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Claiming an address with no binding: bad-binding drop.
        io.packet_in
            .send((
                "Ethernet1".into(),
                10,
                arp_frame(
                    CLIENT_MAC,
                    CLIENT_MAC,
                    "10.0.10.250".parse().unwrap(),
                    false,
                ),
            ))
            .unwrap();
        // A spoofed sender-MAC: bad src-mac drop (the default check).
        io.packet_in
            .send((
                "Ethernet1".into(),
                10,
                arp_frame(CLIENT_MAC, static_mac, "10.0.10.50".parse().unwrap(), false),
            ))
            .unwrap();
        for _ in 0..100 {
            let stats = engine.snapshot().arp_stats[&10];
            if stats.dropped == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let stats = engine.snapshot().arp_stats[&10];
        assert_eq!(stats.forwarded, 2);
        assert_eq!(stats.dropped, 2);
        assert_eq!(stats.bad_binding, 1);
        assert_eq!(stats.bad_src_mac, 1);
    }

    #[tokio::test]
    async fn bindings_persist_across_engine_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("dhcp-bindings.json");
        {
            let (engine, mut io) = Engine::spawn(Some(store.clone()), SWITCH_MAC);
            engine.set_config(seed_config());
            engine.set_l2_view(seed_view());
            let _ = drain_redirects(&mut io).await;
            io.packet_in
                .send((
                    "Ethernet1".into(),
                    10,
                    dhcp_frame(CLIENT_MAC, 1, CLIENT_MAC, Ipv4Addr::UNSPECIFIED, 3, None),
                ))
                .unwrap();
            io.packet_in
                .send((
                    "Ethernet49".into(),
                    10,
                    dhcp_frame(
                        SERVER_MAC,
                        2,
                        CLIENT_MAC,
                        "10.0.10.101".parse().unwrap(),
                        5,
                        Some(3600),
                    ),
                ))
                .unwrap();
            for _ in 0..100 {
                if !engine.snapshot().bindings.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(engine.snapshot().bindings.len(), 1);
        }
        // A fresh engine on the same store comes back with the lease.
        let (engine, _io) = Engine::spawn(Some(store), SWITCH_MAC);
        engine.set_config(seed_config());
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.bindings.len(), 1);
        assert_eq!(snapshot.bindings[0].mac, "00:1c:73:0c:aa:01");
        assert!(snapshot.bindings[0].lease_secs.unwrap() <= 3600);

        // Clearing dynamics empties the table (statics would stay).
        assert_eq!(engine.clear_bindings(None), 1);
        assert!(engine.snapshot().bindings.is_empty());
    }
}
