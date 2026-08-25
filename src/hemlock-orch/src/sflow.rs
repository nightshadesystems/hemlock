//! The sFlow exporter: hardware-sampled frames and periodic counter
//! polls into sFlow v5 datagrams, out to the collectors.
//!
//! The split is the same one the rest of the suite uses. syncd owns the
//! ASIC sampler (the samplepacket session and its port bindings) and
//! streams sampled frames up; orch owns the *export* — sequence
//! numbers, the agent identity, datagram packing and pacing, and the
//! UDP sockets. Nothing here touches SAI.
//!
//! Encoding is XDR: everything big-endian and padded to four bytes.
//! The builder is pure (`Datagram::encode`), so the wire format is
//! pinned byte-exactly by tests rather than by a collector's goodwill.
//!
//! Deliberate scope: ingress flow samples with a raw-packet-header
//! record, plus generic interface counter samples. Egress sampling is
//! out of scope for this suite, and extended flow records (router,
//! switch, gateway) need forwarding context orch does not have.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use tokio::time::{Duration, Instant};

/// The most header bytes a flow sample carries. 128 covers L2 through
/// the transport header of any normal frame, which is all a collector
/// classifies on.
pub const MAX_HEADER_BYTES: usize = 128;

/// sFlow's default collector port.
pub const DEFAULT_COLLECTOR_PORT: u16 = 6343;

/// Samples are packed until the datagram would exceed this, so it fits
/// one ordinary-MTU UDP payload with room to spare.
const MAX_DATAGRAM_BYTES: usize = 1400;

/// How long a partly-filled datagram waits for company before it is
/// sent anyway.
const FLUSH_AFTER: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// False = no collector configured, so nothing is exported.
    pub enabled: bool,
    pub collectors: Vec<SocketAddr>,
    /// 1-in-N.
    pub sample_rate: u32,
    /// Seconds between counter polls.
    pub polling_interval: u32,
    /// The agent address the datagrams carry (Management1).
    pub agent_address: Ipv4Addr,
    /// The interface that address belongs to, for display.
    pub agent_interface: String,
    /// Ports sampling is enabled on, for display.
    pub enabled_ports: Vec<String>,
    /// Ports carrying `sflow disable`, for display.
    pub disabled_ports: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            collectors: Vec::new(),
            sample_rate: 0,
            polling_interval: 0,
            // Unspecified until syncd reports the management address;
            // a datagram carrying it names no agent, which is exactly
            // what an unconfigured exporter should say.
            agent_address: Ipv4Addr::UNSPECIFIED,
            agent_interface: String::new(),
            enabled_ports: Vec::new(),
            disabled_ports: Vec::new(),
        }
    }
}

/// One sampled frame, as syncd delivered it.
#[derive(Debug, Clone)]
pub struct Sample {
    /// ifIndex of the ingress port (the port name is resolved to it
    /// before the sample gets here).
    pub if_index: u32,
    /// The frame's length on the wire.
    pub original_length: u32,
    pub bytes: Vec<u8>,
}

/// One port's generic interface counters, from the same syncd view the
/// SNMP subagent reads.
#[derive(Debug, Clone, Default)]
pub struct InterfaceCounters {
    pub if_index: u32,
    pub if_type: u32,
    pub if_speed_bps: u64,
    /// True = full duplex.
    pub full_duplex: bool,
    pub admin_up: bool,
    pub oper_up: bool,
    pub in_octets: u64,
    pub in_ucast_pkts: u32,
    pub in_mcast_pkts: u32,
    pub in_bcast_pkts: u32,
    pub in_discards: u32,
    pub in_errors: u32,
    pub out_octets: u64,
    pub out_ucast_pkts: u32,
    pub out_mcast_pkts: u32,
    pub out_bcast_pkts: u32,
    pub out_discards: u32,
    pub out_errors: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub config: Config,
    pub samples_taken: u64,
    pub counter_samples: u64,
    pub datagrams_sent: u64,
    pub datagrams_failed: u64,
}

