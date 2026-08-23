//! The spanning-tree engine: one port-role/state machine shared by
//! `mstp` and `rstp` (rapid-pvst is explicitly deferred and rejected at
//! parse time).
//!
//! Same channel shape as the LACP engine: config pushes and link
//! events in, BPDUs in/out over the member hostif netdevs, and port
//! state updates + BPDU-guard errdisable events out (a pusher task
//! turns them into syncd calls).
//!
//! Deliberate simplifications, recorded here:
//! - one CIST state machine drives every MST instance's port states
//!   (the config surface has no per-instance priorities or costs, so
//!   the instances could never diverge anyway; the MST region name/
//!   revision/mapping still programs the hardware VLAN->instance
//!   tables and renders in `show spanning-tree mst configuration`);
//! - transitions are timer-based (discarding -> learning ->
//!   forwarding after forward-delay each) rather than RSTP
//!   proposal/agreement handshakes; portfast (edge) ports skip
//!   straight to forwarding.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

/// The STP multicast destination.
pub const STP_DST: [u8; 6] = [0x01, 0x80, 0xc2, 0x00, 0x00, 0x00];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Mstp,
    Rstp,
    None,
}

impl Mode {
    pub fn word(self) -> &'static str {
        match self {
            Mode::Mstp => "mstp",
            Mode::Rstp => "rstp",
            Mode::None => "none",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PortConfig {
    pub portfast: bool,
    pub bpduguard: bool,
    /// None = derived from link speed (20e9 / kbps).
    pub cost: Option<u32>,
    /// Multiple of 16; default 128.
    pub priority: u8,
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub mode: Mode,
    /// Multiple of 4096; default 32768.
    pub priority: u16,
    pub hello_time: u8,
    pub max_age: u8,
    pub forward_time: u8,
    pub mst_name: String,
    pub mst_revision: u16,
    /// MST instance -> mapped VLANs.
    pub instances: BTreeMap<u8, Vec<u16>>,
    /// Per-port config (ports without an entry run defaults).
    pub ports: BTreeMap<String, PortConfig>,
}

#[derive(Debug, Clone)]
pub struct LinkEvent {
    pub port: String,
    pub up: bool,
    /// Current speed in Mb/s (for the default path cost); 0 = unknown.
    pub speed_mbps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Designated,
    Root,
    Alternate,
    Disabled,
}

impl Role {
    pub fn word(self) -> &'static str {
        match self {
            Role::Designated => "designated",
            Role::Root => "root",
            Role::Alternate => "alternate",
            Role::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortState {
    Discarding,
    Learning,
    Forwarding,
}

impl PortState {
    pub fn word(self) -> &'static str {
        match self {
            PortState::Discarding => "discarding",
            PortState::Learning => "learning",
            PortState::Forwarding => "forwarding",
        }
    }
}

/// A port's forwarding state changed (push to syncd for every
/// instance).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateUpdate {
    pub port: String,
    pub state: PortState,
}

/// BPDU guard tripped: errdisable the port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrdisableEvent {
    pub port: String,
    pub reason: &'static str,
}

/// A bridge identifier: (priority, MAC), ordered exactly like the wire
/// encoding compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BridgeId {
    pub priority: u16,
    pub mac: [u8; 6],
}

/// The priority vector a BPDU carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PriorityVector {
    pub root: BridgeId,
    pub cost: u32,
    pub bridge: BridgeId,
    pub port_id: u16,
}

#[derive(Debug)]
struct Port {
    config: PortConfig,
    port_number: u16,
    link_up: bool,
    speed_mbps: u32,
    /// Best received info on this port, when current.
    received: Option<PriorityVector>,
    last_rx: Option<Instant>,
    role: Role,
    state: PortState,
    state_since: Instant,
    bpdus_rx: u64,
    bpdus_tx: u64,
    last_tx: Option<Instant>,
    errdisabled: bool,
}

