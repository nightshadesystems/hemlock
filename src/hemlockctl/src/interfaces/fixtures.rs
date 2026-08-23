//! Test fixtures: the interface set behind the golden reference outputs.
//!
//! Each function builds one interface exactly as the reference spec
//! describes it; the golden tests pick per-command subsets matching the
//! interfaces each reference output shows.

use std::collections::BTreeMap;

use super::model::*;
use super::name::{InterfaceId, Kind};

fn secs(days: u64, hours: u64, minutes: u64, seconds: u64) -> u64 {
    days * 86_400 + hours * 3_600 + minutes * 60 + seconds
}

fn mac(last: u8) -> String {
    format!("2c:dd:e9:12:00:{last:02x}")
}

fn copper_phys(speed: Option<u64>, autoneg: bool) -> Phys {
    Phys {
        duplex: Duplex::Full,
        speed_mbps: speed,
        autoneg,
        speed_from_autoneg: false,
        uni_link: None,
    }
}

fn copper_caps() -> Capabilities {
    Capabilities {
        model: "Celestica E1031".into(),
        media_type: "1000BASE-T".into(),
        speed_duplex: "10M/half,10M/full,100M/half,100M/full,1G/full,auto".into(),
        flowcontrol: "rx-(off,on,desired),tx-(off,on,desired)".into(),
    }
}

fn flowcontrol(admin: &str, oper: &str) -> FlowControl {
    FlowControl {
        send_admin: admin.into(),
        send_oper: oper.into(),
        recv_admin: admin.into(),
        recv_oper: oper.into(),
    }
}

fn copper_advertisement(pause: &str) -> Advertisement {
    Advertisement {
        speed_duplex: ["10M/half", "10M/full", "100M/half", "100M/full", "1G/full"]
            .into_iter()
            .map(String::from)
            .collect(),
        pause: pause.into(),
    }
}

pub fn et1() -> Interface {
    let mut i = Interface::new(InterfaceId::new(Kind::Ethernet, 1));
    i.description = Some("host rack3-u12 nic1".into());
    i.mac = Some(mac(0x01));
    i.bia = Some(mac(0x01));
    i.mtu = 9214;
    i.bandwidth_kbit = Some(1_000_000);
    i.phys = Some(copper_phys(Some(1000), true));
    i.last_change_secs = Some(secs(12, 4, 33, 12));
    i.loopback_mode = Some("None".into());
    i.counter_meta = Some(CounterMeta {
        link_changes: 2,
        last_clear_secs: Some(secs(12, 4, 33, 12)),
    });
    i.rates = Some(Rates {
        interval_secs: 300,
        in_bps: 24_700_000.0,
        in_pps: 4123,
        in_util_pct: 2.5,
        out_bps: 96_300_000.0,
        out_pps: 9877,
        out_util_pct: 9.9,
    });
    i.counters = Some(Counters {
        in_pkts: 4_294_811_034,
        in_octets: 4_816_030_792_344,
        in_ucast_pkts: 4_294_811_034,
        in_mcast_pkts: 89_127,
        in_bcast_pkts: 15_234,
        out_pkts: 8_812_734_120,
        out_octets: 11_278_449_021_837,
        out_ucast_pkts: 8_812_734_120,
        out_mcast_pkts: 44_506,
        out_bcast_pkts: 1_287,
        out_discards: 2,
        ..Counters::default()
    });
    i.vlan_membership = VlanCell::Access(10);
    i.media = Some("1000BASE-T".into());
    i.queues = vec![
        QueueCounters {
            queue: "UC0".into(),
            pkts: 84_123_441,
            bytes: 81_234_981_234_412,
            ..QueueCounters::default()
        },
        QueueCounters {
            queue: "UC1".into(),
            ..QueueCounters::default()
        },
        QueueCounters {
            queue: "UC2".into(),
            ..QueueCounters::default()
        },
        QueueCounters {
            queue: "UC3".into(),
            pkts: 12_341_123,
            bytes: 9_812_734_412_334,
            ..QueueCounters::default()
        },
        QueueCounters {
            queue: "UC4".into(),
            ..QueueCounters::default()
        },
        QueueCounters {
            queue: "UC5".into(),
            ..QueueCounters::default()
        },
        QueueCounters {
            queue: "UC6".into(),
            pkts: 441_233,
            bytes: 4_412_334_981,
            ..QueueCounters::default()
        },
        QueueCounters {
            queue: "UC7".into(),
            pkts: 8_812_344,
            bytes: 88_123_449_812,
            dropped_pkts: 2,
            dropped_bytes: 3028,
        },
        QueueCounters {
            queue: "MC0".into(),
            pkts: 1_287,
            bytes: 123_981,
            ..QueueCounters::default()
        },
    ];
    i.bins = Some(Bins {
        rx: [
            412_334_981,
            1_123_441_233,
            441_233_441,
            123_441_233,
            88_123_441,
            2_105_745_912,
            0,
        ],
        tx: [
            212_334_981,
            923_441_233,
            341_233_441,
            223_441_233,
            188_123_441,
            6_923_158_791,
            0,
        ],
    });
    i.caps = Some(copper_caps());
    i.flowcontrol = Some(flowcontrol("off", "off"));
    i.negotiation = Some(Negotiation {
        mode: AutonegMode::Ieee8023,
        status: Some("success".into()),
        local: Some(copper_advertisement("None")),
        partner: Some(copper_advertisement("Symmetric")),
        resolution: Some(NegotiationResolution {
            speed_duplex: "1G/full".into(),
            pause: "rx off, tx off".into(),
        }),
    });
    i.phy = Some(Phy {
        state: Some("linkUp".into()),
        interface_state: Some("up".into()),
        hw_resets: Some(1),
        transceiver: Some("1000BASE-T".into()),
        oper_speed: Some("1Gbps".into()),
        interrupt_count: Some(4),
        diags_mode: Some("normalOperation".into()),
        model: Some("HLK-PHY-BCM54282".into()),
        reset_count: Some(1),
        state_changes: Some(2),
        last_change_secs: Some(secs(12, 4, 33, 12)),
        configured_speed: Some("auto".into()),
        autoneg_config: Some(true),
    });
    i.switchport = Some(Switchport {
        access_vlan: 10,
        ..Switchport::default()
    });
    i
}