// ------------------------------------------------------------ encoding

/// An XDR writer: every field is big-endian and four-byte aligned.
#[derive(Debug, Default)]
struct Xdr {
    out: Vec<u8>,
}

impl Xdr {
    fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    /// Opaque bytes: the length, the bytes, then padding to four.
    fn opaque(&mut self, bytes: &[u8]) {
        self.u32(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
        self.out.extend_from_slice(bytes);
        let padding = (4 - bytes.len() % 4) % 4;
        self.out.extend(std::iter::repeat_n(0u8, padding));
    }

    /// Reserve a length slot, run `body`, then backfill the byte count.
    fn sized(&mut self, body: impl FnOnce(&mut Self)) {
        let slot = self.out.len();
        self.u32(0);
        let start = self.out.len();
        body(self);
        let length = u32::try_from(self.out.len() - start).unwrap_or(0);
        self.out[slot..slot + 4].copy_from_slice(&length.to_be_bytes());
    }
}

/// One sample inside a datagram.
#[derive(Debug, Clone)]
pub enum Record {
    Flow {
        sequence: u32,
        if_index: u32,
        sampling_rate: u32,
        /// Packets the sampler has seen on this source.
        sample_pool: u32,
        drops: u32,
        frame_length: u32,
        header: Vec<u8>,
    },
    Counters {
        sequence: u32,
        counters: InterfaceCounters,
    },
}

impl Record {
    fn encode(&self, xdr: &mut Xdr) {
        match self {
            Record::Flow {
                sequence,
                if_index,
                sampling_rate,
                sample_pool,
                drops,
                frame_length,
                header,
            } => {
                // sample_type 1 = flow sample (enterprise 0).
                xdr.u32(1);
                xdr.sized(|xdr| {
                    xdr.u32(*sequence);
                    // source_id: type 0 (ifIndex) in the top byte.
                    xdr.u32(if_index & 0x00ff_ffff);
                    xdr.u32(*sampling_rate);
                    xdr.u32(*sample_pool);
                    xdr.u32(*drops);
                    xdr.u32(*if_index);
                    // Egress interface is unknown at sampling time.
                    xdr.u32(0);
                    xdr.u32(1); // one flow record
                    xdr.u32(1); // data format 1 = raw packet header
                    xdr.sized(|xdr| {
                        xdr.u32(1); // header protocol 1 = ethernet
                        xdr.u32(*frame_length);
                        // Bytes removed from the original: the FCS.
                        xdr.u32(4);
                        xdr.opaque(header);
                    });
                });
            }
            Record::Counters { sequence, counters } => {
                // sample_type 2 = counters sample.
                xdr.u32(2);
                xdr.sized(|xdr| {
                    xdr.u32(*sequence);
                    xdr.u32(counters.if_index & 0x00ff_ffff);
                    xdr.u32(1); // one counter record
                    xdr.u32(1); // data format 1 = generic interface
                    xdr.sized(|xdr| {
                        xdr.u32(counters.if_index);
                        xdr.u32(counters.if_type);
                        xdr.u64(counters.if_speed_bps);
                        // ifDirection: 1 full-duplex, 2 half-duplex.
                        xdr.u32(if counters.full_duplex { 1 } else { 2 });
                        // ifStatus: bit 0 admin up, bit 1 oper up.
                        let status =
                            u32::from(counters.admin_up) | (u32::from(counters.oper_up) << 1);
                        xdr.u32(status);
                        xdr.u64(counters.in_octets);
                        xdr.u32(counters.in_ucast_pkts);
                        xdr.u32(counters.in_mcast_pkts);
                        xdr.u32(counters.in_bcast_pkts);
                        xdr.u32(counters.in_discards);
                        xdr.u32(counters.in_errors);
                        // ifInUnknownProtos: the ASIC does not count it.
                        xdr.u32(0);
                        xdr.u64(counters.out_octets);
                        xdr.u32(counters.out_ucast_pkts);
                        xdr.u32(counters.out_mcast_pkts);
                        xdr.u32(counters.out_bcast_pkts);
                        xdr.u32(counters.out_discards);
                        xdr.u32(counters.out_errors);
                        // ifPromiscuousMode: never on a switch port.
                        xdr.u32(0);
                    });
                });
            }
        }
    }