impl Port {
    fn path_cost(&self) -> u32 {
        if let Some(cost) = self.config.cost {
            return cost;
        }
        // 802.1D-2004 long path cost: 20_000_000_000 / kbps.
        match self.speed_mbps {
            0 => 20_000_000,
            mbps => (20_000_000_000u64 / (u64::from(mbps) * 1000)).max(1) as u32,
        }
    }
}

struct Inner {
    config: Config,
    bridge_mac: [u8; 6],
    ports: BTreeMap<String, Port>,
    topology_changes: u32,
    last_tc: Option<(Instant, String)>,
    last_states: BTreeMap<String, PortState>,
}

impl Inner {
    fn bridge_id(&self) -> BridgeId {
        BridgeId {
            priority: self.config.priority,
            mac: self.bridge_mac,
        }
    }
}

/// Per-port snapshot for the gRPC surface.
#[derive(Debug, Clone)]
pub struct PortSnapshot {
    pub port: String,
    pub role: Role,
    pub state: PortState,
    pub cost: u32,
    pub priority: u8,
    pub portfast: bool,
    pub bpduguard: bool,
    pub bpdus_rx: u64,
    pub bpdus_tx: u64,
    pub errdisabled: bool,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub mode: Mode,
    pub bridge: BridgeId,
    pub root: BridgeId,
    pub root_cost: u32,
    pub root_port: Option<String>,
    pub hello_time: u8,
    pub max_age: u8,
    pub forward_time: u8,
    pub mst_name: String,
    pub mst_revision: u16,
    pub instances: BTreeMap<u8, Vec<u16>>,
    pub topology_changes: u32,
    pub seconds_since_tc: Option<u64>,
    pub last_tc_port: Option<String>,
    pub ports: Vec<PortSnapshot>,
}

pub struct EngineIo {
    pub links: mpsc::UnboundedSender<LinkEvent>,
    pub bpdu_in: mpsc::UnboundedSender<(String, Vec<u8>)>,
    pub bpdu_out: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    pub states: mpsc::UnboundedReceiver<StateUpdate>,
    pub errdisable: mpsc::UnboundedReceiver<ErrdisableEvent>,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
    wake: mpsc::UnboundedSender<()>,
}

impl Engine {
    pub fn spawn(bridge_mac: [u8; 6]) -> (Engine, EngineIo) {
        let (links_tx, links_rx) = mpsc::unbounded_channel();
        let (bpdu_in_tx, bpdu_in_rx) = mpsc::unbounded_channel();
        let (bpdu_out_tx, bpdu_out_rx) = mpsc::unbounded_channel();
        let (states_tx, states_rx) = mpsc::unbounded_channel();
        let (errdisable_tx, errdisable_rx) = mpsc::unbounded_channel();
        let (wake_tx, wake_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Mutex::new(Inner {
            config: Config {
                priority: 32768,
                hello_time: 2,
                max_age: 20,
                forward_time: 15,
                ..Config::default()
            },
            bridge_mac,
            ports: BTreeMap::new(),
            topology_changes: 0,
            last_tc: None,
            last_states: BTreeMap::new(),
        }));
        let engine = Engine {
            inner: inner.clone(),
            wake: wake_tx,
        };
        tokio::spawn(run(
            inner,
            links_rx,
            bpdu_in_rx,
            bpdu_out_tx,
            states_tx,
            errdisable_tx,
            wake_rx,
        ));
        (
            engine,
            EngineIo {
                links: links_tx,
                bpdu_in: bpdu_in_tx,
                bpdu_out: bpdu_out_rx,
                states: states_rx,
                errdisable: errdisable_rx,
            },
        )
    }

