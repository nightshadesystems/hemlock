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
}

pub struct EngineIo {
    /// Trapped frames: (ingress port, VLAN, frame).
    pub packet_in: mpsc::UnboundedSender<(String, u16, Vec<u8>)>,
    /// Re-injections: (egress port, frame).
    pub packet_out: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    /// Redirect programs -> syncd SetSnoopRedirects.
    pub redirects: mpsc::UnboundedReceiver<RedirectProgram>,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
    packet_out: mpsc::UnboundedSender<(String, Vec<u8>)>,
    redirects_tx: mpsc::UnboundedSender<RedirectProgram>,
}

impl Engine {
    /// `store` = the binding persistence file (None in tests that don't
    /// exercise it). Existing bindings load immediately.
    pub fn spawn(store: Option<std::path::PathBuf>) -> (Engine, EngineIo) {
        let (packet_in_tx, mut packet_in_rx) = mpsc::unbounded_channel::<(String, u16, Vec<u8>)>();
        let (packet_out_tx, packet_out_rx) = mpsc::unbounded_channel();
        let (redirects_tx, redirects_rx) = mpsc::unbounded_channel();
        let dynamics = store
            .as_deref()
            .map(load_bindings)
            .unwrap_or_default();
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
        }));
        let engine = Engine {
            inner: inner.clone(),
            packet_out: packet_out_tx,
            redirects_tx,
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
        let Some(dhcp) = parse_dhcp(frame) else { return };
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if !inner.config.dhcp_vlans.contains(&vlan) {
            return;
        }
        let trusted = Self::expand_trusted(&inner.view, &inner.config.dhcp_trusted)
            .contains(port)
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const CLIENT_MAC: [u8; 6] = [0x00, 0x1c, 0x73, 0x0c, 0xaa, 0x01];
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

    #[tokio::test]
    async fn redirects_expand_port_channels_and_split_trust() {
        let (engine, mut io) = Engine::spawn(None);
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
        let (engine, mut io) = Engine::spawn(None);
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
        assert_eq!(snapshot.bindings[0].ip, "10.0.10.101".parse::<Ipv4Addr>().unwrap());

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
        let (engine, mut io) = Engine::spawn(None);
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
                arp_frame(CLIENT_MAC, CLIENT_MAC, "10.0.10.101".parse().unwrap(), false),
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
                arp_frame(CLIENT_MAC, CLIENT_MAC, "10.0.10.250".parse().unwrap(), false),
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
            let (engine, mut io) = Engine::spawn(Some(store.clone()));
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
        let (engine, _io) = Engine::spawn(Some(store));
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
