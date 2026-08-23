//! The IGMP/MLD snooping engine: per-VLAN group->port membership with
//! timers, querier election (plus an optional local querier),
//! fast-leave, and mrouter tracking (dynamic from received queries,
//! static from config).
//!
//! One generic engine, instantiated per family (IGMP over IPv4, MLD
//! over ICMPv6). Same channel shape as the other engines: config and
//! packets in, group/mrouter updates and query frames out — a pusher
//! task turns the updates into syncd L2MC calls.
//!
//! VLAN classification: hostif punt delivery strips VLAN tags, so the
//! transport resolves a frame's VLAN from the ingress port's untagged
//! VLAN (access/native) and hands `(port, vlan, frame)` to the engine.
//! Unknown-multicast restriction (to the mrouter set) is emitted only
//! for VLANs that actually have an mrouter or a local querier — a
//! quiet network keeps flooding rather than blackholing.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Igmp,
    Mld,
}

#[derive(Debug, Clone, Default)]
pub struct VlanConfig {
    pub disabled: bool,
    pub fast_leave: bool,
    pub querier: bool,
    pub querier_address: Option<IpAddr>,
    pub static_mrouters: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub disabled: bool,
    /// Default 2.
    pub robustness: u8,
    pub vlans: BTreeMap<u16, VlanConfig>,
    /// Default 125s; shortened in tests.
    pub query_interval: Duration,
    /// Grace after a leave before the member is dropped (unless
    /// fast-leave drops it immediately). Default 2s.
    pub last_member_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            disabled: false,
            robustness: 2,
            vlans: BTreeMap::new(),
            query_interval: Duration::from_secs(125),
            last_member_interval: Duration::from_secs(2),
        }
    }
}

/// A parsed snooping-relevant message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A membership report (join) for `group`, at protocol `version`.
    Report { group: IpAddr, version: u8 },
    /// A leave / done for `group`.
    Leave { group: IpAddr },
    /// A membership query from `source` (a router or another querier).
    Query { source: IpAddr },
}

/// One (vlan, group)'s wanted output ports (members plus mrouters);
/// `remove` = the group aged out entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupUpdate {
    pub vlan: u16,
    pub group: IpAddr,
    pub ports: Vec<String>,
    pub remove: bool,
}

/// A VLAN's unknown-multicast scope: restricted to `ports` (the
/// mrouter set) or back to flood-all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MrouterUpdate {
    pub vlan: u16,
    pub restrict: bool,
    pub ports: Vec<String>,
}

#[derive(Debug, Clone)]
struct Member {
    last_report: Instant,
    version: u8,
    /// A leave was heard; the member drops when this expires.
    pending_leave: Option<Instant>,
}

#[derive(Debug, Default)]
struct VlanState {
    groups: BTreeMap<IpAddr, BTreeMap<String, Member>>,
    /// Dynamic mrouters (ports where queries were heard).
    dyn_mrouters: BTreeMap<String, Instant>,
    /// Another querier with a lower address is active.
    other_querier: Option<(IpAddr, Instant)>,
    last_query_tx: Option<Instant>,
    /// What was last pushed, for change detection.
    pushed_groups: BTreeMap<IpAddr, Vec<String>>,
    pushed_mrouters: Option<(bool, Vec<String>)>,
}

struct Inner {
    family: Family,
    config: Config,
    /// VLAN -> its member ports (for query transmission), fed from
    /// syncd's interface view.
    vlan_ports: BTreeMap<u16, Vec<String>>,
    vlans: BTreeMap<u16, VlanState>,
    system_mac: [u8; 6],
}

/// Per-VLAN snapshot for the gRPC surface.
#[derive(Debug, Clone)]
pub struct VlanSnapshot {
    pub vlan: u16,
    pub enabled: bool,
    pub fast_leave: bool,
    pub querier_enabled: bool,
    pub querier_address: Option<IpAddr>,
    /// We are actively the querier (enabled and not suppressed).
    pub querier_active: bool,
    pub static_mrouters: Vec<String>,
    pub dynamic_mrouters: Vec<String>,
    pub groups: Vec<GroupSnapshot>,
}

