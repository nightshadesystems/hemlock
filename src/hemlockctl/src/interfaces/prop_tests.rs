//! Property test: no renderer panics for any combination of optional
//! fields and value magnitudes. Uses a deterministic xorshift generator
//! (no external property-testing dependency) over many random models.

use super::model::*;
use super::name::{InterfaceId, Kind};
use super::render;
use super::render::summary::StatusFilter;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64* — deterministic, dependency-free.
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn u64(&mut self) -> u64 {
        self.next()
    }

    fn u32(&mut self, bound: u32) -> u32 {
        (self.next() % u64::from(bound.max(1))) as u32
    }

    fn chance(&mut self) -> bool {
        self.next() & 1 == 0
    }

    fn f64(&mut self) -> f64 {
        // Mix magnitudes from tiny to absurd, including zero.
        match self.next() % 5 {
            0 => 0.0,
            1 => (self.next() % 1000) as f64,
            2 => (self.next() % 1_000_000_000) as f64,
            3 => (self.next() % 1_000_000) as f64 / 997.0,
            _ => self.next() as f64,
        }
    }

    fn string(&mut self) -> String {
        let choices = [
            "",
            "x",
            "a somewhat longer description that will hit truncation limits",
            "trunk",
            "static access",
            "üñíçødé-描述",
        ];
        choices[(self.next() % choices.len() as u64) as usize].to_string()
    }

    fn opt<T>(&mut self, value: T) -> Option<T> {
        self.chance().then_some(value)
    }
}

/// `opt!(rng, expr)` — evaluate `expr` (which may itself use `rng`)
/// before the `opt` call, sidestepping the double mutable borrow.
macro_rules! opt {
    ($rng:expr, $value:expr) => {{
        let value = $value;
        $rng.opt(value)
    }};
}

fn random_kind(rng: &mut Rng) -> Kind {
    match rng.next() % 5 {
        0 => Kind::Ethernet,
        1 => Kind::Loopback,
        2 => Kind::Management,
        3 => Kind::PortChannel,
        _ => Kind::Vlan,
    }
}

