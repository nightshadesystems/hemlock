//! Test fixtures behind the security-suite golden outputs: the state
//! the spec's seed configuration produces.

use super::model::{
    AclBinding, AclRule, AclState, AclTable, ArpInspection, CoppClass, CoppState, DaiVlanStats,
    DhcpSnooping, DhcpStatistics, Dot1xPort, Dot1xState, PortSecurityEntry, SecureMac,
    SnoopBinding, SnoopState, SnoopVlanStats, TcamStage,
};

fn rule(
    number: u32,
    permit: bool,
    protocol: Option<&str>,
    source: &str,
    destination: &str,
    matches: u64,
) -> AclRule {
    AclRule {
        number,
        permit,
        protocol: protocol.map(str::to_string),
        source: source.into(),
        destination: destination.into(),
        port: None,
        log: false,
        police: None,
        matches,
    }
}

/// The ACL state the spec's seed configuration produces: an IPv4 edge
/// filter (bound), an IPv6 management filter, and a MAC list.
pub fn acl_state() -> AclState {
    AclState {
        acls: vec![
            AclTable {
                name: "EDGE-IN".into(),
                family: "ipv4".into(),
                rules: vec![
                    AclRule {
                        port: Some("443".into()),
                        ..rule(10, true, Some("tcp"), "10.0.0.0/8", "10.42.0.0/16", 18_244)
                    },
                    AclRule {
                        port: Some("67-68".into()),
                        ..rule(20, true, Some("udp"), "any", "any", 512)
                    },
                    AclRule {
                        log: true,
                        ..rule(30, false, Some("ip"), "192.0.2.0/24", "any", 7)
                    },
                    AclRule {
                        police: Some("10m 256k".into()),
                        ..rule(40, true, Some("ip"), "any", "any", 90_211)
                    },
                ],
                implicit_deny_matches: 3,
                bindings: vec![AclBinding {
                    port: "Ethernet1".into(),
                    direction: "in".into(),
                }],
            },
            AclTable {
                name: "MGMT6-IN".into(),
                family: "ipv6".into(),
                rules: vec![
                    AclRule {
                        port: Some("22".into()),
                        ..rule(10, true, Some("tcp"), "2001:db8:9::/48", "any", 44)
                    },
                    AclRule {
                        log: true,
                        ..rule(20, false, Some("ipv6"), "any", "any", 0)
                    },
                ],
                implicit_deny_matches: 0,
                bindings: vec![],
            },
            AclTable {
                name: "IOT-MAC".into(),
                family: "mac".into(),
                rules: vec![
                    rule(
                        10,
                        true,
                        None,
                        "00:1c:73:00:00:00/ff:ff:ff:00:00:00",
                        "any",
                        1029,
                    ),
                    rule(20, false, None, "any", "any", 15),
                ],
                implicit_deny_matches: 0,
                bindings: vec![],
            },
        ],
        tcam: vec![
            TcamStage {
                stage: "ingress".into(),
                used: 9,
                available: 512,
            },
            TcamStage {
                stage: "egress".into(),
                used: 0,
                available: 256,
            },
        ],
    }
}

fn class(
    class: &str,
    rate: u32,
    burst: u32,
    overridden: bool,
    conforming: u64,
    dropped: u64,
) -> CoppClass {
    CoppClass {
        class: class.into(),
        rate,
        burst,
        overridden,
        conforming,
        dropped,
    }
}

/// The CoPP state the spec's seed configuration produces: bpdu and arp
/// overridden, everything else at the compiled defaults.
pub fn copp_state() -> CoppState {
    CoppState {
        classes: vec![
            class("bpdu", 512, 128, true, 182_331, 0),
            class("lacp", 1000, 256, false, 92_110, 0),
            class("lldp", 512, 128, false, 37_640, 0),
            class("eapol", 256, 64, false, 412, 0),
            class("igmp", 1000, 256, false, 55_019, 12),
            class("mld", 1000, 256, false, 1044, 0),
            class("arp", 2000, 500, true, 881_236, 3402),
            class("dhcp", 512, 128, false, 9871, 0),
            class("ospf", 2000, 512, false, 771_230, 0),
            class("bgp", 2000, 512, false, 44_127, 0),
            class("vrrp", 512, 128, false, 99_012, 0),
            class("ip2me", 4000, 1024, false, 1_288_812, 0),
            class("acl-log", 64, 32, false, 7, 0),
            class("default", 256, 64, false, 1233, 88),
        ],
    }
}

