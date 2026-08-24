//! Property tests for the routing suite: prefix canonicalization
//! round-trips, the route-code column layout holds for arbitrary
//! tables, and no renderer panics. Deterministic xorshift generator,
//! no external property-testing dependency (the interfaces family's
//! convention).
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

    fn v4(&mut self) -> std::net::IpAddr {
        std::net::IpAddr::V4(std::net::Ipv4Addr::from(self.next() as u32))
    }

    fn v6(&mut self) -> std::net::IpAddr {
        std::net::IpAddr::V6(std::net::Ipv6Addr::from(
            ((self.next() as u128) << 64) | self.next() as u128,
        ))
    }

    fn ip(&mut self) -> std::net::IpAddr {
        if self.chance() {
            self.v4()
        } else {
            self.v6()
        }
    }
}

#[test]
fn prefix_canonicalization_round_trips() {
    let mut rng = Rng(0x726f_7574_696e_6721);
    for _ in 0..2000 {
        let addr = rng.ip();
        let len = match addr {
            std::net::IpAddr::V4(_) => rng.u32(33) as u8,
            std::net::IpAddr::V6(_) => rng.u32(129) as u8,
        };
        let text = format!("{addr}/{len}");
        let canonical = hemlock_common::net::canonical_prefix(&text).unwrap();
        // Canonicalization is idempotent and always passes the
        // host-bits check.
        assert_eq!(
            hemlock_common::net::canonical_prefix(&canonical).unwrap(),
            canonical
        );
        assert_eq!(
            hemlock_common::net::require_canonical_prefix(&canonical).unwrap(),
            canonical
        );
        // Host bits set is an error exactly when canonicalization
        // changes the text.
        assert_eq!(
            hemlock_common::net::require_canonical_prefix(&text).is_ok(),
            canonical == text
        );
    }
}

fn random_route(rng: &mut Rng) -> RouteEntry {
    let protocols = ["connected", "static", "kernel", "ospf", "bgp", "weird"];
    let protocol = protocols[(rng.next() % protocols.len() as u64) as usize].to_string();
    let addr = rng.ip();
    let len = match addr {
        std::net::IpAddr::V4(_) => rng.u32(33) as u8,
        std::net::IpAddr::V6(_) => rng.u32(129) as u8,
    };
    let hops = (0..rng.u32(4))
        .map(|_| NextHop {
            via: rng.ip().to_string(),
            interface: rng.chance().then(|| format!("Ethernet{}", rng.u32(52))),
        })
        .collect();
    RouteEntry {
        interface: rng.chance().then(|| "Null0".to_string()),
        prefix: format!("{}/{len}", hemlock_common::net::network(addr, len)),
        distance: rng.u32(300),
        metric: rng.u32(100_000),
        next_hops: hops,
        fib: rng.chance().then(|| "programmed".to_string()),
        protocol,
    }
}

#[test]
fn route_table_layout_holds_for_arbitrary_tables() {
    let mut rng = Rng(0x6c61_796f_7574_2121);
    for round in 0..200 {
        let table = RouteTable {
            routes: (0..(round % 9)).map(|_| random_route(&mut rng)).collect(),
        };
        let text = render::route_table(&table);
        for line in text.lines().skip(2) {
            if line.is_empty()
                || line.starts_with("Gateway of last resort")
                || line.starts_with("         ")
            {
                continue;
            }
            // A route head line: one leading space, a route-code column
            // of width 6, then the prefix at column 8.
            let chars: Vec<char> = line.chars().collect();
            assert_eq!(chars[0], ' ', "{line:?}");
            assert!(
                chars[1].is_ascii_alphanumeric() || chars[1] == '?',
                "{line:?}"
            );
            assert_eq!(chars[7], ' ', "{line:?}");
            assert_ne!(chars.get(8), Some(&' '), "{line:?}");
        }
        // The summary and the JSON serializer consume the same structs.
        let _ = render::route_summary(&table.summarize(rng.u32(64)));
        assert!(serde_json::to_string(&table).is_ok());
    }
}

#[test]
fn renderers_never_panic() {
    let mut rng = Rng(0x6e6f_7061_6e69_6321);
    for _ in 0..100 {
        let neighbors = NeighborTable {
            entries: (0..rng.u32(6))
                .map(|_| NeighborEntry {
                    ip: rng.ip().to_string(),
                    mac: if rng.chance() {
                        "00:1c:73:0c:aa:07".into()
                    } else {
                        String::new()
                    },
                    interface: format!("Vlan{}", rng.u32(4095)),
                    is_static: rng.chance(),
                    age_secs: rng.chance().then(|| rng.next() % 100_000),
                })
                .collect(),
        };
        let _ = render::neighbor_table(&neighbors);
        assert!(serde_json::to_string(&neighbors).is_ok());
    }
}