fn random_interface(rng: &mut Rng) -> Interface {
    let id = InterfaceId::new(random_kind(rng), rng.u32(5000));
    let mut i = Interface::new(id);
    i.admin = match rng.next() % 3 {
        0 => AdminState::Up,
        1 => AdminState::Down,
        _ => AdminState::AdminDown,
    };
    i.proto = match rng.next() % 4 {
        0 => LineProtocol::Up,
        1 => LineProtocol::Down,
        2 => LineProtocol::LowerLayerDown,
        _ => LineProtocol::NotPresent,
    };
    i.status = match rng.next() % 5 {
        0 => IfStatus::Connected,
        1 => IfStatus::NotConnect,
        2 => IfStatus::Disabled,
        3 => IfStatus::ErrDisabled,
        _ => IfStatus::Inactive,
    };
    i.errdisable_reason = opt!(rng, "link-flap".into());
    i.description = {
        let s = rng.string();
        opt!(rng, s)
    };
    i.mac = opt!(rng, "2c:dd:e9:12:00:01".into());
    i.bia = opt!(rng, "2c:dd:e9:12:00:01".into());
    i.ip = opt!(
        rng,
        IpInfo {
            address: "10.0.0.1/24".into(),
            broadcast: "255.255.255.255".into(),
        }
    );
    i.mtu = rng.u32(70_000);
    i.l3 = rng.chance();
    i.bandwidth_kbit = opt!(rng, rng.u64() % 100_000_000);
    i.phys = opt!(
        rng,
        Phys {
            duplex: if rng.chance() {
                Duplex::Full
            } else {
                Duplex::Half
            },
            speed_mbps: opt!(rng, rng.u64() % 1_000_000),
            autoneg: rng.chance(),
            speed_from_autoneg: rng.chance(),
            uni_link: opt!(rng, rng.string()),
        }
    );
    i.last_change_secs = opt!(rng, rng.u64() % (400 * 86_400));
    i.loopback_mode = opt!(rng, "None".into());
    i.counter_meta = opt!(
        rng,
        CounterMeta {
            link_changes: rng.u64(),
            last_clear_secs: opt!(rng, rng.u64() % (400 * 86_400)),
        }
    );
    i.rates = opt!(
        rng,
        Rates {
            interval_secs: rng.u32(4000),
            in_bps: rng.f64(),
            in_pps: rng.u64() % 1_000_000_000,
            in_util_pct: rng.f64(),
            out_bps: rng.f64(),
            out_pps: rng.u64() % 1_000_000_000,
            out_util_pct: rng.f64(),
        }
    );
    i.counters = opt!(
        rng,
        Counters {
            in_pkts: rng.u64(),
            in_octets: rng.u64(),
            in_ucast_pkts: rng.u64(),
            out_octets: rng.u64(),
            ..Counters::default()
        }
    );
    if rng.chance() {
        i.members = vec![Member {
            id: InterfaceId::new(Kind::Ethernet, rng.u32(64)),
            duplex: Duplex::Full,
            speed_mbps: rng.u64() % 1_000_000,
        }];
    }
    i.fallback_mode = opt!(rng, "off".into());
    i.vlan_membership = match rng.next() % 4 {
        0 => VlanCell::Routed,
        1 => VlanCell::Trunk,
        2 => VlanCell::Access(rng.u32(4095)),
        _ => VlanCell::InPortChannel(rng.u32(64)),
    };
    i.media = opt!(rng, rng.string());
    if rng.chance() {
        i.queues = vec![QueueCounters {
            queue: format!("UC{}", rng.u32(16)),
            pkts: rng.u64(),
            bytes: rng.u64(),
            dropped_pkts: rng.u64(),
            dropped_bytes: rng.u64(),
            wred_dropped: rng.u64(),
            ecn_marked: rng.u64(),
        }];
    }
    i.bins = opt!(
        rng,
        Bins {
            rx: [rng.u64(); 7],
            tx: [rng.u64(); 7],
        }
    );
    i.caps = opt!(
        rng,
        Capabilities {
            model: rng.string(),
            media_type: rng.string(),
            speed_duplex: rng.string(),
            flowcontrol: rng.string(),
        }
    );
    i.flowcontrol = opt!(
        rng,
        FlowControl {
            send_admin: rng.string(),
            send_oper: rng.string(),
            recv_admin: rng.string(),
            recv_oper: rng.string(),
        }
    );
    i.negotiation = opt!(
        rng,
        Negotiation {
            mode: if rng.chance() {
                AutonegMode::Ieee8023
            } else {
                AutonegMode::Off
            },
            status: opt!(rng, rng.string()),
            local: opt!(
                rng,
                Advertisement {
                    speed_duplex: vec![rng.string(), rng.string()],
                    pause: rng.string(),
                }
            ),
            partner: None,
            resolution: opt!(
                rng,
                NegotiationResolution {
                    speed_duplex: rng.string(),
                    pause: rng.string(),
                }
            ),
        }
    );
    i.phy = opt!(
        rng,
        Phy {
            state: opt!(rng, rng.string()),
            interface_state: opt!(rng, rng.string()),
            hw_resets: opt!(rng, rng.u64()),
            transceiver: opt!(rng, rng.string()),
            oper_speed: opt!(rng, rng.string()),
            interrupt_count: opt!(rng, rng.u64()),
            diags_mode: opt!(rng, rng.string()),
            model: opt!(rng, rng.string()),
            reset_count: opt!(rng, rng.u64()),
            state_changes: opt!(rng, rng.u64()),
            last_change_secs: opt!(rng, rng.u64()),
            configured_speed: opt!(rng, rng.string()),
            autoneg_config: opt!(rng, rng.chance()),
        }
    );
    i.mac_detail = opt!(
        rng,
        MacDetail {
            state: rng.string(),
            local_fault: opt!(rng, rng.chance()),
            remote_fault: opt!(rng, rng.chance()),
            fec_mode: opt!(rng, rng.string()),
            fec_corrected: opt!(rng, rng.u64()),
            fec_uncorrected: opt!(rng, rng.u64()),
        }
    );
    i.switchport = opt!(
        rng,
        Switchport {
            enabled: rng.chance(),
            admin_mode: rng.string(),
            oper_mode: if rng.chance() {
                "trunk".into()
            } else {
                rng.string()
            },
            trunk_vlans: opt!(rng, vec![rng.u32(4095), rng.u32(4095)]),
            ..Switchport::default()
        }
    );
    i
}

