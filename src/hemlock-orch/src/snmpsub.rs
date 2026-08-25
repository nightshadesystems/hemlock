//! The SNMP AgentX subagent: serves the IF-MIB from syncd's interface
//! state over net-snmp's AgentX socket.
//!
//! Why this exists at all: syncd owns the ASIC, and the kernel netdevs
//! snmpd would count are hostifs — they see punted control traffic,
//! not the hardware forwarding path. A plain snmpd `ifTable` would
//! therefore report numbers that are simply wrong. So snmpd keeps
//! transport, auth and the system group, and this subagent registers
//! `ifTable`/`ifXTable` and answers them with exactly the counters
//! `show interfaces counters` prints.
//!
//! The registration priority is deliberately better (numerically
//! lower) than the master's own built-in handlers, so the ASIC numbers
//! win wherever both could answer.
//!
//! Shape: the MIB is a sorted `(OID, Value)` vector rebuilt from a
//! syncd snapshot on a short TTL, and request handling is a pure
//! function over it — so `ifTable` values can be tested against mock
//! syncd counters, and the framing against a scripted master, with no
//! sockets in either case.

// The AgentX wire format and the IF-MIB are only *driven* by the
// Linux-only master session (and by the tests, which run everywhere).
// On other hosts they are dead code we still want compiled and tested,
// so the dev-host build does not report the whole module unused.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::sync::{Arc, Mutex};

use hemlock_common::proto::v1 as pb;
use tokio::time::{Duration, Instant};

use crate::agentx::{self, Oid, PduType, Request, SearchRange, Value, VarBind};

/// `1.3.6.1.2.1.2` — the interfaces group (ifNumber + ifTable).
const INTERFACES: [u32; 7] = [1, 3, 6, 1, 2, 1, 2];
/// `1.3.6.1.2.1.31.1.1` — ifXTable's parent.
const IF_X: [u32; 9] = [1, 3, 6, 1, 2, 1, 31, 1, 1];
/// `1.3.6.1.4.1.62742` — Hemlock's enterprise-style subagent id. Only
/// ever used to name the session to the master.
const SUBAGENT_ID: [u32; 7] = [1, 3, 6, 1, 4, 1, 62742];

/// Registration priority. Lower wins, and the master's own handlers
/// sit at the default 127.
const PRIORITY: u8 = 1;

/// How long a syncd snapshot is reused. A poller walking 52 ports
/// issues hundreds of GetNexts in a burst; without this each one would
/// be its own gRPC round trip.
const CACHE_TTL: Duration = Duration::from_secs(5);

/// Config pushed from mgmtd. Absent = SNMP is off and the subagent
/// keeps no session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    pub enabled: bool,
    /// The AgentX master socket snmpd was rendered with.
    pub socket: String,
    pub location: String,
    pub contact: String,
    /// v2c communities in config order: (name, source); an empty
    /// source answers anywhere.
    pub communities: Vec<(String, String)>,
    /// v3 USM user names (passphrases never leave mgmtd's render).
    pub users: Vec<String>,
    /// The interface the agent listens on, for display.
    pub listen_interface: String,
    pub listen_address: String,
}

/// What `show snmp` renders.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub config: Config,
    /// The subagent holds an open AgentX session with the master.
    pub connected: bool,
    pub packets_in: u64,
    pub packets_out: u64,
    pub get_requests: u64,
    pub getnext_requests: u64,
    pub errors: u64,
}

#[derive(Debug, Default)]
struct Counters {
    packets_in: u64,
    packets_out: u64,
    get_requests: u64,
    getnext_requests: u64,
    errors: u64,
}

struct Inner {
    config: Config,
    connected: bool,
    counters: Counters,
    /// The MIB, sorted by OID, with the instant it was built.
    mib: Vec<(Oid, Value)>,
    built: Option<Instant>,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
}

impl Engine {
    pub fn new() -> Engine {
        Engine {
            inner: Arc::new(Mutex::new(Inner {
                config: Config::default(),
                connected: false,
                counters: Counters::default(),
                mib: Vec::new(),
                built: None,
            })),
        }
    }

    /// Replace the configuration (declarative).
    pub fn set_config(&self, config: Config) {
        if let Ok(mut inner) = self.inner.lock() {
            // A config change invalidates nothing about the MIB, but a
            // disable should stop claiming a session.
            if !config.enabled {
                inner.connected = false;
            }
            inner.config = config;
        }
    }

    pub fn config(&self) -> Config {
        self.inner
            .lock()
            .map(|inner| inner.config.clone())
            .unwrap_or_default()
    }