#[derive(Debug, Clone)]
pub struct GroupSnapshot {
    pub group: IpAddr,
    pub version: u8,
    pub ports: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub enabled: bool,
    pub robustness: u8,
    pub vlans: Vec<VlanSnapshot>,
}

pub struct EngineIo {
    /// Feed classified frames here: (ingress port, VLAN, frame).
    pub packet_in: mpsc::UnboundedSender<(String, u16, Vec<u8>)>,
    /// Query frames to transmit: (egress port, frame).
    pub packet_out: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    pub groups: mpsc::UnboundedReceiver<GroupUpdate>,
    pub mrouters: mpsc::UnboundedReceiver<MrouterUpdate>,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
    wake: mpsc::UnboundedSender<()>,
}

impl Engine {
    pub fn spawn(family: Family, system_mac: [u8; 6]) -> (Engine, EngineIo) {
        let (packet_in_tx, packet_in_rx) = mpsc::unbounded_channel();
        let (packet_out_tx, packet_out_rx) = mpsc::unbounded_channel();
        let (groups_tx, groups_rx) = mpsc::unbounded_channel();
        let (mrouters_tx, mrouters_rx) = mpsc::unbounded_channel();
        let (wake_tx, wake_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Mutex::new(Inner {
            family,
            config: Config::default(),
            vlan_ports: BTreeMap::new(),
            vlans: BTreeMap::new(),
            system_mac,
        }));
        let engine = Engine {
            inner: inner.clone(),
            wake: wake_tx,
        };
        tokio::spawn(run(
            inner,
            packet_in_rx,
            packet_out_tx,
            groups_tx,
            mrouters_tx,
            wake_rx,
        ));
        (
            engine,
            EngineIo {
                packet_in: packet_in_tx,
                packet_out: packet_out_rx,
                groups: groups_rx,
                mrouters: mrouters_rx,
            },
        )
    }

