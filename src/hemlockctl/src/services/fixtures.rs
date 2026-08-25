//! Test fixtures behind the services-suite golden outputs: the state
//! the spec's seed configuration produces on a 52-port E1031.

use super::model::{LldpNeighbor, LldpPort, LldpState, NtpState};

fn neighbor(
    port: &str,
    chassis_id: &str,
    port_id: &str,
    system_name: &str,
    age_secs: u64,
) -> LldpNeighbor {
    LldpNeighbor {
        port: port.into(),
        chassis_id: chassis_id.into(),
        chassis_id_subtype: "mac".into(),
        port_id: port_id.into(),
        port_id_subtype: "interface-name".into(),
        port_description: String::new(),
        system_name: system_name.into(),
        system_description: String::new(),
        management_address: String::new(),
        ttl: 120,
        age_secs,
    }
}

fn port(
    name: &str,
    enabled: bool,
    counters: (u64, u64, u64, u64),
    neighbors: Vec<LldpNeighbor>,
) -> LldpPort {
    let (frames_tx, frames_rx, frames_discarded, ageouts) = counters;
    LldpPort {
        port: name.into(),
        enabled,
        frames_tx,
        frames_rx,
        frames_discarded,
        ageouts,
        neighbors,
    }
}

/// The LLDP state the seed produces: default timers, `lldp disable` on
/// Ethernet3, and three neighbors — an access point, a printer, and the
/// core switch on the 10G uplink (the one with the full TLV set).
pub fn lldp_state() -> LldpState {
    let uplink = LldpNeighbor {
        port_description: "uplink to hemlock".into(),
        system_description: "Arista Networks EOS version 4.32.1F".into(),
        management_address: "10.42.0.12".into(),
        age_secs: 12,
        ..neighbor(
            "Ethernet49",
            "2c:dd:e9:77:00:0c",
            "Ethernet12",
            "core-sw-01",
            12,
        )
    };
    LldpState {
        enabled: true,
        tx_interval: 30,
        hold_multiplier: 4,
        chassis_id: "2c:dd:e9:4a:1b:00".into(),
        system_name: "hemlock".into(),
        system_description: "Hemlock NOS version 0.1.0".into(),
        management_address: "10.42.0.9".into(),
        ports: vec![
            port(
                "Ethernet1",
                true,
                (18822, 18790, 0, 1),
                vec![neighbor(
                    "Ethernet1",
                    "b8:27:eb:41:0a:04",
                    "eth0",
                    "access-ap-04",
                    9,
                )],
            ),
            port(
                "Ethernet2",
                true,
                (18821, 18811, 0, 0),
                vec![neighbor(
                    "Ethernet2",
                    "00:1c:73:0c:aa:33",
                    "LAN",
                    "printer-3rdfloor",
                    21,
                )],
            ),
            port("Ethernet3", false, (0, 0, 0, 0), Vec::new()),
            port("Ethernet49", true, (18822, 18822, 0, 0), vec![uplink]),
        ],
    }
}

/// The NTP state the seed produces: two servers, synchronized to the
/// first at stratum 3.
pub fn ntp_state() -> NtpState {
    NtpState {
        enabled: true,
        servers: vec!["10.42.0.5".into(), "pool.ntp.org".into()],
        synchronized: true,
        server: "10.42.0.5".into(),
        stratum: 3,
        poll_interval_secs: 512,
        offset_usecs: -412,
        delay_usecs: 1204,
        jitter_usecs: 88,
        last_sync_secs_ago: Some(4 * 60 + 12),
    }
}
