//! Golden-file tests for the system-suite show family, byte-exact
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
fn system_users() {
    assert_golden(
        &render::users(&fx::users_state()),
        include_str!("../../tests/golden/system_users.txt"),
    );
    assert_golden(
        &as_json("system_users", &fx::users_state()),
        include_str!("../../tests/golden/system_users.json"),
    );
}

/// An empty box says so in both sections rather than printing bare
/// headers, and unmanaged OS accounts get their own trailing note.
#[test]
fn system_users_edge_cases() {
    let empty = super::model::UsersState::default();
    let text = render::users(&empty);
    assert!(
        text.contains("(none — login accounts are not managed by the configuration)"),
        "{text}"
    );
    assert!(text.contains("(none)"), "{text}");

    let mut with_unmanaged = fx::users_state();
    with_unmanaged.unmanaged = vec!["admin".into(), "svc".into()];
    let text = render::users(&with_unmanaged);
    assert!(
        text.contains("Unmanaged accounts (owned by the OS, not the configuration): admin, svc"),
        "{text}"
    );
}

/// Idle times and login stamps have exactly one spelling.
#[test]
fn clocks_and_stamps() {
    assert_eq!(render::clock(0), "00:00:00");
    assert_eq!(render::clock(252), "00:04:12");
    assert_eq!(render::clock(3_661), "01:01:01");
    // Days roll into hours rather than growing a unit.
    assert_eq!(render::clock(90_061), "25:01:01");
    assert_eq!(render::stamp(1_787_649_164), "2026-08-25 09:12:44");
}