    /// Replace the configuration (declarative).
    pub fn set_config(&self, config: Config) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.config = config;
            if inner.config.robustness == 0 {
                inner.config.robustness = 2;
            }
        }
        let _ = self.wake.send(());
    }

    /// The VLAN membership map (for query transmission), from syncd's
    /// interface view.
    pub fn set_vlan_ports(&self, map: BTreeMap<u16, Vec<String>>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.vlan_ports = map;
        }
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        let inner = self.inner.lock().ok()?;
        let now = Instant::now();
        let mut vlans = Vec::new();
        // Configured VLANs always render; VLANs with learned state too.
        let mut ids: Vec<u16> = inner.config.vlans.keys().copied().collect();
        ids.extend(inner.vlans.keys().copied());
        ids.sort_unstable();
        ids.dedup();
        for vlan in ids {
            let config = inner.config.vlans.get(&vlan).cloned().unwrap_or_default();
            let state = inner.vlans.get(&vlan);
            let enabled = !inner.config.disabled && !config.disabled;
            let other_querier = state
                .and_then(|s| s.other_querier)
                .is_some_and(|(_, at)| {
                    now.saturating_duration_since(at) < inner.querier_hold()
                });
            vlans.push(VlanSnapshot {
                vlan,
                enabled,
                fast_leave: config.fast_leave,
                querier_enabled: config.querier,
                querier_address: config.querier_address,
                querier_active: enabled && config.querier && !other_querier,
                static_mrouters: config.static_mrouters.clone(),
                dynamic_mrouters: state
                    .map(|s| s.dyn_mrouters.keys().cloned().collect())
                    .unwrap_or_default(),
                groups: state
                    .map(|s| {
                        s.groups
                            .iter()
                            .map(|(group, members)| GroupSnapshot {
                                group: *group,
                                version: members.values().map(|m| m.version).max().unwrap_or(2),
                                ports: members.keys().cloned().collect(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            });
        }
        Some(Snapshot {
            enabled: !inner.config.disabled,
            robustness: inner.config.robustness,
            vlans,
        })
    }
}

impl Inner {
    /// How long group membership lasts without a refresh.
    fn membership_hold(&self) -> Duration {
        self.config.query_interval * u32::from(self.config.robustness)
            + Duration::from_secs(10)
    }

    /// How long a dynamic mrouter / foreign querier stays valid.
    fn querier_hold(&self) -> Duration {
        self.config.query_interval * 2 + Duration::from_secs(10)
    }

    fn vlan_enabled(&self, vlan: u16) -> bool {
        !self.config.disabled
            && !self
                .config
                .vlans
                .get(&vlan)
                .map(|v| v.disabled)
                .unwrap_or(false)
    }

    /// Our querier source address for a VLAN.
    fn querier_source(&self, vlan: u16) -> IpAddr {
        let config = self.config.vlans.get(&vlan);
        match (self.family, config.and_then(|c| c.querier_address)) {
            (_, Some(address)) => address,
            (Family::Igmp, None) => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            (Family::Mld, None) => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        }
    }

    /// The current mrouter set of a VLAN (static + live dynamic).
    fn mrouters(&self, vlan: u16, now: Instant) -> Vec<String> {
        let hold = self.querier_hold();
        let mut out: Vec<String> = self
            .config
            .vlans
            .get(&vlan)
            .map(|c| c.static_mrouters.clone())
            .unwrap_or_default();
        if let Some(state) = self.vlans.get(&vlan) {
            out.extend(
                state
                    .dyn_mrouters
                    .iter()
                    .filter(|(_, at)| now.saturating_duration_since(**at) < hold)
                    .map(|(port, _)| port.clone()),
            );
        }
        out.sort();
        out.dedup();
        out
    }
}

async fn run(
    inner: Arc<Mutex<Inner>>,
    mut packet_in: mpsc::UnboundedReceiver<(String, u16, Vec<u8>)>,
    packet_out: mpsc::UnboundedSender<(String, Vec<u8>)>,
    groups: mpsc::UnboundedSender<GroupUpdate>,
    mrouters: mpsc::UnboundedSender<MrouterUpdate>,
    mut wake: mpsc::UnboundedReceiver<()>,
) {
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            frame = packet_in.recv() => match frame {
                Some((port, vlan, frame)) => {
                    if let Ok(mut inner) = inner.lock() {
                        let family = inner.family;
                        if inner.vlan_enabled(vlan) {
                            for message in parse(family, &frame) {
                                apply_message(&mut inner, &port, vlan, message);
                            }
                        }
                    }
                }
                None => break,
            },
            _ = wake.recv() => {}
        }
        step(&inner, &packet_out, &groups, &mrouters);
    }
}

fn apply_message(inner: &mut Inner, port: &str, vlan: u16, message: Message) {
    let now = Instant::now();
    let fast_leave = inner
        .config
        .vlans
        .get(&vlan)
        .map(|c| c.fast_leave)
        .unwrap_or(false);
    let our_source = inner.querier_source(vlan);
    let state = inner.vlans.entry(vlan).or_default();
    match message {
        Message::Report { group, version } => {
            let members = state.groups.entry(group).or_default();
            members.insert(
                port.to_string(),
                Member {
                    last_report: now,
                    version,
                    pending_leave: None,
                },
            );
        }
        Message::Leave { group } => {
            if let Some(members) = state.groups.get_mut(&group) {
                if fast_leave {
                    members.remove(port);
                } else if let Some(member) = members.get_mut(port) {
                    member.pending_leave = Some(now);
                }
            }
        }
        Message::Query { source } => {
            // Queries come from routers: the ingress port is an mrouter.
            state.dyn_mrouters.insert(port.to_string(), now);
            // Election: the lower source address stays querier; ours
            // yields when theirs is lower.
            if source < our_source {
                state.other_querier = Some((source, now));
            }
        }
    }
}

/// One evaluation pass: timers, pushes, due queries.
fn step(
    inner: &Arc<Mutex<Inner>>,
    packet_out: &mpsc::UnboundedSender<(String, Vec<u8>)>,
    groups_out: &mpsc::UnboundedSender<GroupUpdate>,
    mrouters_out: &mpsc::UnboundedSender<MrouterUpdate>,
) {
    let Ok(mut inner) = inner.lock() else { return };
    let now = Instant::now();
    let membership_hold = inner.membership_hold();
    let querier_hold = inner.querier_hold();
    let last_member = inner.config.last_member_interval;
    let inner = &mut *inner;

    let vlans: Vec<u16> = inner
        .vlans
        .keys()
        .copied()
        .chain(inner.config.vlans.keys().copied())
        .collect();
    for vlan in vlans {
        let enabled = inner.vlan_enabled(vlan);
        let mrouter_set = inner.mrouters(vlan, now);
        let state = inner.vlans.entry(vlan).or_default();

        // Expire members, pending leaves, dynamic mrouters, foreign
        // queriers. A disabled VLAN sheds all its learned state.
        if !enabled {
            state.groups.clear();
            state.dyn_mrouters.clear();
            state.other_querier = None;
        }
        for members in state.groups.values_mut() {
            members.retain(|_, member| {
                let leave_expired = member
                    .pending_leave
                    .is_some_and(|at| now.saturating_duration_since(at) >= last_member);
                let aged =
                    now.saturating_duration_since(member.last_report) >= membership_hold;
                !(leave_expired || aged)
            });
        }
        state.groups.retain(|_, members| !members.is_empty());
        state
            .dyn_mrouters
            .retain(|_, at| now.saturating_duration_since(*at) < querier_hold);
        if let Some((_, at)) = state.other_querier {
            if now.saturating_duration_since(at) >= querier_hold {
                state.other_querier = None;
            }
        }

        // Push group changes (outputs = members + mrouters).
        let mut wanted: BTreeMap<IpAddr, Vec<String>> = BTreeMap::new();
        for (group, members) in &state.groups {
            let mut ports: Vec<String> = members.keys().cloned().collect();
            ports.extend(mrouter_set.iter().cloned());
            ports.sort();
            ports.dedup();
            wanted.insert(*group, ports);
        }
        for (group, ports) in &wanted {
            if state.pushed_groups.get(group) != Some(ports) {
                state.pushed_groups.insert(*group, ports.clone());
                let _ = groups_out.send(GroupUpdate {
                    vlan,
                    group: *group,
                    ports: ports.clone(),
                    remove: false,
                });
            }
        }
        let stale: Vec<IpAddr> = state
            .pushed_groups
            .keys()
            .filter(|group| !wanted.contains_key(group))
            .copied()
            .collect();
        for group in stale {
            state.pushed_groups.remove(&group);
            let _ = groups_out.send(GroupUpdate {
                vlan,
                group,
                ports: Vec::new(),
                remove: true,
            });
        }

        // Unknown-multicast scope: restrict to the mrouter set only
        // when the VLAN actually has one (or runs our querier).
        let config = inner.config.vlans.get(&vlan).cloned().unwrap_or_default();
        let restrict = enabled && (!mrouter_set.is_empty() || config.querier);
        let push = (restrict, if restrict { mrouter_set.clone() } else { Vec::new() });
        if state.pushed_mrouters.as_ref() != Some(&push) {
            state.pushed_mrouters = Some(push.clone());
            let _ = mrouters_out.send(MrouterUpdate {
                vlan,
                restrict: push.0,
                ports: push.1,
            });
        }

        // Local querier: general queries at the query interval, unless
        // a lower-addressed querier is active.
        let suppressed = state.other_querier.is_some();
        if enabled && config.querier && !suppressed {
            let due = match state.last_query_tx {
                Some(at) => now.saturating_duration_since(at) >= inner.config.query_interval,
                None => true,
            };
            if due {
                state.last_query_tx = Some(now);
                let source = match (inner.family, config.querier_address) {
                    (_, Some(address)) => address,
                    (Family::Igmp, None) => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                    (Family::Mld, None) => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
                };
                let frame = encode_general_query(inner.family, inner.system_mac, source);
                for port in inner.vlan_ports.get(&vlan).cloned().unwrap_or_default() {
                    let _ = packet_out.send((port, frame.clone()));
                }
            }
        }
    }
}

/// Parse the snooping-relevant messages out of an Ethernet frame.
pub fn parse(family: Family, frame: &[u8]) -> Vec<Message> {
    match family {
        Family::Igmp => parse_igmp(frame),
        Family::Mld => parse_mld(frame),
    }
}

fn parse_igmp(frame: &[u8]) -> Vec<Message> {
    // Ethernet + IPv4 + IGMP.
    if frame.len() < 14 + 20 + 8 || frame[12..14] != [0x08, 0x00] {
        return Vec::new();
    }
    let ip = &frame[14..];
    let ihl = usize::from(ip[0] & 0x0f) * 4;
    if ip[0] >> 4 != 4 || ip.len() < ihl + 8 || ip[9] != 2 {
        return Vec::new();
    }
    let source = IpAddr::V4(std::net::Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]));
    let igmp = &ip[ihl..];
    let group_at = |at: usize| {
        IpAddr::V4(std::net::Ipv4Addr::new(
            igmp[at],
            igmp[at + 1],
            igmp[at + 2],
            igmp[at + 3],
        ))
    };
    match igmp[0] {
        0x11 => vec![Message::Query { source }],
        0x12 => vec![Message::Report {
            group: group_at(4),
            version: 1,
        }],
        0x16 => vec![Message::Report {
            group: group_at(4),
            version: 2,
        }],
        0x17 => vec![Message::Leave { group: group_at(4) }],
        0x22 => {
            // IGMPv3 report: group records.
            if igmp.len() < 8 {
                return Vec::new();
            }
            let count = usize::from(u16::from_be_bytes([igmp[6], igmp[7]]));
            let mut out = Vec::new();
            let mut at = 8;
            for _ in 0..count {
                if igmp.len() < at + 8 {
                    break;
                }
                let record_type = igmp[at];
                let aux_len = usize::from(igmp[at + 1]) * 4;
                let sources = usize::from(u16::from_be_bytes([igmp[at + 2], igmp[at + 3]]));
                let group = group_at(at + 4);
                // TO_IN / IS_IN with no sources = leave; everything else
                // that names a group = join.
                match record_type {
                    1 | 3 if sources == 0 => out.push(Message::Leave { group }),
                    1..=6 => out.push(Message::Report { group, version: 3 }),
                    _ => {}
                }
                at += 8 + sources * 4 + aux_len;
            }
            out
        }
        _ => Vec::new(),
    }
}

