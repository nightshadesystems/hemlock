//! Protocol-frame transport over the ports' hostif netdevs (Linux).
//!
//! syncd creates one kernel netdev per front-panel port (named after
//! it) and installs the SAI hostif traps that punt protocol frames;
//! orch opens an AF_PACKET socket bound to each port netdev and
//! reads/writes whole Ethernet frames — no gRPC packet proxying.
//!
//! One socket per port carries every protocol: received frames are
//! dispatched by type (slow-protocols ethertype -> LACP, the STP
//! multicast -> STP, the LLDP ethertype -> LLDP, IGMP/MLD -> the
//! snooping engines, classified to a VLAN by the ingress port's
//! untagged VLAN since hostif delivery strips tags).
//!
//! The socket is created with `ETH_P_ALL` and bound through the
//! interface's own `getifaddrs` link address (which carries the
//! ifindex), so no raw `sockaddr_ll` construction is needed.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use tokio::sync::mpsc;
use tokio::time::Duration;
use tracing::{debug, warn};

use crate::{lacp, lldp, stp};

type Sockets = Mutex<HashMap<String, Arc<OwnedFd>>>;
type SnoopSinks = RwLock<Vec<(u16, mpsc::UnboundedSender<(String, u16, Vec<u8>)>)>>;

fn sockets() -> &'static Sockets {
    static SOCKETS: OnceLock<Sockets> = OnceLock::new();
    SOCKETS.get_or_init(Default::default)
}

fn snoop_sinks() -> &'static SnoopSinks {
    static SINKS: OnceLock<SnoopSinks> = OnceLock::new();
    SINKS.get_or_init(Default::default)
}

fn pvids() -> &'static RwLock<BTreeMap<String, u16>> {
    static PVIDS: OnceLock<RwLock<BTreeMap<String, u16>>> = OnceLock::new();
    PVIDS.get_or_init(Default::default)
}

/// The untagged-VLAN map used to classify snooped frames.
pub fn set_pvids(map: BTreeMap<String, u16>) {
    if let Ok(mut pvids) = pvids().write() {
        *pvids = map;
    }
}

/// Attach a snooping engine: frames of `ethertype` (0x0800 carrying
/// IGMP, 0x86dd carrying ICMPv6) go to `packet_in`; its `packet_out`
/// query frames transmit like any other protocol frame.
pub fn register_snoop(
    ethertype: u16,
    packet_in: mpsc::UnboundedSender<(String, u16, Vec<u8>)>,
    mut packet_out: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
) {
    if let Ok(mut sinks) = snoop_sinks().write() {
        sinks.push((ethertype, packet_in));
    }
    tokio::spawn(async move {
        while let Some((port, frame)) = packet_out.recv().await {
            transmit(&port, frame).await;
        }
    });
}

type LldpSink = RwLock<Option<mpsc::UnboundedSender<(String, Vec<u8>)>>>;

fn lldp_sink() -> &'static LldpSink {
    static SINK: OnceLock<LldpSink> = OnceLock::new();
    SINK.get_or_init(Default::default)
}

/// Attach the LLDP engine: trapped 0x88cc frames go to `packet_in`;
/// its advertisements transmit like any other protocol frame.
pub fn register_lldp(
    packet_in: mpsc::UnboundedSender<(String, Vec<u8>)>,
    packet_out: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
) {
    if let Ok(mut sink) = lldp_sink().write() {
        *sink = Some(packet_in);
    }
    spawn_tx(packet_out);
}

type SnoopsecSink = RwLock<Option<mpsc::UnboundedSender<(String, u16, Vec<u8>)>>>;

fn snoopsec_sink() -> &'static SnoopsecSink {
    static SINK: OnceLock<SnoopsecSink> = OnceLock::new();
    SINK.get_or_init(Default::default)
}

/// Attach the DHCP-snooping/DAI engine: trapped ARP frames and
/// DHCP-over-UDP frames go to `packet_in`; validated re-injections
/// transmit like any other protocol frame.
pub fn register_snoopsec(
    packet_in: mpsc::UnboundedSender<(String, u16, Vec<u8>)>,
    mut packet_out: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
) {
    if let Ok(mut sink) = snoopsec_sink().write() {
        *sink = Some(packet_in);
    }
    tokio::spawn(async move {
        while let Some((port, frame)) = packet_out.recv().await {
            transmit(&port, frame).await;
        }
    });
}

