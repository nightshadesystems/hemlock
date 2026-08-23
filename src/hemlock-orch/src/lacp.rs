//! The LACP engine: per-member actor/partner state machines, static
//! (`mode on`) aggregation, fallback, and min-links gating.
//!
//! Pure state machine over channels — no sockets, no gRPC:
//!
//! - inputs: config pushes ([`Engine::set_configs`]), member link
//!   events, received LACPDUs;
//! - outputs: LACPDUs to transmit and [`GateUpdate`]s (the full wanted
//!   membership of one LAG, each member with its collect/distribute
//!   gate) that a pusher task turns into syncd `SetLagMembers` calls.
//!
//! Frames ride the member ports' hostif netdevs: syncd owns the SAI
//! hostif traps; orch opens raw sockets on those netdevs directly
//! rather than proxying packets over gRPC (fewer copies, no stream
//! plumbing, and the netdevs already exist per port). The same channel
//! pair lets tests wire two engines back-to-back as each other's
//! partners.
//!
//! Simplifications vs IEEE 802.1AX, chosen deliberately:
//! - `fallback individual` gates the lowest-priority member open and
//!   reports it `individual` (a full implementation would detach it
//!   from the LAG in hardware);
//! - the mux machine is collapsed to selected -> synced -> bundled
//!   (no separate ATTACH/COLLECT states).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

/// Slow-protocols multicast destination (802.3 annex 57A).
pub const LACP_DST: [u8; 6] = [0x01, 0x80, 0xc2, 0x00, 0x00, 0x02];
/// Slow-protocols ethertype.
pub const LACP_ETHERTYPE: u16 = 0x8809;

/// IEEE 802.1AX actor/partner state bits.
pub mod state {
    pub const ACTIVITY: u8 = 0x01;
    pub const TIMEOUT: u8 = 0x02;
    pub const AGGREGATION: u8 = 0x04;
    pub const SYNC: u8 = 0x08;
    pub const COLLECTING: u8 = 0x10;
    pub const DISTRIBUTING: u8 = 0x20;
    pub const DEFAULTED: u8 = 0x40;
    pub const EXPIRED: u8 = 0x80;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Active,
    Passive,
    /// Static aggregation, no LACP.
    On,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    Static,
    Individual,
}

#[derive(Debug, Clone)]
pub struct MemberConfig {
    pub mode: Mode,
    pub rate_fast: bool,
    /// Default 32768.
    pub port_priority: u16,
}

#[derive(Debug, Clone)]
pub struct LagConfig {
    pub group: u16,
    pub min_links: u8,
    pub fallback: Option<Fallback>,
    pub fallback_timeout: Duration,
    /// Member port name -> config.
    pub members: BTreeMap<String, MemberConfig>,
}

/// A member's link state changed.
#[derive(Debug, Clone)]
pub struct LinkEvent {
    pub port: String,
    pub up: bool,
}

/// The full wanted membership of one LAG (None = LAG deconfigured, its
/// members released).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateUpdate {
    pub group: u16,
    /// (port, collect/distribute gate), sorted by port name.
    pub members: Vec<(String, bool)>,
}

/// What the partner last told us (their actor TLV).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partner {
    pub system_priority: u16,
    pub system: [u8; 6],
    pub key: u16,
    pub port_priority: u16,
    pub port: u16,
    pub state: u8,
}

#[derive(Debug)]
struct Member {
    config: MemberConfig,
    /// From the port name's digits (Ethernet49 -> 49).
    port_number: u16,
    link_up: bool,
    partner: Option<Partner>,
    last_rx: Option<Instant>,
    last_tx: Option<Instant>,
    /// Gate currently wanted (collecting/distributing).
    bundled: bool,
    /// Forwarding as an individual port under fallback.
    individual: bool,
    pdus_rx: u64,
    pdus_tx: u64,
    churn: u32,
}

