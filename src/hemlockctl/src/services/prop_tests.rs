//! Property tests for the services suite: the LLDP TTL is always the
//! interval times the multiplier (and never overflows the render), the
//! NTP renderer survives every sync posture, and no renderer panics or
//! breaks its column layout whatever the engine reports. Deterministic xorshift generator, no external
//! property-testing dependency (the interfaces family's convention).
#![allow(clippy::unwrap_used)]

use super::model::{LldpNeighbor, LldpPort, LldpState, NtpState};
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

    fn u64(&mut self) -> u64 {
        self.next()
    }

    fn chance(&mut self) -> bool {
        self.next() & 1 == 0
    }
}

/// TTL = tx-interval x hold-multiplier across the whole configurable
/// range, and the header line always names the same number.
#[test]
fn ttl_is_the_interval_times_the_multiplier() {
    for tx_interval in 5..=300u32 {
        for hold_multiplier in 2..=10u32 {
            let state = LldpState {
                enabled: true,
                tx_interval,
                hold_multiplier,
                ..LldpState::default()
            };
            let ttl = tx_interval * hold_multiplier;
            assert_eq!(state.ttl(), ttl);
            let text = render::lldp(&state);
            assert!(
                text.contains(&format!(
                    "Tx interval: {tx_interval}s   Hold multiplier: {hold_multiplier} (TTL {ttl}s)"
                )),
                "header wrong for {tx_interval}/{hold_multiplier}"
            );
        }
    }
    // Saturating, not wrapping: a nonsense engine reply still renders.
    let state = LldpState {
        tx_interval: u32::MAX,
        hold_multiplier: 4,
        ..LldpState::default()
    };
    assert_eq!(state.ttl(), u32::MAX);
}

fn random_state(rng: &mut Rng) -> LldpState {
    let ports = (0..rng.u32(8))
        .map(|index| {
            let port = format!("Ethernet{}", index + 1);
            let neighbors = (0..rng.u32(3))
                .map(|n| LldpNeighbor {
                    port: port.clone(),
                    chassis_id: format!("02:00:00:00:00:{n:02x}"),
                    chassis_id_subtype: if rng.chance() { "mac" } else { "local" }.into(),
                    port_id: format!("Ethernet{}", rng.u32(64)),
                    port_id_subtype: if rng.chance() {
                        "interface-name"
                    } else {
                        "agent-circuit-id"
                    }
                    .into(),
                    port_description: if rng.chance() {
                        String::new()
                    } else {
                        "x".repeat(40)
                    },
                    system_name: if rng.chance() {
                        String::new()
                    } else {
                        format!("neighbor-{n}")
                    },
                    system_description: if rng.chance() {
                        String::new()
                    } else {
                        "d".repeat(60)
                    },
                    management_address: if rng.chance() {
                        String::new()
                    } else {
                        "10.42.0.12".into()
                    },
                    ttl: rng.u32(65_535),
                    age_secs: rng.u64() % 1_000_000,
                })
                .collect();
            LldpPort {
                port,
                enabled: rng.chance(),
                frames_tx: rng.u64(),
                frames_rx: rng.u64(),
                frames_discarded: rng.u64(),
                ageouts: rng.u64(),
                neighbors,
            }
        })
        .collect();
    LldpState {
        enabled: rng.chance(),
        tx_interval: 5 + rng.u32(296),
        hold_multiplier: 2 + rng.u32(9),
        chassis_id: "2c:dd:e9:4a:1b:00".into(),
        system_name: "hemlock".into(),
        system_description: "Hemlock NOS version 0.1.0".into(),
        management_address: "10.42.0.9".into(),
        ports,
    }
}

/// Every renderer survives arbitrary engine state, leaves no trailing
/// whitespace, and keeps the neighbor count in the table honest.
#[test]
fn renderers_never_panic_or_pad_past_end_of_line() {
    let mut rng = Rng(0x6c6c_6470_7072_6f70);
    for _ in 0..500 {
        let state = random_state(&mut rng);
        let table = render::lldp(&state);
        for line in table.lines() {
            assert_eq!(line.trim_end(), line, "trailing whitespace in {line:?}");
        }
        for port in &state.ports {
            assert!(table.contains(&format!("{}", port.neighbors.len())));
        }
        for text in [
            render::lldp_neighbors(&state),
            render::lldp_neighbors_detail(&state),
        ] {
            for line in text.lines() {
                assert_eq!(line.trim_end(), line, "trailing whitespace in {line:?}");
            }
        }
        // The flat grid lists exactly the neighbors the ports hold.
        assert_eq!(
            state.neighbors().len(),
            state.ports.iter().map(|p| p.neighbors.len()).sum::<usize>()
        );
    }
}

