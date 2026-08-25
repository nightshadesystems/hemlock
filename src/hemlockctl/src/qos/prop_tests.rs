//! Property tests for the QoS suite: DSCP/CoS list-and-range expansion
//! round-trips, shaper rate suffixes parse/format round-trip, the queue
//! table's layout holds for every program shape, and no renderer
//! panics. Deterministic xorshift generator, no external
//! property-testing dependency (the interfaces family's convention).
#![allow(clippy::unwrap_used)]

use super::model::*;
use super::render;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn u32(&mut self, bound: u32) -> u32 {
        (self.next() % u64::from(bound.max(1))) as u32
    }

    fn u8(&mut self, bound: u8) -> u8 {
        (self.next() % u64::from(bound.max(1))) as u8
    }

    fn u64(&mut self) -> u64 {
        self.next()
    }

    fn chance(&mut self) -> bool {
        self.next() & 1 == 0
    }
}

#[test]
fn value_lists_expand_and_compact_round_trip() {
    let mut rng = Rng(0x6473_6370_6c69_7374);
    for _ in 0..2000 {
        // A random DSCP set, rendered to its compact list form and read
        // back: `set qos map dscp-to-tc dscp 40-46,48 tc 5` must expand
        // to exactly the values it names.
        let mut values: Vec<u8> = (0..64u8).filter(|_| rng.u32(4) == 0).collect();
        if values.is_empty() {
            values.push(rng.u8(64));
        }
        values.sort_unstable();
        values.dedup();
        let text = hemlock_common::net::format_value_list(&values);
        assert_eq!(
            hemlock_common::net::parse_value_list(&text, 63, "dscp").unwrap(),
            values,
            "round trip failed for {text:?}"
        );
        // Compaction is canonical: re-rendering changes nothing.
        assert_eq!(hemlock_common::net::format_value_list(&values), text);
    }
}

#[test]
fn shaper_rates_parse_and_format_round_trip() {
    let mut rng = Rng(0x7368_6170_6572_2121);
    for _ in 0..2000 {
        // Rates land on the k/m/g boundaries the config language uses,
        // at or above the 64k granularity floor.
        let (scale, suffix) = match rng.u32(3) {
            0 => (1_000u64, "k"),
            1 => (1_000_000, "m"),
            _ => (1_000_000_000, "g"),
        };
        let units = u64::from(rng.u32(1000)) + 1;
        let bps = units * scale;
        if bps < hemlock_common::net::SHAPE_RATE_FLOOR_BPS {
            continue;
        }
        let text = format!("{units}{suffix}");
        let parsed = hemlock_common::net::parse_shape_rate(&text).unwrap();
        assert_eq!(parsed, bps);
        // The canonical form re-parses to the same rate, and the
        // display form never panics.
        let canonical = hemlock_common::net::format_shape_rate(parsed);
        assert_eq!(
            hemlock_common::net::parse_shape_rate(&canonical).unwrap(),
            parsed
        );
        assert!(!hemlock_common::net::display_shape_rate(parsed).is_empty());
    }
}

fn random_queue(rng: &mut Rng, queue: u8) -> QueueQos {
    let strict = rng.u32(4) == 0;
    QueueQos {
        queue,
        mode: if strict { "strict" } else { "dwrr" }.into(),
        weight: (!strict).then(|| u32::from(rng.u8(127)) + 1),
        shaper: rng.chance().then(|| {
            hemlock_common::net::display_shape_rate(u64::from(rng.u32(1000) + 1) * 1_000_000)
        }),
        wred_profile: rng.chance().then(|| "BULK".to_string()),
        ecn: rng.chance(),
        tx_packets: rng.u64(),
        tx_bytes: rng.u64(),
        dropped: rng.u64(),
        wred_dropped: rng.u64(),
        ecn_marked: rng.u64(),
    }
}

fn random_port(rng: &mut Rng, index: u32) -> PortQos {
    PortQos {
        port: format!("Ethernet{}", index + 1),
        trust: ["untrusted", "dscp", "cos"][rng.u32(3) as usize].into(),
        default_tc: rng.u8(8),
        shaper: rng.chance().then(|| {
            hemlock_common::net::display_shape_rate(u64::from(rng.u32(1000) + 1) * 1_000_000)
        }),
        queues: (0..8).map(|queue| random_queue(rng, queue)).collect(),
        configured: rng.u32(4) != 0,
        via_port_channel: rng.chance().then(|| "Port-Channel1".to_string()),
    }
}

/// The queue table's columns stay aligned whatever the program shape:
/// every row's cells start exactly where the header's columns do (a
/// cell wider than its field would push the rest along), and no line
/// carries trailing whitespace.
#[test]
fn queue_table_layout_holds_for_arbitrary_programs() {
    // The queue table's column starts: a two-space indent, then
    // Queue(7) Mode(8) Weight(8) Shaper(11) WRED(9), with ECN running
    // to end-of-line.
    const STARTS: [usize; 6] = [2, 9, 17, 25, 36, 45];
    let mut rng = Rng(0x7175_6575_655f_7462);
    for _ in 0..500 {
        let index = rng.u32(52);
        let state = PortQosState {
            ports: vec![random_port(&mut rng, index)],
            default_ports: rng.u32(52),
        };
        let text = render::interface(&state);
        let lines: Vec<&str> = text.lines().collect();
        let header = lines
            .iter()
            .position(|line| line.trim_start().starts_with("Queue"))
            .expect("queue table header");
        // The header, its rule, and one row per queue.
        assert_eq!(lines.len(), header + 2 + state.ports[0].queues.len());
        for line in &lines[header..] {
            let bytes = line.as_bytes();
            for (column, start) in STARTS.iter().enumerate() {
                assert!(
                    bytes.len() > *start,
                    "row {line:?} stops before column {column}"
                );
                assert_ne!(
                    bytes[*start], b' ',
                    "column {column} of {line:?} does not start at {start}"
                );
                if column > 0 {
                    assert_eq!(
                        bytes[*start - 1],
                        b' ',
                        "column {column} of {line:?} runs into the one before it"
                    );
                }
            }
            assert_eq!(line.trim_end(), *line, "trailing whitespace in {line:?}");
        }
    }
}

/// No renderer panics, whatever state syncd reports.
#[test]
fn renderers_never_panic() {
    let mut rng = Rng(0x716f_735f_7061_6e69);
    for _ in 0..500 {
        let ports: Vec<PortQos> = (0..rng.u32(6)).map(|i| random_port(&mut rng, i)).collect();
        let state = PortQosState {
            ports,
            default_ports: rng.u32(52),
        };
        let _ = render::interface(&state);
        let _ = render::interfaces(&state);
        let _ = render::wred(&WredState {
            profiles: vec![WredProfile {
                name: "BULK".into(),
                min_threshold: rng.u32(4096),
                max_threshold: rng.u32(4096),
                drop_probability: rng.u32(101),
                ecn: rng.chance(),
                references: Vec::new(),
            }],
            buffer_kb: rng.u32(8192),
            supported: rng.chance(),
        });
        let _ = render::maps(&MapState {
            tables: vec![MapTable {
                table: "dscp-to-tc".into(),
                title: "DSCP to Traffic-Class map".into(),
                key_label: "DSCP".into(),
                value_label: "TC".into(),
                default_note: "0".into(),
                entries: (0..rng.u32(8))
                    .map(|_| MapEntry {
                        key: rng.u8(64),
                        value: rng.u8(8),
                    })
                    .collect(),
            }],
        });
    }
}