impl Member {
    fn new(config: MemberConfig, port_number: u16) -> Self {
        Self {
            config,
            port_number,
            link_up: false,
            partner: None,
            last_rx: None,
            last_tx: None,
            bundled: false,
            individual: false,
            pdus_rx: 0,
            pdus_tx: 0,
            churn: 0,
        }
    }

    /// Partner information still current? (3 x our requested interval.)
    fn partner_current(&self, now: Instant) -> bool {
        let timeout = Duration::from_secs(if self.config.rate_fast { 3 } else { 90 });
        matches!(self.last_rx, Some(at) if now.saturating_duration_since(at) < timeout)
    }
}

#[derive(Debug)]
struct Lag {
    config: LagConfig,
    members: BTreeMap<String, Member>,
    /// When this LAG last heard any partner (fallback arms from here,
    /// or from config time when nothing was ever heard).
    partnerless_since: Instant,
    fallback_active: bool,
    last_gates: Option<Vec<(String, bool)>>,
}

struct Inner {
    system_priority: u16,
    system_mac: [u8; 6],
    lags: BTreeMap<u16, Lag>,
    /// Port name -> owning group.
    port_to_lag: BTreeMap<String, u16>,
    /// Link states remembered across reconfigs (a port may join a LAG
    /// after its link came up).
    link_states: BTreeMap<String, bool>,
}

/// Runtime state of one member, for the gRPC snapshot.
#[derive(Debug, Clone)]
pub struct MemberSnapshot {
    pub port: String,
    pub status: &'static str,
    pub rate_fast: bool,
    pub actor_state: u8,
    pub partner_state: u8,
    pub partner_system: Option<(u16, [u8; 6])>,
    pub partner_port: u16,
    pub partner_key: u16,
    pub partner_priority: u16,
    pub pdus_rx: u64,
    pub pdus_tx: u64,
    pub churn: u32,
}

#[derive(Debug, Clone)]
pub struct LagSnapshot {
    pub group: u16,
    pub lacp: bool,
    pub active_mode: bool,
    pub bundled: u32,
    pub total: u32,
    pub up: bool,
    pub min_links: u8,
    pub fallback: Option<Fallback>,
    pub fallback_timeout: Duration,
    pub fallback_active: bool,
    pub members: Vec<MemberSnapshot>,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub system_priority: u16,
    pub system_mac: [u8; 6],
    pub lags: Vec<LagSnapshot>,
}

/// The engine's channel endpoints for the transport and gate pusher.
pub struct EngineIo {
    /// Feed member link transitions here.
    pub links: mpsc::UnboundedSender<LinkEvent>,
    /// Feed received frames here: (ingress port, whole Ethernet frame).
    pub pdu_in: mpsc::UnboundedSender<(String, Vec<u8>)>,
    /// Frames to transmit: (egress port, whole Ethernet frame).
    pub pdu_out: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    /// Wanted membership/gates per LAG, emitted on change.
    pub gates: mpsc::UnboundedReceiver<GateUpdate>,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
    wake: mpsc::UnboundedSender<()>,
}

impl Engine {
    /// Spawn the engine task. `system_mac` seeds the actor system id.
    pub fn spawn(system_mac: [u8; 6]) -> (Engine, EngineIo) {
        let (links_tx, links_rx) = mpsc::unbounded_channel();
        let (pdu_in_tx, pdu_in_rx) = mpsc::unbounded_channel();
        let (pdu_out_tx, pdu_out_rx) = mpsc::unbounded_channel();
        let (gates_tx, gates_rx) = mpsc::unbounded_channel();
        let (wake_tx, wake_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Mutex::new(Inner {
            system_priority: 32768,
            system_mac,
            lags: BTreeMap::new(),
            port_to_lag: BTreeMap::new(),
            link_states: BTreeMap::new(),
        }));
        let engine = Engine {
            inner: inner.clone(),
            wake: wake_tx,
        };
        tokio::spawn(run(inner, links_rx, pdu_in_rx, pdu_out_tx, gates_tx, wake_rx));
        (
            engine,
            EngineIo {
                links: links_tx,
                pdu_in: pdu_in_tx,
                pdu_out: pdu_out_rx,
                gates: gates_rx,
            },
        )
    }