    /// Replace the configuration (declarative).
    pub fn set_config(&self, config: Config) {
        if let Ok(mut inner) = self.inner.lock() {
            for (name, port) in inner.ports.iter_mut() {
                port.config = config.ports.get(name).cloned().unwrap_or_default();
                if port.config.priority == 0 {
                    port.config.priority = 128;
                }
                // A config change lifts BPDU-guard errdisable only when
                // the guard is gone.
                if port.errdisabled && !port.config.bpduguard {
                    port.errdisabled = false;
                }
            }
            inner.config = config;
            if inner.config.priority == 0 {
                inner.config.priority = 32768;
            }
            if inner.config.hello_time == 0 {
                inner.config.hello_time = 2;
            }
            if inner.config.max_age == 0 {
                inner.config.max_age = 20;
            }
            if inner.config.forward_time == 0 {
                inner.config.forward_time = 15;
            }
        }
        let _ = self.wake.send(());
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        let inner = self.inner.lock().ok()?;
        let now = Instant::now();
        let (root, root_cost, root_port) = compute_root(&inner, now);
        Some(Snapshot {
            mode: inner.config.mode,
            bridge: inner.bridge_id(),
            root,
            root_cost,
            root_port,
            hello_time: inner.config.hello_time,
            max_age: inner.config.max_age,
            forward_time: inner.config.forward_time,
            mst_name: inner.config.mst_name.clone(),
            mst_revision: inner.config.mst_revision,
            instances: inner.config.instances.clone(),
            topology_changes: inner.topology_changes,
            seconds_since_tc: inner
                .last_tc
                .as_ref()
                .map(|(at, _)| now.saturating_duration_since(*at).as_secs()),
            last_tc_port: inner.last_tc.as_ref().map(|(_, port)| port.clone()),
            ports: inner
                .ports
                .iter()
                .filter(|(_, p)| p.link_up || p.errdisabled)
                .map(|(name, p)| PortSnapshot {
                    port: name.clone(),
                    role: p.role,
                    state: p.state,
                    cost: p.path_cost(),
                    priority: p.config.priority,
                    portfast: p.config.portfast,
                    bpduguard: p.config.bpduguard,
                    bpdus_rx: p.bpdus_rx,
                    bpdus_tx: p.bpdus_tx,
                    errdisabled: p.errdisabled,
                })
                .collect(),
        })
    }
}

/// The root bridge, our cost to it, and the root port, from current
/// received information.
fn compute_root(inner: &Inner, now: Instant) -> (BridgeId, u32, Option<String>) {
    let bridge = inner.bridge_id();
    let mut best: Option<(PriorityVector, u32, String)> = None;
    let max_age = Duration::from_secs(u64::from(inner.config.max_age));
    for (name, port) in &inner.ports {
        if !port.link_up || port.errdisabled {
            continue;
        }
        let Some(info) = port.received else { continue };
        if !matches!(port.last_rx, Some(at) if now.saturating_duration_since(at) < max_age) {
            continue;
        }
        if info.root >= bridge {
            continue; // we are at least as good a root
        }
        let cost = info.cost.saturating_add(port.path_cost());
        let candidate = (info, cost, name.clone());
        let better = match &best {
            None => true,
            Some((have, have_cost, _)) => {
                (info.root, cost, info.bridge, info.port_id)
                    < (have.root, *have_cost, have.bridge, have.port_id)
            }
        };
        if better {
            best = Some(candidate);
        }
    }
    match best {
        Some((info, cost, port)) => (info.root, cost, Some(port)),
        None => (bridge, 0, None),
    }
}

/// Encode an RST BPDU (LLC-framed).
#[allow(clippy::too_many_arguments)]
fn encode_bpdu(
    src_mac: [u8; 6],
    rstp: bool,
    root: BridgeId,
    cost: u32,
    bridge: BridgeId,
    port_id: u16,
    hello: u8,
    max_age: u8,
    forward_delay: u8,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(40);
    body.extend_from_slice(&[0x00, 0x00]); // protocol id
    body.push(if rstp { 0x02 } else { 0x00 }); // version
    body.push(if rstp { 0x02 } else { 0x00 }); // bpdu type
    body.push(0x00); // flags (simplified: none)
    body.extend_from_slice(&root.priority.to_be_bytes());
    body.extend_from_slice(&root.mac);
    body.extend_from_slice(&cost.to_be_bytes());
    body.extend_from_slice(&bridge.priority.to_be_bytes());
    body.extend_from_slice(&bridge.mac);
    body.extend_from_slice(&port_id.to_be_bytes());
    body.extend_from_slice(&0u16.to_be_bytes()); // message age
    body.extend_from_slice(&(u16::from(max_age) * 256).to_be_bytes());
    body.extend_from_slice(&(u16::from(hello) * 256).to_be_bytes());
    body.extend_from_slice(&(u16::from(forward_delay) * 256).to_be_bytes());
    if rstp {
        body.push(0x00); // version 1 length
    }

    let mut frame = Vec::with_capacity(17 + body.len());
    frame.extend_from_slice(&STP_DST);
    frame.extend_from_slice(&src_mac);
    frame.extend_from_slice(&((body.len() + 3) as u16).to_be_bytes()); // 802.3 length
    frame.extend_from_slice(&[0x42, 0x42, 0x03]); // LLC: STP SAP, UI
    frame.extend_from_slice(&body);
    frame
}

