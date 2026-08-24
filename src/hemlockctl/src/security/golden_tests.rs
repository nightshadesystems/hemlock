//! Golden-file tests for the security-suite show family, byte-exact
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
fn acl() {
    assert_golden(
        &render::acl(&fx::acl_state()),
        include_str!("../../tests/golden/acl.txt"),
    );
    assert_golden(
        &as_json("acl", &fx::acl_state()),
        include_str!("../../tests/golden/acl.json"),
    );
}

#[test]
fn acl_summary() {
    assert_golden(
        &render::acl_summary(&fx::acl_state()),
        include_str!("../../tests/golden/acl_summary.txt"),
    );
}

#[test]
fn copp() {
    assert_golden(
        &render::copp(&fx::copp_state()),
        include_str!("../../tests/golden/copp.txt"),
    );
    assert_golden(
        &as_json("copp", &fx::copp_state()),
        include_str!("../../tests/golden/copp.json"),
    );
}

#[test]
fn port_security() {
    assert_golden(
        &render::port_security(&fx::port_security_rows()),
        include_str!("../../tests/golden/port_security.txt"),
    );
    assert_golden(
        &render::port_security_detail(&fx::port_security_rows()),
        include_str!("../../tests/golden/port_security_detail.txt"),
    );
    assert_golden(
        &as_json("port_security", &fx::port_security_rows()),
        include_str!("../../tests/golden/port_security.json"),
    );
}

#[test]
fn dot1x() {
    assert_golden(
        &render::dot1x(&fx::dot1x_state()),
        include_str!("../../tests/golden/dot1x.txt"),
    );
    assert_golden(
        &as_json("dot1x", &fx::dot1x_state()),
        include_str!("../../tests/golden/dot1x.json"),
    );
}

#[test]
fn dhcp_snooping() {
    let state = fx::snoop_state();
    assert_golden(
        &render::dhcp_snooping(&state.dhcp),
        include_str!("../../tests/golden/dhcp_snooping.txt"),
    );
    assert_golden(
        &render::dhcp_snooping_binding(&state.dhcp.bindings),
        include_str!("../../tests/golden/dhcp_snooping_binding.txt"),
    );
    assert_golden(
        &render::dhcp_snooping_statistics(&state.dhcp.statistics),
        include_str!("../../tests/golden/dhcp_snooping_statistics.txt"),
    );
    assert_golden(
        &as_json("dhcp_snooping_binding", &state.dhcp.bindings),
        include_str!("../../tests/golden/dhcp_snooping_binding.json"),
    );
}

#[test]
fn arp_inspection() {
    let state = fx::snoop_state();
    assert_golden(
        &render::arp_inspection(&state.arp),
        include_str!("../../tests/golden/arp_inspection.txt"),
    );
    assert_golden(
        &render::arp_inspection_statistics(&state.arp.statistics),
        include_str!("../../tests/golden/arp_inspection_statistics.txt"),
    );
    assert_golden(
        &as_json("arp_inspection", &state.arp),
        include_str!("../../tests/golden/arp_inspection.json"),
    );
}