/// The port-security state the spec's seed configuration produces:
/// Ethernet5 at 3 of 4 learned, errdisabled after one violation.
pub fn port_security_rows() -> Vec<PortSecurityEntry> {
    let learned = |mac: &str, age_secs: u64| SecureMac {
        mac: mac.into(),
        age_secs,
    };
    vec![PortSecurityEntry {
        port: "Ethernet5".into(),
        maximum: 4,
        shutdown: true,
        learned: vec![
            learned("00:1c:73:0c:aa:05", 2462),
            learned("a0:36:9f:44:be:05", 727),
            learned("00:50:56:be:ef:05", 93),
        ],
        violations: 1,
        last_violation_mac: Some("00:50:56:be:ef:44".into()),
        last_violation_secs_ago: Some(131),
        errdisabled: true,
    }]
}

/// The 802.1X state the spec's seed configuration produces: one RADIUS
/// server, two authorized ports and one silent one.
pub fn dot1x_state() -> Dot1xState {
    Dot1xState {
        radius_servers: vec!["10.42.0.5:1812".into()],
        reauth_interval_secs: 3600,
        ports: vec![
            Dot1xPort {
                port: "Ethernet10".into(),
                status: "authorized".into(),
                supplicant_mac: Some("00:1c:73:0c:aa:10".into()),
                last_auth_secs_ago: Some(2462),
                failures: 0,
            },
            Dot1xPort {
                port: "Ethernet11".into(),
                status: "unauthorized".into(),
                supplicant_mac: None,
                last_auth_secs_ago: None,
                failures: 2,
            },
            Dot1xPort {
                port: "Ethernet12".into(),
                status: "authorized".into(),
                supplicant_mac: Some("a0:36:9f:44:be:12".into()),
                last_auth_secs_ago: Some(7855),
                failures: 1,
            },
        ],
    }
}

/// The snooping-security state the spec's seed configuration produces:
/// DHCP snooping on VLANs 10/20 with two dynamic and one static
/// binding, ARP inspection on VLAN 10.
pub fn snoop_state() -> SnoopState {
    let binding =
        |mac: &str, ip: &str, lease_secs: Option<u64>, vlan: u32, interface: &str| SnoopBinding {
            mac: mac.into(),
            ip: ip.into(),
            is_static: lease_secs.is_none(),
            lease_secs,
            vlan,
            interface: interface.into(),
        };
    SnoopState {
        dhcp: DhcpSnooping {
            vlans: vec![10, 20],
            trusted: vec!["Port-Channel1".into()],
            bindings: vec![
                binding(
                    "00:1c:73:0c:aa:01",
                    "10.0.10.101",
                    Some(85_122),
                    10,
                    "Ethernet1",
                ),
                binding(
                    "a0:36:9f:44:be:02",
                    "10.0.10.102",
                    Some(84_330),
                    10,
                    "Ethernet3",
                ),
                binding("00:50:56:be:ef:99", "10.0.20.50", None, 20, "Ethernet7"),
            ],
            statistics: DhcpStatistics {
                vlans: vec![
                    SnoopVlanStats {
                        vlan: 10,
                        packets: 18_822,
                        dropped: 14,
                    },
                    SnoopVlanStats {
                        vlan: 20,
                        packets: 4021,
                        dropped: 0,
                    },
                ],
                untrusted_server_drops: 14,
            },
        },
        arp: ArpInspection {
            vlans: vec![10],
            validate: vec!["src-mac".into()],
            trusted: vec!["Port-Channel1".into()],
            statistics: vec![DaiVlanStats {
                vlan: 10,
                forwarded: 99_120,
                dropped: 41,
                bad_binding: 38,
                bad_src_mac: 3,
            }],
        },
    }
}
