//! Golden-file tests for the services-suite show family, byte-exact
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
fn lldp() {
    assert_golden(
        &render::lldp(&fx::lldp_state()),
        include_str!("../../tests/golden/lldp.txt"),
    );
    assert_golden(
        &as_json("lldp", &fx::lldp_state()),
        include_str!("../../tests/golden/lldp.json"),
    );
}

#[test]
fn lldp_neighbors() {
    assert_golden(
        &render::lldp_neighbors(&fx::lldp_state()),
        include_str!("../../tests/golden/lldp_neighbors.txt"),
    );
    assert_golden(
        &render::lldp_neighbors_detail(&fx::lldp_state()),
        include_str!("../../tests/golden/lldp_neighbors_detail.txt"),
    );
}

#[test]
fn ntp() {
    assert_golden(
        &render::ntp(&fx::ntp_state()),
        include_str!("../../tests/golden/ntp.txt"),
    );
    assert_golden(
        &as_json("ntp", &fx::ntp_state()),
        include_str!("../../tests/golden/ntp.json"),
    );
}

#[test]
fn snmp() {
    assert_golden(
        &render::snmp(&fx::snmp_state()),
        include_str!("../../tests/golden/snmp.txt"),
    );
    assert_golden(
        &as_json("snmp", &fx::snmp_state()),
        include_str!("../../tests/golden/snmp.json"),
    );
}

#[test]
fn sflow() {
    assert_golden(
        &render::sflow(&fx::sflow_state()),
        include_str!("../../tests/golden/sflow.txt"),
    );
    assert_golden(
        &as_json("sflow", &fx::sflow_state()),
        include_str!("../../tests/golden/sflow.json"),
    );
}