    /// Replace the full desired LAG/LACP state (declarative). Groups
    /// absent from `configs` are torn down.
    pub fn set_configs(&self, system_priority: u16, configs: Vec<LagConfig>) {
        {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            let now = Instant::now();
            inner.system_priority = system_priority;
            let link_states = inner.link_states.clone();
            let wanted: std::collections::BTreeSet<u16> =
                configs.iter().map(|c| c.group).collect();
            inner.lags.retain(|group, _| wanted.contains(group));
            for config in configs {
                let group = config.group;
                match inner.lags.get_mut(&group) {
                    Some(lag) => {
                        // Keep runtime state of members that stay.
                        lag.members
                            .retain(|name, _| config.members.contains_key(name));
                        for (name, member_config) in &config.members {
                            match lag.members.get_mut(name) {
                                Some(member) => member.config = member_config.clone(),
                                None => {
                                    let mut member =
                                        Member::new(member_config.clone(), port_number_of(name));
                                    member.link_up =
                                        link_states.get(name).copied().unwrap_or(false);
                                    lag.members.insert(name.clone(), member);
                                }
                            }
                        }
                        lag.config = config;
                    }
                    None => {
                        let mut members = BTreeMap::new();
                        for (name, member_config) in &config.members {
                            let mut member =
                                Member::new(member_config.clone(), port_number_of(name));
                            member.link_up =
                                link_states.get(name).copied().unwrap_or(false);
                            members.insert(name.clone(), member);
                        }
                        inner.lags.insert(
                            group,
                            Lag {
                                config,
                                members,
                                partnerless_since: now,
                                fallback_active: false,
                                last_gates: None,
                            },
                        );
                    }
                }
            }
            inner.port_to_lag = inner
                .lags
                .iter()
                .flat_map(|(group, lag)| {
                    lag.members.keys().map(move |name| (name.clone(), *group))
                })
                .collect();
        }
        let _ = self.wake.send(());
    }

    /// Runtime state for the gRPC surface.
    pub fn snapshot(&self) -> Snapshot {
        let Ok(inner) = self.inner.lock() else {
            return Snapshot {
                system_priority: 32768,
                system_mac: [0; 6],
                lags: Vec::new(),
            };
        };
        let now = Instant::now();
        Snapshot {
            system_priority: inner.system_priority,
            system_mac: inner.system_mac,
            lags: inner
                .lags
                .values()
                .map(|lag| {
                    let lacp = lag
                        .config
                        .members
                        .values()
                        .next()
                        .map(|m| m.mode != Mode::On)
                        .unwrap_or(true);
                    let active_mode = lag
                        .config
                        .members
                        .values()
                        .next()
                        .map(|m| m.mode == Mode::Active)
                        .unwrap_or(false);
                    let bundled =
                        lag.members.values().filter(|m| m.bundled && !m.individual).count() as u32;
                    LagSnapshot {
                        group: lag.config.group,
                        lacp,
                        active_mode,
                        bundled,
                        total: lag.members.len() as u32,
                        up: bundled > 0
                            || lag.members.values().any(|m| m.bundled && m.individual),
                        min_links: lag.config.min_links,
                        fallback: lag.config.fallback,
                        fallback_timeout: lag.config.fallback_timeout,
                        fallback_active: lag.fallback_active,
                        members: lag
                            .members
                            .iter()
                            .map(|(name, member)| {
                                let current = member.partner_current(now);
                                MemberSnapshot {
                                    port: name.clone(),
                                    status: if member.individual {
                                        "individual"
                                    } else if member.bundled {
                                        "bundled"
                                    } else if member.link_up {
                                        "standby"
                                    } else {
                                        "down"
                                    },
                                    rate_fast: member.config.rate_fast,
                                    actor_state: actor_state_bits(member, current),
                                    partner_state: member
                                        .partner
                                        .map(|p| p.state)
                                        .unwrap_or(0),
                                    partner_system: member
                                        .partner
                                        .filter(|_| current)
                                        .map(|p| (p.system_priority, p.system)),
                                    partner_port: member.partner.map(|p| p.port).unwrap_or(0),
                                    partner_key: member.partner.map(|p| p.key).unwrap_or(0),
                                    partner_priority: member
                                        .partner
                                        .map(|p| p.port_priority)
                                        .unwrap_or(0),
                                    pdus_rx: member.pdus_rx,
                                    pdus_tx: member.pdus_tx,
                                    churn: member.churn,
                                }
                            })
                            .collect(),
                    }
                })
                .collect(),
        }
    }
}