pub fn et2() -> Interface {
    let mut i = Interface::new(InterfaceId::new(Kind::Ethernet, 2));
    i.description = Some("trunk to lab-ap1".into());
    i.mac = Some(mac(0x02));
    i.bia = Some(mac(0x02));
    i.mtu = 9214;
    i.bandwidth_kbit = Some(1_000_000);
    i.phys = Some(copper_phys(Some(1000), true));
    i.last_change_secs = Some(secs(12, 4, 31, 2));
    i.loopback_mode = Some("None".into());
    i.counter_meta = Some(CounterMeta {
        link_changes: 1,
        last_clear_secs: None,
    });
    i.rates = Some(Rates {
        interval_secs: 300,
        in_bps: 3_110_000.0,
        in_pps: 1204,
        in_util_pct: 0.3,
        out_bps: 1_020_000.0,
        out_pps: 655,
        out_util_pct: 0.1,
    });
    i.counters = Some(Counters {
        in_pkts: 102_981_234,
        in_octets: 90_238_471_234,
        in_ucast_pkts: 102_981_234,
        in_mcast_pkts: 412_987,
        in_bcast_pkts: 88_123,
        out_pkts: 88_123_911,
        out_octets: 71_234_098_123,
        out_ucast_pkts: 88_123_911,
        out_mcast_pkts: 128_730,
        out_bcast_pkts: 9_812,
        ..Counters::default()
    });
    i.vlan_membership = VlanCell::Trunk;
    i.media = Some("1000BASE-T".into());
    i.flowcontrol = Some(flowcontrol("off", "off"));
    i.switchport = Some(Switchport {
        admin_mode: "trunk".into(),
        oper_mode: "trunk".into(),
        trunk_vlans: Some(vec![10, 20, 30, 99]),
        ..Switchport::default()
    });
    i
}

pub fn et3() -> Interface {
    let mut i = Interface::new(InterfaceId::new(Kind::Ethernet, 3));
    i.admin = AdminState::AdminDown;
    i.proto = LineProtocol::Down;
    i.status = IfStatus::Disabled;
    i.mac = Some(mac(0x03));
    i.bia = Some(mac(0x03));
    i.mtu = 9214;
    i.bandwidth_kbit = Some(1_000_000);
    i.phys = Some(copper_phys(None, false));
    i.last_change_secs = Some(secs(12, 4, 40, 51));
    i.loopback_mode = Some("None".into());
    i.counter_meta = Some(CounterMeta {
        link_changes: 0,
        last_clear_secs: None,
    });
    i.rates = Some(Rates {
        interval_secs: 300,
        ..Rates::default()
    });
    i.counters = Some(Counters::default());
    i.vlan_membership = VlanCell::Access(1);
    i.media = Some("1000BASE-T".into());
    i
}