    /// The bytes this record adds to a datagram — used for packing
    /// without re-encoding.
    fn encoded_len(&self) -> usize {
        let mut xdr = Xdr::default();
        self.encode(&mut xdr);
        xdr.out.len()
    }
}

/// One sFlow v5 datagram.
#[derive(Debug, Clone)]
pub struct Datagram {
    pub agent_address: Ipv4Addr,
    pub sub_agent_id: u32,
    pub sequence: u32,
    /// Milliseconds since the agent started.
    pub uptime_ms: u32,
    pub records: Vec<Record>,
}

impl Datagram {
    pub fn encode(&self) -> Vec<u8> {
        let mut xdr = Xdr::default();
        xdr.u32(5); // version
        xdr.u32(1); // agent address type 1 = IPv4
        xdr.out.extend_from_slice(&self.agent_address.octets());
        xdr.u32(self.sub_agent_id);
        xdr.u32(self.sequence);
        xdr.u32(self.uptime_ms);
        xdr.u32(u32::try_from(self.records.len()).unwrap_or(0));
        for record in &self.records {
            record.encode(&mut xdr);
        }
        xdr.out
    }
}

/// The fixed datagram header: version, address type, address, sub-agent,
/// sequence, uptime, sample count.
const HEADER_BYTES: usize = 7 * 4;

// -------------------------------------------------------------- engine

#[derive(Debug, Default)]
struct Counters {
    samples_taken: u64,
    counter_samples: u64,
    datagrams_sent: u64,
    datagrams_failed: u64,
}

struct Inner {
    config: Config,
    counters: Counters,
    /// Datagram sequence number (per agent, not per collector).
    sequence: u32,
    /// Per-source flow-sample sequence numbers, keyed by ifIndex.
    flow_sequences: BTreeMap<u32, u32>,
    /// Per-source counter-sample sequence numbers.
    counter_sequences: BTreeMap<u32, u32>,
    /// Packets the sampler is estimated to have seen per source — the
    /// `sample_pool` a collector scales by.
    sample_pool: BTreeMap<u32, u32>,
    /// Records waiting to be packed into a datagram.
    pending: Vec<Record>,
    pending_bytes: usize,
    first_pending: Option<Instant>,
    started: Instant,
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
                config: Config::default(),
                counters: Counters::default(),
                sequence: 0,
                flow_sequences: BTreeMap::new(),
                counter_sequences: BTreeMap::new(),
                sample_pool: BTreeMap::new(),
                pending: Vec::new(),
                pending_bytes: HEADER_BYTES,
                first_pending: None,
                started: Instant::now(),
            })),
        }
    }

    /// Replace the configuration (declarative). Turning sFlow off
    /// discards anything still queued: a collector that comes back
    /// wants current samples, not a backlog.
    pub fn set_config(&self, config: Config) {
        if let Ok(mut inner) = self.inner.lock() {
            if !config.enabled {
                inner.pending.clear();
                inner.pending_bytes = HEADER_BYTES;
                inner.first_pending = None;
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
            samples_taken: inner.counters.samples_taken,
            counter_samples: inner.counters.counter_samples,
            datagrams_sent: inner.counters.datagrams_sent,
            datagrams_failed: inner.counters.datagrams_failed,
        }
    }

    /// Queue one hardware sample. Returns the datagram to send when
    /// this sample filled one.
    pub fn take_sample(&self, sample: &Sample) -> Option<Datagram> {
        let Ok(mut inner) = self.inner.lock() else {
            return None;
        };
        if !inner.config.enabled {
            return None;
        }
        inner.counters.samples_taken += 1;
        let sequence = next(&mut inner.flow_sequences, sample.if_index);
        let rate = inner.config.sample_rate.max(1);
        // Each sample stands for `rate` packets; the pool is the
        // collector's scaling factor.
        let pool = {
            let entry = inner.sample_pool.entry(sample.if_index).or_insert(0);
            *entry = entry.wrapping_add(rate);
            *entry
        };
        let header: Vec<u8> = sample
            .bytes
            .iter()
            .copied()
            .take(MAX_HEADER_BYTES)
            .collect();
        let record = Record::Flow {
            sequence,
            if_index: sample.if_index,
            sampling_rate: rate,
            sample_pool: pool,
            drops: 0,
            frame_length: sample.original_length,
            header,
        };
        push(&mut inner, record)
    }

    /// Queue one poll's worth of counter samples, returning whatever
    /// datagrams they filled.
    pub fn take_counters(&self, counters: &[InterfaceCounters]) -> Vec<Datagram> {
        let Ok(mut inner) = self.inner.lock() else {
            return Vec::new();
        };
        if !inner.config.enabled {
            return Vec::new();
        }
        let mut ready = Vec::new();
        for entry in counters {
            inner.counters.counter_samples += 1;
            let sequence = next(&mut inner.counter_sequences, entry.if_index);
            let record = Record::Counters {
                sequence,
                counters: entry.clone(),
            };
            if let Some(datagram) = push(&mut inner, record) {
                ready.push(datagram);
            }
        }
        ready
    }

    /// The datagram a partly-filled queue owes once it has waited long
    /// enough — the pacing half of the exporter.
    pub fn flush_due(&self, now: Instant) -> Option<Datagram> {
        let Ok(mut inner) = self.inner.lock() else {
            return None;
        };
        let waited = inner
            .first_pending
            .is_some_and(|at| now.saturating_duration_since(at) >= FLUSH_AFTER);
        if !waited || inner.pending.is_empty() {
            return None;
        }
        Some(seal(&mut inner))
    }

    fn count_send(&self, ok: bool) {
        if let Ok(mut inner) = self.inner.lock() {
            if ok {
                inner.counters.datagrams_sent += 1;
            } else {
                inner.counters.datagrams_failed += 1;
            }
        }
    }
}