    pub fn snapshot(&self) -> Snapshot {
        let Ok(inner) = self.inner.lock() else {
            return Snapshot::default();
        };
        Snapshot {
            config: inner.config.clone(),
            connected: inner.connected,
            packets_in: inner.counters.packets_in,
            packets_out: inner.counters.packets_out,
            get_requests: inner.counters.get_requests,
            getnext_requests: inner.counters.getnext_requests,
            errors: inner.counters.errors,
        }
    }

    fn set_connected(&self, connected: bool) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.connected = connected;
        }
    }

    /// Is the cached MIB still fresh?
    fn mib_fresh(&self, now: Instant) -> bool {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.built)
            .is_some_and(|built| now.saturating_duration_since(built) < CACHE_TTL)
    }

    fn store_mib(&self, mib: Vec<(Oid, Value)>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.mib = mib;
            inner.built = Some(Instant::now());
        }
    }

    /// Serve one decoded request against the cached MIB, counting it.
    fn serve(&self, request: &Request) -> Vec<u8> {
        let Ok(mut inner) = self.inner.lock() else {
            return Vec::new();
        };
        inner.counters.packets_in += 1;
        match request.header.pdu_type {
            PduType::Get => inner.counters.get_requests += 1,
            PduType::GetNext | PduType::GetBulk => inner.counters.getnext_requests += 1,
            _ => {}
        }
        let binds = answer(&inner.mib, request);
        inner.counters.packets_out += 1;
        agentx::response_pdu(&request.header, agentx::ERROR_NONE, 0, &binds)
    }

    fn count_error(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.counters.errors += 1;
        }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------------- the MIB

/// The IF-MIB ifIndex of one interface. Front-panel ports use the
/// platform manifest's index, which is stable across reboots and
/// matches the port's name; the other kinds get a disjoint band each,
/// so a Port-Channel can never collide with a physical port.
pub fn if_index(kind: &str, name: &str, index: u32) -> Option<u32> {
    let trailing = |prefix: &str| name.strip_prefix(prefix)?.parse::<u32>().ok();
    match kind {
        "ethernet" if index > 0 => Some(index),
        // A port with no manifest index would alias another's; skip it
        // rather than report the wrong interface's counters.
        "ethernet" => None,
        "management" => trailing("Management").map(|n| 1_000 + n),
        "port-channel" => trailing("Port-Channel").map(|n| 2_000 + n),
        "vlan" => trailing("Vlan").map(|n| 10_000 + n),
        _ => None,
    }
}

fn octets(text: &str) -> Value {
    Value::OctetString(text.as_bytes().to_vec())
}

/// A colon-separated MAC as its six bytes; anything unparsable renders
/// as an empty physical address rather than a wrong one.
fn phys_address(mac: &str) -> Value {
    let bytes: Vec<u8> = mac
        .split(':')
        .filter_map(|byte| u8::from_str_radix(byte, 16).ok())
        .collect();
    Value::OctetString(if bytes.len() == 6 { bytes } else { Vec::new() })
}

/// Counter32 truncation of a 64-bit counter — the low 32 bits, which
/// is exactly what a 32-bit counter would have wrapped to.
fn counter32(value: u64) -> Value {
    Value::Counter32((value & 0xffff_ffff) as u32)
}

