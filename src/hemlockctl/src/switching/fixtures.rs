//! Test fixtures behind the switching-suite golden outputs: the state
//! the spec's seed configuration produces.

use crate::interfaces::name::{InterfaceId, Kind};

use super::model::{
    LacpSystem, MacEntry, MacTable, MirrorSession, PortChannel, PortChannelMember, SnoopGroupView,
    SnoopVlanView, SnoopingView, StormRow, StpBridge, StpPortRow, VlanRow,
};

fn et(num: u32) -> InterfaceId {
    InterfaceId::new(Kind::Ethernet, num)
}

fn po(num: u32) -> InterfaceId {
    InterfaceId::new(Kind::PortChannel, num)
}

fn secs(hours: u64, minutes: u64, seconds: u64) -> u64 {
    hours * 3_600 + minutes * 60 + seconds
}

/// The VLAN table: Et1 access 10, Et2 a trunk carrying everything,
/// Et3-5 default access, Po1 trunking the user VLANs, VLAN 20 suspended.
pub fn vlans() -> Vec<VlanRow> {
    vec![
        VlanRow {
            id: 1,
            name: None,
            suspended: false,
            ports: vec![et(2), et(3), et(4), et(5), po(1)],
        },
        VlanRow {
            id: 10,
            name: Some("LAN-USERS".into()),
            suspended: false,
            ports: vec![et(1), et(2), po(1)],
        },
        VlanRow {
            id: 20,
            name: Some("VOICE".into()),
            suspended: true,
            ports: vec![et(2), po(1)],
        },
        VlanRow {
            id: 30,
            name: None,
            suspended: false,
            ports: vec![et(2), po(1)],
        },
        VlanRow {
            id: 99,
            name: None,
            suspended: false,
            ports: vec![et(2), po(1)],
        },
    ]
}

pub fn mac_table() -> MacTable {
    MacTable {
        aging_time_secs: 300,
        entries: vec![
            MacEntry {
                vlan: 10,
                mac: "00:1c:73:0c:aa:01".into(),
                is_static: false,
                port: Some(et(1)),
                drop: false,
                moves: 1,
                last_move_secs: Some(secs(0, 14, 22)),
            },
            MacEntry {
                vlan: 10,
                mac: "a0:36:9f:44:be:02".into(),
                is_static: false,
                port: Some(po(1)),
                drop: false,
                moves: 2,
                last_move_secs: Some(secs(1, 2, 10)),
            },
            MacEntry {
                vlan: 99,
                mac: "d4:af:f7:12:9c:01".into(),
                is_static: false,
                port: Some(et(2)),
                drop: false,
                moves: 1,
                last_move_secs: Some(secs(3, 55, 41)),
            },
            MacEntry {
                vlan: 10,
                mac: "00:50:56:be:ef:01".into(),
                is_static: true,
                port: Some(et(3)),
                drop: false,
                moves: 0,
                last_move_secs: None,
            },
        ],
    }
}

pub fn storm_control() -> Vec<StormRow> {
    vec![
        StormRow {
            port: et(1),
            kind: "broadcast".into(),
            level: "10.00".into(),
            rate_kbps: 100_000,
            drops: 1284,
            active: true,
        },
        StormRow {
            port: et(1),
            kind: "unknown-unicast".into(),
            level: "5.00".into(),
            rate_kbps: 50_000,
            drops: 0,
            active: true,
        },
        StormRow {
            port: po(1),
            kind: "broadcast".into(),
            level: "10.00".into(),
            rate_kbps: 2_000_000,
            drops: 0,
            active: true,
        },
    ]
}

/// Po1 as the seed config runs it: LACP active, both uplinks bundled,
/// fallback individual configured but dormant.
pub fn port_channels() -> Vec<PortChannel> {
    // Actor and partner both report activity + sync + collecting +
    // distributing, with the fast-rate (timeout) bit set.
    let state = 0x01 | 0x02 | 0x08 | 0x10 | 0x20;
    let member = |num: u32, partner_port: u32, tx: u64, rx: u64| PortChannelMember {
        id: et(num),
        status: "bundled".into(),
        rate_fast: true,
        actor_state: state,
        partner_state: state,
        partner_system: "32768,d4:af:f7:12:9c:00".into(),
        partner_port,
        partner_key: 1,
        partner_priority: 32768,
        pdus_rx: rx,
        pdus_tx: tx,
        churn: 2,
    };
    vec![PortChannel {
        group: 1,
        description: "uplink to core".into(),
        admin_up: true,
        up: true,
        lacp: true,
        active_mode: true,
        bundled: 2,
        total: 2,
        min_links: 1,
        fallback_mode: "individual".into(),
        fallback_timeout_secs: 90,
        fallback_active: false,
        mac: "2c:dd:e9:4a:1b:31".into(),
        members: vec![member(49, 49, 120, 118), member(50, 50, 119, 117)],
    }]
}