fn random_transceiver(rng: &mut Rng) -> Transceiver {
    Transceiver {
        id: InterfaceId::new(Kind::Ethernet, rng.u32(64)),
        media_type: rng.string(),
        vendor: rng.string(),
        part_number: rng.string(),
        serial: rng.string(),
        date_code: rng.string(),
        temp_c: opt!(rng, rng.f64()),
        voltage_v: opt!(rng, rng.f64()),
        bias_ma: opt!(rng, rng.f64()),
        tx_dbm: opt!(rng, -rng.f64()),
        rx_dbm: opt!(rng, -rng.f64()),
        thresholds: opt!(rng, DomThresholds::default()),
        eeprom_a0: (0..rng.u32(300)).map(|b| b as u8).collect(),
        eeprom_a2: (0..rng.u32(300)).map(|b| b as u8).collect(),
        age_secs: rng.u64() % (400 * 86_400),
    }
}

#[test]
fn renderers_never_panic() {
    let mut rng = Rng(0x6865_6d6c_6f63_6b21);
    for round in 0..200 {
        let n = (round % 7) as usize;
        let interfaces: Vec<Interface> = (0..n).map(|_| random_interface(&mut rng)).collect();
        let transceivers: Vec<Transceiver> = (0..n).map(|_| random_transceiver(&mut rng)).collect();
        let ctx = super::model::Context {
            default_switchport_mode: rng.string(),
            vlan_names: Default::default(),
            active_vlans: vec![rng.u32(4095), rng.u32(4095)],
            system_time: opt!(rng, rng.string()),
        };

        let _ = render::detail::render(&interfaces);
        let _ = render::summary::description(&interfaces);
        for filter in [
            StatusFilter::All,
            StatusFilter::Connected,
            StatusFilter::NotConnect,
            StatusFilter::ErrDisabled,
            StatusFilter::Inactive,
        ] {
            let _ = render::summary::status(&interfaces, filter);
        }
        let _ = render::counters::counters(&interfaces);
        let _ = render::counters::errors(&interfaces);
        let _ = render::counters::discards(&interfaces);
        let _ = render::counters::rates(&interfaces);
        let _ = render::counters::queues(&interfaces);
        let _ = render::counters::bins(&interfaces);
        let _ = render::transceiver::summary(&transceivers);
        let _ = render::transceiver::detail(&transceivers);
        let _ = render::transceiver::properties(&transceivers, &interfaces);
        let _ = render::transceiver::eeprom(&transceivers);
        let _ = render::phys::capabilities(&interfaces);
        let _ = render::phys::flowcontrol(&interfaces);
        let _ = render::phys::negotiation(&interfaces);
        let _ = render::phys::negotiation_detail(&interfaces);
        let _ = render::phys::phy(&interfaces);
        let _ = render::phys::phy_detail(&interfaces, &ctx);
        let _ = render::phys::mac(&interfaces);
        let _ = render::phys::mac_detail(&interfaces);
        let _ = render::l2::switchport(&interfaces, &ctx);
        let _ = render::l2::trunk(&interfaces, &ctx);
        let _ = render::l2::vlans(&interfaces, &ctx);

        // The JSON serializer consumes the same structs; it must always
        // succeed too.
        let json = serde_json::to_string(&interfaces);
        assert!(json.is_ok());
    }
}
