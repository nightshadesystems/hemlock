//! `show interfaces [<name>]` — the per-interface detail block.

use crate::interfaces::fmt;
use crate::interfaces::model::{Counters, Interface, LineProtocol};
use crate::interfaces::name::Kind;
use crate::interfaces::table::Text;

/// Render the detail blocks for `interfaces`, in EOS detail order, blocks
/// separated by a blank line.
pub fn render(interfaces: &[Interface]) -> String {
    let mut out = Text::new();
    let sorted = super::sorted_detail(interfaces);
    for (i, interface) in sorted.iter().enumerate() {
        if i > 0 {
            out.blank();
        }
        block(&mut out, interface);
    }
    out.finish()
}

fn block(out: &mut Text, i: &Interface) {
    out.line(format!(
        "{} is {}, line protocol is {} ({})",
        i.id.full_name(),
        i.admin.state_line(),
        i.proto.word(),
        i.status.word()
    ));

    match (&i.mac, &i.bia) {
        (Some(mac), Some(bia)) => {
            out.line(format!(
                "  Hardware is {}, address is {mac} (bia {bia})",
                i.hardware()
            ));
        }
        (Some(mac), None) => {
            out.line(format!("  Hardware is {}, address is {mac}", i.hardware()));
        }
        _ => out.line(format!("  Hardware is {}", i.hardware())),
    }

    if let Some(description) = &i.description {
        out.line(format!("  Description: {description}"));
    }

    if let Some(ip) = &i.ip {
        out.line(format!("  Internet address is {}", ip.address));
        out.line(format!("  Broadcast address is {}", ip.broadcast));
    }

    let mtu_line = if i.l3 {
        format!("  IP MTU {} bytes", i.mtu)
    } else {
        format!("  Ethernet MTU {} bytes", i.mtu)
    };
    match i.bandwidth_kbit {
        Some(bw) => out.line(format!("{mtu_line}, BW {bw} kbit")),
        None => out.line(mtu_line),
    }

    // Physical line — front-panel and management ports only.
    if matches!(i.id.kind, Kind::Ethernet | Kind::Management) {
        if let Some(phys) = &i.phys {
            out.line(format!(
                "  {}, {}, auto negotiation: {}, uni-link: {}",
                phys.duplex.detail(),
                fmt::speed_detail(phys.speed_mbps),
                if phys.autoneg { "on" } else { "off" },
                phys.uni_link.as_deref().unwrap_or("n/a")
            ));
        }
    }

    if let Some(secs) = i.last_change_secs {
        let word = if i.proto == LineProtocol::Up {
            "Up"
        } else {
            "Down"
        };
        out.line(format!("  {word} {}", fmt::duration_verbose(secs)));
    }

    if let Some(mode) = &i.loopback_mode {
        out.line(format!("  Loopback Mode : {mode}"));
    }

    if let Some(meta) = &i.counter_meta {
        out.line(format!(
            "  {} link status changes since last clear",
            meta.link_changes
        ));
        let cleared = match meta.last_clear_secs {
            None => "never".into(),
            Some(secs) => format!("{} ago", fmt::duration_compact(secs)),
        };
        out.line(format!(
            "  Last clearing of \"show interface\" counters {cleared}"
        ));
    }

    if i.id.kind == Kind::PortChannel {
        out.line(format!(
            "  Active members in this channel: {}",
            i.members.len()
        ));
        for member in &i.members {
            out.line(format!(
                "  ... {} , {}, {}",
                member.id.full_name(),
                member.duplex.detail(),
                fmt::speed_detail(Some(member.speed_mbps))
            ));
        }
        if let Some(mode) = &i.fallback_mode {
            out.line(format!("  Fallback mode is: {mode}"));
        }
    }

    if let Some(rates) = &i.rates {
        let label = fmt::load_interval_label(rates.interval_secs);
        out.line(format!(
            "  {label} input rate {} ({} with framing overhead), {} packets/sec",
            fmt::rate_bps(rates.in_bps),
            fmt::pct(rates.in_util_pct),
            rates.in_pps
        ));
        out.line(format!(
            "  {label} output rate {} ({} with framing overhead), {} packets/sec",
            fmt::rate_bps(rates.out_bps),
            fmt::pct(rates.out_util_pct),
            rates.out_pps
        ));
    }

    if let Some(counters) = &i.counters {
        if i.id.kind == Kind::PortChannel {
            reduced_counters(out, counters);
        } else {
            full_counters(out, counters);
        }
    }
}

fn full_counters(out: &mut Text, c: &Counters) {
    out.line(format!(
        "     {} packets input, {} bytes",
        c.in_pkts, c.in_octets
    ));
    out.line(format!(
        "     Received {} broadcasts, {} multicast",
        c.in_bcast_pkts, c.in_mcast_pkts
    ));
    out.line(format!("     {} runts, {} giants", c.in_runts, c.in_giants));
    out.line(format!(
        "     {} input errors, {} CRC, {} alignment, {} symbol, {} input discards",
        c.in_errors, c.in_crc_errors, c.in_alignment_errors, c.in_symbol_errors, c.in_discards
    ));
    out.line(format!("     {} PAUSE input", c.in_pause));
    out.line(format!(
        "     {} packets output, {} bytes",
        c.out_pkts, c.out_octets
    ));
    out.line(format!(
        "     Sent {} broadcasts, {} multicast",
        c.out_bcast_pkts, c.out_mcast_pkts
    ));
    out.line(format!(
        "     {} output errors, {} collisions",
        c.out_errors, c.collisions
    ));
    out.line(format!(
        "     {} late collision, {} deferred, {} output discards",
        c.late_collisions, c.deferred, c.out_discards
    ));
    out.line(format!("     {} PAUSE output", c.out_pause));
}

/// Port-channels aggregate member counters and print the reduced block
/// (no runts/giants/CRC/collision lines).
fn reduced_counters(out: &mut Text, c: &Counters) {
    out.line(format!(
        "     {} packets input, {} bytes",
        c.in_pkts, c.in_octets
    ));
    out.line(format!(
        "     Received {} broadcasts, {} multicast",
        c.in_bcast_pkts, c.in_mcast_pkts
    ));
    out.line(format!(
        "     {} input errors, {} input discards",
        c.in_errors, c.in_discards
    ));
    out.line(format!(
        "     {} packets output, {} bytes",
        c.out_pkts, c.out_octets
    ));
    out.line(format!(
        "     Sent {} broadcasts, {} multicast",
        c.out_bcast_pkts, c.out_mcast_pkts
    ));
    out.line(format!(
        "     {} output errors, {} output discards",
        c.out_errors, c.out_discards
    ));
}