/// Decode a (config or RST) BPDU's priority vector; None when the frame
/// is not a spanning-tree BPDU.
pub fn decode_bpdu(frame: &[u8]) -> Option<PriorityVector> {
    // LLC header at 14: DSAP/SSAP 0x42, UI.
    if frame.len() < 17 + 31 || frame[14] != 0x42 || frame[15] != 0x42 {
        return None;
    }
    let body = &frame[17..];
    if body[0] != 0 || body[1] != 0 {
        return None;
    }
    let bpdu_type = body[3];
    if bpdu_type != 0x00 && bpdu_type != 0x02 {
        return None; // TCN or unknown
    }
    let be16 = |a: usize| u16::from_be_bytes([body[a], body[a + 1]]);
    let mac = |a: usize| {
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&body[a..a + 6]);
        mac
    };
    Some(PriorityVector {
        root: BridgeId {
            priority: be16(5),
            mac: mac(7),
        },
        cost: u32::from_be_bytes([body[13], body[14], body[15], body[16]]),
        bridge: BridgeId {
            priority: be16(17),
            mac: mac(19),
        },
        port_id: be16(25),
    })
}

#[allow(clippy::too_many_arguments)]
async fn run(
    inner: Arc<Mutex<Inner>>,
    mut links: mpsc::UnboundedReceiver<LinkEvent>,
    mut bpdu_in: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    bpdu_out: mpsc::UnboundedSender<(String, Vec<u8>)>,
    states: mpsc::UnboundedSender<StateUpdate>,
    errdisable: mpsc::UnboundedSender<ErrdisableEvent>,
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
                        let config = inner
                            .config
                            .ports
                            .get(&event.port)
                            .cloned()
                            .unwrap_or_default();
                        let now = Instant::now();
                        let port = inner.ports.entry(event.port.clone()).or_insert(Port {
                            config,
                            port_number: port_number_of(&event.port),
                            link_up: false,
                            speed_mbps: 0,
                            received: None,
                            last_rx: None,
                            role: Role::Disabled,
                            state: PortState::Discarding,
                            state_since: now,
                            bpdus_rx: 0,
                            bpdus_tx: 0,
                            last_tx: None,
                            errdisabled: false,
                        });
                        if port.config.priority == 0 {
                            port.config.priority = 128;
                        }
                        if event.speed_mbps > 0 {
                            port.speed_mbps = event.speed_mbps;
                        }
                        if port.link_up != event.up {
                            port.link_up = event.up;
                            port.received = None;
                            port.last_rx = None;
                            port.state = PortState::Discarding;
                            port.state_since = now;
                        }
                    }
                }
                None => break,
            },
            frame = bpdu_in.recv() => match frame {
                Some((port_name, frame)) => {
                    if let Some(info) = decode_bpdu(&frame) {
                        let mut guard_trip = false;
                        if let Ok(mut inner) = inner.lock() {
                            if let Some(port) = inner.ports.get_mut(&port_name) {
                                port.bpdus_rx += 1;
                                if port.config.bpduguard && !port.errdisabled {
                                    port.errdisabled = true;
                                    port.state = PortState::Discarding;
                                    guard_trip = true;
                                } else if !port.errdisabled {
                                    port.received = Some(info);
                                    port.last_rx = Some(Instant::now());
                                }
                            }
                        }
                        if guard_trip {
                            let _ = errdisable.send(ErrdisableEvent {
                                port: port_name,
                                reason: "bpduguard",
                            });
                        }
                    }
                }
                None => break,
            },
            _ = wake.recv() => {}
        }
        step(&inner, &bpdu_out, &states);
    }
}

