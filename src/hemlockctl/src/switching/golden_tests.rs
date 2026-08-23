//! Golden-file tests for the switching-suite show family, byte-exact
//! against `tests/golden/` (text and `| json` forms both).

use super::fixtures as fx;
use super::render;

fn norm(text: &str) -> String {
    text.replace("\r\n", "\n")
}

#[track_caller]
fn assert_golden(rendered: &str, golden: &str) {
    let golden = norm(golden);
    if rendered != golden {
        for (n, (got, want)) in rendered.lines().zip(golden.lines()).enumerate() {
            assert_eq!(got, want, "first mismatch at line {}", n + 1);
        }
        assert_eq!(
            rendered.lines().count(),
            golden.lines().count(),
            "line count mismatch"
        );
        assert_eq!(rendered, golden, "whitespace-only mismatch");
    }
}

/// The `| json` form, exactly as the CLI prints it.
fn as_json<T: serde::Serialize>(label: &str, value: &T) -> String {
    let root = serde_json::json!({ label: value });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&root).unwrap_or_else(|_| "{}".into())
    )
}

#[test]
fn vlan() {
    assert_golden(
        &render::vlan(&fx::vlans()),
        include_str!("../../tests/golden/vlan.txt"),
    );
    assert_golden(
        &as_json("vlans", &fx::vlans()),
        include_str!("../../tests/golden/vlan.json"),
    );
}

#[test]
fn vlan_summary() {
    assert_golden(
        &render::vlan_summary(&fx::vlans()),
        include_str!("../../tests/golden/vlan_summary.txt"),
    );
}

#[test]
fn mac_address_table() {
    assert_golden(
        &render::mac_address_table(&fx::mac_table()),
        include_str!("../../tests/golden/mac_address_table.txt"),
    );
    assert_golden(
        &as_json("mac_address_table", &fx::mac_table()),
        include_str!("../../tests/golden/mac_address_table.json"),
    );
}

#[test]
fn mac_count_and_aging() {
    assert_golden(
        &render::mac_count(&fx::mac_table()),
        include_str!("../../tests/golden/mac_count.txt"),
    );
    assert_golden(
        &render::mac_aging_time(&fx::mac_table()),
        include_str!("../../tests/golden/mac_aging_time.txt"),
    );
}

#[test]
fn storm_control() {
    assert_golden(
        &render::storm_control(&fx::storm_control()),
        include_str!("../../tests/golden/storm_control.txt"),
    );
    assert_golden(
        &as_json("storm_control", &fx::storm_control()),
        include_str!("../../tests/golden/storm_control.json"),
    );
}

#[test]
fn port_channel() {
    assert_golden(
        &render::port_channel_summary(&fx::port_channels()),
        include_str!("../../tests/golden/port_channel_summary.txt"),
    );
    assert_golden(
        &render::port_channel_detail(&fx::port_channels()),
        include_str!("../../tests/golden/port_channel_detail.txt"),
    );
    assert_golden(
        &as_json("port_channels", &fx::port_channels()),
        include_str!("../../tests/golden/port_channel.json"),
    );
}

#[test]
fn lacp() {
    assert_golden(
        &render::lacp_neighbor(&fx::port_channels()),
        include_str!("../../tests/golden/lacp_neighbor.txt"),
    );
    assert_golden(
        &render::lacp_neighbor_detail(&fx::port_channels()),
        include_str!("../../tests/golden/lacp_neighbor_detail.txt"),
    );
    assert_golden(
        &render::lacp_counters(&fx::port_channels()),
        include_str!("../../tests/golden/lacp_counters.txt"),
    );
    assert_golden(
        &render::lacp_sys_id(&fx::lacp_system()),
        include_str!("../../tests/golden/lacp_sys_id.txt"),
    );
}

#[test]
fn spanning_tree() {
    assert_golden(
        &render::spanning_tree(&fx::stp_bridge()),
        include_str!("../../tests/golden/spanning_tree.txt"),
    );
    assert_golden(
        &render::spanning_tree(&fx::stp_bridge_nonroot()),
        include_str!("../../tests/golden/spanning_tree_nonroot.txt"),
    );
    assert_golden(
        &render::spanning_tree_detail(&fx::stp_bridge()),
        include_str!("../../tests/golden/spanning_tree_detail.txt"),
    );
    assert_golden(
        &render::spanning_tree_blockedports(&fx::stp_bridge_nonroot()),
        include_str!("../../tests/golden/spanning_tree_blockedports.txt"),
    );
    assert_golden(
        &render::spanning_tree_mst_configuration(&fx::stp_bridge()),
        include_str!("../../tests/golden/spanning_tree_mst_configuration.txt"),
    );
    assert_golden(
        &as_json("spanning_tree", &fx::stp_bridge()),
        include_str!("../../tests/golden/spanning_tree.json"),
    );
}

#[test]
fn mirror() {
    assert_golden(
        &render::mirror(&fx::mirror()),
        include_str!("../../tests/golden/mirror.txt"),
    );
    assert_golden(
        &as_json("mirror_sessions", &fx::mirror()),
        include_str!("../../tests/golden/mirror.json"),
    );
}

#[test]
fn snooping() {
    assert_golden(
        &render::snooping(&fx::snooping()),
        include_str!("../../tests/golden/snooping.txt"),
    );
    assert_golden(
        &render::snooping_groups(&fx::snooping()),
        include_str!("../../tests/golden/snooping_groups.txt"),
    );
    assert_golden(
        &render::snooping_querier(&fx::snooping()),
        include_str!("../../tests/golden/snooping_querier.txt"),
    );
    assert_golden(
        &as_json("snooping", &fx::snooping()),
        include_str!("../../tests/golden/snooping.json"),
    );
}
