//! The `show interfaces counters` family.

use crate::interfaces::fmt;
use crate::interfaces::model::{Interface, BIN_LABELS};
use crate::interfaces::table::{pad, Col, Text};

/// `show interfaces counters` — stacked input/output octet tables.
pub fn counters(interfaces: &[Interface]) -> String {
    const COLS: [Col; 5] = [
        Col::left(4),
        Col::right(25),
        Col::right(16),
        Col::right(18),
        Col::right(16),
    ];
    let rows: Vec<&Interface> = super::sorted_tabular(interfaces)
        .into_iter()
        .filter(|i| super::has_counter_rows(i))
        .collect();

    let mut out = Text::new();
    out.row(
        &COLS,
        &[
            "Port",
            "InOctets",
            "InUcastPkts",
            "InMcastPkts",
            "InBcastPkts",
        ],
    );
    for i in &rows {
        let Some(c) = &i.counters else { continue };
        out.row(
            &COLS,
            &[
                &i.id.abbrev(),
                &c.in_octets.to_string(),
                &c.in_ucast_pkts.to_string(),
                &c.in_mcast_pkts.to_string(),
                &c.in_bcast_pkts.to_string(),
            ],
        );
    }
    out.blank();
    out.row(
        &COLS,
        &[
            "Port",
            "OutOctets",
            "OutUcastPkts",
            "OutMcastPkts",
            "OutBcastPkts",
        ],
    );
    for i in &rows {
        let Some(c) = &i.counters else { continue };
        out.row(
            &COLS,
            &[
                &i.id.abbrev(),
                &c.out_octets.to_string(),
                &c.out_ucast_pkts.to_string(),
                &c.out_mcast_pkts.to_string(),
                &c.out_bcast_pkts.to_string(),
            ],
        );
    }
    out.finish()
}

/// `show interfaces counters errors`.
pub fn errors(interfaces: &[Interface]) -> String {
    const COLS: [Col; 8] = [
        Col::left(4),
        Col::right(16),
        Col::right(12),
        Col::right(12),
        Col::right(13),
        Col::right(12),
        Col::right(12),
        Col::right(12),
    ];
    let mut out = Text::new();
    out.row(
        &COLS,
        &[
            "Port",
            "FCSErr",
            "AlignErr",
            "SymbolErr",
            "RxErr",
            "Runts",
            "Giants",
            "TxErr",
        ],
    );
    for i in super::sorted_tabular(interfaces) {
        if !super::has_counter_rows(i) {
            continue;
        }
        let Some(c) = &i.counters else { continue };
        out.row(
            &COLS,
            &[
                &i.id.abbrev(),
                &c.in_crc_errors.to_string(),
                &c.in_alignment_errors.to_string(),
                &c.in_symbol_errors.to_string(),
                &c.in_errors.to_string(),
                &c.in_runts.to_string(),
                &c.in_giants.to_string(),
                &c.out_errors.to_string(),
            ],
        );
    }
    out.finish()
}

/// `show interfaces counters discards`.
pub fn discards(interfaces: &[Interface]) -> String {
    const COLS: [Col; 3] = [Col::left(4), Col::right(19), Col::right(18)];
    let mut out = Text::new();
    out.row(&COLS, &["Port", "InDiscards", "OutDiscards"]);
    for i in super::sorted_tabular(interfaces) {
        if !super::has_counter_rows(i) {
            continue;
        }
        let Some(c) = &i.counters else { continue };
        out.row(
            &COLS,
            &[
                &i.id.abbrev(),
                &c.in_discards.to_string(),
                &c.out_discards.to_string(),
            ],
        );
    }
    out.finish()
}

/// `show interfaces counters rates`.
pub fn rates(interfaces: &[Interface]) -> String {
    const COLS: [Col; 8] = [
        Col::left(10),
        Col::right(5),
        Col::right(9),
        Col::right(12),
        Col::right(7),
        Col::right(10),
        Col::right(12),
        Col::right(8),
    ];
    let mut out = Text::new();
    out.row(
        &COLS,
        &[
            "Port", "Intvl", "InMbps", "InKpps", "InPct", "OutMbps", "OutKpps", "OutPct",
        ],
    );
    for i in super::sorted_tabular(interfaces) {
        if !super::has_counter_rows(i) {
            continue;
        }
        let Some(r) = &i.rates else { continue };
        out.row(
            &COLS,
            &[
                &i.id.abbrev(),
                &fmt::intvl_cell(r.interval_secs),
                &format!("{:.1}", r.in_bps / 1e6),
                &format!("{:.1}", r.in_pps as f64 / 1e3),
                &fmt::pct(r.in_util_pct),
                &format!("{:.1}", r.out_bps / 1e6),
                &format!("{:.1}", r.out_pps as f64 / 1e3),
                &fmt::pct(r.out_util_pct),
            ],
        );
    }
    out.finish()
}

/// `show interfaces counters queue` — one row per egress queue per port.
pub fn queues(interfaces: &[Interface]) -> String {
    const COLS: [Col; 6] = [
        Col::left(10),
        Col::left(3),
        Col::right(16),
        Col::right(21),
        Col::right(18),
        Col::right(18),
    ];
    let mut out = Text::new();
    out.row(
        &COLS,
        &[
            "Port",
            "TxQ",
            "Counter/pkts",
            "Counter/bytes",
            "Dropped/pkts",
            "Dropped/bytes",
        ],
    );
    for i in super::sorted_tabular(interfaces) {
        for q in &i.queues {
            out.row(
                &COLS,
                &[
                    &i.id.abbrev(),
                    &q.queue,
                    &q.pkts.to_string(),
                    &q.bytes.to_string(),
                    &q.dropped_pkts.to_string(),
                    &q.dropped_bytes.to_string(),
                ],
            );
        }
    }
    out.finish()
}

/// `show interfaces counters bins` — RMON frame-size distribution.
pub fn bins(interfaces: &[Interface]) -> String {
    let mut out = Text::new();
    let mut first = true;
    for i in super::sorted_tabular(interfaces) {
        let Some(bins) = &i.bins else { continue };
        if !first {
            out.blank();
        }
        first = false;
        out.line(i.id.full_name());
        out.line("  Received frame size distribution:");
        for (label, value) in BIN_LABELS.iter().zip(bins.rx.iter()) {
            out.line(bin_line(label, *value));
        }
        out.line("  Transmitted frame size distribution:");
        for (label, value) in BIN_LABELS.iter().zip(bins.tx.iter()) {
            out.line(bin_line(label, *value));
        }
    }
    out.finish()
}

/// `    64 bytes:                412334981` — label + count right-aligned
/// in a 34-character field.
fn bin_line(label: &str, value: u64) -> String {
    let count = pad(
        &value.to_string(),
        Col::right(34usize.saturating_sub(label.len())),
    );
    format!("    {label}{count}")
}