pub fn lacp_system() -> LacpSystem {
    LacpSystem {
        system_id: "32768,2c:dd:e9:4a:1b:00".into(),
    }
}

/// The seed bridge as the root: Et1 a guarded edge, Et2 a trunk, Po1
/// the uplink LAG.
pub fn stp_bridge() -> StpBridge {
    let port = |id: InterfaceId, role: &str, state: &str, cost: u32, portfast, bpduguard, tx, rx| {
        StpPortRow {
            id,
            role: role.into(),
            state: state.into(),
            cost,
            priority: 128,
            portfast,
            bpduguard,
            bpdus_tx: tx,
            bpdus_rx: rx,
            errdisabled: false,
        }
    };
    StpBridge {
        mode: "mstp".into(),
        bridge_priority: 32768,
        bridge_mac: "2c:dd:e9:4a:1b:00".into(),
        root_priority: 32768,
        root_mac: "2c:dd:e9:4a:1b:00".into(),
        is_root: true,
        root_cost: 0,
        root_port: None,
        hello_time: 2,
        max_age: 20,
        forward_time: 15,
        mst_name: "QS-CORE".into(),
        mst_revision: 3,
        instances: vec![(1, vec![10, 20, 30]), (2, vec![99])],
        topology_changes: 3,
        seconds_since_tc: Some(862),
        last_tc_port: Some("Ethernet2".into()),
        ports: vec![
            port(et(1), "designated", "forwarding", 20000, true, true, 1284, 2),
            port(et(2), "designated", "forwarding", 20000, false, false, 1200, 3),
            port(po(1), "designated", "forwarding", 10000, false, false, 900, 1),
        ],
    }
}

/// The same bridge as a non-root with a blocked alternate port.
pub fn stp_bridge_nonroot() -> StpBridge {
    let mut bridge = stp_bridge();
    bridge.root_priority = 4096;
    bridge.root_mac = "d4:af:f7:12:9c:00".into();
    bridge.is_root = false;
    bridge.root_cost = 20000;
    bridge.root_port = Some(et(1));
    bridge.ports[0].role = "root".into();
    bridge.ports[0].portfast = false;
    bridge.ports[0].bpduguard = false;
    bridge.ports.push(StpPortRow {
        id: et(3),
        role: "alternate".into(),
        state: "discarding".into(),
        cost: 20000,
        priority: 128,
        portfast: false,
        bpduguard: false,
        bpdus_tx: 0,
        bpdus_rx: 480,
        errdisabled: false,
    });
    bridge
}

/// IGMP snooping as the seed configures it: vlan 10 fast-leave with the
/// Po1 mrouter, vlan 20 running the local querier.
pub fn snooping() -> SnoopingView {
    SnoopingView {
        family: "IGMP".into(),
        enabled: true,
        robustness: 2,
        vlans: vec![
            SnoopVlanView {
                vlan: 10,
                enabled: true,
                fast_leave: true,
                querier_enabled: false,
                querier_address: None,
                querier_active: false,
                static_mrouters: vec![po(1)],
                dynamic_mrouters: vec![],
                groups: vec![
                    SnoopGroupView {
                        group: "239.1.1.10".into(),
                        version: 2,
                        ports: vec![et(1), et(3)],
                    },
                    SnoopGroupView {
                        group: "239.255.255.250".into(),
                        version: 2,
                        ports: vec![et(1), et(3), et(5)],
                    },
                ],
            },
            SnoopVlanView {
                vlan: 20,
                enabled: true,
                fast_leave: false,
                querier_enabled: true,
                querier_address: Some("10.0.20.1".into()),
                querier_active: true,
                static_mrouters: vec![],
                dynamic_mrouters: vec![],
                groups: vec![SnoopGroupView {
                    group: "239.20.0.7".into(),
                    version: 3,
                    ports: vec![et(2)],
                }],
            },
        ],
    }
}

pub fn mirror() -> Vec<MirrorSession> {
    vec![MirrorSession {
        session: 1,
        rx: vec![],
        tx: vec![],
        both: vec![et(1), et(2)],
        destination: Some(et(4)),
        destination_active: true,
    }]
}