pub fn et6() -> Interface {
    // QinQ tunnel port: S-VLAN 100, customer tags ride through.
    let mut i = Interface::new(InterfaceId::new(Kind::Ethernet, 6));
    i.description = Some("customer qinq drop".into());
    i.mac = Some(mac(0x06));
    i.bia = Some(mac(0x06));
    i.mtu = 9214;
    i.phys = Some(copper_phys(Some(1000), true));
    i.vlan_membership = VlanCell::Access(100);
    i.media = Some("1000BASE-T".into());
    i.switchport = Some(Switchport {
        admin_mode: "dot1q-tunnel".into(),
        oper_mode: "dot1q-tunnel".into(),
        tpid: "0x88a8".into(),
        access_vlan: 100,
        ..Switchport::default()
    });
    i
}

pub fn et7() -> Interface {
    let mut i = Interface::new(InterfaceId::new(Kind::Ethernet, 7));
    i.proto = LineProtocol::Down;
    i.status = IfStatus::ErrDisabled;
    i.errdisable_reason = Some("link-flap".into());
    i.mtu = 9214;
    i.phys = Some(copper_phys(None, true));
    i.vlan_membership = VlanCell::Access(1);
    i.media = Some("1000BASE-T".into());
    i
}

pub fn et8() -> Interface {
    // BPDU guard tripped: orch errdisabled the port through syncd.
    let mut i = Interface::new(InterfaceId::new(Kind::Ethernet, 8));
    i.proto = LineProtocol::Down;
    i.status = IfStatus::ErrDisabled;
    i.errdisable_reason = Some("bpduguard".into());
    i.mtu = 9214;
    i.phys = Some(copper_phys(None, true));
    i.vlan_membership = VlanCell::Access(1);
    i.media = Some("1000BASE-T".into());
    i
}

fn sfp_uplink(num: u32, mac_last: u8) -> Interface {
    let mut i = Interface::new(InterfaceId::new(Kind::Ethernet, num));
    i.description = Some("LAG member Po1".into());
    i.mac = Some(mac(mac_last));
    i.bia = Some(mac(mac_last));
    i.mtu = 9214;
    i.bandwidth_kbit = Some(10_000_000);
    i.phys = Some(copper_phys(Some(10_000), false));
    i.vlan_membership = VlanCell::InPortChannel(1);
    i.media = Some("10GBASE-SR".into());
    i.flowcontrol = Some(flowcontrol("desired", "on"));
    i.negotiation = Some(Negotiation::default());
    i
}

pub fn et49() -> Interface {
    let mut i = sfp_uplink(49, 0x31);
    i.rates = Some(Rates {
        interval_secs: 300,
        in_bps: 105_200_000.0,
        in_pps: 12_100,
        in_util_pct: 1.1,
        out_bps: 94_400_000.0,
        out_pps: 10_900,
        out_util_pct: 0.9,
    });
    i.counters = Some(Counters {
        in_octets: 5_123_098_123_441,
        in_ucast_pkts: 48_123_911_223,
        in_mcast_pkts: 412_334,
        in_bcast_pkts: 12,
        in_crc_errors: 12,
        in_symbol_errors: 3,
        in_errors: 15,
        in_pause: 12,
        out_octets: 45_012_098_123_441,
        out_ucast_pkts: 39_123_911_223,
        out_mcast_pkts: 406_877,
        out_bcast_pkts: 22,
        out_discards: 41_234,
        ..Counters::default()
    });
    i.caps = Some(Capabilities {
        model: "Celestica E1031".into(),
        media_type: "10GBASE-SR".into(),
        speed_duplex: "1G/full,10G/full,auto".into(),
        flowcontrol: "rx-(off,on,desired),tx-(off,on,desired)".into(),
    });
    i.phy = Some(Phy {
        configured_speed: Some("10G".into()),
        ..Phy::default()
    });
    i.mac_detail = Some(MacDetail {
        state: "linkUp".into(),
        local_fault: Some(false),
        remote_fault: Some(false),
        fec_mode: Some("Disabled".into()),
        fec_corrected: Some(0),
        fec_uncorrected: Some(0),
    });
    i
}