/// Build the sorted MIB from one syncd interface snapshot.
pub fn build_mib(interfaces: &[pb::InterfaceState]) -> Vec<(Oid, Value)> {
    let mut rows: Vec<(u32, &pb::InterfaceState)> = interfaces
        .iter()
        .filter_map(|iface| {
            if_index(&iface.kind, &iface.name, iface.index).map(|index| (index, iface))
        })
        .collect();
    rows.sort_by_key(|(index, _)| *index);

    let mut mib: Vec<(Oid, Value)> = Vec::with_capacity(rows.len() * 36 + 1);
    // ifNumber.0
    mib.push((
        vec![1, 3, 6, 1, 2, 1, 2, 1, 0],
        Value::Integer(i32::try_from(rows.len()).unwrap_or(i32::MAX)),
    ));

    for (index, iface) in &rows {
        let index = *index;
        let counters = iface.counters.unwrap_or_default();
        let up = iface.oper_status == pb::OperStatus::Up as i32;
        let admin_up = iface.admin_state != pb::AdminState::Down as i32;
        // ifSpeed is bits per second in a Gauge32, so anything at or
        // above 4.294 Gb/s reports the RFC 2863 sentinel and the real
        // rate lives in ifHighSpeed.
        let speed_bps = iface.speed_mbps.saturating_mul(1_000_000);
        let if_speed = u32::try_from(speed_bps).unwrap_or(u32::MAX);
        let last_change = iface
            .seconds_since_change
            .map(|secs| u32::try_from(secs.saturating_mul(100)).unwrap_or(u32::MAX))
            .unwrap_or(0);

        let mut column = |table: &[u32], col: u32, value: Value| {
            let mut oid = table.to_vec();
            oid.push(col);
            oid.push(index);
            mib.push((oid, value));
        };
        // ifTable: 1.3.6.1.2.1.2.2.1.<col>.<index>
        let if_table = [1, 3, 6, 1, 2, 1, 2, 2, 1];
        column(&if_table, 1, Value::Integer(index as i32));
        column(&if_table, 2, octets(&iface.name));
        // ethernetCsmacd(6) for real ports, propVirtual(53) otherwise.
        let if_type = if iface.kind == "ethernet" { 6 } else { 53 };
        column(&if_table, 3, Value::Integer(if_type));
        column(&if_table, 4, Value::Integer(iface.mtu as i32));
        column(&if_table, 5, Value::Gauge32(if_speed));
        column(&if_table, 6, phys_address(&iface.mac));
        column(&if_table, 7, Value::Integer(if admin_up { 1 } else { 2 }));
        column(&if_table, 8, Value::Integer(if up { 1 } else { 2 }));
        column(&if_table, 9, Value::TimeTicks(last_change));
        column(&if_table, 10, counter32(counters.in_octets));
        column(&if_table, 11, counter32(counters.in_ucast_pkts));
        column(
            &if_table,
            12,
            counter32(counters.in_mcast_pkts + counters.in_bcast_pkts),
        );
        column(&if_table, 13, counter32(counters.in_discards));
        column(&if_table, 14, counter32(counters.in_errors));
        column(&if_table, 16, counter32(counters.out_octets));
        column(&if_table, 17, counter32(counters.out_ucast_pkts));
        column(
            &if_table,
            18,
            counter32(counters.out_mcast_pkts + counters.out_bcast_pkts),
        );
        column(&if_table, 19, counter32(counters.out_discards));
        column(&if_table, 20, counter32(counters.out_errors));

        // ifXTable: 1.3.6.1.2.1.31.1.1.1.<col>.<index>
        let if_x = [1, 3, 6, 1, 2, 1, 31, 1, 1, 1];
        column(&if_x, 1, octets(&iface.name));
        column(&if_x, 2, counter32(counters.in_mcast_pkts));
        column(&if_x, 3, counter32(counters.in_bcast_pkts));
        column(&if_x, 4, counter32(counters.out_mcast_pkts));
        column(&if_x, 5, counter32(counters.out_bcast_pkts));
        column(&if_x, 6, Value::Counter64(counters.in_octets));
        column(&if_x, 7, Value::Counter64(counters.in_ucast_pkts));
        column(&if_x, 8, Value::Counter64(counters.in_mcast_pkts));
        column(&if_x, 9, Value::Counter64(counters.in_bcast_pkts));
        column(&if_x, 10, Value::Counter64(counters.out_octets));
        column(&if_x, 11, Value::Counter64(counters.out_ucast_pkts));
        column(&if_x, 12, Value::Counter64(counters.out_mcast_pkts));
        column(&if_x, 13, Value::Counter64(counters.out_bcast_pkts));
        // ifLinkUpDownTrapEnable: disabled(2) — traps are out of scope.
        column(&if_x, 14, Value::Integer(2));
        column(
            &if_x,
            15,
            Value::Gauge32(u32::try_from(iface.speed_mbps).unwrap_or(u32::MAX)),
        );
        // ifPromiscuousMode false(2), ifConnectorPresent true(1).
        column(&if_x, 16, Value::Integer(2));
        column(&if_x, 17, Value::Integer(1));
        column(&if_x, 18, octets(&iface.description));
        // ifCounterDiscontinuityTime: the last `clear counters`.
        let discontinuity = iface
            .seconds_since_clear
            .map(|secs| u32::try_from(secs.saturating_mul(100)).unwrap_or(u32::MAX))
            .unwrap_or(0);
        column(&if_x, 19, Value::TimeTicks(discontinuity));
    }
    mib.sort_by(|(left, _), (right, _)| left.cmp(right));
    mib
}

