//! The LLDP engine: 802.1AB advertisement transmission on every
//! enabled front-panel port, a TTL-aged neighbor table built from
//! received frames, and per-port frame counters.
//!
//! Native, like the other orch engines — there is no `lldpd` in the
//! image. Same channel shape as LACP/STP: config and link events in,
//! frames in/out over the ports' hostif netdevs (syncd owns the SAI
//! trap that punts them), state served to `show lldp ...`.
//!
//! LLDP is on by default: `services { lldp { disable; } }` is the
//! global off switch and `interfaces { <port> { lldp disable; } }` the
//! per-port one, matching the IGMP-snooping convention. A port only
//! transmits while its link is up; a disabled port sheds the neighbors
//! it had learned.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

/// The LLDP multicast destination (nearest-bridge group).
pub const LLDP_DST: [u8; 6] = [0x01, 0x80, 0xc2, 0x00, 0x00, 0x0e];

/// EtherType 0x88cc.
pub const LLDP_ETHERTYPE: u16 = 0x88cc;

/// The default advertisement interval, in seconds.
pub const DEFAULT_TX_INTERVAL: u32 = 30;

/// The default TTL multiplier (TTL = interval x multiplier).
pub const DEFAULT_HOLD_MULTIPLIER: u8 = 4;

#[derive(Debug, Clone)]
pub struct Config {
    /// The global off switch (LLDP runs by default).
    pub disabled: bool,
    pub tx_interval: Duration,
    pub hold_multiplier: u8,
    /// Ports carrying `lldp disable`.
    pub disabled_ports: BTreeSet<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            disabled: false,
            tx_interval: Duration::from_secs(u64::from(DEFAULT_TX_INTERVAL)),
            hold_multiplier: DEFAULT_HOLD_MULTIPLIER,
            disabled_ports: BTreeSet::new(),
        }
    }
}

/// Local identity advertised in every frame, refreshed from syncd's
/// interface view plus the OS hostname.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct System {
    /// The switch base MAC (chassis id, subtype 4).
    pub chassis_mac: [u8; 6],
    pub name: String,
    pub description: String,
    /// Management1's address, advertised in the management-address
    /// TLV; empty = the TLV is omitted.
    pub management_address: String,
}

/// One local front-panel port LLDP may run on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortInfo {
    pub mac: [u8; 6],
    /// The configured `description`, advertised as the port
    /// description TLV.
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct LinkEvent {
    pub port: String,
    pub up: bool,
}

/// One neighbor as it was last heard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Neighbor {
    pub chassis_id: String,
    pub chassis_id_subtype: String,
    pub port_id: String,
    pub port_id_subtype: String,
    pub port_description: String,
    pub system_name: String,
    pub system_description: String,
    pub management_address: String,
    pub ttl: u16,
}

#[derive(Debug, Clone)]
struct Learned {
    neighbor: Neighbor,
    last_seen: Instant,
}

#[derive(Debug, Default)]
struct PortState {
    info: PortInfo,
    up: bool,
    frames_tx: u64,
    frames_rx: u64,
    frames_discarded: u64,
    ageouts: u64,
    last_tx: Option<Instant>,
    /// Keyed by (chassis id, port id) — the MSAP identifier.
    neighbors: BTreeMap<(String, String), Learned>,
}

struct Inner {
    config: Config,
    system: System,
    ports: BTreeMap<String, PortState>,
}

impl Inner {
    /// The advertised TTL, clamped to what the TLV can carry.
    fn ttl(&self) -> u16 {
        let secs =
            self.config.tx_interval.as_secs() * u64::from(self.config.hold_multiplier.max(1));
        u16::try_from(secs).unwrap_or(u16::MAX)
    }

    fn port_enabled(&self, port: &str) -> bool {
        !self.config.disabled && !self.config.disabled_ports.contains(port)
    }
}

#[derive(Debug, Clone)]
pub struct NeighborSnapshot {
    pub neighbor: Neighbor,
    /// Seconds since the last frame from this neighbor.
    pub age_secs: u64,
}

#[derive(Debug, Clone)]
pub struct PortSnapshot {
    pub port: String,
    pub enabled: bool,
    pub frames_tx: u64,
    pub frames_rx: u64,
    pub frames_discarded: u64,
    pub ageouts: u64,
    pub neighbors: Vec<NeighborSnapshot>,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub enabled: bool,
    pub tx_interval_secs: u32,
    pub hold_multiplier: u8,
    pub chassis_id: String,
    pub system_name: String,
    pub system_description: String,
    pub management_address: String,
    pub ports: Vec<PortSnapshot>,
}