/// One evaluation pass: roles, state progression, due BPDUs, state
/// pushes.
fn step(
    inner: &Arc<Mutex<Inner>>,
    bpdu_out: &mpsc::UnboundedSender<(String, Vec<u8>)>,
    states: &mpsc::UnboundedSender<StateUpdate>,
) {
    let Ok(mut inner) = inner.lock() else { return };
    let now = Instant::now();
    let inner = &mut *inner;

    if inner.config.mode == Mode::None {
        // Spanning tree off: everything with link forwards.
        for port in inner.ports.values_mut() {
            port.role = Role::Designated;
            port.state = if port.link_up && !port.errdisabled {
                PortState::Forwarding
            } else {
                PortState::Discarding
            };
        }
    } else {
        let bridge = inner.bridge_id();
        let (root, root_cost, root_port) = compute_root(inner, now);

        let forward_delay = Duration::from_secs(u64::from(inner.config.forward_time));
        let max_age = Duration::from_secs(u64::from(inner.config.max_age));
        let hello = Duration::from_secs(u64::from(inner.config.hello_time));

        let mut tc: Option<String> = None;
        for (name, port) in inner.ports.iter_mut() {
            if !port.link_up || port.errdisabled {
                port.role = Role::Disabled;
                port.state = PortState::Discarding;
                continue;
            }
            // Age out stale info.
            if let Some(at) = port.last_rx {
                if now.saturating_duration_since(at) >= max_age {
                    port.received = None;
                    port.last_rx = None;
                }
            }
            // Role.
            let role = if Some(name) == root_port.as_ref() {
                Role::Root
            } else {
                // What we would send on this port vs what we hear.
                let ours = PriorityVector {
                    root,
                    cost: root_cost,
                    bridge,
                    port_id: (u16::from(port.config.priority) << 8) | port.port_number,
                };
                match port.received {
                    Some(theirs)
                        if (theirs.root, theirs.cost, theirs.bridge)
                            < (ours.root, ours.cost, ours.bridge) =>
                    {
                        Role::Alternate
                    }
                    _ => Role::Designated,
                }
            };
            if port.role != role {
                port.role = role;
                port.state = PortState::Discarding;
                port.state_since = now;
            }
            // State progression.
            let next = match role {
                Role::Alternate | Role::Disabled => PortState::Discarding,
                Role::Root | Role::Designated if port.config.portfast => PortState::Forwarding,
                Role::Root | Role::Designated => match port.state {
                    PortState::Discarding
                        if now.saturating_duration_since(port.state_since) >= forward_delay =>
                    {
                        PortState::Learning
                    }
                    PortState::Learning
                        if now.saturating_duration_since(port.state_since) >= forward_delay =>
                    {
                        PortState::Forwarding
                    }
                    state => state,
                },
            };
            if next != port.state {
                port.state = next;
                port.state_since = now;
                // Topology change: a non-edge port reached forwarding.
                if next == PortState::Forwarding && !port.config.portfast {
                    tc = Some(name.clone());
                }
            }
            // Designated ports (and the root bridge's ports) send BPDUs.
            if role == Role::Designated {
                let due = match port.last_tx {
                    Some(at) => now.saturating_duration_since(at) >= hello,
                    None => true,
                };
                if due {
                    let frame = encode_bpdu(
                        inner.bridge_mac,
                        true,
                        root,
                        root_cost,
                        bridge,
                        (u16::from(port.config.priority) << 8) | port.port_number,
                        inner.config.hello_time,
                        inner.config.max_age,
                        inner.config.forward_time,
                    );
                    port.last_tx = Some(now);
                    port.bpdus_tx += 1;
                    let _ = bpdu_out.send((name.clone(), frame));
                }
            }
        }
        if let Some(port) = tc {
            inner.topology_changes += 1;
            inner.last_tc = Some((now, port));
        }
    }

    // Push state changes.
    for (name, port) in &inner.ports {
        let state = if port.link_up || port.errdisabled {
            port.state
        } else {
            PortState::Discarding
        };
        if inner.last_states.get(name) != Some(&state) {
            inner.last_states.insert(name.clone(), state);
            let _ = states.send(StateUpdate {
                port: name.clone(),
                state,
            });
        }
    }
}

