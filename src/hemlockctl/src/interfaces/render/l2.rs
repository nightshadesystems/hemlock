//! `show interfaces switchport | trunk | vlans`.

use crate::interfaces::fmt;
use crate::interfaces::model::{Context, Interface, Switchport};
use crate::interfaces::name::Kind;
use crate::interfaces::table::{Col, Text};

/// `show interfaces switchport`. Note the EOS quirk: this command prints
/// abbreviated names (`Name: Et1`) despite being detail-style output.
pub fn switchport(interfaces: &[Interface], ctx: &Context) -> String {
    let mut out = Text::new();
    out.line(format!(
        "Default switchport mode: {}",
        ctx.default_switchport_mode
    ));
    for i in super::sorted_tabular(interfaces) {
        let Some(sp) = &i.switchport else { continue };
        out.blank();
        out.line(format!("Name: {}", i.id.abbrev()));
        if !sp.enabled {
            out.line("Switchport: Disabled");
            continue;
        }
        out.line("Switchport: Enabled");
        out.line(format!("Administrative Mode: {}", sp.admin_mode));
        out.line(format!("Operational Mode: {}", sp.oper_mode));
        out.line(format!(
            "MAC Address Learning: {}",
            enabled_word(sp.mac_learning)
        ));
        out.line(format!(
            "Dot1q ethertype/TPID: {} ({})",
            sp.tpid,
            if sp.tpid_active { "active" } else { "inactive" }
        ));
        out.line(format!(
            "Dot1q Source Port Filter: {}",
            sp.source_port_filter
        ));
        out.line(format!(
            "Access Mode VLAN: {}",
            ctx.vlan_with_name(sp.access_vlan)
        ));
        out.line(format!(
            "Trunking Native Mode VLAN: {}",
            ctx.vlan_with_name(sp.native_vlan)
        ));
        out.line(format!(
            "Administrative Native VLAN tagging: {}",
            enabled_word(sp.native_tagging)
        ));
        let trunk_vlans = match &sp.trunk_vlans {
            None => "ALL".to_string(),
            Some(vlans) => fmt::compress_vlans(vlans),
        };
        out.line(format!("Trunking VLANs Enabled: {trunk_vlans}"));
        out.line(format!(
            "Static Trunk Groups: {}",
            sp.static_trunk_groups.join(" ")
        ));
        out.line(format!(
            "Dynamic Trunk Groups: {}",
            sp.dynamic_trunk_groups.join(" ")
        ));
        out.line(format!(
            "Source interface filtering: {}",
            enabled_word(sp.source_interface_filtering)
        ));
    }
    out.finish()
}

fn enabled_word(enabled: bool) -> &'static str {
    if enabled {
        "enabled"
    } else {
        "disabled"
    }
}

fn is_trunking(i: &Interface) -> Option<&Switchport> {
    i.switchport
        .as_ref()
        .filter(|sp| sp.enabled && sp.oper_mode == "trunk")
}

/// The allowed-VLAN list of a trunk (explicit list, or every active VLAN
/// when the trunk allows ALL).
fn allowed_vlans(sp: &Switchport, ctx: &Context) -> Vec<u32> {
    match &sp.trunk_vlans {
        Some(vlans) => vlans.clone(),
        None => ctx.active_vlans.clone(),
    }
}

/// `show interfaces trunk` — four stacked tables.
pub fn trunk(interfaces: &[Interface], ctx: &Context) -> String {
    const COLS: [Col; 3] = [Col::left(11), Col::left(14), Col::left(16)];
    const LIST_COLS: [Col; 1] = [Col::left(11)];
    let trunks: Vec<&Interface> = super::sorted_tabular(interfaces)
        .into_iter()
        .filter(|i| is_trunking(i).is_some())
        .collect();

    let mut out = Text::new();
    out.row(&COLS, &["Port", "Mode", "Native vlan", "Status"]);
    for i in &trunks {
        let Some(sp) = is_trunking(i) else { continue };
        out.row(
            &COLS,
            &[
                &i.id.abbrev(),
                "trunk",
                &sp.native_vlan.to_string(),
                "trunking",
            ],
        );
    }

    out.blank();
    out.row(&LIST_COLS, &["Port", "Vlans allowed"]);
    for i in &trunks {
        let Some(sp) = is_trunking(i) else { continue };
        out.row(
            &LIST_COLS,
            &[
                &i.id.abbrev(),
                &fmt::compress_vlans(&allowed_vlans(sp, ctx)),
            ],
        );
    }

    // Allowed-and-active: the allowed set intersected with the VLANs
    // that exist in the management domain.
    out.blank();
    out.row(
        &LIST_COLS,
        &["Port", "Vlans allowed and active in management domain"],
    );
    let active = |sp: &Switchport| -> Vec<u32> {
        allowed_vlans(sp, ctx)
            .into_iter()
            .filter(|v| ctx.active_vlans.contains(v))
            .collect()
    };
    for i in &trunks {
        let Some(sp) = is_trunking(i) else { continue };
        out.row(
            &LIST_COLS,
            &[&i.id.abbrev(), &fmt::compress_vlans(&active(sp))],
        );
    }

    // Spanning-tree forwarding state per VLAN. Hemlock has no STP yet, so
    // every allowed-and-active VLAN reports forwarding; once STP lands
    // this table must reflect real per-VLAN port states.
    out.blank();
    out.row(
        &LIST_COLS,
        &["Port", "Vlans in spanning tree forwarding state"],
    );
    for i in &trunks {
        let Some(sp) = is_trunking(i) else { continue };
        out.row(
            &LIST_COLS,
            &[&i.id.abbrev(), &fmt::compress_vlans(&active(sp))],
        );
    }
    out.finish()
}

/// `show interfaces vlans` — untagged/tagged VLANs per interface.
pub fn vlans(interfaces: &[Interface], ctx: &Context) -> String {
    const COLS: [Col; 2] = [Col::left(11), Col::left(11)];
    let mut out = Text::new();
    out.row(&COLS, &["Port", "Untagged", "Tagged"]);
    for i in super::sorted_tabular(interfaces) {
        let (untagged, tagged) = match i.id.kind {
            // SVIs carry their own VLAN untagged.
            Kind::Vlan => (i.id.num.to_string(), "None".to_string()),
            _ => match &i.switchport {
                Some(sp) if sp.enabled => {
                    if sp.oper_mode == "trunk" {
                        (
                            sp.native_vlan.to_string(),
                            fmt::compress_vlans(&allowed_vlans(sp, ctx)),
                        )
                    } else {
                        (sp.access_vlan.to_string(), "None".to_string())
                    }
                }
                _ => continue, // routed / no switchport state
            },
        };
        out.row(&COLS, &[&i.id.abbrev(), &untagged, &tagged]);
    }
    out.finish()
}