pub struct EngineIo {
    pub links: mpsc::UnboundedSender<LinkEvent>,
    /// Trapped LLDP frames: (ingress port, whole Ethernet frame).
    pub frame_in: mpsc::UnboundedSender<(String, Vec<u8>)>,
    /// Advertisements to transmit: (egress port, frame).
    pub frame_out: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
    wake: mpsc::UnboundedSender<()>,
}

impl Engine {
    pub fn spawn(system_mac: [u8; 6]) -> (Engine, EngineIo) {
        let (links_tx, links_rx) = mpsc::unbounded_channel();
        let (frame_in_tx, frame_in_rx) = mpsc::unbounded_channel();
        let (frame_out_tx, frame_out_rx) = mpsc::unbounded_channel();
        let (wake_tx, wake_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Mutex::new(Inner {
            config: Config::default(),
            system: System {
                chassis_mac: system_mac,
                ..System::default()
            },
            ports: BTreeMap::new(),
        }));
        let engine = Engine {
            inner: inner.clone(),
            wake: wake_tx,
        };
        tokio::spawn(run(inner, links_rx, frame_in_rx, frame_out_tx, wake_rx));
        (
            engine,
            EngineIo {
                links: links_tx,
                frame_in: frame_in_tx,
                frame_out: frame_out_rx,
            },
        )
    }

    /// Replace the configuration (declarative).
    pub fn set_config(&self, config: Config) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.config = config;
            if inner.config.tx_interval.is_zero() {
                inner.config.tx_interval = Duration::from_secs(u64::from(DEFAULT_TX_INTERVAL));
            }
            if inner.config.hold_multiplier == 0 {
                inner.config.hold_multiplier = DEFAULT_HOLD_MULTIPLIER;
            }
        }
        let _ = self.wake.send(());
    }

    /// Refresh the advertised identity (hostname, version, management
    /// address, chassis MAC).
    pub fn set_system(&self, system: System) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.system = system;
        }
    }

    /// The front-panel ports LLDP runs on, from syncd's interface view.
    /// Ports that disappear take their learned neighbors with them.
    pub fn set_ports(&self, ports: BTreeMap<String, PortInfo>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.ports.retain(|name, _| ports.contains_key(name));
        for (name, info) in ports {
            let state = inner.ports.entry(name).or_default();
            state.info = info;
        }
    }

    /// Zero the frame counters, optionally scoped to one port. Returns
    /// how many ports were cleared.
    pub fn clear_counters(&self, port: &str) -> u32 {
        let Ok(mut inner) = self.inner.lock() else {
            return 0;
        };
        let mut cleared = 0;
        for (name, state) in inner.ports.iter_mut() {
            if !port.is_empty() && name != port {
                continue;
            }
            state.frames_tx = 0;
            state.frames_rx = 0;
            state.frames_discarded = 0;
            state.ageouts = 0;
            cleared += 1;
        }
        cleared
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        let inner = self.inner.lock().ok()?;
        let now = Instant::now();
        let ports = inner
            .ports
            .iter()
            .map(|(name, state)| PortSnapshot {
                port: name.clone(),
                enabled: inner.port_enabled(name),
                frames_tx: state.frames_tx,
                frames_rx: state.frames_rx,
                frames_discarded: state.frames_discarded,
                ageouts: state.ageouts,
                neighbors: state
                    .neighbors
                    .values()
                    .map(|learned| NeighborSnapshot {
                        neighbor: learned.neighbor.clone(),
                        age_secs: now.saturating_duration_since(learned.last_seen).as_secs(),
                    })
                    .collect(),
            })
            .collect();
        Some(Snapshot {
            enabled: !inner.config.disabled,
            tx_interval_secs: u32::try_from(inner.config.tx_interval.as_secs()).unwrap_or(u32::MAX),
            hold_multiplier: inner.config.hold_multiplier,
            chassis_id: mac_text(&inner.system.chassis_mac),
            system_name: inner.system.name.clone(),
            system_description: inner.system.description.clone(),
            management_address: inner.system.management_address.clone(),
            ports,
        })
    }
}