pub fn et50() -> Interface {
    let mut i = sfp_uplink(50, 0x32);
    i.rates = Some(Rates {
        interval_secs: 300,
        in_bps: 104_800_000.0,
        in_pps: 12_000,
        in_util_pct: 1.0,
        out_bps: 94_600_000.0,
        out_pps: 11_000,
        out_util_pct: 0.9,
    });
    i.counters = Some(Counters {
        in_octets: 5_012_098_123_441,
        in_ucast_pkts: 47_123_911_223,
        in_mcast_pkts: 409_877,
        in_bcast_pkts: 10,
        in_pause: 9,
        out_octets: 46_222_049_021_837,
        out_ucast_pkts: 39_000_000_997,
        out_mcast_pkts: 405_877,
        out_bcast_pkts: 22,
        out_discards: 40_997,
        ..Counters::default()
    });
    i
}

pub fn et51() -> Interface {
    let mut i = Interface::new(InterfaceId::new(Kind::Ethernet, 51));
    i.proto = LineProtocol::Down;
    i.status = IfStatus::NotConnect;
    i.description = Some("spare uplink".into());
    i.mtu = 9214;
    i.phys = Some(copper_phys(None, false));
    i.vlan_membership = VlanCell::Access(1);
    i.media = Some("10GBASE-SR".into());
    i
}

pub fn ma1() -> Interface {
    let mut i = Interface::new(InterfaceId::new(Kind::Management, 1));
    i.description = Some("OOB management".into());
    i.mac = Some(mac(0x00));
    i.bia = Some(mac(0x00));
    i.ip = Some(IpInfo {
        address: "10.10.0.21/24".into(),
        broadcast: "255.255.255.255".into(),
    });
    i.mtu = 1500;
    i.l3 = true;
    i.bandwidth_kbit = Some(1_000_000);
    i.phys = Some(Phys {
        duplex: Duplex::Full,
        speed_mbps: Some(1000),
        autoneg: true,
        speed_from_autoneg: true,
        uni_link: None,
    });
    i.last_change_secs = Some(secs(12, 4, 41, 3));
    i.loopback_mode = Some("None".into());
    i.counter_meta = Some(CounterMeta {
        link_changes: 1,
        last_clear_secs: None,
    });
    i.rates = Some(Rates {
        interval_secs: 300,
        in_bps: 12_300.0,
        in_pps: 14,
        in_util_pct: 0.0,
        out_bps: 8_910.0,
        out_pps: 9,
        out_util_pct: 0.0,
    });
    i.counters = Some(Counters {
        in_pkts: 1_412_341,
        in_octets: 212_398_123,
        in_ucast_pkts: 1_412_341,
        in_mcast_pkts: 98_123,
        in_bcast_pkts: 41_234,
        out_pkts: 998_123,
        out_octets: 141_234_123,
        out_ucast_pkts: 998_123,
        out_mcast_pkts: 2_098,
        out_bcast_pkts: 12,
        ..Counters::default()
    });
    i.vlan_membership = VlanCell::Routed;
    i.media = Some("10/100/1000".into());
    i.switchport = Some(Switchport {
        enabled: false,
        ..Switchport::default()
    });
    i
}

pub fn lo0() -> Interface {
    let mut i = Interface::new(InterfaceId::new(Kind::Loopback, 0));
    i.description = Some("Router-ID".into());
    i.ip = Some(IpInfo {
        address: "10.255.0.11/32".into(),
        broadcast: "255.255.255.255".into(),
    });
    i.mtu = 65_535;
    i.l3 = true;
    i.last_change_secs = Some(secs(12, 4, 41, 10));
    i
}

pub fn vl10() -> Interface {
    let mut i = Interface::new(InterfaceId::new(Kind::Vlan, 10));
    i.description = Some("LAN-USERS".into());
    i.mac = Some(mac(0x01));
    i.bia = Some(mac(0x01));
    i.ip = Some(IpInfo {
        address: "10.20.10.2/24".into(),
        broadcast: "255.255.255.255".into(),
    });
    i.mtu = 1500;
    i.l3 = true;
    i.bandwidth_kbit = Some(1_000_000);
    i.last_change_secs = Some(secs(12, 4, 31, 0));
    i
}