fn port_number_of(name: &str) -> u16 {
    name.chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn config(priority: u16) -> Config {
        Config {
            mode: Mode::Mstp,
            priority,
            hello_time: 1,
            max_age: 6,
            forward_time: 1,
            ..Config::default()
        }
    }

    fn link(port: &str, up: bool) -> LinkEvent {
        LinkEvent {
            port: port.into(),
            up,
            speed_mbps: 1000,
        }
    }

    /// A full-mesh wiring harness: (bridge index, port) pairs connected
    /// pairwise; frames sent by one side arrive at the other.
    fn connect(mut io_a: EngineIo, a_map: BTreeMap<String, (usize, String)>, targets: Vec<mpsc::UnboundedSender<(String, Vec<u8>)>>) {
        tokio::spawn(async move {
            while let Some((port, frame)) = io_a.bpdu_out.recv().await {
                if let Some((peer, peer_port)) = a_map.get(&port) {
                    let _ = targets[*peer].send((peer_port.clone(), frame));
                }
            }
        });
    }

    async fn settle(secs: u64) {
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }

    /// Three bridges in a triangle converge: one root, one blocked port
    /// breaking the loop.
    #[tokio::test(flavor = "multi_thread")]
    async fn three_bridge_triangle_converges() {
        // Bridge A (lowest MAC) <-> B, A <-> C, B <-> C.
        let (a, io_a) = Engine::spawn([0x02, 0, 0, 0, 0, 0x01]);
        let (b, io_b) = Engine::spawn([0x02, 0, 0, 0, 0, 0x02]);
        let (c, io_c) = Engine::spawn([0x02, 0, 0, 0, 0, 0x03]);
        for engine in [&a, &b, &c] {
            engine.set_config(config(32768));
        }
        let ins = vec![
            io_a.bpdu_in.clone(),
            io_b.bpdu_in.clone(),
            io_c.bpdu_in.clone(),
        ];
        // Port wiring: X's Ethernet<n> connects to peer n.
        // A: Et2 -> B.Et1, Et3 -> C.Et1; B: Et1 -> A.Et2, Et3 -> C.Et2;
        // C: Et1 -> A.Et3, Et2 -> B.Et3.
        let map =
            |pairs: &[(&str, usize, &str)]| -> BTreeMap<String, (usize, String)> {
                pairs
                    .iter()
                    .map(|(port, peer, peer_port)| {
                        ((*port).to_string(), (*peer, (*peer_port).to_string()))
                    })
                    .collect()
            };
        let links_a = io_a.links.clone();
        let links_b = io_b.links.clone();
        let links_c = io_c.links.clone();
        connect(io_a, map(&[("Ethernet2", 1, "Ethernet1"), ("Ethernet3", 2, "Ethernet1")]), ins.clone());
        connect(io_b, map(&[("Ethernet1", 0, "Ethernet2"), ("Ethernet3", 2, "Ethernet2")]), ins.clone());
        connect(io_c, map(&[("Ethernet1", 0, "Ethernet3"), ("Ethernet2", 1, "Ethernet3")]), ins.clone());

        for port in ["Ethernet2", "Ethernet3"] {
            links_a.send(link(port, true)).unwrap();
        }
        for port in ["Ethernet1", "Ethernet3"] {
            links_b.send(link(port, true)).unwrap();
        }
        for port in ["Ethernet1", "Ethernet2"] {
            links_c.send(link(port, true)).unwrap();
        }
        // Convergence: 2x forward-delay (1s each) plus propagation.
        settle(5).await;

        let snap_a = a.snapshot().unwrap();
        let snap_b = b.snapshot().unwrap();
        let snap_c = c.snapshot().unwrap();

        // A (lowest bridge id) is the root; all its ports designated
        // and forwarding.
        assert_eq!(snap_a.root, snap_a.bridge, "A is the root");
        assert!(snap_a
            .ports
            .iter()
            .all(|p| p.role == Role::Designated && p.state == PortState::Forwarding));
        assert!(snap_a.topology_changes > 0);

        // B and C both see A as root through their direct link.
        for snap in [&snap_b, &snap_c] {
            assert_eq!(snap.root, snap_a.bridge);
            assert_eq!(snap.root_port.as_deref(), Some("Ethernet1"));
            assert_eq!(snap.root_cost, 20_000, "one 1G hop");
        }
        // The B<->C link carries exactly one blocked (alternate) end.
        let b3 = snap_b.ports.iter().find(|p| p.port == "Ethernet3").unwrap();
        let c2 = snap_c.ports.iter().find(|p| p.port == "Ethernet2").unwrap();
        let blocked = [b3, c2]
            .iter()
            .filter(|p| p.role == Role::Alternate && p.state == PortState::Discarding)
            .count();
        assert_eq!(blocked, 1, "the loop is broken exactly once");
    }

    /// BPDU guard errdisables a portfast edge that hears a bridge.
    #[tokio::test(flavor = "multi_thread")]
    async fn bpdu_guard_errdisables() {
        let (a, mut io) = Engine::spawn([0x02, 0, 0, 0, 0, 0x10]);
        let mut cfg = config(32768);
        cfg.ports.insert(
            "Ethernet1".into(),
            PortConfig {
                portfast: true,
                bpduguard: true,
                cost: None,
                priority: 128,
            },
        );
        a.set_config(cfg);
        io.links.send(link("Ethernet1", true)).unwrap();
        settle(1).await;
        // The edge port forwards immediately.
        let snap = a.snapshot().unwrap();
        assert_eq!(snap.ports[0].state, PortState::Forwarding);

        // A BPDU arrives: guard trips, port errdisables.
        let rogue = encode_bpdu(
            [0x02, 0, 0, 0, 0, 0x99],
            true,
            BridgeId {
                priority: 0,
                mac: [0x02, 0, 0, 0, 0, 0x99],
            },
            0,
            BridgeId {
                priority: 0,
                mac: [0x02, 0, 0, 0, 0, 0x99],
            },
            0x8001,
            2,
            20,
            15,
        );
        io.bpdu_in.send(("Ethernet1".into(), rogue)).unwrap();
        settle(1).await;
        let event = io.errdisable.try_recv().unwrap();
        assert_eq!(event.port, "Ethernet1");
        assert_eq!(event.reason, "bpduguard");
        let snap = a.snapshot().unwrap();
        assert!(snap.ports[0].errdisabled);
        assert_eq!(snap.ports[0].state, PortState::Discarding);
    }

    #[test]
    fn bpdu_round_trips() {
        let root = BridgeId {
            priority: 4096,
            mac: [1, 2, 3, 4, 5, 6],
        };
        let bridge = BridgeId {
            priority: 32768,
            mac: [6, 5, 4, 3, 2, 1],
        };
        let frame = encode_bpdu([9; 6], true, root, 20000, bridge, 0x8001, 2, 20, 15);
        let decoded = decode_bpdu(&frame).unwrap();
        assert_eq!(decoded.root, root);
        assert_eq!(decoded.cost, 20000);
        assert_eq!(decoded.bridge, bridge);
        assert_eq!(decoded.port_id, 0x8001);
        assert!(decode_bpdu(&frame[..20]).is_none());
        assert!(decode_bpdu(&[0u8; 60]).is_none());
    }

    #[test]
    fn bridge_ids_order_like_the_wire() {
        let low = BridgeId {
            priority: 4096,
            mac: [0xff; 6],
        };
        let high = BridgeId {
            priority: 32768,
            mac: [0x00; 6],
        };
        assert!(low < high, "priority dominates the MAC");
    }
}