async fn run(
    inner: Arc<Mutex<Inner>>,
    mut links: mpsc::UnboundedReceiver<LinkEvent>,
    mut frame_in: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    frame_out: mpsc::UnboundedSender<(String, Vec<u8>)>,
    mut wake: mpsc::UnboundedReceiver<()>,
) {
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            event = links.recv() => match event {
                Some(event) => {
                    if let Ok(mut inner) = inner.lock() {
                        if let Some(state) = inner.ports.get_mut(&event.port) {
                            if state.up != event.up {
                                state.up = event.up;
                                // A flapped link re-advertises at once
                                // rather than waiting out the interval.
                                state.last_tx = None;
                                if !event.up {
                                    state.neighbors.clear();
                                }
                            }
                        }
                    }
                }
                None => break,
            },
            frame = frame_in.recv() => match frame {
                Some((port, frame)) => receive(&inner, &port, &frame),
                None => break,
            },
            _ = wake.recv() => {}
        }
        step(&inner, &frame_out);
    }
}

/// One received frame: parsed into a neighbor, or counted as a discard.
fn receive(inner: &Arc<Mutex<Inner>>, port: &str, frame: &[u8]) {
    let Ok(mut inner) = inner.lock() else { return };
    if !inner.port_enabled(port) {
        return;
    }
    let Some(state) = inner.ports.get_mut(port) else {
        return;
    };
    match decode(frame) {
        Some(neighbor) => {
            state.frames_rx += 1;
            let key = (neighbor.chassis_id.clone(), neighbor.port_id.clone());
            // TTL 0 is a shutdown PDU: the neighbor withdraws itself.
            if neighbor.ttl == 0 {
                state.neighbors.remove(&key);
                return;
            }
            state.neighbors.insert(
                key,
                Learned {
                    neighbor,
                    last_seen: Instant::now(),
                },
            );
        }
        None => {
            state.frames_rx += 1;
            state.frames_discarded += 1;
        }
    }
}

/// One evaluation pass: due advertisements out, expired neighbors gone.
fn step(inner: &Arc<Mutex<Inner>>, frame_out: &mpsc::UnboundedSender<(String, Vec<u8>)>) {
    let Ok(mut inner) = inner.lock() else { return };
    let now = Instant::now();
    let ttl = inner.ttl();
    let interval = inner.config.tx_interval;
    let system = inner.system.clone();
    let enabled: BTreeMap<String, bool> = inner
        .ports
        .keys()
        .map(|port| (port.clone(), inner.port_enabled(port)))
        .collect();

    for (port, state) in inner.ports.iter_mut() {
        let enabled = enabled.get(port).copied().unwrap_or(false);
        if !enabled {
            // A disabled port keeps its counters but sheds what it
            // learned — `show lldp` must not list stale neighbors.
            state.neighbors.clear();
            state.last_tx = None;
            continue;
        }
        // Age out neighbors whose advertised TTL has run out.
        let expired: Vec<(String, String)> = state
            .neighbors
            .iter()
            .filter(|(_, learned)| {
                now.saturating_duration_since(learned.last_seen).as_secs()
                    >= u64::from(learned.neighbor.ttl)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            state.neighbors.remove(&key);
            state.ageouts += 1;
        }

        if !state.up {
            state.last_tx = None;
            continue;
        }
        let due = match state.last_tx {
            Some(at) => now.saturating_duration_since(at) >= interval,
            None => true,
        };
        if !due {
            continue;
        }
        state.last_tx = Some(now);
        state.frames_tx += 1;
        let _ = frame_out.send((port.clone(), encode(&system, port, &state.info, ttl)));
    }
}

// ------------------------------------------------------------- framing

fn mac_text(mac: &[u8]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Append one TLV: 7-bit type, 9-bit length, then the payload.
fn tlv(out: &mut Vec<u8>, kind: u8, payload: &[u8]) {
    let len = u16::try_from(payload.len()).unwrap_or(0) & 0x1ff;
    let header = (u16::from(kind) << 9) | len;
    out.extend_from_slice(&header.to_be_bytes());
    out.extend_from_slice(&payload[..usize::from(len)]);
}

/// Build one advertisement for `port`: the three mandatory TLVs plus
/// the port/system description, system name and management address.
pub fn encode(system: &System, port: &str, info: &PortInfo, ttl: u16) -> Vec<u8> {
    let mut frame = Vec::with_capacity(256);
    frame.extend_from_slice(&LLDP_DST);
    frame.extend_from_slice(&info.mac);
    frame.extend_from_slice(&LLDP_ETHERTYPE.to_be_bytes());

    // Chassis ID, subtype 4 (MAC address).
    let mut chassis = vec![4u8];
    chassis.extend_from_slice(&system.chassis_mac);
    tlv(&mut frame, 1, &chassis);

    // Port ID, subtype 5 (interface name).
    let mut port_id = vec![5u8];
    port_id.extend_from_slice(port.as_bytes());
    tlv(&mut frame, 2, &port_id);

    tlv(&mut frame, 3, &ttl.to_be_bytes());

    if !info.description.is_empty() {
        tlv(&mut frame, 4, info.description.as_bytes());
    }
    if !system.name.is_empty() {
        tlv(&mut frame, 5, system.name.as_bytes());
    }
    if !system.description.is_empty() {
        tlv(&mut frame, 6, system.description.as_bytes());
    }
    if let Ok(address) = system.management_address.parse::<std::net::Ipv4Addr>() {
        // address string length (subtype + 4 bytes), IPv4 subtype, the
        // address, interface numbering subtype 1 (unknown), interface
        // number, and an empty OID.
        let mut payload = vec![5u8, 1u8];
        payload.extend_from_slice(&address.octets());
        payload.push(1);
        payload.extend_from_slice(&0u32.to_be_bytes());
        payload.push(0);
        tlv(&mut frame, 8, &payload);
    }
    tlv(&mut frame, 0, &[]);
    // Pad to the 60-byte minimum the wire wants (the FCS follows).
    if frame.len() < 60 {
        frame.resize(60, 0);
    }
    frame
}

fn chassis_subtype(subtype: u8) -> &'static str {
    match subtype {
        1 => "chassis-component",
        2 => "interface-alias",
        3 => "port-component",
        4 => "mac",
        5 => "network-address",
        6 => "interface-name",
        7 => "local",
        _ => "unknown",
    }
}

fn port_subtype(subtype: u8) -> &'static str {
    match subtype {
        1 => "interface-alias",
        2 => "port-component",
        3 => "mac",
        4 => "network-address",
        5 => "interface-name",
        6 => "agent-circuit-id",
        7 => "local",
        _ => "unknown",
    }
}

