//! Builds the interface model from the daemons.
//!
//! syncd serves interface state/counters/rates (`GetInterfaces`); pmon
//! serves transceiver DOM data (`ListTransceivers`). Everything the
//! daemons cannot know yet (switchport config, VLAN names) is filled
//! with phase-1 defaults here, so the renderers degrade gracefully.

use anyhow::{Context as _, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;

use super::fmt;
use super::model::*;
use super::name::{self, InterfaceId, Kind};

/// Everything `show interfaces` needs from the daemons.
pub struct Fetched {
    pub interfaces: Vec<Interface>,
    pub ctx: Context,
}

pub async fn interfaces(syncd: &IpcEndpoint) -> Result<Fetched> {
    let channel = syncd.connect().await.context("connecting to syncd")?;
    let mut client = pb::syncd_client::SyncdClient::new(channel);
    let response = client
        .get_interfaces(pb::GetInterfacesRequest { names: Vec::new() })
        .await?
        .into_inner();

    let interfaces: Vec<Interface> = response
        .interfaces
        .iter()
        .filter_map(|state| convert(state, &response.platform_model))
        .collect();
    let ctx = Context {
        default_switchport_mode: "access".into(),
        vlan_names: response
            .vlan_names
            .iter()
            .map(|(id, name)| (*id, name.clone()))
            .collect(),
        active_vlans: if response.active_vlans.is_empty() {
            vec![1] // pre-VLAN syncd
        } else {
            response.active_vlans.clone()
        },
        system_time: Some(
            chrono::Local::now()
                .format("%a %b %e %H:%M:%S %Y")
                .to_string(),
        ),
    };
    Ok(Fetched { interfaces, ctx })
}

fn kind_of(state: &pb::InterfaceState) -> Option<Kind> {
    Some(match state.kind.as_str() {
        "ethernet" => Kind::Ethernet,
        "management" => Kind::Management,
        "loopback" => Kind::Loopback,
        "vlan" => Kind::Vlan,
        "port-channel" => Kind::PortChannel,
        _ => return None,
    })
}

fn convert(state: &pb::InterfaceState, platform_model: &str) -> Option<Interface> {
    let id = name::parse_one(&state.name)?;
    if Some(id.kind) != kind_of(state) {
        // A name that does not match its declared family is daemon
        // confusion; skip rather than mislabel.
        return None;
    }
    let admin_down = state.admin_state == pb::AdminState::Down as i32;
    let oper_up = state.oper_status == pb::OperStatus::Up as i32;

    let mut i = Interface::new(id);
    i.admin = if admin_down {
        AdminState::AdminDown
    } else if oper_up {
        AdminState::Up
    } else {
        AdminState::Down
    };
    i.proto = if oper_up {
        LineProtocol::Up
    } else {
        LineProtocol::Down
    };
    i.status = if admin_down {
        IfStatus::Disabled
    } else if !state.errdisable_reason.is_empty() {
        IfStatus::ErrDisabled
    } else if oper_up {
        IfStatus::Connected
    } else {
        IfStatus::NotConnect
    };
    i.errdisable_reason =
        (!state.errdisable_reason.is_empty()).then(|| state.errdisable_reason.clone());
    i.description = (!state.description.is_empty()).then(|| state.description.clone());
    i.mac = (!state.mac.is_empty()).then(|| state.mac.clone());
    i.bia = (!state.bia_mac.is_empty()).then(|| state.bia_mac.clone());
    i.ip = state.ip_addresses.first().map(|address| IpInfo {
        address: address.clone(),
        broadcast: "255.255.255.255".into(),
    });
    i.mtu = state.mtu;
    // Ethernet ports are L2 by default; an address puts them in L3 mode.
    i.l3 = matches!(id.kind, Kind::Management | Kind::Loopback | Kind::Vlan)
        || (id.kind == Kind::Ethernet && !state.ip_addresses.is_empty());
    i.bandwidth_kbit = (state.speed_mbps > 0).then_some(state.speed_mbps * 1000);

    let physical = matches!(id.kind, Kind::Ethernet | Kind::Management);
    if physical {
        i.phys = Some(Phys {
            duplex: if state.duplex == "half" {
                Duplex::Half
            } else {
                Duplex::Full
            },
            speed_mbps: (state.speed_mbps > 0).then_some(state.speed_mbps),
            autoneg: state.autoneg,
            speed_from_autoneg: state.autoneg,
            uni_link: None,
        });
        i.loopback_mode = Some("None".into());
        i.counter_meta = Some(CounterMeta {
            link_changes: state.link_changes,
            last_clear_secs: state.seconds_since_clear,
        });
    }
    i.last_change_secs = state.seconds_since_change;
    i.rates = state.rates.as_ref().map(|r| Rates {
        interval_secs: r.interval_secs,
        in_bps: r.in_bps,
        in_pps: r.in_pps,
        in_util_pct: r.in_util_pct,
        out_bps: r.out_bps,
        out_pps: r.out_pps,
        out_util_pct: r.out_util_pct,
    });
    i.counters = state.counters.as_ref().map(|c| Counters {
        in_pkts: c.in_pkts,
        in_octets: c.in_octets,
        in_ucast_pkts: c.in_ucast_pkts,
        in_mcast_pkts: c.in_mcast_pkts,
        in_bcast_pkts: c.in_bcast_pkts,
        in_discards: c.in_discards,
        in_errors: c.in_errors,
        in_crc_errors: c.in_crc_errors,
        in_alignment_errors: c.in_alignment_errors,
        in_symbol_errors: c.in_symbol_errors,
        in_runts: c.in_runts,
        in_giants: c.in_giants,
        in_pause: c.in_pause,
        out_pkts: c.out_pkts,
        out_octets: c.out_octets,
        out_ucast_pkts: c.out_ucast_pkts,
        out_mcast_pkts: c.out_mcast_pkts,
        out_bcast_pkts: c.out_bcast_pkts,
        out_discards: c.out_discards,
        out_errors: c.out_errors,
        out_pause: c.out_pause,
        collisions: c.collisions,
        late_collisions: c.late_collisions,
        deferred: c.deferred,
    });
    i.members = state
        .members
        .iter()
        .filter_map(|m| name::parse_one(m))
        .map(|id| Member {
            id,
            duplex: Duplex::Full,
            speed_mbps: 0,
        })
        .collect();
    if id.kind == Kind::PortChannel {
        i.fallback_mode = Some("off".into());
    }
    let trunk = state.switchport_mode == "trunk";
    let access_vlan = if state.access_vlan == 0 {
        1
    } else {
        state.access_vlan
    };
    i.vlan_membership = if i.l3 {
        VlanCell::Routed
    } else if trunk {
        VlanCell::Trunk
    } else {
        // Default L2: an access port in the default VLAN.
        VlanCell::Access(access_vlan)
    };
    i.media = (!state.media.is_empty()).then(|| state.media.clone());
    i.queues = state
        .queues
        .iter()
        .map(|q| QueueCounters {
            queue: q.queue.clone(),
            pkts: q.pkts,
            bytes: q.bytes,
            dropped_pkts: q.dropped_pkts,
            dropped_bytes: q.dropped_bytes,
        })
        .collect();
    i.bins = state.bins.as_ref().map(|b| {
        let seven = |v: &[u64]| std::array::from_fn(|n| v.get(n).copied().unwrap_or(0));
        Bins {
            rx: seven(&b.rx),
            tx: seven(&b.tx),
        }
    });
    if id.kind == Kind::Ethernet && !state.supported_modes.is_empty() {
        i.caps = Some(Capabilities {
            model: platform_model.to_string(),
            media_type: state.media.clone(),
            speed_duplex: state.supported_modes.join(","),
            flowcontrol: "rx-(off,on,desired),tx-(off,on,desired)".into(),
        });
    }
    if id.kind == Kind::Ethernet {
        // Flow-control configuration lands with QoS intents; phase 1 runs
        // with pause negotiation off everywhere.
        i.flowcontrol = Some(FlowControl {
            send_admin: "off".into(),
            send_oper: "off".into(),
            recv_admin: "off".into(),
            recv_oper: "off".into(),
        });
        i.negotiation = Some(negotiation(state, oper_up));
        i.phy = Some(phy(state, oper_up, admin_down));
        i.mac_detail = Some(MacDetail {
            state: String::new(), // derived from admin/proto by the model
            local_fault: None,
            remote_fault: None,
            fec_mode: None,
            fec_corrected: None,
            fec_uncorrected: None,
        });
        i.switchport = Some(if i.l3 {
            // Routed port: the two-line "not a switchport" block.
            Switchport {
                enabled: false,
                ..Switchport::default()
            }
        } else {
            let mode = match state.switchport_mode.as_str() {
                "trunk" => "trunk",
                "dot1q-tunnel" => "dot1q-tunnel",
                _ => "static access",
            };
            Switchport {
                admin_mode: mode.into(),
                oper_mode: mode.into(),
                // Tunnel ports carry the QinQ outer TPID.
                tpid: if mode == "dot1q-tunnel" {
                    "0x88a8".into()
                } else {
                    "0x8100".into()
                },
                access_vlan,
                native_vlan: if state.native_vlan == 0 {
                    1
                } else {
                    state.native_vlan
                },
                trunk_vlans: trunk.then(|| state.trunk_vlans.clone()),
                ..Switchport::default()
            }
        });
    }
    if id.kind == Kind::Management {
        i.switchport = Some(Switchport {
            enabled: false,
            ..Switchport::default()
        });
    }
    Some(i)
}

fn negotiation(state: &pb::InterfaceState, oper_up: bool) -> Negotiation {
    if !state.autoneg {
        return Negotiation::default();
    }
    let advertised: Vec<String> = state
        .supported_modes
        .iter()
        .filter(|m| *m != "auto")
        .cloned()
        .collect();
    Negotiation {
        mode: AutonegMode::Ieee8023,
        status: Some(if oper_up { "success" } else { "in progress" }.into()),
        local: Some(Advertisement {
            speed_duplex: advertised,
            pause: "None".into(),
        }),
        // Link-partner pages are not read back from the PHY yet.
        partner: None,
        resolution: oper_up.then(|| NegotiationResolution {
            speed_duplex: format!("{}/full", fmt::speed_tabular(Some(state.speed_mbps))),
            pause: "rx off, tx off".into(),
        }),
    }
}

fn phy(state: &pb::InterfaceState, oper_up: bool, admin_down: bool) -> Phy {
    Phy {
        state: Some(
            if admin_down {
                "phyOff"
            } else if oper_up {
                "linkUp"
            } else {
                "linkDown"
            }
            .into(),
        ),
        interface_state: Some(if oper_up { "up" } else { "down" }.into()),
        hw_resets: None,
        transceiver: (!state.media.is_empty()).then(|| state.media.clone()),
        oper_speed: (state.speed_mbps > 0).then(|| {
            if state.speed_mbps >= 1000 && state.speed_mbps % 1000 == 0 {
                format!("{}Gbps", state.speed_mbps / 1000)
            } else {
                format!("{}Mbps", state.speed_mbps)
            }
        }),
        interrupt_count: None,
        diags_mode: None,
        model: (!state.phy_model.is_empty()).then(|| state.phy_model.clone()),
        reset_count: None,
        state_changes: Some(state.link_changes),
        last_change_secs: state.seconds_since_change,
        configured_speed: Some(if state.autoneg {
            "auto".into()
        } else {
            fmt::speed_tabular((state.speed_mbps > 0).then_some(state.speed_mbps))
        }),
        autoneg_config: Some(state.autoneg),
    }
}

pub async fn transceivers(pmon: &IpcEndpoint) -> Result<Vec<Transceiver>> {
    let channel = pmon.connect().await.context("connecting to pmon")?;
    let mut client = pb::pmon_client::PmonClient::new(channel);
    let response = client
        .list_transceivers(pb::ListTransceiversRequest {})
        .await?
        .into_inner();
    Ok(response
        .transceivers
        .iter()
        .filter(|x| x.present)
        .filter_map(|x| {
            let id = name::parse_one(&x.port)?;
            let threshold = |t: Option<&pb::DomThreshold>| {
                let t = t?;
                Some(Thresholds {
                    high_alarm: t.high_alarm,
                    high_warn: t.high_warn,
                    low_alarm: t.low_alarm,
                    low_warn: t.low_warn,
                })
            };
            Some(Transceiver {
                id,
                media_type: if x.media_type.is_empty() {
                    x.form_factor.clone()
                } else {
                    x.media_type.clone()
                },
                vendor: x.vendor.clone(),
                part_number: x.part_number.clone(),
                serial: x.serial.clone(),
                date_code: x.date_code.clone(),
                temp_c: x.dom.as_ref().map(|d| d.temp_c),
                voltage_v: x.dom.as_ref().map(|d| d.voltage_v),
                bias_ma: x.dom.as_ref().map(|d| d.bias_ma),
                tx_dbm: x.dom.as_ref().map(|d| d.tx_dbm),
                rx_dbm: x.dom.as_ref().map(|d| d.rx_dbm),
                thresholds: x.thresholds.as_ref().map(|t| DomThresholds {
                    temperature: threshold(t.temperature.as_ref()).unwrap_or_default(),
                    voltage: threshold(t.voltage.as_ref()).unwrap_or_default(),
                    bias: threshold(t.bias.as_ref()).unwrap_or_default(),
                    tx_power: threshold(t.tx_power.as_ref()).unwrap_or_default(),
                    rx_power: threshold(t.rx_power.as_ref()).unwrap_or_default(),
                }),
                eeprom_a0: x.eeprom_a0.clone(),
                eeprom_a2: x.eeprom_a2.clone(),
                age_secs: x.age_secs,
            })
        })
        .collect())
}

/// Selected interface ids validated against the fetched set.
pub fn select(
    interfaces: Vec<Interface>,
    selection: Option<&[InterfaceId]>,
) -> Result<Vec<Interface>, String> {
    let Some(wanted) = selection else {
        return Ok(interfaces);
    };
    let selected: Vec<Interface> = interfaces
        .into_iter()
        .filter(|i| wanted.contains(&i.id))
        .collect();
    if selected.is_empty() {
        let names: Vec<String> = wanted.iter().map(|id| id.full_name()).collect();
        return Err(format!("% No such interface: {}", names.join(", ")));
    }
    Ok(selected)
}