fn parse_mld(frame: &[u8]) -> Vec<Message> {
    // Ethernet + IPv6 (+ hop-by-hop) + ICMPv6.
    if frame.len() < 14 + 40 + 8 || frame[12..14] != [0x86, 0xdd] {
        return Vec::new();
    }
    let ip = &frame[14..];
    if ip[0] >> 4 != 6 || ip.len() < 40 {
        return Vec::new();
    }
    let mut source_bytes = [0u8; 16];
    source_bytes.copy_from_slice(&ip[8..24]);
    let source = IpAddr::V6(std::net::Ipv6Addr::from(source_bytes));
    let mut next = ip[6];
    let mut at = 40;
    // MLD rides behind a hop-by-hop router-alert option.
    if next == 0 {
        if ip.len() < at + 8 {
            return Vec::new();
        }
        next = ip[at];
        at += (usize::from(ip[at + 1]) + 1) * 8;
    }
    if next != 58 || ip.len() < at + 8 {
        return Vec::new();
    }
    let icmp = &ip[at..];
    let group_at = |at: usize| {
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&icmp[at..at + 16]);
        IpAddr::V6(std::net::Ipv6Addr::from(bytes))
    };
    match icmp[0] {
        130 => vec![Message::Query { source }],
        131 if icmp.len() >= 24 => vec![Message::Report {
            group: group_at(8),
            version: 1,
        }],
        132 if icmp.len() >= 24 => vec![Message::Leave { group: group_at(8) }],
        143 => {
            if icmp.len() < 8 {
                return Vec::new();
            }
            let count = usize::from(u16::from_be_bytes([icmp[6], icmp[7]]));
            let mut out = Vec::new();
            let mut record_at = 8;
            for _ in 0..count {
                if icmp.len() < record_at + 20 {
                    break;
                }
                let record_type = icmp[record_at];
                let aux_len = usize::from(icmp[record_at + 1]) * 4;
                let sources =
                    usize::from(u16::from_be_bytes([icmp[record_at + 2], icmp[record_at + 3]]));
                let group = group_at(record_at + 4);
                match record_type {
                    1 | 3 if sources == 0 => out.push(Message::Leave { group }),
                    1..=6 => out.push(Message::Report { group, version: 2 }),
                    _ => {}
                }
                record_at += 20 + sources * 16 + aux_len;
            }
            out
        }
        _ => Vec::new(),
    }
}