/// An identifier TLV body: a MAC subtype renders as a MAC, everything
/// else as (lossy) text.
fn identifier(subtype: u8, mac_subtype: u8, body: &[u8]) -> String {
    if subtype == mac_subtype && body.len() == 6 {
        mac_text(body)
    } else {
        String::from_utf8_lossy(body).trim_end_matches('\0').into()
    }
}

/// Parse a received frame into a neighbor. None = not a well-formed
/// LLDPDU (the caller counts it as discarded).
pub fn decode(frame: &[u8]) -> Option<Neighbor> {
    let ethertype = u16::from_be_bytes([*frame.get(12)?, *frame.get(13)?]);
    if ethertype != LLDP_ETHERTYPE {
        return None;
    }
    let mut rest = frame.get(14..)?;
    let mut neighbor = Neighbor {
        chassis_id: String::new(),
        chassis_id_subtype: String::new(),
        port_id: String::new(),
        port_id_subtype: String::new(),
        port_description: String::new(),
        system_name: String::new(),
        system_description: String::new(),
        management_address: String::new(),
        ttl: 0,
    };
    let mut seen_ttl = false;
    loop {
        if rest.len() < 2 {
            break;
        }
        let header = u16::from_be_bytes([rest[0], rest[1]]);
        let kind = u8::try_from(header >> 9).ok()?;
        let len = usize::from(header & 0x1ff);
        let body = rest.get(2..2 + len)?;
        rest = &rest[2 + len..];
        match kind {
            0 => break,
            1 => {
                let subtype = *body.first()?;
                neighbor.chassis_id_subtype = chassis_subtype(subtype).into();
                neighbor.chassis_id = identifier(subtype, 4, &body[1..]);
            }
            2 => {
                let subtype = *body.first()?;
                neighbor.port_id_subtype = port_subtype(subtype).into();
                neighbor.port_id = identifier(subtype, 3, &body[1..]);
            }
            3 => {
                neighbor.ttl = u16::from_be_bytes([*body.first()?, *body.get(1)?]);
                seen_ttl = true;
            }
            4 => neighbor.port_description = String::from_utf8_lossy(body).into(),
            5 => neighbor.system_name = String::from_utf8_lossy(body).into(),
            6 => neighbor.system_description = String::from_utf8_lossy(body).into(),
            8 => {
                // address string length, subtype, then the address.
                let length = body.first().copied().unwrap_or(0);
                if length == 5 && body.get(1) == Some(&1) {
                    if let Some(octets) = body.get(2..6) {
                        neighbor.management_address =
                            std::net::Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3])
                                .to_string();
                    }
                }
            }
            _ => {}
        }
    }
    // The three mandatory TLVs decide whether this was an LLDPDU at all.
    if neighbor.chassis_id.is_empty() || neighbor.port_id.is_empty() || !seen_ttl {
        return None;
    }
    Some(neighbor)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn system() -> System {
        System {
            chassis_mac: [0x2c, 0xdd, 0xe9, 0x4a, 0x1b, 0x00],
            name: "hemlock".into(),
            description: "Hemlock NOS version 0.1.0".into(),
            management_address: "10.42.0.9".into(),
        }
    }

    fn port_info() -> PortInfo {
        PortInfo {
            mac: [0x2c, 0xdd, 0xe9, 0x4a, 0x1b, 0x31],
            description: "uplink to core-1".into(),
        }
    }

    #[test]
    fn encode_decode_round_trip() {
        let frame = encode(&system(), "Ethernet49", &port_info(), 120);
        assert_eq!(frame[..6], LLDP_DST);
        assert_eq!(frame[6..12], port_info().mac);
        assert_eq!(u16::from_be_bytes([frame[12], frame[13]]), LLDP_ETHERTYPE);
        let neighbor = decode(&frame).unwrap();
        assert_eq!(neighbor.chassis_id, "2c:dd:e9:4a:1b:00");
        assert_eq!(neighbor.chassis_id_subtype, "mac");
        assert_eq!(neighbor.port_id, "Ethernet49");
        assert_eq!(neighbor.port_id_subtype, "interface-name");
        assert_eq!(neighbor.port_description, "uplink to core-1");
        assert_eq!(neighbor.system_name, "hemlock");
        assert_eq!(neighbor.system_description, "Hemlock NOS version 0.1.0");
        assert_eq!(neighbor.management_address, "10.42.0.9");
        assert_eq!(neighbor.ttl, 120);
    }

    /// The optional TLVs really are optional: an advertisement with
    /// only the mandatory three still decodes.
    #[test]
    fn minimal_advertisement_decodes() {
        let system = System {
            chassis_mac: [0x02, 0, 0, 0, 0, 1],
            ..System::default()
        };
        let frame = encode(&system, "Ethernet1", &PortInfo::default(), 120);
        let neighbor = decode(&frame).unwrap();
        assert_eq!(neighbor.chassis_id, "02:00:00:00:00:01");
        assert!(neighbor.system_name.is_empty());
        assert!(neighbor.management_address.is_empty());
    }

    #[test]
    fn junk_and_foreign_frames_do_not_decode() {
        // Wrong ethertype.
        let mut frame = encode(&system(), "Ethernet1", &port_info(), 120);
        frame[12] = 0x08;
        frame[13] = 0x00;
        assert!(decode(&frame).is_none());
        // Truncated mid-TLV.
        let frame = encode(&system(), "Ethernet1", &port_info(), 120);
        assert!(decode(&frame[..17]).is_none());
        // A frame with no TLVs at all.
        assert!(decode(&[0u8; 60]).is_none());
    }

    fn engine_with_ports() -> Engine {
        let (engine, _io) = Engine::spawn([0x2c, 0xdd, 0xe9, 0x4a, 0x1b, 0x00]);
        engine.set_system(system());
        engine.set_ports(BTreeMap::from([
            ("Ethernet1".to_string(), port_info()),
            ("Ethernet2".to_string(), port_info()),
        ]));
        engine
    }

    #[tokio::test]
    async fn transmits_on_up_ports_and_learns_neighbors() {
        let (engine, mut io) = Engine::spawn([0x2c, 0xdd, 0xe9, 0x4a, 0x1b, 0x00]);
        engine.set_system(system());
        engine.set_ports(BTreeMap::from([("Ethernet1".to_string(), port_info())]));
        io.links
            .send(LinkEvent {
                port: "Ethernet1".into(),
                up: true,
            })
            .unwrap();
        let (port, frame) = io.frame_out.recv().await.unwrap();
        assert_eq!(port, "Ethernet1");

        // Feed the switch its own advertisement back: a neighbor with
        // our own identity, which is exactly the loopback test case.
        io.frame_in.send(("Ethernet1".into(), frame)).unwrap();
        // Let the engine drain the channel.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let snapshot = engine.snapshot().unwrap();
        let port = &snapshot.ports[0];
        assert_eq!(port.frames_tx, 1);
        assert_eq!(port.frames_rx, 1);
        assert_eq!(port.frames_discarded, 0);
        assert_eq!(port.neighbors.len(), 1);
        assert_eq!(port.neighbors[0].neighbor.system_name, "hemlock");
    }

    #[tokio::test]
    async fn a_disabled_port_neither_transmits_nor_learns() {
        let (engine, mut io) = Engine::spawn([0x2c, 0xdd, 0xe9, 0x4a, 0x1b, 0x00]);
        engine.set_system(system());
        engine.set_ports(BTreeMap::from([("Ethernet3".to_string(), port_info())]));
        engine.set_config(Config {
            disabled_ports: BTreeSet::from(["Ethernet3".to_string()]),
            ..Config::default()
        });
        io.links
            .send(LinkEvent {
                port: "Ethernet3".into(),
                up: true,
            })
            .unwrap();
        let frame = encode(&system(), "Ethernet12", &port_info(), 120);
        io.frame_in.send(("Ethernet3".into(), frame)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(io.frame_out.try_recv().is_err());
        let snapshot = engine.snapshot().unwrap();
        assert!(!snapshot.ports[0].enabled);
        assert_eq!(snapshot.ports[0].frames_tx, 0);
        assert!(snapshot.ports[0].neighbors.is_empty());
    }

    /// A neighbor whose TTL runs out disappears and bumps the ageout
    /// counter; a shutdown PDU (TTL 0) withdraws it immediately.
    #[tokio::test]
    async fn neighbors_age_out_and_withdraw() {
        let (engine, io) = Engine::spawn([0x2c, 0xdd, 0xe9, 0x4a, 0x1b, 0x00]);
        engine.set_ports(BTreeMap::from([("Ethernet1".to_string(), port_info())]));
        let neighbor = System {
            chassis_mac: [0x2c, 0xdd, 0xe9, 0x77, 0x00, 0x0c],
            name: "core-sw-01".into(),
            ..System::default()
        };
        io.frame_in
            .send((
                "Ethernet1".into(),
                encode(&neighbor, "Ethernet12", &port_info(), 1),
            ))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(engine.snapshot().unwrap().ports[0].neighbors.len(), 1);

        tokio::time::sleep(Duration::from_millis(1100)).await;
        let snapshot = engine.snapshot().unwrap();
        assert!(snapshot.ports[0].neighbors.is_empty());
        assert_eq!(snapshot.ports[0].ageouts, 1);

        // Re-learn, then withdraw with a TTL-0 shutdown PDU.
        io.frame_in
            .send((
                "Ethernet1".into(),
                encode(&neighbor, "Ethernet12", &port_info(), 120),
            ))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(engine.snapshot().unwrap().ports[0].neighbors.len(), 1);
        io.frame_in
            .send((
                "Ethernet1".into(),
                encode(&neighbor, "Ethernet12", &port_info(), 0),
            ))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let snapshot = engine.snapshot().unwrap();
        assert!(snapshot.ports[0].neighbors.is_empty());
        // A withdrawal is not an ageout.
        assert_eq!(snapshot.ports[0].ageouts, 1);
    }

    #[tokio::test]
    async fn ttl_is_the_interval_times_the_multiplier() {
        let engine = engine_with_ports();
        engine.set_config(Config {
            tx_interval: Duration::from_secs(45),
            hold_multiplier: 3,
            ..Config::default()
        });
        let inner = engine.inner.lock().unwrap();
        assert_eq!(inner.ttl(), 135);
    }

    #[tokio::test]
    async fn clearing_counters_is_scoped() {
        let engine = engine_with_ports();
        if let Ok(mut inner) = engine.inner.lock() {
            for state in inner.ports.values_mut() {
                state.frames_tx = 10;
                state.ageouts = 2;
            }
        }
        assert_eq!(engine.clear_counters("Ethernet1"), 1);
        let snapshot = engine.snapshot().unwrap();
        assert_eq!(snapshot.ports[0].frames_tx, 0);
        assert_eq!(snapshot.ports[1].frames_tx, 10);
        assert_eq!(engine.clear_counters(""), 2);
        assert!(engine
            .snapshot()
            .unwrap()
            .ports
            .iter()
            .all(|p| p.frames_tx == 0 && p.ageouts == 0));
    }
}