fn next(sequences: &mut BTreeMap<u32, u32>, key: u32) -> u32 {
    let entry = sequences.entry(key).or_insert(0);
    *entry = entry.wrapping_add(1);
    *entry
}

/// Queue a record, sealing a datagram when the next one would not fit.
fn push(inner: &mut Inner, record: Record) -> Option<Datagram> {
    let size = record.encoded_len();
    let ready = if inner.pending_bytes + size > MAX_DATAGRAM_BYTES && !inner.pending.is_empty() {
        Some(seal(inner))
    } else {
        None
    };
    if inner.pending.is_empty() {
        inner.first_pending = Some(Instant::now());
    }
    inner.pending_bytes += size;
    inner.pending.push(record);
    ready
}

/// Take everything queued as one datagram.
fn seal(inner: &mut Inner) -> Datagram {
    inner.sequence = inner.sequence.wrapping_add(1);
    let uptime_ms = u32::try_from(
        Instant::now()
            .saturating_duration_since(inner.started)
            .as_millis(),
    )
    .unwrap_or(u32::MAX);
    let datagram = Datagram {
        agent_address: inner.config.agent_address,
        sub_agent_id: 0,
        sequence: inner.sequence,
        uptime_ms,
        records: std::mem::take(&mut inner.pending),
    };
    inner.pending_bytes = HEADER_BYTES;
    inner.first_pending = None;
    datagram
}

