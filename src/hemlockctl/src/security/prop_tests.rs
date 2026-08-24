//! Property tests for the security suite: ACL rule rendering holds
//! for every field combination, MAC masks and rate suffixes
//! parse/format round-trip, and no renderer panics. Deterministic
//! xorshift generator, no external property-testing dependency (the
//! interfaces family's convention).
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

    fn chance(&mut self) -> bool {
        self.next() & 1 == 0
    }

    fn mac(&mut self) -> String {
        (0..6)
            .map(|_| format!("{:02x}", self.next() as u8))
            .collect::<Vec<_>>()
            .join(":")
    }
}

#[test]
fn mac_masks_parse_and_format_round_trip() {
    let mut rng = Rng(0x6d61_636d_6173_6b21);
    for _ in 0..2000 {
        let mask = rng.mac();
        // The colon form is already canonical; parsing is idempotent.
        let canonical = hemlock_common::net::parse_mac_mask(&mask).unwrap();
        assert_eq!(canonical, mask);
        assert_eq!(
            hemlock_common::net::parse_mac_mask(&canonical).unwrap(),
            canonical
        );
        // The dotted and dashed spellings collapse to the same mask.
        let bare: String = mask.split(':').collect();
        let dotted = format!("{}.{}.{}", &bare[0..4], &bare[4..8], &bare[8..12]);
        assert_eq!(hemlock_common::net::parse_mac_mask(&dotted).unwrap(), mask);
        let dashed = mask.replace(':', "-");
        assert_eq!(hemlock_common::net::parse_mac_mask(&dashed).unwrap(), mask);
    }
}

#[test]
fn rate_suffixes_parse_and_format_round_trip() {
    let mut rng = Rng(0x7261_7465_7375_6621);
    for _ in 0..2000 {
        // A formatted rate re-parses to the same numeric value, both
        // meter types.
        let pps = rng.chance();
        let rate = u64::from(rng.u32(1_000_000)) + 1;
        let rate = if !pps && rng.chance() {
            rate * [1, 1_000, 1_000_000, 1_000_000_000][rng.u32(4) as usize]
        } else {
            rate
        };
        let text = hemlock_common::net::format_police_rate(rate, pps);
        assert_eq!(
            hemlock_common::net::parse_police_rate(&text).unwrap(),
            (rate, pps),
            "{text}"
        );
        let burst = u64::from(rng.u32(1_000_000)) + 1;
        let text = hemlock_common::net::format_police_burst(burst, pps);
        assert_eq!(
            hemlock_common::net::parse_police_burst(&text).unwrap(),
            (burst, pps),
            "{text}"
        );
    }
}

/// One random rule exercising every field combination the model can
/// carry (family-appropriate columns included and excluded at random).
fn random_rule(rng: &mut Rng, mac_family: bool) -> AclRule {
    AclRule {
        number: rng.u32(4_294_967_295) + 1,
        permit: rng.chance(),
        protocol: if mac_family {
            None
        } else {
            Some(
                ["tcp", "udp", "icmp", "89", "ip"][rng.u32(5) as usize].to_string(),
            )
        },
        source: if rng.chance() {
            "any".into()
        } else if mac_family {
            format!("{}/{}", rng.mac(), rng.mac())
        } else {
            format!("10.{}.0.0/16", rng.u32(256))
        },
        destination: if rng.chance() {
            "any".into()
        } else if mac_family {
            rng.mac()
        } else {
            format!("2001:db8:{:x}::/48", rng.u32(0xffff))
        },
        port: rng.chance().then(|| {
            if rng.chance() {
                rng.u32(65536).to_string()
            } else {
                let low = rng.u32(65_000);
                format!("{low}-{}", low + rng.u32(500) + 1)
            }
        }),
        log: rng.chance(),
        police: rng.chance().then(|| {
            format!(
                "{} {}",
                hemlock_common::net::format_police_rate(
                    u64::from(rng.u32(1_000_000) + 1) * 1_000,
                    false
                ),
                hemlock_common::net::format_police_burst(u64::from(rng.u32(1_000_000) + 1), false)
            )
        }),
        matches: rng.next() % 1_000_000_000,
    }
}

