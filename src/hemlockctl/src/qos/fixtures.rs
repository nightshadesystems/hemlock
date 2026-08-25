//! Test fixtures behind the QoS-suite golden outputs: the state the
//! spec's seed configuration produces on a 52-port E1031.

use super::model::{
    MapEntry, MapState, MapTable, PortQos, PortQosState, QueueQos, WredProfile, WredState,
};

fn table(
    table: &str,
    title: &str,
    key: &str,
    value: &str,
    note: &str,
    entries: &[(u8, u8)],
) -> MapTable {
    MapTable {
        table: table.into(),
        title: title.into(),
        key_label: key.into(),
        value_label: value.into(),
        default_note: note.into(),
        entries: entries
            .iter()
            .map(|(key, value)| MapEntry {
                key: *key,
                value: *value,
            })
            .collect(),
    }
}

/// The seed's four global maps: EF/AF31/CS1 into their traffic classes,
/// the matching CoS classification, and the rewrite maps back out.
pub fn map_state() -> MapState {
    MapState {
        tables: vec![
            table(
                "dscp-to-tc",
                "DSCP to Traffic-Class map",
                "DSCP",
                "TC",
                "0",
                &[(8, 1), (26, 3), (46, 5)],
            ),
            table(
                "cos-to-tc",
                "CoS to Traffic-Class map",
                "CoS",
                "TC",
                "0",
                &[(3, 3), (5, 5)],
            ),
            table(
                "tc-to-dscp",
                "Traffic-Class to DSCP rewrite map",
                "TC",
                "DSCP",
                "no rewrite",
                &[(3, 26), (5, 46)],
            ),
            table(
                "tc-to-cos",
                "Traffic-Class to CoS rewrite map",
                "TC",
                "CoS",
                "no rewrite",
                &[(3, 3), (5, 5)],
            ),
        ],
    }
}

/// The seed's one WRED profile, referenced by Ethernet1's queue 3.
pub fn wred_state() -> WredState {
    WredState {
        profiles: vec![WredProfile {
            name: "BULK".into(),
            min_threshold: 64,
            max_threshold: 256,
            drop_probability: 10,
            ecn: true,
            references: vec!["Et1 (q3)".into()],
        }],
        // The Helix4's 4 MB shared packet buffer.
        buffer_kb: 4096,
        supported: true,
    }
}

/// A queue at the platform default: DWRR, weight 1, unshaped, no WRED.
fn default_queue(queue: u8) -> QueueQos {
    QueueQos {
        queue,
        mode: "dwrr".into(),
        weight: Some(1),
        shaper: None,
        wred_profile: None,
        ecn: false,
        tx_packets: 0,
        tx_bytes: 0,
        dropped: 0,
        wred_dropped: 0,
        ecn_marked: 0,
    }
}

fn default_queues() -> Vec<QueueQos> {
    (0..8).map(default_queue).collect()
}

/// Ethernet1: DSCP-trusting, default TC 1, a strict top queue, a
/// weighted-and-shaped queue 5, and a WRED-profiled queue 3.
pub fn ethernet1() -> PortQos {
    let mut queues = default_queues();
    queues[7] = QueueQos {
        mode: "strict".into(),
        weight: None,
        tx_packets: 77_812,
        tx_bytes: 8_812_231,
        ..default_queue(7)
    };
    queues[5] = QueueQos {
        weight: Some(40),
        shaper: Some("100 Mbps".into()),
        tx_packets: 421_900,
        tx_bytes: 530_122_831,
        ..default_queue(5)
    };
    queues[3] = QueueQos {
        weight: Some(30),
        wred_profile: Some("BULK".into()),
        ecn: true,
        tx_packets: 88_123,
        tx_bytes: 101_233_911,
        dropped: 1204,
        wred_dropped: 1187,
        ecn_marked: 3320,
        ..default_queue(3)
    };
    queues[1] = QueueQos {
        tx_packets: 9_912,
        tx_bytes: 1_288_812,
        ..default_queue(1)
    };
    queues[0] = QueueQos {
        tx_packets: 182_231,
        tx_bytes: 23_310_021,
        ..default_queue(0)
    };
    PortQos {
        port: "Ethernet1".into(),
        trust: "dscp".into(),
        default_tc: 1,
        shaper: None,
        queues,
        configured: true,
        via_port_channel: None,
    }
}

/// Ethernet2: an explicitly-configured port sitting at the defaults —
/// it appears in the summary grid, unlike an unconfigured one.
pub fn ethernet2() -> PortQos {
    PortQos {
        port: "Ethernet2".into(),
        trust: "untrusted".into(),
        default_tc: 0,
        shaper: None,
        queues: default_queues(),
        configured: true,
        via_port_channel: None,
    }
}

/// Ethernet49: a Port-Channel1 member carrying the LAG's program. The
/// summary grid folds it into the Po1 row.
pub fn ethernet49() -> PortQos {
    PortQos {
        port: "Ethernet49".into(),
        trust: "dscp".into(),
        default_tc: 0,
        shaper: Some("800 Mbps".into()),
        queues: default_queues(),
        configured: true,
        via_port_channel: Some("Port-Channel1".into()),
    }
}

/// Port-Channel1: DSCP-trusting with an 800 Mbps port shaper.
pub fn port_channel1() -> PortQos {
    PortQos {
        port: "Port-Channel1".into(),
        trust: "dscp".into(),
        default_tc: 0,
        shaper: Some("800 Mbps".into()),
        queues: default_queues(),
        configured: true,
        via_port_channel: None,
    }
}

/// `show qos interface Ethernet1`.
pub fn interface_state() -> PortQosState {
    PortQosState {
        ports: vec![ethernet1()],
        default_ports: 49,
    }
}

/// `show qos interfaces`: three configured rows over a 52-port board,
/// leaving 49 ports at the defaults.
pub fn interfaces_state() -> PortQosState {
    PortQosState {
        ports: vec![ethernet1(), ethernet2(), ethernet49(), port_channel1()],
        default_ports: 49,
    }
}
