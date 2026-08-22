//! `show interfaces description` and `show interfaces status`.

use crate::interfaces::fmt;
use crate::interfaces::model::{IfStatus, Interface};
use crate::interfaces::table::{Col, Text};

/// `show interfaces description`.
pub fn description(interfaces: &[Interface]) -> String {
    const COLS: [Col; 3] = [Col::left(31), Col::left(15), Col::left(19)];
    let mut out = Text::new();
    out.row(&COLS, &["Interface", "Status", "Protocol", "Description"]);
    for i in super::sorted_tabular(interfaces) {
        out.row(
            &COLS,
            &[
                &i.id.abbrev(),
                i.admin.table_word(),
                i.proto.word(),
                i.description.as_deref().unwrap_or(""),
            ],
        );
    }
    out.finish()
}

/// Row filter for `show interfaces status [<filter>]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    All,
    Connected,
    NotConnect,
    ErrDisabled,
    Inactive,
}

impl StatusFilter {
    fn matches(self, status: IfStatus) -> bool {
        match self {
            StatusFilter::All => true,
            StatusFilter::Connected => status == IfStatus::Connected,
            StatusFilter::NotConnect => status == IfStatus::NotConnect,
            StatusFilter::ErrDisabled => status == IfStatus::ErrDisabled,
            StatusFilter::Inactive => status == IfStatus::Inactive,
        }
    }
}

/// `show interfaces status`. The errdisabled filter switches to the
/// Port/Name/Status/Reason layout; every other filter keeps the full
/// header and restricts rows.
pub fn status(interfaces: &[Interface], filter: StatusFilter) -> String {
    if filter == StatusFilter::ErrDisabled {
        return status_errdisabled(interfaces);
    }
    const COLS: [Col; 8] = [
        Col::left(11),
        Col::left(30),
        Col::left(13),
        Col::left(9),
        Col::left(7),
        Col::left(7),
        Col::left(16),
        Col::left(6),
    ];
    let mut out = Text::new();
    out.row(
        &COLS,
        &[
            "Port",
            "Name",
            "Status",
            "Vlan",
            "Duplex",
            "Speed",
            "Type",
            "Flags",
            "Encapsulation",
        ],
    );
    for i in super::sorted_tabular(interfaces) {
        if !super::is_port_like(i) || !filter.matches(i.status) {
            continue;
        }
        let name = fmt::truncate_hard(i.description.as_deref().unwrap_or(""), 26);
        let (duplex, speed) = duplex_speed_cells(i);
        out.row(
            &COLS,
            &[
                &i.id.abbrev(),
                &name,
                i.status.word(),
                &i.vlan_membership.cell(),
                &duplex,
                &speed,
                i.media.as_deref().unwrap_or("N/A"),
                "",
                "",
            ],
        );
    }
    out.finish()
}

/// Duplex/Speed cells with the `a-` prefix for auto-negotiated values.
fn duplex_speed_cells(i: &Interface) -> (String, String) {
    match &i.phys {
        Some(phys) => {
            let prefix = if phys.speed_from_autoneg { "a-" } else { "" };
            (
                format!("{prefix}{}", phys.duplex.cell()),
                format!("{prefix}{}", fmt::speed_tabular(phys.speed_mbps)),
            )
        }
        // No physical layer (port-channels): full duplex at the
        // aggregate bandwidth.
        None => ("full".into(), fmt::speed_tabular(i.speed_mbps())),
    }
}

/// `show interfaces status errdisabled`.
fn status_errdisabled(interfaces: &[Interface]) -> String {
    const COLS: [Col; 3] = [Col::left(11), Col::left(30), Col::left(13)];
    let mut out = Text::new();
    out.row(&COLS, &["Port", "Name", "Status", "Reason"]);
    for i in super::sorted_tabular(interfaces) {
        if !super::is_port_like(i) || i.status != IfStatus::ErrDisabled {
            continue;
        }
        let name = fmt::truncate_hard(i.description.as_deref().unwrap_or(""), 26);
        out.row(
            &COLS,
            &[
                &i.id.abbrev(),
                &name,
                "errdisabled",
                i.errdisable_reason.as_deref().unwrap_or(""),
            ],
        );
    }
    out.finish()
}