/// The port number encoded in a display name (`Ethernet49` -> 49).
fn port_number_of(name: &str) -> u16 {
    name.chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

/// A member's current actor state octet.
fn actor_state_bits(member: &Member, partner_current: bool) -> u8 {
    let mut bits = state::AGGREGATION;
    if member.config.mode == Mode::Active {
        bits |= state::ACTIVITY;
    }
    if member.config.rate_fast {
        bits |= state::TIMEOUT;
    }
    if member.link_up && (partner_current || member.config.mode == Mode::On) {
        bits |= state::SYNC;
    }
    if member.bundled {
        bits |= state::COLLECTING | state::DISTRIBUTING;
    }
    if !partner_current {
        bits |= state::DEFAULTED;
        // A partner we once heard and lost reads expired, not merely
        // defaulted.
        if member.partner.is_some() {
            bits |= state::EXPIRED;
        }
    }
    bits
}

/// Encode one LACPDU (whole Ethernet frame, 110-byte LACPDU body).
#[allow(clippy::too_many_arguments)]
fn encode_pdu(
    src_mac: [u8; 6],
    system_priority: u16,
    system_mac: [u8; 6],
    key: u16,
    port_priority: u16,
    port: u16,
    actor_state: u8,
    partner: Option<&Partner>,
) -> Vec<u8> {
    let mut frame = Vec::with_capacity(124);
    frame.extend_from_slice(&LACP_DST);
    frame.extend_from_slice(&src_mac);
    frame.extend_from_slice(&LACP_ETHERTYPE.to_be_bytes());
    frame.push(0x01); // subtype: LACP
    frame.push(0x01); // version
    // Actor TLV.
    frame.push(0x01);
    frame.push(20);
    frame.extend_from_slice(&system_priority.to_be_bytes());
    frame.extend_from_slice(&system_mac);
    frame.extend_from_slice(&key.to_be_bytes());
    frame.extend_from_slice(&port_priority.to_be_bytes());
    frame.extend_from_slice(&port.to_be_bytes());
    frame.push(actor_state);
    frame.extend_from_slice(&[0; 3]);
    // Partner TLV (what we believe about them; zeros when defaulted).
    frame.push(0x02);
    frame.push(20);
    match partner {
        Some(p) => {
            frame.extend_from_slice(&p.system_priority.to_be_bytes());
            frame.extend_from_slice(&p.system);
            frame.extend_from_slice(&p.key.to_be_bytes());
            frame.extend_from_slice(&p.port_priority.to_be_bytes());
            frame.extend_from_slice(&p.port.to_be_bytes());
            frame.push(p.state);
            frame.extend_from_slice(&[0; 3]);
        }
        None => frame.extend_from_slice(&[0; 18]),
    }
    // Collector TLV + terminator + reserved padding to the standard
    // 110-byte LACPDU.
    frame.push(0x03);
    frame.push(16);
    frame.extend_from_slice(&[0; 14]);
    frame.push(0x00);
    frame.push(0x00);
    frame.resize(14 + 110, 0);
    frame
}

/// Decode the sender's actor TLV out of an Ethernet frame; None when it
/// is not an LACPDU.
pub fn decode_pdu(frame: &[u8]) -> Option<Partner> {
    if frame.len() < 14 + 2 + 20 {
        return None;
    }
    if frame[12..14] != LACP_ETHERTYPE.to_be_bytes() || frame[14] != 0x01 {
        return None;
    }
    // Actor TLV at fixed offset 16.
    let tlv = &frame[16..];
    if tlv[0] != 0x01 || tlv[1] != 20 {
        return None;
    }
    let be16 = |a: usize| u16::from_be_bytes([tlv[a], tlv[a + 1]]);
    let mut system = [0u8; 6];
    system.copy_from_slice(&tlv[4..10]);
    Some(Partner {
        system_priority: be16(2),
        system,
        key: be16(10),
        port_priority: be16(12),
        port: be16(14),
        state: tlv[16],
    })
}

async fn run(
    inner: Arc<Mutex<Inner>>,
    mut links: mpsc::UnboundedReceiver<LinkEvent>,
    mut pdu_in: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    pdu_out: mpsc::UnboundedSender<(String, Vec<u8>)>,
    gates: mpsc::UnboundedSender<GateUpdate>,
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
                        inner.link_states.insert(event.port.clone(), event.up);
                        if let Some(group) = inner.port_to_lag.get(&event.port).copied() {
                            if let Some(lag) = inner.lags.get_mut(&group) {
                                if let Some(member) = lag.members.get_mut(&event.port) {
                                    member.link_up = event.up;
                                    if !event.up {
                                        member.partner = None;
                                        member.last_rx = None;
                                    }
                                }
                            }
                        }
                    }
                }
                None => break,
            },
            frame = pdu_in.recv() => match frame {
                Some((port, frame)) => {
                    if let Some(partner) = decode_pdu(&frame) {
                        if let Ok(mut inner) = inner.lock() {
                            if let Some(group) = inner.port_to_lag.get(&port).copied() {
                                if let Some(member) = inner
                                    .lags
                                    .get_mut(&group)
                                    .and_then(|lag| lag.members.get_mut(&port))
                                {
                                    member.partner = Some(partner);
                                    member.last_rx = Some(Instant::now());
                                    member.pdus_rx += 1;
                                }
                            }
                        }
                    }
                }
                None => break,
            },
            _ = wake.recv() => {}
        }
        step(&inner, &pdu_out, &gates);
    }
}

