//! `show interfaces capabilities | flowcontrol | negotiation | phy | mac`.

use crate::interfaces::fmt;
use crate::interfaces::model::{Context, Interface};
use crate::interfaces::name::Kind;
use crate::interfaces::table::{pad, Col, Text};

/// `show interfaces capabilities`.
pub fn capabilities(interfaces: &[Interface]) -> String {
    let mut out = Text::new();
    let mut first = true;
    for i in super::sorted_tabular(interfaces) {
        let Some(caps) = &i.caps else { continue };
        if !first {
            out.blank();
        }
        first = false;
        out.line(i.id.full_name());
        out.line(format!("  {}{}", pad("Model:", Col::left(16)), caps.model));
        out.line(format!(
            "  {}{}",
            pad("Type:", Col::left(16)),
            caps.media_type
        ));
        out.line(format!(
            "  {}{}",
            pad("Speed/Duplex:", Col::left(16)),
            caps.speed_duplex
        ));
        out.line(format!(
            "  {}{}",
            pad("Flowcontrol:", Col::left(16)),
            caps.flowcontrol
        ));
    }
    out.finish()
}

/// `show interfaces flowcontrol`.
pub fn flowcontrol(interfaces: &[Interface]) -> String {
    let mut out = Text::new();
    out.line("Port       Send FlowControl   Receive FlowControl    RxPause   TxPause");
    out.line("           admin   oper       admin   oper");
    out.line("---------  -----   -----      -----   -----          -------   -------");
    for i in super::sorted_tabular(interfaces) {
        let Some(fc) = &i.flowcontrol else { continue };
        let (rx_pause, tx_pause) = i
            .counters
            .as_ref()
            .map(|c| (c.in_pause, c.out_pause))
            .unwrap_or((0, 0));
        out.line(format!(
            "{}{}{}{}{}{}{}",
            pad(&i.id.abbrev(), Col::left(11)),
            pad(&fc.send_admin, Col::left(8)),
            pad(&fc.send_oper, Col::left(11)),
            pad(&fc.recv_admin, Col::left(8)),
            pad(&fc.recv_oper, Col::left(14)),
            pad(&rx_pause.to_string(), Col::right(8)),
            pad(&tx_pause.to_string(), Col::right(10)),
        ));
    }
    out.finish()
}

/// Greedy word-wrap of a speed/duplex advertisement list into lines of at
/// most `width` characters.
fn wrap_words(words: &[String], width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in words {
        match lines.last_mut() {
            Some(line) if line.len() + 1 + word.len() <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.clone()),
        }
    }
    lines
}

/// `show interfaces negotiation`.
pub fn negotiation(interfaces: &[Interface]) -> String {
    const PORT: Col = Col::left(8);
    const MODE: Col = Col::left(13);
    const STATUS: Col = Col::left(33);
    const SPEED: Col = Col::left(20);
    let mut out = Text::new();
    out.line("Port    Auto-Negotiation                              Local Advertisement");
    out.line("        Mode         Status                           Speed/Duplex        Pause");
    for i in super::sorted_tabular(interfaces) {
        if i.id.kind != Kind::Ethernet {
            continue;
        }
        let Some(n) = &i.negotiation else { continue };
        let status = n.status.as_deref().unwrap_or("n/a");
        let (speed_lines, pause) = match &n.local {
            Some(local) => (
                wrap_words(&local.speed_duplex, SPEED.width - 1),
                local.pause.as_str(),
            ),
            None => (vec!["n/a".to_string()], "n/a"),
        };
        let mut speed_lines = speed_lines.into_iter();
        let first_speed = speed_lines.next().unwrap_or_default();
        out.line(format!(
            "{}{}{}{}{}",
            pad(&i.id.abbrev(), PORT),
            pad(n.mode.cell(), MODE),
            pad(status, STATUS),
            pad(&first_speed, SPEED),
            pause
        ));
        let indent = " ".repeat(PORT.width + MODE.width + STATUS.width);
        for line in speed_lines {
            out.line(format!("{indent}{line}"));
        }
    }
    out.finish()
}

