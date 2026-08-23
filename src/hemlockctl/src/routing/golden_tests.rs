//! Golden-file tests for the routing-suite show family, byte-exact
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
fn ip_route() {
    assert_golden(
        &render::route_table(&fx::ip_route_table()),
        include_str!("../../tests/golden/ip_route.txt"),
    );
    assert_golden(
        &as_json("ip_route", &fx::ip_route_table()),
        include_str!("../../tests/golden/ip_route.json"),
    );
}

#[test]
fn ip_route_summary() {
    assert_golden(
        &render::route_summary(&fx::ip_route_table().summarize(2)),
        include_str!("../../tests/golden/ip_route_summary.txt"),
    );
    assert_golden(
        &as_json("ip_route_summary", &fx::ip_route_table().summarize(2)),
        include_str!("../../tests/golden/ip_route_summary.json"),
    );
}

#[test]
fn ip_route_entry() {
    assert_golden(
        &render::route_entry(&fx::ip_route_table().routes[5]),
        include_str!("../../tests/golden/ip_route_entry.txt"),
    );
}

#[test]
fn ipv6_route() {
    assert_golden(
        &render::route_table(&fx::ipv6_route_table()),
        include_str!("../../tests/golden/ipv6_route.txt"),
    );
    assert_golden(
        &as_json("ipv6_route", &fx::ipv6_route_table()),
        include_str!("../../tests/golden/ipv6_route.json"),
    );
}