pub fn po1() -> Interface {
    let mut i = Interface::new(InterfaceId::new(Kind::PortChannel, 1));
    i.description = Some("uplink LAG to qs-hq-spine".into());
    i.mac = Some(mac(0x31));
    i.bia = Some(mac(0x31));
    i.mtu = 9214;
    i.bandwidth_kbit = Some(20_000_000);
    i.last_change_secs = Some(secs(12, 3, 58, 44));
    i.members = vec![
        Member {
            id: InterfaceId::new(Kind::Ethernet, 49),
            duplex: Duplex::Full,
            speed_mbps: 10_000,
        },
        Member {
            id: InterfaceId::new(Kind::Ethernet, 50),
            duplex: Duplex::Full,
            speed_mbps: 10_000,
        },
    ];
    i.fallback_mode = Some("off".into());
    i.rates = Some(Rates {
        interval_secs: 300,
        in_bps: 210_000_000.0,
        in_pps: 24_123,
        in_util_pct: 1.1,
        out_bps: 189_000_000.0,
        out_pps: 21_877,
        out_util_pct: 1.0,
    });
    i.counters = Some(Counters {
        in_pkts: 84_812_734_120,
        in_octets: 101_278_449_021_837,
        in_mcast_pkts: 991_234,
        in_bcast_pkts: 812,
        out_pkts: 78_123_911_223,
        out_octets: 91_234_098_123_441,
        out_mcast_pkts: 812_734,
        out_bcast_pkts: 44,
        ..Counters::default()
    });
    i.vlan_membership = VlanCell::Trunk;
    i.switchport = Some(Switchport {
        admin_mode: "trunk".into(),
        oper_mode: "trunk".into(),
        trunk_vlans: Some(vec![10, 20, 30, 99]),
        ..Switchport::default()
    });
    i
}

pub fn context() -> Context {
    Context {
        default_switchport_mode: "access".into(),
        vlan_names: BTreeMap::from([(10, "LAN-USERS".to_string())]),
        active_vlans: vec![1, 10, 20, 30, 99],
        system_time: Some("Thu Aug 20 14:02:11 2026".into()),
    }
}

pub fn xcvr49() -> Transceiver {
    Transceiver {
        id: InterfaceId::new(Kind::Ethernet, 49),
        media_type: "10GBASE-SR".into(),
        vendor: "FINISAR CORP.".into(),
        part_number: "FTLX8574D3BCL".into(),
        serial: "UWM01B7".into(),
        date_code: "210412".into(),
        temp_c: Some(33.45),
        voltage_v: Some(3.28),
        bias_ma: Some(6.42),
        tx_dbm: Some(-1.87),
        rx_dbm: Some(-2.35),
        thresholds: Some(DomThresholds {
            temperature: Thresholds {
                high_alarm: 75.0,
                high_warn: 70.0,
                low_alarm: -5.0,
                low_warn: 0.0,
            },
            voltage: Thresholds {
                high_alarm: 3.63,
                high_warn: 3.46,
                low_alarm: 2.97,
                low_warn: 3.13,
            },
            bias: Thresholds {
                high_alarm: 11.8,
                high_warn: 10.8,
                low_alarm: 4.0,
                low_warn: 5.0,
            },
            tx_power: Thresholds {
                high_alarm: 1.7,
                high_warn: -1.3,
                low_alarm: -9.5,
                low_warn: -8.3,
            },
            rx_power: Thresholds {
                high_alarm: 2.0,
                high_warn: -1.0,
                low_alarm: -13.1,
                low_warn: -12.1,
            },
        }),
        eeprom_a0: vec![
            0x03, 0x04, 0x07, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x67, 0x00,
            0x0a, 0x64, 0x00, 0x00, 0x00, 0x00, 0x46, 0x49, 0x4e, 0x49, 0x53, 0x41, 0x52, 0x20,
            0x43, 0x4f, 0x52, 0x50,
        ],
        eeprom_a2: Vec::new(),
        age_secs: 4,
    }
}

fn bare_xcvr(num: u32, temp: f64, volt: f64, bias: f64, rx: f64, tx: f64) -> Transceiver {
    Transceiver {
        id: InterfaceId::new(Kind::Ethernet, num),
        media_type: "10GBASE-SR".into(),
        vendor: "FINISAR CORP.".into(),
        part_number: "FTLX8574D3BCL".into(),
        serial: String::new(),
        date_code: String::new(),
        temp_c: Some(temp),
        voltage_v: Some(volt),
        bias_ma: Some(bias),
        tx_dbm: Some(tx),
        rx_dbm: Some(rx),
        thresholds: None,
        eeprom_a0: Vec::new(),
        eeprom_a2: Vec::new(),
        age_secs: 4,
    }
}

pub fn xcvr50() -> Transceiver {
    bare_xcvr(50, 29.11, 3.30, 7.01, -1.02, -0.95)
}

pub fn xcvr51() -> Transceiver {
    bare_xcvr(51, 29.87, 3.29, 6.88, -1.11, -0.98)
}
