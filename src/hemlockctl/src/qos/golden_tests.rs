//! Golden-file tests for the QoS-suite show family, byte-exact against
//! `tests/golden/` (text and `| json` forms both).

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
fn qos_maps() {
    assert_golden(
        &render::maps(&fx::map_state()),
        include_str!("../../tests/golden/qos_maps.txt"),
    );
    assert_golden(
        &as_json("qos_maps", &fx::map_state()),
        include_str!("../../tests/golden/qos_maps.json"),
    );
}

#[test]
fn qos_wred() {
    assert_golden(
        &render::wred(&fx::wred_state()),
        include_str!("../../tests/golden/qos_wred.txt"),
    );
    assert_golden(
        &as_json("qos_wred", &fx::wred_state()),
        include_str!("../../tests/golden/qos_wred.json"),
    );
}

#[test]
fn qos_interface() {
    assert_golden(
        &render::interface(&fx::interface_state()),
        include_str!("../../tests/golden/qos_interface.txt"),
    );
    assert_golden(
        &as_json("qos_interface", &fx::interface_state()),
        include_str!("../../tests/golden/qos_interface.json"),
    );
}

#[test]
fn qos_interfaces() {
    assert_golden(
        &render::interfaces(&fx::interfaces_state()),
        include_str!("../../tests/golden/qos_interfaces.txt"),
    );
    assert_golden(
        &as_json("qos_interfaces", &fx::interfaces_state()),
        include_str!("../../tests/golden/qos_interfaces.json"),
    );
}

/// A platform whose SAI serves no WRED says so instead of rendering a
/// table that could never take effect.
#[test]
fn qos_wred_unsupported_says_so() {
    let mut state = fx::wred_state();
    state.supported = false;
    let text = render::wred(&state);
    assert!(text.ends_with("WRED is not supported by this platform's SAI.\n"));
}