/// Send one datagram to every configured collector, counting the
/// result. A collector that refuses the write counts as failed and the
/// others still get their copy.
pub async fn export(engine: &Engine, socket: &tokio::net::UdpSocket, datagram: &Datagram) {
    let collectors = engine.config().collectors;
    let bytes = datagram.encode();
    for collector in collectors {
        let ok = socket.send_to(&bytes, collector).await.is_ok();
        engine.count_send(ok);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            enabled: true,
            collectors: vec![
                "10.42.0.20:6343".parse().unwrap(),
                "10.42.0.21:6344".parse().unwrap(),
            ],
            sample_rate: 16384,
            polling_interval: 30,
            agent_address: Ipv4Addr::new(10, 42, 0, 9),
            agent_interface: "Management1".into(),
            enabled_ports: vec!["Ethernet1".into()],
            disabled_ports: vec!["Ethernet4".into()],
        }
    }

    /// A minimal sampled frame: an IPv4/TCP packet, the shape a
    /// collector classifies on.
    fn frame() -> Vec<u8> {
        let mut frame = Vec::new();
        frame.extend_from_slice(&[0x2c, 0xdd, 0xe9, 0x77, 0x00, 0x0c]); // dst
        frame.extend_from_slice(&[0x00, 0x1c, 0x73, 0x0c, 0xaa, 0x01]); // src
        frame.extend_from_slice(&[0x08, 0x00]); // IPv4
        frame.extend_from_slice(&[0x45, 0x00, 0x00, 0x3c]);
        frame.extend_from_slice(&[0x1c, 0x46, 0x40, 0x00, 0x40, 0x06, 0x00, 0x00]);
        frame.extend_from_slice(&[10, 0, 10, 101]);
        frame.extend_from_slice(&[10, 42, 0, 5]);
        frame.extend_from_slice(&[0xc3, 0x50, 0x01, 0xbb]);
        frame
    }

    fn engine() -> Engine {
        let engine = Engine::new();
        engine.set_config(config());
        engine
    }

    /// The datagram header is the seven fixed XDR words the v5 spec
    /// names, in order.
    #[test]
    fn datagram_header_is_byte_exact() {
        let datagram = Datagram {
            agent_address: Ipv4Addr::new(10, 42, 0, 9),
            sub_agent_id: 0,
            sequence: 7,
            uptime_ms: 1_234_567,
            records: Vec::new(),
        };
        let bytes = datagram.encode();
        assert_eq!(bytes.len(), HEADER_BYTES);
        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            0, 0, 0, 5,             // version 5
            0, 0, 0, 1,             // address type IPv4
            10, 42, 0, 9,           // agent address
            0, 0, 0, 0,             // sub-agent id
            0, 0, 0, 7,             // datagram sequence
            0, 0x12, 0xd6, 0x87,    // uptime 1234567 ms
            0, 0, 0, 0,             // sample count
        ];
        assert_eq!(bytes, expected);
    }

    /// One flow sample, encoded field by field against the v5 layout.
    #[test]
    fn flow_samples_are_byte_exact() {
        let header = frame();
        let record = Record::Flow {
            sequence: 1,
            if_index: 1,
            sampling_rate: 16384,
            sample_pool: 16384,
            drops: 0,
            frame_length: 1518,
            header: header.clone(),
        };
        let mut xdr = Xdr::default();
        record.encode(&mut xdr);
        let bytes = xdr.out;

        let word = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
        assert_eq!(word(0), 1, "sample type = flow sample");
        assert_eq!(word(4) as usize, bytes.len() - 8, "length covers the body");
        assert_eq!(word(8), 1, "flow sequence");
        assert_eq!(word(12), 1, "source id (ifIndex, type 0)");
        assert_eq!(word(16), 16384, "sampling rate");
        assert_eq!(word(20), 16384, "sample pool");
        assert_eq!(word(24), 0, "drops");
        assert_eq!(word(28), 1, "input ifIndex");
        assert_eq!(word(32), 0, "output ifIndex (unknown)");
        assert_eq!(word(36), 1, "one flow record");
        assert_eq!(word(40), 1, "data format = raw packet header");
        assert_eq!(word(48), 1, "header protocol = ethernet");
        assert_eq!(word(52), 1518, "frame length on the wire");
        assert_eq!(word(56), 4, "stripped bytes (FCS)");
        assert_eq!(word(60) as usize, header.len(), "header length");
        assert_eq!(&bytes[64..64 + header.len()], &header[..]);
        // Everything is four-byte aligned.
        assert_eq!(bytes.len() % 4, 0);
    }

    /// The generic counter record is exactly the 88 bytes the v5 spec
    /// defines — a collector reads it positionally.
    #[test]
    fn counter_samples_are_byte_exact() {
        let counters = InterfaceCounters {
            if_index: 1,
            if_type: 6,
            if_speed_bps: 1_000_000_000,
            full_duplex: true,
            admin_up: true,
            oper_up: true,
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
        };
        let mut xdr = Xdr::default();
        Record::Counters {
            sequence: 3,
            counters: counters.clone(),
        }
        .encode(&mut xdr);
        let bytes = xdr.out;
        let word = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
        let long = |at: usize| u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap());

        assert_eq!(word(0), 2, "sample type = counters sample");
        assert_eq!(word(8), 3, "counter sequence");
        assert_eq!(word(12), 1, "source id");
        assert_eq!(word(16), 1, "one counter record");
        assert_eq!(word(20), 1, "data format = generic interface");
        assert_eq!(word(24), 88, "generic counters are 88 bytes");
        assert_eq!(word(28), 1, "ifIndex");
        assert_eq!(word(32), 6, "ifType ethernetCsmacd");
        assert_eq!(long(36), 1_000_000_000, "ifSpeed");
        assert_eq!(word(44), 1, "ifDirection full-duplex");
        assert_eq!(word(48), 3, "ifStatus admin+oper up");
        assert_eq!(long(52), 4_816_030_792_344, "ifInOctets");
        assert_eq!(word(60), 4_294_811_034, "ifInUcastPkts");
        assert_eq!(long(84), 2_118_030_792_344, "ifOutOctets");
        // 8 (sample header) + 12 (sample body) + 8 (record header) + 88.
        assert_eq!(bytes.len(), 8 + 12 + 8 + 88);
    }

    /// Long frames are truncated to the header cap, and the wire length
    /// is still reported in full — that is how a collector scales bytes.
    #[test]
    fn oversized_headers_truncate_but_keep_the_wire_length() {
        let engine = engine();
        let sample = Sample {
            if_index: 1,
            original_length: 9000,
            bytes: vec![0xab; 9000],
        };
        engine.take_sample(&sample);
        let datagram = engine.flush_due(Instant::now() + FLUSH_AFTER).unwrap();
        let Record::Flow {
            frame_length,
            header,
            ..
        } = &datagram.records[0]
        else {
            panic!("expected a flow sample");
        };
        assert_eq!(*frame_length, 9000);
        assert_eq!(header.len(), MAX_HEADER_BYTES);
    }

    /// Sequence numbers advance per source, and the sample pool grows
    /// by the rate for every sample taken.
    #[test]
    fn sequences_and_pools_advance_per_source() {
        let engine = engine();
        for _ in 0..3 {
            engine.take_sample(&Sample {
                if_index: 1,
                original_length: 64,
                bytes: frame(),
            });
        }
        engine.take_sample(&Sample {
            if_index: 2,
            original_length: 64,
            bytes: frame(),
        });
        let datagram = engine.flush_due(Instant::now() + FLUSH_AFTER).unwrap();
        let sequences: Vec<(u32, u32, u32)> = datagram
            .records
            .iter()
            .map(|record| match record {
                Record::Flow {
                    sequence,
                    if_index,
                    sample_pool,
                    ..
                } => (*if_index, *sequence, *sample_pool),
                _ => panic!("expected flow samples"),
            })
            .collect();
        assert_eq!(
            sequences,
            vec![(1, 1, 16384), (1, 2, 32768), (1, 3, 49152), (2, 1, 16384),]
        );
        // The datagram sequence is per agent, and starts at 1.
        assert_eq!(datagram.sequence, 1);
        assert_eq!(engine.snapshot().samples_taken, 4);
    }

    /// A datagram seals when the next record would overflow the MTU
    /// budget, and every sealed datagram stays inside it.
    #[test]
    fn datagrams_pack_up_to_the_mtu_budget() {
        let engine = engine();
        let mut sealed = Vec::new();
        for _ in 0..40 {
            if let Some(datagram) = engine.take_sample(&Sample {
                if_index: 1,
                original_length: 1518,
                bytes: vec![0x5a; MAX_HEADER_BYTES],
            }) {
                sealed.push(datagram);
            }
        }
        assert!(!sealed.is_empty(), "nothing sealed at 40 full-size samples");
        for datagram in &sealed {
            assert!(
                datagram.encode().len() <= MAX_DATAGRAM_BYTES,
                "datagram over budget: {} bytes",
                datagram.encode().len()
            );
            assert_eq!(datagram.records.len() as u32, {
                let bytes = datagram.encode();
                u32::from_be_bytes(bytes[24..28].try_into().unwrap())
            });
        }
        // Sequence numbers are consecutive across sealed datagrams.
        let numbers: Vec<u32> = sealed.iter().map(|d| d.sequence).collect();
        assert_eq!(numbers, (1..=numbers.len() as u32).collect::<Vec<_>>());
    }

    /// A partly-filled datagram waits, then goes out on its own.
    #[test]
    fn pacing_flushes_a_partial_datagram() {
        let engine = engine();
        assert!(
            engine.flush_due(Instant::now()).is_none(),
            "nothing queued yet"
        );
        engine.take_sample(&Sample {
            if_index: 1,
            original_length: 64,
            bytes: frame(),
        });
        // The clock starts when the first record is queued, so `now`
        // has to be read after it.
        let now = Instant::now();
        assert!(engine.flush_due(now).is_none(), "flushed too early");
        let datagram = engine.flush_due(now + FLUSH_AFTER).unwrap();
        assert_eq!(datagram.records.len(), 1);
        // ...and the queue is empty again.
        assert!(engine.flush_due(now + FLUSH_AFTER * 4).is_none());
    }

    /// Counter polling produces one sample per port, counted
    /// separately from flow samples.
    #[test]
    fn counter_polls_are_counted_separately() {
        let engine = engine();
        let counters: Vec<InterfaceCounters> = (1..=3)
            .map(|if_index| InterfaceCounters {
                if_index,
                if_type: 6,
                if_speed_bps: 1_000_000_000,
                full_duplex: true,
                admin_up: true,
                oper_up: true,
                ..InterfaceCounters::default()
            })
            .collect();
        engine.take_counters(&counters);
        engine.take_counters(&counters);
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.counter_samples, 6);
        assert_eq!(snapshot.samples_taken, 0);
        let datagram = engine.flush_due(Instant::now() + FLUSH_AFTER).unwrap();
        assert_eq!(datagram.records.len(), 6);
        // Per-source counter sequences advance independently.
        let sequences: Vec<(u32, u32)> = datagram
            .records
            .iter()
            .map(|record| match record {
                Record::Counters { sequence, counters } => (counters.if_index, *sequence),
                _ => panic!("expected counter samples"),
            })
            .collect();
        assert_eq!(
            sequences,
            vec![(1, 1), (2, 1), (3, 1), (1, 2), (2, 2), (3, 2)]
        );
    }

    /// Disabled sFlow exports nothing and drops whatever was queued —
    /// a collector coming back wants live samples, not a backlog.
    #[test]
    fn disabling_stops_export_and_clears_the_queue() {
        let engine = engine();
        engine.take_sample(&Sample {
            if_index: 1,
            original_length: 64,
            bytes: frame(),
        });
        engine.set_config(Config::default());
        assert!(engine.flush_due(Instant::now() + FLUSH_AFTER).is_none());
        assert!(engine
            .take_sample(&Sample {
                if_index: 1,
                original_length: 64,
                bytes: frame(),
            })
            .is_none());
        assert!(engine
            .take_counters(&[InterfaceCounters::default()])
            .is_empty());
        // The sample that was already taken still counted.
        assert_eq!(engine.snapshot().samples_taken, 1);
    }
}