/// RFC 1071 checksum.
fn checksum(chunks: &[&[u8]]) -> u16 {
    let mut sum = 0u32;
    let mut carry: Option<u8> = None;
    for chunk in chunks {
        for byte in *chunk {
            match carry.take() {
                Some(high) => sum += u32::from(u16::from_be_bytes([high, *byte])),
                None => carry = Some(*byte),
            }
        }
    }
    if let Some(high) = carry {
        sum += u32::from(u16::from_be_bytes([high, 0]));
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Encode a general membership query as a whole Ethernet frame.
pub fn encode_general_query(family: Family, src_mac: [u8; 6], source: IpAddr) -> Vec<u8> {
    match (family, source) {
        (Family::Igmp, IpAddr::V4(source)) => {
            // IGMPv2 general query to 224.0.0.1.
            let mut igmp = vec![0x11, 100, 0, 0, 0, 0, 0, 0];
            let sum = checksum(&[&igmp]);
            igmp[2..4].copy_from_slice(&sum.to_be_bytes());

            let mut ip = Vec::with_capacity(20);
            ip.extend_from_slice(&[0x45, 0xc0]); // v4, ihl 5, tos
            ip.extend_from_slice(&(20u16 + igmp.len() as u16).to_be_bytes());
            ip.extend_from_slice(&[0, 0, 0, 0]); // id, flags
            ip.push(1); // ttl
            ip.push(2); // protocol IGMP
            ip.extend_from_slice(&[0, 0]); // checksum below
            ip.extend_from_slice(&source.octets());
            ip.extend_from_slice(&[224, 0, 0, 1]);
            let sum = checksum(&[&ip]);
            ip[10..12].copy_from_slice(&sum.to_be_bytes());

            let mut frame = Vec::with_capacity(14 + ip.len() + igmp.len());
            frame.extend_from_slice(&[0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]);
            frame.extend_from_slice(&src_mac);
            frame.extend_from_slice(&[0x08, 0x00]);
            frame.extend_from_slice(&ip);
            frame.extend_from_slice(&igmp);
            frame
        }
        (Family::Mld, IpAddr::V6(source)) => {
            // MLDv1 general query to ff02::1.
            let dst = std::net::Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);
            let mut icmp = vec![130, 0, 0, 0, 0x27, 0x10, 0, 0];
            icmp.extend_from_slice(&[0u8; 16]); // group: unspecified
            let length = (icmp.len() as u32).to_be_bytes();
            let pseudo_tail = [0u8, 0, 0, 58];
            let sum = checksum(&[
                &source.octets(),
                &dst.octets(),
                &length,
                &pseudo_tail,
                &icmp,
            ]);
            icmp[2..4].copy_from_slice(&sum.to_be_bytes());

            let mut ip = Vec::with_capacity(40);
            ip.extend_from_slice(&[0x60, 0, 0, 0]);
            ip.extend_from_slice(&(icmp.len() as u16).to_be_bytes());
            ip.push(58); // next header ICMPv6 (router-alert HBH omitted)
            ip.push(1); // hop limit
            ip.extend_from_slice(&source.octets());
            ip.extend_from_slice(&dst.octets());

            let mut frame = Vec::with_capacity(14 + ip.len() + icmp.len());
            frame.extend_from_slice(&[0x33, 0x33, 0x00, 0x00, 0x00, 0x01]);
            frame.extend_from_slice(&src_mac);
            frame.extend_from_slice(&[0x86, 0xdd]);
            frame.extend_from_slice(&ip);
            frame.extend_from_slice(&icmp);
            frame
        }
        // Family/address mismatch: nothing sensible to send.
        _ => Vec::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// A v2 membership report frame for `group` from `source`.
    fn v2_report(source: Ipv4Addr, group: Ipv4Addr) -> Vec<u8> {
        build_igmp(source, 0x16, group)
    }

    fn v2_leave(source: Ipv4Addr, group: Ipv4Addr) -> Vec<u8> {
        build_igmp(source, 0x17, group)
    }

    fn build_igmp(source: Ipv4Addr, kind: u8, group: Ipv4Addr) -> Vec<u8> {
        let mut igmp = vec![kind, 0, 0, 0];
        igmp.extend_from_slice(&group.octets());
        let sum = checksum(&[&igmp]);
        igmp[2..4].copy_from_slice(&sum.to_be_bytes());
        let mut ip = vec![0x45, 0, 0, 28, 0, 0, 0, 0, 1, 2, 0, 0];
        ip.extend_from_slice(&source.octets());
        ip.extend_from_slice(&group.octets());
        let sum = checksum(&[&ip]);
        ip[10..12].copy_from_slice(&sum.to_be_bytes());
        let mut frame = vec![0x01, 0x00, 0x5e, 0, 0, 1, 2, 0, 0, 0, 0, 9, 0x08, 0x00];
        frame.extend_from_slice(&ip);
        frame.extend_from_slice(&igmp);
        frame
    }

    fn fast_config(vlans: &[(u16, VlanConfig)]) -> Config {
        Config {
            robustness: 2,
            query_interval: Duration::from_secs(1),
            last_member_interval: Duration::from_millis(500),
            vlans: vlans.iter().cloned().collect(),
            ..Config::default()
        }
    }

    fn drain_groups(rx: &mut mpsc::UnboundedReceiver<GroupUpdate>) -> Vec<GroupUpdate> {
        let mut out = Vec::new();
        while let Ok(update) = rx.try_recv() {
            out.push(update);
        }
        out
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn join_leave_and_fast_leave() {
        let (engine, mut io) = Engine::spawn(Family::Igmp, [0x02, 0, 0, 0, 0, 1]);
        engine.set_config(fast_config(&[(
            10,
            VlanConfig {
                fast_leave: true,
                ..VlanConfig::default()
            },
        )]));
        let group: Ipv4Addr = "239.1.1.10".parse().unwrap();

        // Two hosts join on different ports.
        io.packet_in
            .send(("Ethernet1".into(), 10, v2_report("10.0.10.5".parse().unwrap(), group)))
            .unwrap();
        io.packet_in
            .send(("Ethernet3".into(), 10, v2_report("10.0.10.6".parse().unwrap(), group)))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        let snapshot = engine.snapshot().unwrap();
        let vlan = snapshot.vlans.iter().find(|v| v.vlan == 10).unwrap();
        assert_eq!(vlan.groups.len(), 1);
        assert_eq!(vlan.groups[0].ports, ["Ethernet1", "Ethernet3"]);
        assert_eq!(vlan.groups[0].version, 2);
        let updates = drain_groups(&mut io.groups);
        assert!(updates
            .iter()
            .any(|u| u.ports == ["Ethernet1", "Ethernet3"] && !u.remove));

        // Fast-leave drops the member immediately.
        io.packet_in
            .send(("Ethernet1".into(), 10, v2_leave("10.0.10.5".parse().unwrap(), group)))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        let snapshot = engine.snapshot().unwrap();
        let vlan = snapshot.vlans.iter().find(|v| v.vlan == 10).unwrap();
        assert_eq!(vlan.groups[0].ports, ["Ethernet3"]);

        // The remaining member ages out without refreshes
        // (robustness 2 x 1s interval + 10s grace).
        tokio::time::sleep(Duration::from_secs(13)).await;
        let snapshot = engine.snapshot().unwrap();
        let vlan = snapshot.vlans.iter().find(|v| v.vlan == 10).unwrap();
        assert!(vlan.groups.is_empty(), "membership aged out");
        let updates = drain_groups(&mut io.groups);
        assert!(updates.iter().any(|u| u.remove));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn querier_election_and_mrouter_learning() {
        let (engine, mut io) = Engine::spawn(Family::Igmp, [0x02, 0, 0, 0, 0, 1]);
        engine.set_config(fast_config(&[(
            20,
            VlanConfig {
                querier: true,
                querier_address: Some("10.0.20.1".parse().unwrap()),
                ..VlanConfig::default()
            },
        )]));
        engine.set_vlan_ports([(20u16, vec!["Ethernet2".to_string()])].into());

        // Our querier transmits general queries on the VLAN's ports.
        tokio::time::sleep(Duration::from_millis(1600)).await;
        let (port, frame) = io.packet_out.try_recv().unwrap();
        assert_eq!(port, "Ethernet2");
        assert_eq!(parse(Family::Igmp, &frame), vec![Message::Query {
            source: "10.0.20.1".parse().unwrap()
        }]);
        assert!(engine.snapshot().unwrap().vlans[0].querier_active);

        // A lower-addressed querier appears: we yield, and its port
        // becomes a dynamic mrouter.
        let foreign = encode_general_query(
            Family::Igmp,
            [0x02, 0, 0, 0, 0, 9],
            "10.0.20.0".parse().unwrap(),
        );
        io.packet_in.send(("Ethernet7".into(), 20, foreign)).unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        let snapshot = engine.snapshot().unwrap();
        let vlan = &snapshot.vlans[0];
        assert!(!vlan.querier_active, "suppressed by the lower address");
        assert_eq!(vlan.dynamic_mrouters, ["Ethernet7"]);

        // The mrouter set restricts unknown multicast.
        let mut last = None;
        while let Ok(update) = io.mrouters.try_recv() {
            last = Some(update);
        }
        let last = last.unwrap();
        assert!(last.restrict);
        assert_eq!(last.ports, ["Ethernet7"]);

        // Members' l2mc outputs include the mrouter port.
        let group: Ipv4Addr = "239.20.0.7".parse().unwrap();
        io.packet_in
            .send(("Ethernet2".into(), 20, v2_report("10.0.20.9".parse().unwrap(), group)))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        let updates = drain_groups(&mut io.groups);
        assert!(updates
            .iter()
            .any(|u| u.ports == ["Ethernet2", "Ethernet7"]));

        // The foreign querier goes quiet: we take over again.
        tokio::time::sleep(Duration::from_secs(13)).await;
        assert!(engine.snapshot().unwrap().vlans[0].querier_active);
    }

    #[test]
    fn igmpv3_and_mld_parse() {
        // IGMPv3: one TO_EX join and one TO_IN({}) leave.
        let mut igmp = vec![0x22, 0, 0, 0, 0, 0, 0, 2];
        igmp.extend_from_slice(&[4, 0, 0, 0, 239, 1, 1, 10]); // TO_EX join
        igmp.extend_from_slice(&[3, 0, 0, 0, 239, 1, 1, 11]); // TO_IN leave
        let sum = checksum(&[&igmp]);
        igmp[2..4].copy_from_slice(&sum.to_be_bytes());
        let mut ip = vec![0x45, 0, 0, 44, 0, 0, 0, 0, 1, 2, 0, 0];
        ip.extend_from_slice(&[10, 0, 0, 5]);
        ip.extend_from_slice(&[224, 0, 0, 22]);
        let mut frame = vec![0x01, 0x00, 0x5e, 0, 0, 22, 2, 0, 0, 0, 0, 9, 0x08, 0x00];
        frame.extend_from_slice(&ip);
        frame.extend_from_slice(&igmp);
        let messages = parse(Family::Igmp, &frame);
        assert_eq!(
            messages,
            vec![
                Message::Report {
                    group: "239.1.1.10".parse().unwrap(),
                    version: 3
                },
                Message::Leave {
                    group: "239.1.1.11".parse().unwrap()
                },
            ]
        );

        // MLD: an encoded general query round-trips through the parser.
        let query = encode_general_query(
            Family::Mld,
            [0x02, 0, 0, 0, 0, 1],
            "fe80::1".parse().unwrap(),
        );
        assert_eq!(
            parse(Family::Mld, &query),
            vec![Message::Query {
                source: "fe80::1".parse().unwrap()
            }]
        );

        // Junk stays junk.
        assert!(parse(Family::Igmp, &[0u8; 20]).is_empty());
        assert!(parse(Family::Mld, &[0u8; 20]).is_empty());
    }
}
