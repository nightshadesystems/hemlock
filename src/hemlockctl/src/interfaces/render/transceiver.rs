//! The `show interfaces transceiver` family.

use crate::interfaces::fmt;
use crate::interfaces::model::{DomThresholds, Interface, Thresholds, Transceiver};
use crate::interfaces::table::{pad, Col, Text};

fn sorted(transceivers: &[Transceiver]) -> Vec<&Transceiver> {
    let mut refs: Vec<&Transceiver> = transceivers.iter().collect();
    refs.sort_by_key(|x| x.id);
    refs
}

fn dom_cell(value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{v:.2}"),
        None => "N/A".into(),
    }
}

/// `show interfaces transceiver` — the DOM summary table.
pub fn summary(transceivers: &[Transceiver]) -> String {
    let mut out = Text::new();
    out.line("If system temperature is too high, transceiver temperature will rise 5 C");
    out.line("per 1 C rise in system temperature");
    // The stacked header carries mixed alignment; kept literal.
    out.line("                                                    Rx Power   Tx Power");
    out.line(
        "Port       Temp (C)  Voltage (V)  Bias (mA)         (dBm)      (dBm)     Last Update",
    );
    out.line("---------- --------- ------------ ----------------- ---------- --------- -------------------");
    for x in sorted(transceivers) {
        out.line(format!(
            "{} {} {} {} {} {} {} ago",
            pad(&x.id.abbrev(), Col::left(10)),
            pad(&dom_cell(x.temp_c), Col::right(9)),
            pad(&dom_cell(x.voltage_v), Col::right(12)),
            pad(&dom_cell(x.bias_ma), Col::right(17)),
            pad(&dom_cell(x.rx_dbm), Col::right(10)),
            pad(&dom_cell(x.tx_dbm), Col::right(10)),
            fmt::duration_compact(x.age_secs)
        ));
    }
    out.finish()
}

/// One threshold pair line for the detail view: values right-aligned to
/// end at column 32 (alarm) and 64 (warn), unit appended, the warn label
/// starting at column 37.
fn threshold_lines(out: &mut Text, t: &Thresholds, unit: &str) {
    for (alarm_label, warn_label, alarm, warn) in [
        (
            "High alarm threshold:",
            "High warn threshold:",
            t.high_alarm,
            t.high_warn,
        ),
        (
            "Low alarm threshold:",
            "Low warn threshold:",
            t.low_alarm,
            t.low_warn,
        ),
    ] {
        let mut line = format!("    {alarm_label}");
        let value = format!("{alarm:.2}");
        line.push_str(&pad(&value, Col::right(32usize.saturating_sub(line.len()))));
        line.push(' ');
        line.push_str(unit);
        let mut line = pad(&line, Col::left(37));
        line.push_str(warn_label);
        let value = format!("{warn:.2}");
        line.push_str(&pad(&value, Col::right(64usize.saturating_sub(line.len()))));
        line.push(' ');
        line.push_str(unit);
        out.line(line);
    }
}

/// `show interfaces transceiver detail`.
pub fn detail(transceivers: &[Transceiver]) -> String {
    let mut out = Text::new();
    let mut first = true;
    for x in sorted(transceivers) {
        if !first {
            out.blank();
        }
        first = false;
        out.line(x.id.full_name());
        out.line(format!("  Transceiver Type: {}", x.media_type));
        out.line(format!("  Vendor Name: {}", x.vendor));
        out.line(format!("  Vendor Part Number: {}", x.part_number));
        out.line(format!("  Vendor Serial Number: {}", x.serial));
        out.line(format!("  Vendor Date Code: {}", x.date_code));
        type Pick = fn(&DomThresholds) -> &Thresholds;
        let sections: [(&str, Option<f64>, &str, Pick); 5] = [
            ("Temperature", x.temp_c, "C", |t| &t.temperature),
            ("Voltage", x.voltage_v, "V", |t| &t.voltage),
            ("Tx Bias", x.bias_ma, "mA", |t| &t.bias),
            ("Tx Power", x.tx_dbm, "dBm", |t| &t.tx_power),
            ("Rx Power", x.rx_dbm, "dBm", |t| &t.rx_power),
        ];
        for (label, value, unit, pick) in sections {
            let Some(value) = value else { continue };
            out.line(format!("  {label}: {value:.2} {unit}"));
            if let Some(thresholds) = &x.thresholds {
                threshold_lines(&mut out, pick(thresholds), unit);
            }
        }
    }
    out.finish()
}

/// `show interfaces transceiver properties`. Needs the interface model
/// for the admin/oper speed and duplex facts.
pub fn properties(transceivers: &[Transceiver], interfaces: &[Interface]) -> String {
    let mut out = Text::new();
    let mut first = true;
    for x in sorted(transceivers) {
        if !first {
            out.blank();
        }
        first = false;
        let interface = interfaces.iter().find(|i| i.id == x.id);
        let phys = interface.and_then(|i| i.phys.as_ref());
        let admin_speed = interface
            .and_then(|i| i.phy.as_ref())
            .and_then(|p| p.configured_speed.clone())
            .unwrap_or_else(|| {
                phys.map(|p| fmt::speed_tabular(p.speed_mbps))
                    .unwrap_or_else(|| "auto".into())
            });
        let oper_speed = phys
            .map(|p| fmt::speed_tabular(p.speed_mbps))
            .unwrap_or_else(|| "unconf".into());
        let duplex = phys.map(|p| p.duplex.cell()).unwrap_or("full");
        // EOS prints `Name : <abbrev>` (space before the colon) here.
        out.line(format!("Name : {}", x.id.abbrev()));
        out.line(format!("Administrative Speed: {admin_speed}"));
        out.line(format!("Administrative Duplex: {duplex}"));
        out.line(format!("Operational Speed: {oper_speed}"));
        out.line(format!("Operational Duplex: {duplex}"));
        out.line(format!("Media Type: {}", x.media_type));
    }
    out.finish()
}

/// `show interfaces transceiver eeprom` — raw SFF page hex dump.
pub fn eeprom(transceivers: &[Transceiver]) -> String {
    let mut out = Text::new();
    let mut first = true;
    for x in sorted(transceivers) {
        if !first {
            out.blank();
        }
        first = false;
        out.line(format!("{}:", x.id.full_name()));
        for (page, bytes) in [("A0", &x.eeprom_a0), ("A2", &x.eeprom_a2)] {
            if bytes.is_empty() {
                continue;
            }
            out.line(format!("  {page} page:"));
            for (row, chunk) in bytes.chunks(16).enumerate() {
                let mut line = format!("    {:04x}:", row * 16);
                for (i, byte) in chunk.iter().enumerate() {
                    if i == 8 {
                        line.push(' ');
                    }
                    line.push_str(&format!(" {byte:02x}"));
                }
                out.line(line);
            }
        }
    }
    out.finish()
}