/// Capitalize the first character (`success` -> `Success`).
fn title_case(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// `show interfaces negotiation detail`.
pub fn negotiation_detail(interfaces: &[Interface]) -> String {
    let mut out = Text::new();
    let mut first = true;
    for i in super::sorted_tabular(interfaces) {
        if i.id.kind != Kind::Ethernet {
            continue;
        }
        let Some(n) = &i.negotiation else { continue };
        if !first {
            out.blank();
        }
        first = false;
        out.line(i.id.full_name());
        out.line(format!("  Auto-Negotiation Mode: {}", n.mode.detail()));
        out.line(format!(
            "  Auto-Negotiation Status: {}",
            n.status
                .as_deref()
                .map(title_case)
                .unwrap_or_else(|| "n/a".into())
        ));
        for (label, advert) in [
            ("Local Advertisement", &n.local),
            ("Link Partner Advertisement", &n.partner),
        ] {
            let Some(advert) = advert else { continue };
            out.line(format!("  {label}"));
            out.line(format!(
                "    Speed/Duplex: {}",
                advert.speed_duplex.join(" ")
            ));
            out.line(format!("    Pause: {}", advert.pause));
        }
        if let Some(resolution) = &n.resolution {
            out.line("  Resolution");
            out.line(format!("    Speed/Duplex: {}", resolution.speed_duplex));
            out.line(format!("    Pause: {}", resolution.pause));
        }
    }
    out.finish()
}

/// A `label` / `value` row for the phy blocks: value starts at column 45.
fn phy_row(out: &mut Text, indent: usize, label: &str, value: &str) {
    let mut line = format!("{}{label}", " ".repeat(indent));
    line = pad(&line, Col::left(45));
    line.push_str(value);
    out.line(line);
}

/// `show interfaces phy` — the summary table.
pub fn phy(interfaces: &[Interface]) -> String {
    const COLS: [Col; 3] = [Col::left(8), Col::left(13), Col::left(13)];
    let mut out = Text::new();
    out.row(&COLS, &["Port", "PHY State", "Oper Speed", "Model"]);
    for i in super::sorted_tabular(interfaces) {
        let Some(phy) = &i.phy else { continue };
        out.row(
            &COLS,
            &[
                &i.id.abbrev(),
                phy.state.as_deref().unwrap_or("n/a"),
                phy.oper_speed.as_deref().unwrap_or("n/a"),
                phy.model.as_deref().unwrap_or(""),
            ],
        );
    }
    out.finish()
}

/// `show interfaces phy detail`.
pub fn phy_detail(interfaces: &[Interface], ctx: &Context) -> String {
    let mut out = Text::new();
    if let Some(time) = &ctx.system_time {
        out.line(format!("Current System Time: {time}"));
    }
    for i in super::sorted_tabular(interfaces) {
        let Some(phy) = &i.phy else { continue };
        out.line(i.id.full_name());
        out.line("  Current State");
        let rows: [(&str, Option<String>); 9] = [
            ("PHY state", phy.state.clone()),
            ("Interface state", phy.interface_state.clone()),
            ("HW resets", phy.hw_resets.map(|v| v.to_string())),
            ("Transceiver", phy.transceiver.clone()),
            ("Oper speed", phy.oper_speed.clone()),
            (
                "Interrupt count",
                phy.interrupt_count.map(|v| v.to_string()),
            ),
            ("Diags mode", phy.diags_mode.clone()),
            ("Model", phy.model.clone()),
            ("Reset count", phy.reset_count.map(|v| v.to_string())),
        ];
        for (label, value) in rows {
            if let Some(value) = value {
                phy_row(&mut out, 4, label, &value);
            }
        }
        if let Some(changes) = phy.state_changes {
            phy_row(&mut out, 4, "PHY state changes", &changes.to_string());
            if let Some(secs) = phy.last_change_secs {
                phy_row(
                    &mut out,
                    6,
                    "Last change",
                    &format!("{} ago", fmt::duration_compact(secs)),
                );
            }
        }
        if phy.configured_speed.is_some() || phy.autoneg_config.is_some() {
            out.line("  Speed Configuration");
            if let Some(speed) = &phy.configured_speed {
                phy_row(&mut out, 4, "Configured speed", speed);
            }
            if let Some(autoneg) = phy.autoneg_config {
                phy_row(
                    &mut out,
                    4,
                    "Auto-negotiation",
                    if autoneg { "on" } else { "off" },
                );
            }
        }
    }
    out.finish()
}

/// `show interfaces mac`.
pub fn mac(interfaces: &[Interface]) -> String {
    const COLS: [Col; 2] = [Col::left(8), Col::left(21)];
    let mut out = Text::new();
    out.row(&COLS, &["Port", "MAC Address", "State"]);
    for i in super::sorted_tabular(interfaces) {
        if i.id.kind != Kind::Ethernet {
            continue;
        }
        let Some(mac) = &i.mac else { continue };
        out.row(&COLS, &[&i.id.abbrev(), mac, &i.mac_state()]);
    }
    out.finish()
}

/// `show interfaces mac detail`.
pub fn mac_detail(interfaces: &[Interface]) -> String {
    let mut out = Text::new();
    let mut first = true;
    for i in super::sorted_tabular(interfaces) {
        if i.id.kind != Kind::Ethernet {
            continue;
        }
        let Some(detail) = &i.mac_detail else {
            continue;
        };
        if !first {
            out.blank();
        }
        first = false;
        out.line(i.id.full_name());
        let row = |out: &mut Text, label: &str, value: &str| {
            out.line(format!("  {}{value}", pad(label, Col::left(24))));
        };
        if let Some(mac) = &i.mac {
            row(&mut out, "MAC address:", mac);
        }
        row(&mut out, "MAC state:", &i.mac_state());
        let flag = |value: bool| if value { "True" } else { "False" };
        if let Some(fault) = detail.local_fault {
            row(&mut out, "Local fault:", flag(fault));
        }
        if let Some(fault) = detail.remote_fault {
            row(&mut out, "Remote fault:", flag(fault));
        }
        if let Some(mode) = &detail.fec_mode {
            row(&mut out, "FEC mode:", mode);
        }
        // The codeword counters right-align to end at column 29.
        if let Some(corrected) = detail.fec_corrected {
            let label = "FEC corrected codewords:";
            out.line(format!(
                "  {label}{}",
                pad(&corrected.to_string(), Col::right(28 - label.len()))
            ));
        }
        if let Some(uncorrected) = detail.fec_uncorrected {
            let label = "FEC uncorrected codewords:";
            out.line(format!(
                "  {label}{}",
                pad(&uncorrected.to_string(), Col::right(28 - label.len()))
            ));
        }
    }
    out.finish()
}