/// A switch that has heard nothing renders an explicit line rather
/// than an empty detail page.
#[test]
fn silent_switch_says_so() {
    let state = LldpState {
        enabled: true,
        tx_interval: 30,
        hold_multiplier: 4,
        ports: vec![LldpPort {
            port: "Ethernet1".into(),
            enabled: true,
            frames_tx: 3,
            frames_rx: 0,
            frames_discarded: 0,
            ageouts: 0,
            neighbors: Vec::new(),
        }],
        ..LldpState::default()
    };
    assert_eq!(
        render::lldp_neighbors_detail(&state),
        "No LLDP neighbors.\n"
    );
    // The flat grid degrades to its headings.
    assert_eq!(render::lldp_neighbors(&state).lines().count(), 2);
}

/// The NTP block renders for every posture the engine can report:
/// unconfigured, configured-but-stopped, running-but-unsynchronized,
/// and synchronized — never panicking, never padding past end of line.
#[test]
fn ntp_renders_every_sync_posture() {
    let mut rng = Rng(0x6e74_705f_7072_6f70);
    let base = || NtpState {
        servers: vec!["10.42.0.5".into(), "pool.ntp.org".into()],
        server: "10.42.0.5".into(),
        stratum: 3,
        poll_interval_secs: 512,
        ..NtpState::default()
    };

    // No servers: one line, and it never claims a sync.
    let text = render::ntp(&NtpState::default());
    assert_eq!(text, "NTP is disabled (no servers configured)\n");

    // Configured but the unit is down — distinguishable from both.
    let text = render::ntp(&base());
    assert!(text.contains("systemd-timesyncd is not running"));
    assert!(text.contains("Not synchronized"));

    let text = render::ntp(&NtpState {
        enabled: true,
        ..base()
    });
    assert!(text.contains("NTP is enabled (systemd-timesyncd)"));
    assert!(text.contains("Not synchronized"));
    assert!(!text.contains("Stratum"));

    // An unreadable last-sync timestamp says so rather than lying.
    let text = render::ntp(&NtpState {
        enabled: true,
        synchronized: true,
        last_sync_secs_ago: None,
        ..base()
    });
    assert!(text.contains("Last sync: unknown"));

    for _ in 0..2000 {
        let state = NtpState {
            enabled: rng.chance(),
            synchronized: rng.chance(),
            stratum: rng.u32(16),
            poll_interval_secs: rng.u32(4096),
            offset_usecs: i64::from(rng.u32(4_000_000)) - 2_000_000,
            delay_usecs: u64::from(rng.u32(1_000_000)),
            jitter_usecs: u64::from(rng.u32(1_000_000)),
            last_sync_secs_ago: rng.chance().then(|| rng.u64() % 10_000_000),
            ..base()
        };
        let text = render::ntp(&state);
        for line in text.lines() {
            assert_eq!(line.trim_end(), line, "trailing whitespace in {line:?}");
        }
        // The offset keeps its sign through the millisecond render.
        if state.synchronized {
            let negative = text.contains("Offset -");
            assert_eq!(negative, state.offset_usecs < 0, "sign lost for {state:?}");
        }
    }
}

/// Microseconds render as milliseconds to three places, and ages read
/// as the compact form the spec's sample uses.
#[test]
fn offsets_and_ages_render_compactly() {
    let millis = |usecs: i64| {
        let state = NtpState {
            enabled: true,
            synchronized: true,
            servers: vec!["a".into()],
            offset_usecs: usecs,
            ..NtpState::default()
        };
        let text = render::ntp(&state);
        let line = text.lines().find(|l| l.contains("Offset")).unwrap();
        line.trim().to_string()
    };
    assert!(millis(-412).starts_with("Offset -0.412 ms"));
    assert!(millis(1204).starts_with("Offset 1.204 ms"));
    assert!(millis(0).starts_with("Offset 0.000 ms"));
    assert!(millis(-2_000_000).starts_with("Offset -2000.000 ms"));

    let age = |secs: u64| {
        let state = NtpState {
            enabled: true,
            synchronized: true,
            servers: vec!["a".into()],
            last_sync_secs_ago: Some(secs),
            ..NtpState::default()
        };
        let text = render::ntp(&state);
        let line = text.lines().find(|l| l.contains("Last sync")).unwrap();
        line.trim()
            .trim_start_matches("Last sync: ")
            .trim_end_matches(" ago")
            .to_string()
    };
    assert_eq!(age(41), "41s");
    assert_eq!(age(4 * 60 + 12), "4m12s");
    assert_eq!(age(2 * 3600 + 4 * 60), "2h04m");
    assert_eq!(age(3 * 86_400 + 5 * 3600), "3d05h");
}