/// One evaluation pass: selection + gates + due transmissions.
fn step(
    inner: &Arc<Mutex<Inner>>,
    pdu_out: &mpsc::UnboundedSender<(String, Vec<u8>)>,
    gates: &mpsc::UnboundedSender<GateUpdate>,
) {
    let Ok(mut inner) = inner.lock() else { return };
    let now = Instant::now();
    let system_priority = inner.system_priority;
    let system_mac = inner.system_mac;

    for lag in inner.lags.values_mut() {
        let group = lag.config.group;
        let lacp = lag
            .config
            .members
            .values()
            .next()
            .map(|m| m.mode != Mode::On)
            .unwrap_or(true);

        // Fallback arming: the clock restarts whenever any partner is
        // heard.
        let any_partner = lag.members.values().any(|m| m.partner_current(now));
        if any_partner {
            lag.partnerless_since = now;
        }
        let fallback_armed = lacp
            && lag.config.fallback.is_some()
            && !any_partner
            && now.saturating_duration_since(lag.partnerless_since) >= lag.config.fallback_timeout;
        lag.fallback_active = fallback_armed;

        // Selection: which members want their gate open?
        let mut wanted: BTreeMap<String, (bool, bool)> = BTreeMap::new(); // (gate, individual)
        if !lacp {
            for (name, member) in &lag.members {
                wanted.insert(name.clone(), (member.link_up, false));
            }
        } else if fallback_armed {
            match lag.config.fallback {
                Some(Fallback::Static) => {
                    for (name, member) in &lag.members {
                        wanted.insert(name.clone(), (member.link_up, false));
                    }
                }
                Some(Fallback::Individual) => {
                    // The lowest (port-priority, port-number) live member
                    // forwards as an individual port.
                    let chosen = lag
                        .members
                        .iter()
                        .filter(|(_, m)| m.link_up)
                        .min_by_key(|(_, m)| (m.config.port_priority, m.port_number))
                        .map(|(name, _)| name.clone());
                    for name in lag.members.keys() {
                        let individual = Some(name) == chosen.as_ref();
                        wanted.insert(name.clone(), (individual, individual));
                    }
                }
                None => {}
            }
        } else {
            for (name, member) in &lag.members {
                let gate = member.link_up
                    && member.partner_current(now)
                    && member
                        .partner
                        .map(|p| p.state & state::SYNC != 0)
                        .unwrap_or(false);
                wanted.insert(name.clone(), (gate, false));
            }
        }

        // min-links applies to the aggregate (individual fallback is
        // exempt: the port forwards standalone).
        let bundled = wanted
            .iter()
            .filter(|(_, (gate, individual))| *gate && !individual)
            .count();
        if lag.config.min_links > 0 && bundled < usize::from(lag.config.min_links) {
            for (gate, individual) in wanted.values_mut() {
                if !*individual {
                    *gate = false;
                }
            }
        }

        // Apply + churn accounting.
        for (name, member) in lag.members.iter_mut() {
            let (gate, individual) = wanted.get(name).copied().unwrap_or((false, false));
            if member.bundled != gate {
                member.bundled = gate;
                member.churn += 1;
            }
            member.individual = individual;
        }

        // Gates out (full declarative membership), on change only.
        let gate_list: Vec<(String, bool)> = lag
            .members
            .iter()
            .map(|(name, member)| (name.clone(), member.bundled))
            .collect();
        if lag.last_gates.as_ref() != Some(&gate_list) {
            lag.last_gates = Some(gate_list.clone());
            let _ = gates.send(GateUpdate {
                group,
                members: gate_list,
            });
        }

        // Due transmissions. We transmit at the rate the partner asks
        // for (their TIMEOUT bit); defaulted members use their own.
        if lacp {
            for (name, member) in lag.members.iter_mut() {
                if !member.link_up {
                    continue;
                }
                let current = member.partner_current(now);
                let active = member.config.mode == Mode::Active;
                if !active && !current {
                    continue; // passive with nobody talking
                }
                let fast = match member.partner.filter(|_| current) {
                    Some(partner) => partner.state & state::TIMEOUT != 0,
                    None => member.config.rate_fast,
                };
                let interval = Duration::from_secs(if fast { 1 } else { 30 });
                let due = match member.last_tx {
                    Some(at) => now.saturating_duration_since(at) >= interval,
                    None => true,
                };
                if !due {
                    continue;
                }
                let actor_state = actor_state_bits(member, current);
                let frame = encode_pdu(
                    system_mac,
                    system_priority,
                    system_mac,
                    group,
                    member.config.port_priority,
                    member.port_number,
                    actor_state,
                    member.partner.as_ref().filter(|_| current),
                );
                member.last_tx = Some(now);
                member.pdus_tx += 1;
                let _ = pdu_out.send((name.clone(), frame));
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn member(mode: Mode, rate_fast: bool) -> MemberConfig {
        MemberConfig {
            mode,
            rate_fast,
            port_priority: 32768,
        }
    }

    fn lag(group: u16, mode: Mode, ports: &[&str]) -> LagConfig {
        LagConfig {
            group,
            min_links: 0,
            fallback: None,
            fallback_timeout: Duration::from_secs(90),
            members: ports
                .iter()
                .map(|p| ((*p).to_string(), member(mode, true)))
                .collect(),
        }
    }

    /// Wire two engines back-to-back: every frame engine A emits on a
    /// port arrives at engine B on the same port name, and vice versa.
    fn cross_connect(mut a: EngineIo, mut b: EngineIo) -> (
        mpsc::UnboundedSender<LinkEvent>,
        mpsc::UnboundedSender<LinkEvent>,
        mpsc::UnboundedReceiver<GateUpdate>,
        mpsc::UnboundedReceiver<GateUpdate>,
    ) {
        let a_pdu_in = a.pdu_in.clone();
        let b_pdu_in = b.pdu_in.clone();
        tokio::spawn(async move {
            while let Some(frame) = a.pdu_out.recv().await {
                let _ = b_pdu_in.send(frame);
            }
        });
        tokio::spawn(async move {
            while let Some(frame) = b.pdu_out.recv().await {
                let _ = a_pdu_in.send(frame);
            }
        });
        (a.links, b.links, a.gates, b.gates)
    }

    async fn settle() {
        // Real timers (the engine ticks at 500ms); a couple of seconds
        // covers fast-rate convergence.
        tokio::time::sleep(Duration::from_millis(2500)).await;
    }

    /// Drain a gate channel, returning the last update per group.
    fn latest_gates(rx: &mut mpsc::UnboundedReceiver<GateUpdate>) -> BTreeMap<u16, GateUpdate> {
        let mut out = BTreeMap::new();
        while let Ok(update) = rx.try_recv() {
            out.insert(update.group, update);
        }
        out
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_engines_bundle_and_expire() {
        let (a, a_io) = Engine::spawn([0x02, 0, 0, 0, 0, 0xaa]);
        let (b, b_io) = Engine::spawn([0x02, 0, 0, 0, 0, 0xbb]);
        let (a_links, b_links, mut a_gates, _b_gates) = cross_connect(a_io, b_io);

        a.set_configs(32768, vec![lag(1, Mode::Active, &["Ethernet49", "Ethernet50"])]);
        b.set_configs(32768, vec![lag(1, Mode::Active, &["Ethernet49", "Ethernet50"])]);
        for links in [&a_links, &b_links] {
            for port in ["Ethernet49", "Ethernet50"] {
                links
                    .send(LinkEvent {
                        port: port.into(),
                        up: true,
                    })
                    .unwrap();
            }
        }
        settle().await;

        let snapshot = a.snapshot();
        let lag_state = &snapshot.lags[0];
        assert_eq!(lag_state.bundled, 2, "both members bundle");
        assert!(lag_state.up);
        let m = &lag_state.members[0];
        assert_eq!(m.status, "bundled");
        assert_eq!(m.partner_system, Some((32768, [0x02, 0, 0, 0, 0, 0xbb])));
        assert!(m.actor_state & state::COLLECTING != 0);
        assert!(m.pdus_rx > 0 && m.pdus_tx > 0);

        let gates = latest_gates(&mut a_gates);
        assert_eq!(
            gates[&1].members,
            vec![("Ethernet49".to_string(), true), ("Ethernet50".to_string(), true)]
        );

        // Partner loses a link: that member unbundles on both sides.
        b_links
            .send(LinkEvent {
                port: "Ethernet50".into(),
                up: false,
            })
            .unwrap();
        // Expiry horizon for fast rate is 3s.
        tokio::time::sleep(Duration::from_secs(4)).await;
        let snapshot = a.snapshot();
        let m50 = snapshot.lags[0]
            .members
            .iter()
            .find(|m| m.port == "Ethernet50")
            .unwrap();
        assert_eq!(m50.status, "standby", "partner gone, link still up");
        assert_eq!(snapshot.lags[0].bundled, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn static_mode_bundles_without_a_partner() {
        let (a, mut io) = Engine::spawn([0x02, 0, 0, 0, 0, 0x01]);
        a.set_configs(32768, vec![lag(2, Mode::On, &["Ethernet1", "Ethernet2"])]);
        io.links
            .send(LinkEvent {
                port: "Ethernet1".into(),
                up: true,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let snapshot = a.snapshot();
        assert!(!snapshot.lags[0].lacp);
        assert_eq!(snapshot.lags[0].bundled, 1, "only the live member");
        // Static mode never transmits LACPDUs.
        assert!(io.pdu_out.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn min_links_holds_the_lag_down() {
        let (a, a_io) = Engine::spawn([0x02, 0, 0, 0, 0, 0xaa]);
        let (b, b_io) = Engine::spawn([0x02, 0, 0, 0, 0, 0xbb]);
        let (a_links, b_links, _a_gates, _b_gates) = cross_connect(a_io, b_io);
        let mut config = lag(1, Mode::Active, &["Ethernet49", "Ethernet50"]);
        config.min_links = 2;
        a.set_configs(32768, vec![config.clone()]);
        b.set_configs(32768, vec![lag(1, Mode::Active, &["Ethernet49", "Ethernet50"])]);
        // Only one link is up: below min-links, nothing forwards.
        for links in [&a_links, &b_links] {
            links
                .send(LinkEvent {
                    port: "Ethernet49".into(),
                    up: true,
                })
                .unwrap();
        }
        settle().await;
        let snapshot = a.snapshot();
        assert_eq!(snapshot.lags[0].bundled, 0);
        assert!(!snapshot.lags[0].up);
        // The second link satisfies the minimum.
        for links in [&a_links, &b_links] {
            links
                .send(LinkEvent {
                    port: "Ethernet50".into(),
                    up: true,
                })
                .unwrap();
        }
        settle().await;
        assert_eq!(a.snapshot().lags[0].bundled, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fallback_individual_after_timeout() {
        let (a, io) = Engine::spawn([0x02, 0, 0, 0, 0, 0xaa]);
        let mut config = lag(1, Mode::Active, &["Ethernet49", "Ethernet50"]);
        config.fallback = Some(Fallback::Individual);
        config.fallback_timeout = Duration::from_secs(1);
        a.set_configs(32768, vec![config]);
        for port in ["Ethernet49", "Ethernet50"] {
            io.links
                .send(LinkEvent {
                    port: port.into(),
                    up: true,
                })
                .unwrap();
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        let snapshot = a.snapshot();
        let lag_state = &snapshot.lags[0];
        assert!(lag_state.fallback_active);
        assert!(lag_state.up, "individual member forwards");
        let m49 = lag_state.members.iter().find(|m| m.port == "Ethernet49").unwrap();
        let m50 = lag_state.members.iter().find(|m| m.port == "Ethernet50").unwrap();
        assert_eq!(m49.status, "individual", "lowest port number wins");
        assert_eq!(m50.status, "standby");
    }

    #[test]
    fn pdu_round_trips() {
        let partner = Partner {
            system_priority: 100,
            system: [1, 2, 3, 4, 5, 6],
            key: 7,
            port_priority: 200,
            port: 49,
            state: state::ACTIVITY | state::SYNC,
        };
        let frame = encode_pdu(
            [9; 6],
            32768,
            [0xaa; 6],
            1,
            32768,
            49,
            state::ACTIVITY | state::AGGREGATION,
            Some(&partner),
        );
        assert_eq!(frame.len(), 124);
        let decoded = decode_pdu(&frame).unwrap();
        assert_eq!(decoded.system, [0xaa; 6]);
        assert_eq!(decoded.key, 1);
        assert_eq!(decoded.port, 49);
        assert_eq!(decoded.state, state::ACTIVITY | state::AGGREGATION);
        assert!(decode_pdu(&frame[..20]).is_none());
        assert!(decode_pdu(&[0u8; 40]).is_none());
    }
}
