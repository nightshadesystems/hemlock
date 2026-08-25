//! EOS-style text renderers for the QoS-suite show family.

use crate::interfaces::table::{Col, Text};

use super::model::{MapState, PortQos, PortQosState, WredState};

/// The abbreviated interface form for tabular output
/// ("Ethernet1" -> "Et1").
fn short_name(interface: &str) -> String {
    crate::interfaces::name::parse_one(interface)
        .map(|id| id.abbrev())
        .unwrap_or_else(|| interface.to_string())
}

/// A queue list for the summary grid ("7", "7, 6"); `-` when empty.
fn queue_list(queues: &[u8]) -> String {
    if queues.is_empty() {
        return "-".into();
    }
    queues
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

// ------------------------------------------------- Global maps

/// `show qos maps` — the four tables, each with its unmapped-value
/// footer.
pub fn maps(state: &MapState) -> String {
    // A two-space indent column, then the key column; the value column
    // runs to end-of-line.
    const COLS: [Col; 2] = [Col::left(2), Col::left(6)];
    let mut out = Text::new();
    for (index, table) in state.tables.iter().enumerate() {
        if index > 0 {
            out.blank();
        }
        out.line(format!("{}:", table.title));
        out.row(&COLS, &["", &table.key_label, &table.value_label]);
        out.row(&COLS, &["", "----", &"-".repeat(table.value_label.len())]);
        for entry in &table.entries {
            out.row(
                &COLS,
                &["", &entry.key.to_string(), &entry.value.to_string()],
            );
        }
        out.line(format!("  (all others -> {})", table.default_note));
    }
    out.finish()
}

// ------------------------------------------------- WRED profiles

/// `show qos wred`.
pub fn wred(state: &WredState) -> String {
    const COLS: [Col; 5] = [
        Col::left(9),
        Col::left(10),
        Col::left(10),
        Col::left(11),
        Col::left(5),
    ];
    let mut out = Text::new();
    out.row(
        &COLS,
        &[
            "Profile",
            "Min (KB)",
            "Max (KB)",
            "Drop Prob",
            "ECN",
            "References",
        ],
    );
    out.row(
        &COLS,
        &[
            "-------",
            "--------",
            "--------",
            "---------",
            "---",
            "----------",
        ],
    );
    for profile in &state.profiles {
        let references = if profile.references.is_empty() {
            "-".to_string()
        } else {
            profile.references.join(", ")
        };
        out.row(
            &COLS,
            &[
                &profile.name,
                &profile.min_threshold.to_string(),
                &profile.max_threshold.to_string(),
                &format!("{}%", profile.drop_probability),
                if profile.ecn { "yes" } else { "no" },
                &references,
            ],
        );
    }
    // A platform whose SAI serves no WRED would silently drop every
    // profile, so say so rather than render a table that does nothing.
    if !state.supported {
        out.blank();
        out.line("WRED is not supported by this platform's SAI.");
    }
    out.finish()
}

// ------------------------------------------------- Per-port QoS

/// `show qos interface <port>` — the per-port detail block.
pub fn interface(state: &PortQosState) -> String {
    let mut out = Text::new();
    for (index, port) in state.ports.iter().enumerate() {
        if index > 0 {
            out.blank();
        }
        out.line(format!("{}:", port.port));
        let field = |name: &str, value: &str| format!("  {name:<15}: {value}");
        out.line(field("Trust mode", &port.trust));
        out.line(field("Default TC", &port.default_tc.to_string()));
        out.line(field(
            "Port shaper",
            port.shaper.as_deref().unwrap_or("none"),
        ));
        if let Some(lag) = &port.via_port_channel {
            out.line(field("Configured on", lag));
        }
        out.blank();

        const COLS: [Col; 6] = [
            Col::left(2),
            Col::left(7),
            Col::left(8),
            Col::left(8),
            Col::left(11),
            Col::left(9),
        ];
        out.row(
            &COLS,
            &["", "Queue", "Mode", "Weight", "Shaper", "WRED", "ECN"],
        );
        out.row(
            &COLS,
            &[
                "",
                "-----",
                "------",
                "------",
                "---------",
                "-------",
                "---",
            ],
        );
        // Highest queue first: the strict band lives at the top of the
        // scheduler, so it reads down the priority order.
        let mut queues: Vec<&super::model::QueueQos> = port.queues.iter().collect();
        queues.sort_by_key(|queue| std::cmp::Reverse(queue.queue));
        for queue in queues {
            out.row(
                &COLS,
                &[
                    "",
                    &queue.queue.to_string(),
                    &queue.mode,
                    &queue
                        .weight
                        .map(|w| w.to_string())
                        .unwrap_or_else(|| "-".into()),
                    queue.shaper.as_deref().unwrap_or("-"),
                    queue.wred_profile.as_deref().unwrap_or("-"),
                    if queue.wred_profile.is_none() {
                        "-"
                    } else if queue.ecn {
                        "yes"
                    } else {
                        "no"
                    },
                ],
            );
        }
    }
    out.finish()
}

/// `show qos interfaces` — the summary grid. Only ports with
/// non-default config appear; a member carrying its Port-Channel's
/// program is folded into the Po row, and the trailing line counts the
/// front-panel ports left at the defaults.
pub fn interfaces(state: &PortQosState) -> String {
    const COLS: [Col; 6] = [
        Col::left(7),
        Col::left(11),
        Col::left(8),
        Col::left(11),
        Col::left(11),
        Col::left(7),
    ];
    let mut out = Text::new();
    out.row(
        &COLS,
        &["Port", "Trust", "Def-TC", "Strict Qs", "Shaper", "WRED Qs"],
    );
    out.row(
        &COLS,
        &[
            "-----",
            "---------",
            "------",
            "---------",
            "---------",
            "-------",
        ],
    );
    for port in shown(state) {
        out.row(
            &COLS,
            &[
                &short_name(&port.port),
                &port.trust,
                &port.default_tc.to_string(),
                &queue_list(&port.strict_queues()),
                port.shaper.as_deref().unwrap_or("-"),
                &queue_list(&port.wred_queues()),
            ],
        );
    }
    if state.default_ports > 0 {
        out.line(format!(
            "... {} ports with default QoS configuration",
            state.default_ports
        ));
    }
    out.finish()
}

/// The summary grid's rows: configured ports, with Port-Channel members
/// folded into their Po row (the program is the Port-Channel's, so
/// listing both would double-count it).
fn shown(state: &PortQosState) -> impl Iterator<Item = &PortQos> {
    state
        .ports
        .iter()
        .filter(|port| port.configured && port.via_port_channel.is_none())
}