/// The next entry at or after `start` (at only when `include`), bounded
/// by `end` when the range names one.
fn next_after<'a>(mib: &'a [(Oid, Value)], range: &SearchRange) -> Option<&'a (Oid, Value)> {
    let found = mib.iter().find(|(oid, _)| {
        if range.include {
            *oid >= range.start
        } else {
            *oid > range.start
        }
    })?;
    if !range.end.is_empty() && found.0 >= range.end {
        return None;
    }
    Some(found)
}

/// Answer one decoded request against a MIB snapshot. Pure: this is
/// where every retrieval semantic lives.
pub fn answer(mib: &[(Oid, Value)], request: &Request) -> Vec<VarBind> {
    match request.header.pdu_type {
        PduType::Get => request
            .ranges
            .iter()
            .map(
                |range| match mib.iter().find(|(oid, _)| *oid == range.start) {
                    Some((oid, value)) => VarBind {
                        name: oid.clone(),
                        value: value.clone(),
                    },
                    // A named column with no such instance is distinct
                    // from an OID under no column at all: drop the
                    // instance sub-id and see whether the column exists.
                    None => VarBind {
                        name: range.start.clone(),
                        value: if names_a_column(mib, &range.start) {
                            Value::NoSuchInstance
                        } else {
                            Value::NoSuchObject
                        },
                    },
                },
            )
            .collect(),
        PduType::GetNext => request
            .ranges
            .iter()
            .map(|range| bind_or_end(next_after(mib, range), &range.start))
            .collect(),
        PduType::GetBulk => {
            let non_repeaters = usize::from(request.non_repeaters).min(request.ranges.len());
            let mut binds = Vec::new();
            for range in &request.ranges[..non_repeaters] {
                binds.push(bind_or_end(next_after(mib, range), &range.start));
            }
            // Each remaining range repeats, walking on from whatever
            // the previous repetition returned.
            let mut cursors: Vec<SearchRange> = request.ranges[non_repeaters..].to_vec();
            for _ in 0..request.max_repetitions {
                let mut progressed = false;
                for cursor in cursors.iter_mut() {
                    match next_after(mib, cursor) {
                        Some((oid, value)) => {
                            binds.push(VarBind {
                                name: oid.clone(),
                                value: value.clone(),
                            });
                            cursor.start = oid.clone();
                            cursor.include = false;
                            progressed = true;
                        }
                        None => binds.push(VarBind {
                            name: cursor.start.clone(),
                            value: Value::EndOfMibView,
                        }),
                    }
                }
                if !progressed {
                    break;
                }
            }
            binds
        }
        _ => Vec::new(),
    }
}

/// Does `oid`'s column (everything but its last sub-id) exist in the
/// MIB? A Get for a real column at a missing index is
/// `noSuchInstance`; anything else is `noSuchObject`.
fn names_a_column(mib: &[(Oid, Value)], oid: &Oid) -> bool {
    let Some((_, column)) = oid.split_last() else {
        return false;
    };
    !column.is_empty() && mib.iter().any(|(entry, _)| entry.starts_with(column))
}

fn bind_or_end(found: Option<&(Oid, Value)>, start: &Oid) -> VarBind {
    match found {
        Some((oid, value)) => VarBind {
            name: oid.clone(),
            value: value.clone(),
        },
        None => VarBind {
            name: start.clone(),
            value: Value::EndOfMibView,
        },
    }
}

// ------------------------------------------------------- the master session

#[cfg(target_os = "linux")]
pub use session::run;

#[cfg(target_os = "linux")]
mod session {
    use super::*;
    use crate::agentx::Header;
    use hemlock_common::ipc::IpcEndpoint;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tracing::{debug, info, warn};