#[test]
fn acl_rendering_holds_for_arbitrary_rule_combinations() {
    let mut rng = Rng(0x6163_6c72_756c_6521);
    for round in 0..300 {
        let mac_family = round % 3 == 2;
        let family = ["ipv4", "ipv6", "mac"][round % 3].to_string();
        let table = AclTable {
            name: format!("ACL-{round}"),
            family,
            rules: (0..rng.u32(8)).map(|_| random_rule(&mut rng, mac_family)).collect(),
            implicit_deny_matches: rng.next() % 10_000,
            bindings: (0..rng.u32(3))
                .map(|i| AclBinding {
                    port: format!("Ethernet{}", i + 1),
                    direction: if rng.chance() { "in" } else { "out" }.into(),
                })
                .collect(),
        };
        let state = AclState {
            acls: vec![table],
            tcam: vec![
                TcamStage {
                    stage: "ingress".into(),
                    used: rng.u32(512),
                    available: rng.u32(512),
                },
                TcamStage {
                    stage: "egress".into(),
                    used: rng.u32(256),
                    available: rng.u32(256),
                },
            ],
        };
        let text = render::acl(&state);
        // Every rule line carries its bracketed match counter, indented
        // under the block header.
        for rule in &state.acls[0].rules {
            assert!(
                text.lines()
                    .any(|l| l.starts_with("        ") && l.contains(&format!("[match {}]", rule.matches))),
                "rule {} missing from:\n{text}",
                rule.number
            );
        }
        assert!(text.contains("implicit deny"));
        let _ = render::acl_summary(&state);
        // The JSON serializer consumes the same structs.
        assert!(serde_json::to_string(&state).is_ok());
    }
}

#[test]
fn renderers_never_panic() {
    let mut rng = Rng(0x6e6f_7061_6e69_6322);
    for _ in 0..100 {
        let copp = CoppState {
            classes: (0..rng.u32(14))
                .map(|i| CoppClass {
                    class: format!("class-{i}"),
                    rate: rng.u32(10_000),
                    burst: rng.u32(2_000),
                    overridden: rng.chance(),
                    conforming: rng.next() % 10_000_000,
                    dropped: rng.next() % 10_000,
                })
                .collect(),
        };
        let _ = render::copp(&copp);

        let rows: Vec<PortSecurityEntry> = (0..rng.u32(5))
            .map(|i| PortSecurityEntry {
                port: format!("Ethernet{}", i + 1),
                maximum: rng.u32(1024) + 1,
                shutdown: rng.chance(),
                learned: (0..rng.u32(4))
                    .map(|_| SecureMac {
                        mac: rng.mac(),
                        age_secs: rng.next() % 100_000,
                    })
                    .collect(),
                violations: rng.u32(10),
                last_violation_mac: rng.chance().then(|| rng.mac()),
                last_violation_secs_ago: rng.chance().then(|| rng.next() % 100_000),
                errdisabled: rng.chance(),
            })
            .collect();
        let _ = render::port_security(&rows);
        let _ = render::port_security_detail(&rows);

        let dot1x = Dot1xState {
            radius_servers: vec!["10.42.0.5:1812".into()],
            reauth_interval_secs: rng.u32(86_400),
            ports: (0..rng.u32(4))
                .map(|i| Dot1xPort {
                    port: format!("Ethernet{}", i + 10),
                    status: if rng.chance() {
                        "authorized"
                    } else {
                        "unauthorized"
                    }
                    .into(),
                    supplicant_mac: rng.chance().then(|| rng.mac()),
                    last_auth_secs_ago: rng.chance().then(|| rng.next() % 100_000),
                    failures: rng.u32(5),
                })
                .collect(),
        };
        let _ = render::dot1x(&dot1x);

        let snoop = SnoopState {
            dhcp: DhcpSnooping {
                vlans: (0..rng.u32(4)).map(|_| rng.u32(4094) + 1).collect(),
                trusted: vec!["Port-Channel1".into()],
                bindings: (0..rng.u32(4))
                    .map(|i| SnoopBinding {
                        mac: rng.mac(),
                        ip: format!("10.0.{}.{}", rng.u32(256), rng.u32(256)),
                        lease_secs: rng.chance().then(|| rng.next() % 100_000),
                        is_static: rng.chance(),
                        vlan: rng.u32(4094) + 1,
                        interface: format!("Ethernet{}", i + 1),
                    })
                    .collect(),
                statistics: DhcpStatistics {
                    vlans: (0..rng.u32(3))
                        .map(|_| SnoopVlanStats {
                            vlan: rng.u32(4094) + 1,
                            packets: rng.next() % 100_000,
                            dropped: rng.next() % 100,
                        })
                        .collect(),
                    untrusted_server_drops: rng.next() % 100,
                },
            },
            arp: ArpInspection {
                vlans: (0..rng.u32(3)).map(|_| rng.u32(4094) + 1).collect(),
                validate: vec!["src-mac".into()],
                trusted: vec!["Port-Channel1".into()],
                statistics: (0..rng.u32(3))
                    .map(|_| DaiVlanStats {
                        vlan: rng.u32(4094) + 1,
                        forwarded: rng.next() % 100_000,
                        dropped: rng.next() % 100,
                        bad_binding: rng.next() % 50,
                        bad_src_mac: rng.next() % 50,
                    })
                    .collect(),
            },
        };
        let _ = render::dhcp_snooping(&snoop.dhcp);
        let _ = render::dhcp_snooping_binding(&snoop.dhcp.bindings);
        let _ = render::dhcp_snooping_statistics(&snoop.dhcp.statistics);
        let _ = render::arp_inspection(&snoop.arp);
        let _ = render::arp_inspection_statistics(&snoop.arp.statistics);
        assert!(serde_json::to_string(&snoop).is_ok());
    }
}