/// Is this frame ARP, or DHCP inside IPv4/UDP? (The security engine's
/// pre-filter; the CoPP arp/dhcp classes bound the rate upstream.)
fn snoopsec_relevant(ethertype: u16, frame: &[u8]) -> bool {
    match ethertype {
        0x0806 => true,
        0x0800 => {
            let Some(ip) = frame.get(14..) else {
                return false;
            };
            if ip.len() < 20 || ip[0] >> 4 != 4 || ip[9] != 17 {
                return false;
            }
            let ihl = usize::from(ip[0] & 0xf) * 4;
            let Some(dport) = ip
                .get(ihl + 2..ihl + 4)
                .map(|b| u16::from_be_bytes([b[0], b[1]]))
            else {
                return false;
            };
            matches!(dport, 67 | 68)
        }
        _ => false,
    }
}

async fn transmit(port: &str, frame: Vec<u8>) {
    let fd = sockets().lock().ok().and_then(|map| map.get(port).cloned());
    let Some(fd) = fd else { return };
    let Ok(clone) = fd.try_clone() else { return };
    let port = port.to_string();
    // A short blocking write on a packet socket.
    let result = tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::from(clone);
        file.write_all(&frame)
    })
    .await;
    if let Ok(Err(e)) = result {
        debug!(%port, error = %e, "protocol frame tx failed");
    }
}