    /// Keep an AgentX session with the master while SNMP is configured:
    /// connect, open, register the two subtrees, then serve until the
    /// socket drops. A disabled agent simply idles here.
    pub async fn run(engine: Engine, syncd: IpcEndpoint) {
        loop {
            let config = engine.config();
            if !config.enabled || config.socket.is_empty() {
                engine.set_connected(false);
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            match serve_session(&engine, &syncd, &config.socket).await {
                Ok(()) => debug!("agentx session closed"),
                Err(err) => debug!(%err, "agentx session ended"),
            }
            engine.set_connected(false);
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    async fn serve_session(
        engine: &Engine,
        syncd: &IpcEndpoint,
        socket: &str,
    ) -> anyhow::Result<()> {
        let mut stream = tokio::net::UnixStream::connect(socket).await?;
        let mut packet_id = 1u32;

        stream
            .write_all(&agentx::open_pdu(packet_id, &SUBAGENT_ID, "hemlock-orch"))
            .await?;
        let (header, payload) = read_pdu(&mut stream).await?;
        let (session_id, error) = agentx::parse_response(&header, &payload)
            .ok_or_else(|| anyhow::anyhow!("unreadable open response"))?;
        if error != agentx::ERROR_NONE {
            anyhow::bail!("agentx open refused (error {error})");
        }

        for subtree in [INTERFACES.as_slice(), IF_X.as_slice()] {
            packet_id += 1;
            stream
                .write_all(&agentx::register_pdu(
                    session_id, packet_id, subtree, PRIORITY,
                ))
                .await?;
            let (header, payload) = read_pdu(&mut stream).await?;
            if let Some((_, error)) = agentx::parse_response(&header, &payload) {
                if error != agentx::ERROR_NONE && !agentx::is_duplicate_registration(error) {
                    anyhow::bail!("agentx registration refused (error {error})");
                }
            }
        }
        engine.set_connected(true);
        info!(%socket, "agentx subagent registered the IF-MIB");

        loop {
            let (header, payload) = read_pdu(&mut stream).await?;
            let reply = match header.pdu_type {
                PduType::Get | PduType::GetNext | PduType::GetBulk => {
                    refresh_mib(engine, syncd).await;
                    match agentx::parse_request(header, &payload) {
                        Some(request) => engine.serve(&request),
                        None => {
                            engine.count_error();
                            agentx::response_pdu(&header, agentx::ERROR_NONE, 0, &[])
                        }
                    }
                }
                // Read-only: every Set phase is refused up front, so
                // the master never reaches CommitSet.
                PduType::TestSet => {
                    engine.count_error();
                    agentx::response_pdu(&header, agentx::ERROR_NOT_WRITABLE, 1, &[])
                }
                PduType::CommitSet | PduType::UndoSet | PduType::CleanupSet | PduType::Ping => {
                    agentx::response_pdu(&header, agentx::ERROR_NONE, 0, &[])
                }
                PduType::Response => continue,
                other => {
                    debug!(?other, "ignoring agentx pdu");
                    agentx::response_pdu(&header, agentx::ERROR_NONE, 0, &[])
                }
            };
            stream.write_all(&reply).await?;
        }
    }

    /// One framed PDU: the fixed header, then its declared payload.
    async fn read_pdu(stream: &mut tokio::net::UnixStream) -> anyhow::Result<(Header, Vec<u8>)> {
        let mut head = [0u8; agentx::HEADER_LEN];
        stream.read_exact(&mut head).await?;
        let header = Header::parse(&head).ok_or_else(|| anyhow::anyhow!("bad agentx header"))?;
        // A hostile length would otherwise be an allocation bomb.
        if header.payload_len > 1 << 20 {
            anyhow::bail!("agentx payload too large ({})", header.payload_len);
        }
        let mut payload = vec![0u8; header.payload_len as usize];
        stream.read_exact(&mut payload).await?;
        Ok((header, payload))
    }

    /// Rebuild the MIB from syncd unless the cached one is still fresh.
    async fn refresh_mib(engine: &Engine, syncd: &IpcEndpoint) {
        if engine.mib_fresh(Instant::now()) {
            return;
        }
        let fetch = async {
            let channel = syncd.connect().await?;
            let response = pb::syncd_client::SyncdClient::new(channel)
                .get_interfaces(pb::GetInterfacesRequest { names: vec![] })
                .await?
                .into_inner();
            anyhow::Ok(response.interfaces)
        };
        match fetch.await {
            Ok(interfaces) => engine.store_mib(build_mib(&interfaces)),
            // Keep serving the stale snapshot: a poller reading numbers
            // a few seconds old beats one reading endOfMibView.
            Err(err) => warn!(%err, "cannot refresh the IF-MIB from syncd"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::agentx::{frame, Header, HEADER_LEN};

    fn interface(name: &str, kind: &str, index: u32) -> pb::InterfaceState {
        pb::InterfaceState {
            name: name.into(),
            kind: kind.into(),
            index,
            admin_state: pb::AdminState::Up as i32,
            oper_status: pb::OperStatus::Up as i32,
            mac: "2c:dd:e9:4a:1b:01".into(),
            mtu: 9100,
            speed_mbps: 1000,
            description: "uplink to core-1".into(),
            seconds_since_change: Some(3600),
            counters: Some(pb::InterfaceCounters {
                in_octets: 4_816_030_792_344,
                in_ucast_pkts: 4_294_811_034,
                in_mcast_pkts: 12_004,
                in_bcast_pkts: 881,
                in_discards: 3,
                in_errors: 1,
                out_octets: 2_118_030_792_344,
                out_ucast_pkts: 2_294_811_034,
                out_mcast_pkts: 9_004,
                out_bcast_pkts: 441,
                out_discards: 2,
                out_errors: 0,
                ..pb::InterfaceCounters::default()
            }),
            ..pb::InterfaceState::default()
        }
    }

    fn seed_mib() -> Vec<(Oid, Value)> {
        build_mib(&[
            interface("Ethernet1", "ethernet", 1),
            interface("Ethernet2", "ethernet", 2),
            interface("Management1", "management", 0),
            interface("Port-Channel1", "port-channel", 0),
            interface("Vlan99", "vlan", 0),
        ])
    }

    fn get(oid: &[u32]) -> Request {
        Request {
            header: Header::parse(&frame(PduType::Get, 1, 1, 1, Vec::new())).unwrap(),
            ranges: vec![SearchRange {
                start: oid.to_vec(),
                include: false,
                end: Vec::new(),
            }],
            non_repeaters: 0,
            max_repetitions: 0,
        }
    }

    fn get_next(oid: &[u32], include: bool) -> Request {
        Request {
            header: Header::parse(&frame(PduType::GetNext, 1, 1, 1, Vec::new())).unwrap(),
            ranges: vec![SearchRange {
                start: oid.to_vec(),
                include,
                end: Vec::new(),
            }],
            non_repeaters: 0,
            max_repetitions: 0,
        }
    }

    /// ifIndex bands never collide, and a front-panel port with no
    /// manifest index is skipped rather than aliased onto another.
    #[test]
    fn if_indexes_are_stable_and_disjoint() {
        assert_eq!(if_index("ethernet", "Ethernet1", 1), Some(1));
        assert_eq!(if_index("ethernet", "Ethernet49", 49), Some(49));
        assert_eq!(if_index("ethernet", "Ethernet1", 0), None);
        assert_eq!(if_index("management", "Management1", 0), Some(1001));
        assert_eq!(if_index("port-channel", "Port-Channel1", 0), Some(2001));
        assert_eq!(if_index("vlan", "Vlan99", 0), Some(10_099));
        assert_eq!(if_index("loopback", "Loopback0", 0), None);
        // The physical band tops out well below the next one.
        assert!(if_index("ethernet", "Ethernet52", 52).unwrap() < 1_000);
    }

    /// ifTable values come straight from the mock syncd counters.
    #[test]
    fn if_table_serves_syncd_counters() {
        let mib = seed_mib();
        let value = |oid: &[u32]| {
            let binds = answer(&mib, &get(oid));
            binds[0].value.clone()
        };
        // ifNumber counts every indexed interface.
        assert_eq!(value(&[1, 3, 6, 1, 2, 1, 2, 1, 0]), Value::Integer(5));
        // ifDescr/ifName, ifType, ifMtu, ifSpeed, ifOperStatus.
        assert_eq!(
            value(&[1, 3, 6, 1, 2, 1, 2, 2, 1, 2, 1]),
            Value::OctetString(b"Ethernet1".to_vec())
        );
        assert_eq!(value(&[1, 3, 6, 1, 2, 1, 2, 2, 1, 3, 1]), Value::Integer(6));
        assert_eq!(
            value(&[1, 3, 6, 1, 2, 1, 2, 2, 1, 4, 1]),
            Value::Integer(9100)
        );
        assert_eq!(
            value(&[1, 3, 6, 1, 2, 1, 2, 2, 1, 5, 1]),
            Value::Gauge32(1_000_000_000)
        );
        assert_eq!(
            value(&[1, 3, 6, 1, 2, 1, 2, 2, 1, 6, 1]),
            Value::OctetString(vec![0x2c, 0xdd, 0xe9, 0x4a, 0x1b, 0x01])
        );
        assert_eq!(value(&[1, 3, 6, 1, 2, 1, 2, 2, 1, 8, 1]), Value::Integer(1));
        // The 32-bit counters are the low half of the real ones...
        assert_eq!(
            value(&[1, 3, 6, 1, 2, 1, 2, 2, 1, 10, 1]),
            Value::Counter32((4_816_030_792_344u64 & 0xffff_ffff) as u32)
        );
        // ...and ifXTable carries the whole number.
        assert_eq!(
            value(&[1, 3, 6, 1, 2, 1, 31, 1, 1, 1, 6, 1]),
            Value::Counter64(4_816_030_792_344)
        );
        assert_eq!(
            value(&[1, 3, 6, 1, 2, 1, 31, 1, 1, 1, 15, 1]),
            Value::Gauge32(1000)
        );
        assert_eq!(
            value(&[1, 3, 6, 1, 2, 1, 31, 1, 1, 1, 18, 1]),
            Value::OctetString(b"uplink to core-1".to_vec())
        );
    }

    /// A down port reports operStatus down and an unparsable MAC an
    /// empty physical address, rather than lying either way.
    #[test]
    fn degraded_interfaces_render_honestly() {
        let mut iface = interface("Ethernet3", "ethernet", 3);
        iface.oper_status = pb::OperStatus::Down as i32;
        iface.admin_state = pb::AdminState::Down as i32;
        iface.mac = "not-a-mac".into();
        iface.speed_mbps = 0;
        let mib = build_mib(&[iface]);
        let value = |oid: &[u32]| answer(&mib, &get(oid))[0].value.clone();
        assert_eq!(value(&[1, 3, 6, 1, 2, 1, 2, 2, 1, 7, 3]), Value::Integer(2));
        assert_eq!(value(&[1, 3, 6, 1, 2, 1, 2, 2, 1, 8, 3]), Value::Integer(2));
        assert_eq!(
            value(&[1, 3, 6, 1, 2, 1, 2, 2, 1, 6, 3]),
            Value::OctetString(Vec::new())
        );
        assert_eq!(value(&[1, 3, 6, 1, 2, 1, 2, 2, 1, 5, 3]), Value::Gauge32(0));
    }

    /// Get distinguishes "no such instance" from "no such object".
    #[test]
    fn get_reports_the_right_exception() {
        let mib = seed_mib();
        // A real column, an index that does not exist.
        assert_eq!(
            answer(&mib, &get(&[1, 3, 6, 1, 2, 1, 2, 2, 1, 8, 77]))[0].value,
            Value::NoSuchInstance
        );
        // Nothing under this OID at all.
        assert_eq!(
            answer(&mib, &get(&[1, 3, 6, 1, 2, 1, 99, 1]))[0].value,
            Value::NoSuchObject
        );
    }

    /// A walk visits every OID once, in order, and ends cleanly.
    #[test]
    fn getnext_walks_the_whole_mib() {
        let mib = seed_mib();
        let mut oid: Oid = INTERFACES.to_vec();
        let mut visited: Vec<Oid> = Vec::new();
        loop {
            let binds = answer(&mib, &get_next(&oid, false));
            let bind = &binds[0];
            if bind.value == Value::EndOfMibView {
                break;
            }
            assert!(bind.name > oid, "walk went backwards at {oid:?}");
            visited.push(bind.name.clone());
            oid = bind.name.clone();
            assert!(visited.len() <= mib.len(), "walk did not terminate");
        }
        // Both tables are inside one contiguous walk from 1.3.6.1.2.1.2.
        assert_eq!(visited.len(), mib.len());
        assert_eq!(
            visited,
            mib.iter().map(|(oid, _)| oid.clone()).collect::<Vec<_>>()
        );

        // include=true returns the entry at the start OID itself.
        let first = &mib[0].0;
        let binds = answer(&mib, &get_next(first, true));
        assert_eq!(&binds[0].name, first);
    }

    /// A bounded range stops at its end rather than spilling past it.
    #[test]
    fn getnext_respects_the_range_end() {
        let mib = seed_mib();
        let request = Request {
            header: Header::parse(&frame(PduType::GetNext, 1, 1, 1, Vec::new())).unwrap(),
            ranges: vec![SearchRange {
                start: vec![1, 3, 6, 1, 2, 1, 2, 2, 1, 8, 2],
                include: false,
                end: vec![1, 3, 6, 1, 2, 1, 2, 2, 1, 9],
            }],
            non_repeaters: 0,
            max_repetitions: 0,
        };
        let binds = answer(&mib, &request);
        // The next ifOperStatus instance is inside the range...
        assert_eq!(binds[0].name, vec![1, 3, 6, 1, 2, 1, 2, 2, 1, 8, 1001]);
        // ...but walking past the last one ends the view.
        let request = Request {
            ranges: vec![SearchRange {
                start: vec![1, 3, 6, 1, 2, 1, 2, 2, 1, 8, 10_099],
                include: false,
                end: vec![1, 3, 6, 1, 2, 1, 2, 2, 1, 9],
            }],
            ..request
        };
        assert_eq!(answer(&mib, &request)[0].value, Value::EndOfMibView);
    }

    /// GetBulk repeats each range and honours non-repeaters.
    #[test]
    fn getbulk_repeats_and_terminates() {
        let mib = seed_mib();
        let request = Request {
            header: Header::parse(&frame(PduType::GetBulk, 1, 1, 1, Vec::new())).unwrap(),
            ranges: vec![SearchRange {
                start: vec![1, 3, 6, 1, 2, 1, 2, 2, 1, 1],
                include: false,
                end: Vec::new(),
            }],
            non_repeaters: 0,
            max_repetitions: 5,
        };
        let binds = answer(&mib, &request);
        assert_eq!(binds.len(), 5);
        // The five ifIndex instances, in index order.
        assert_eq!(
            binds.iter().map(|b| b.value.clone()).collect::<Vec<_>>(),
            vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(1001),
                Value::Integer(2001),
                Value::Integer(10_099),
            ]
        );
        // A bulk that runs off the end stops instead of spinning.
        let request = Request {
            ranges: vec![SearchRange {
                start: mib.last().unwrap().0.clone(),
                include: false,
                end: Vec::new(),
            }],
            max_repetitions: 50,
            ..request
        };
        let binds = answer(&mib, &request);
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].value, Value::EndOfMibView);
    }

    /// The counters `show snmp` prints track what the master
    /// dispatched, and a response is emitted for every request.
    #[test]
    fn requests_are_counted() {
        let engine = Engine::new();
        engine.store_mib(seed_mib());
        for _ in 0..3 {
            let reply = engine.serve(&get(&[1, 3, 6, 1, 2, 1, 2, 1, 0]));
            assert_eq!(Header::parse(&reply).unwrap().pdu_type, PduType::Response);
        }
        engine.serve(&get_next(&INTERFACES, false));
        engine.count_error();
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.packets_in, 4);
        assert_eq!(snapshot.packets_out, 4);
        assert_eq!(snapshot.get_requests, 3);
        assert_eq!(snapshot.getnext_requests, 1);
        assert_eq!(snapshot.errors, 1);
    }

    /// A scripted master: frame a Get the way net-snmp would, hand the
    /// bytes to the decoder, and read the response back off the wire.
    #[test]
    fn framing_round_trips_against_a_scripted_master() {
        let engine = Engine::new();
        engine.store_mib(seed_mib());

        // The master frames a Get for ifHCInOctets.1 (with the OID
        // prefix shorthand it really uses).
        let mut payload = vec![7, 2, 0, 0];
        for sub in [1u32, 31, 1, 1, 1, 6, 1] {
            payload.extend_from_slice(&sub.to_be_bytes());
        }
        payload.extend_from_slice(&[0, 0, 0, 0]);
        let wire = frame(PduType::Get, 42, 7, 99, payload);

        let header = Header::parse(&wire).unwrap();
        assert_eq!(header.payload_len as usize, wire.len() - HEADER_LEN);
        let request = agentx::parse_request(header, &wire[HEADER_LEN..]).unwrap();
        let reply = engine.serve(&request);

        // The master reads a Response echoing its ids...
        let header = Header::parse(&reply).unwrap();
        assert_eq!(header.pdu_type, PduType::Response);
        assert_eq!(
            (header.session_id, header.transaction_id, header.packet_id),
            (42, 7, 99)
        );
        assert_eq!(header.payload_len as usize, reply.len() - HEADER_LEN);
        // ...carrying sysUpTime, error 0, index 0, then the varbind.
        let body = &reply[HEADER_LEN..];
        assert_eq!(u16::from_be_bytes([body[4], body[5]]), agentx::ERROR_NONE);
        assert_eq!(u16::from_be_bytes([body[6], body[7]]), 0);
        // Counter64 (70), reserved, then the OID and the 8-byte value.
        assert_eq!(u16::from_be_bytes([body[8], body[9]]), 70);
        let value = &body[body.len() - 8..];
        assert_eq!(
            u64::from_be_bytes(value.try_into().unwrap()),
            4_816_030_792_344
        );
    }

    /// Disabling SNMP drops the claimed session immediately, so
    /// `show snmp` cannot report a live agent that is being torn down.
    #[test]
    fn disabling_drops_the_session() {
        let engine = Engine::new();
        engine.set_config(Config {
            enabled: true,
            socket: "/var/agentx/master".into(),
            ..Config::default()
        });
        engine.set_connected(true);
        assert!(engine.snapshot().connected);
        engine.set_config(Config::default());
        let snapshot = engine.snapshot();
        assert!(!snapshot.connected);
        assert!(!snapshot.config.enabled);
    }
}