/// Keep the ports' sockets attached: STP wants every link-up port, LACP
/// its member ports; snooping rides the same sockets.
pub async fn run(
    engine: lacp::Engine,
    stp_engine: stp::Engine,
    lldp_engine: lldp::Engine,
    pdu_in: mpsc::UnboundedSender<(String, Vec<u8>)>,
    pdu_out: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    bpdu_in: mpsc::UnboundedSender<(String, Vec<u8>)>,
    bpdu_out: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
) {
    spawn_tx(pdu_out);
    spawn_tx(bpdu_out);

    // Attach/detach loop.
    loop {
        let mut wanted: Vec<String> = engine
            .snapshot()
            .lags
            .iter()
            .flat_map(|lag| lag.members.iter().map(|m| m.port.clone()))
            .collect();
        if let Some(snapshot) = stp_engine.snapshot() {
            wanted.extend(snapshot.ports.into_iter().map(|p| p.port));
        }
        // LLDP runs on every front-panel port by default, so its port
        // list is usually the widest of the three.
        if let Some(snapshot) = lldp_engine.snapshot() {
            wanted.extend(snapshot.ports.into_iter().map(|p| p.port));
        }
        // Snooping listens wherever a PVID is known (every bridged port).
        if let Ok(map) = pvids().read() {
            wanted.extend(map.keys().cloned());
        }
        wanted.sort();
        wanted.dedup();
        {
            let current: Vec<String> = sockets()
                .lock()
                .map(|map| map.keys().cloned().collect())
                .unwrap_or_default();
            for port in &current {
                if !wanted.contains(port) {
                    if let Ok(mut map) = sockets().lock() {
                        map.remove(port);
                    }
                }
            }
            for port in &wanted {
                let attached = sockets()
                    .lock()
                    .map(|map| map.contains_key(port))
                    .unwrap_or(true);
                if attached {
                    continue;
                }
                match open_socket(port) {
                    Ok(fd) => {
                        let fd = Arc::new(fd);
                        if let Ok(mut map) = sockets().lock() {
                            map.insert(port.clone(), fd.clone());
                        }
                        spawn_reader(port.clone(), fd, pdu_in.clone(), bpdu_in.clone());
                    }
                    Err(e) => {
                        debug!(%port, error = %e, "cannot open protocol socket (netdev absent?)");
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

fn spawn_tx(mut frames: mpsc::UnboundedReceiver<(String, Vec<u8>)>) {
    tokio::spawn(async move {
        while let Some((port, frame)) = frames.recv().await {
            transmit(&port, frame).await;
        }
    });
}

/// Open an `AF_PACKET`/`ETH_P_ALL` socket bound to `port`'s netdev.
fn open_socket(port: &str) -> anyhow::Result<OwnedFd> {
    use nix::sys::socket::{bind, socket, AddressFamily, SockFlag, SockProtocol, SockType};

    let fd = socket(
        AddressFamily::Packet,
        SockType::Raw,
        SockFlag::empty(),
        SockProtocol::EthAll,
    )?;
    // The interface's own link address (from getifaddrs) carries the
    // ifindex bind() needs.
    let addrs = nix::ifaddrs::getifaddrs()?;
    let link = addrs
        .into_iter()
        .find(|ifa| ifa.interface_name == port)
        .and_then(|ifa| ifa.address)
        .and_then(|addr| addr.as_link_addr().cloned())
        .ok_or_else(|| anyhow::anyhow!("no link address for netdev {port:?}"))?;
    bind(fd.as_raw_fd(), &link)?;
    Ok(fd)
}

/// Is this frame snooping-relevant for its ethertype? (IGMP inside
/// IPv4; ICMPv6/hop-by-hop inside IPv6 — cheap pre-filter so the
/// engines never see bulk traffic.)
fn snoop_relevant(ethertype: u16, frame: &[u8]) -> bool {
    match ethertype {
        0x0800 => frame.len() > 14 + 9 && frame[14 + 9] == 2,
        0x86dd => frame.len() > 14 + 6 && matches!(frame[14 + 6], 58 | 0),
        _ => false,
    }
}

/// Blocking reader thread: frames from the netdev, dispatched by type.
fn spawn_reader(
    port: String,
    fd: Arc<OwnedFd>,
    pdu_in: mpsc::UnboundedSender<(String, Vec<u8>)>,
    bpdu_in: mpsc::UnboundedSender<(String, Vec<u8>)>,
) {
    std::thread::Builder::new()
        .name(format!("proto-rx-{port}"))
        .spawn(move || {
            let Ok(clone) = fd.try_clone() else { return };
            let mut file = std::fs::File::from(clone);
            let mut buffer = [0u8; 2048];
            loop {
                // Detached? Stop reading.
                let attached = sockets()
                    .lock()
                    .map(|map| map.contains_key(&port))
                    .unwrap_or(false);
                if !attached {
                    break;
                }
                match file.read(&mut buffer) {
                    Ok(n) if n >= 14 => {
                        let frame = &buffer[..n];
                        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
                        if ethertype == lacp::LACP_ETHERTYPE {
                            let _ = pdu_in.send((port.clone(), frame.to_vec()));
                        } else if ethertype == lldp::LLDP_ETHERTYPE {
                            if let Ok(sink) = lldp_sink().read() {
                                if let Some(sink) = sink.as_ref() {
                                    let _ = sink.send((port.clone(), frame.to_vec()));
                                }
                            }
                        } else if frame[..6] == stp::STP_DST {
                            let _ = bpdu_in.send((port.clone(), frame.to_vec()));
                        } else if snoop_relevant(ethertype, frame) {
                            let vlan = pvids()
                                .read()
                                .ok()
                                .and_then(|map| map.get(&port).copied())
                                .unwrap_or(1);
                            if let Ok(sinks) = snoop_sinks().read() {
                                for (wanted, sink) in sinks.iter() {
                                    if *wanted == ethertype {
                                        let _ = sink.send((port.clone(), vlan, frame.to_vec()));
                                    }
                                }
                            }
                        } else if snoopsec_relevant(ethertype, frame) {
                            let vlan = pvids()
                                .read()
                                .ok()
                                .and_then(|map| map.get(&port).copied())
                                .unwrap_or(1);
                            if let Ok(sink) = snoopsec_sink().read() {
                                if let Some(sink) = sink.as_ref() {
                                    let _ = sink.send((port.clone(), vlan, frame.to_vec()));
                                }
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!(%port, error = %e, "protocol socket read failed");
                        break;
                    }
                }
            }
        })
        .ok();
}
