//! Golden-file tests for the system-suite show family, byte-exact
//! against `tests/golden/` (text and `| json` forms both).
#![allow(clippy::unwrap_used)]

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

#[test]
fn logging() {
    assert_golden(
        &render::logging(&fx::logging_state()),
        include_str!("../../tests/golden/logging.txt"),
    );
    assert_golden(
        &as_json("logging", &fx::logging_state()),
        include_str!("../../tests/golden/logging.json"),
    );
}

/// Nothing forwarded, and an unreadable journal, both say so rather
/// than rendering as a quiet switch.
#[test]
fn logging_edge_cases() {
    let mut state = super::model::LoggingState {
        level: "informational".into(),
        requested: 50,
        journal_available: true,
        ..Default::default()
    };
    let text = render::logging(&state);
    assert!(
        text.contains("Remote hosts : none (local journal only)"),
        "{text}"
    );
    assert!(text.contains("(the journal is empty)"), "{text}");

    state.journal_available = false;
    let text = render::logging(&state);
    assert!(
        text.contains("% the system journal is not readable here"),
        "{text}"
    );
    // With no journal there is no tail footer to print.
    assert!(!text.contains("for more"), "{text}");
}

#[test]
fn system_commits() {
    assert_golden(
        &render::commits(&fx::commits_state()),
        include_str!("../../tests/golden/system_commits.txt"),
    );
    assert_golden(
        &as_json("system_commits", &fx::commits_state()),
        include_str!("../../tests/golden/system_commits.json"),
    );
}

#[test]
fn system_image() {
    assert_golden(
        &render::image(&fx::image_state()),
        include_str!("../../tests/golden/system_image.txt"),
    );
    assert_golden(
        &as_json("system_image", &fx::image_state()),
        include_str!("../../tests/golden/system_image.json"),
    );
}

/// An armed rescue boot changes what the next boot says, and a commit
/// entry with a comment shows it instead of the dash.
#[test]
fn system_image_and_commits_edge_cases() {
    let mut image = fx::image_state();
    image.onie_rescue_armed = true;
    image.next_boot = "ONIE rescue (this boot only)".into();
    let text = render::image(&image);
    assert!(
        text.contains("ONIE rescue    : armed for the next boot"),
        "{text}"
    );
    assert!(
        text.contains("Next boot      : ONIE rescue (this boot only)"),
        "{text}"
    );

    // Nothing recorded at all still renders a full block of dashes.
    let text = render::image(&super::model::ImageState::default());
    assert!(text.contains("Current image  : -"), "{text}");
    assert!(!text.contains("installed"), "{text}");

    let mut commits = fx::commits_state();
    commits.commits[1].comment = "pre-maintenance".into();
    let text = render::commits(&commits);
    assert!(text.contains("pre-maintenance"), "{text}");
    // Index 0 always reads as the running config.
    assert!(text.contains("(current)"), "{text}");
    let text = render::commits(&super::model::CommitsState::default());
    assert!(text.contains("(no commits recorded)"), "{text}");
}

#[test]
fn cable_diagnostics() {
    assert_golden(
        &render::cable_diagnostics(&fx::cable_diag_state()),
        include_str!("../../tests/golden/cable_diagnostics.txt"),
    );
    assert_golden(
        &as_json("cable_diagnostics", &fx::cable_diag_state()),
        include_str!("../../tests/golden/cable_diagnostics.json"),
    );
}

/// A pair the PHY would not measure prints a dash, not a zero-metre
/// run — those are different findings.
#[test]
fn unmeasured_pairs_render_a_dash() {
    let mut state = fx::cable_diag_state();
    state.pairs[2].state = "unknown".into();
    state.pairs[2].length_m = 0;
    let text = render::cable_diagnostics(&state);
    assert!(text.contains("C     unknown    -"), "{text}");
    assert!(text.contains("A     ok         42 m"), "{text}");
}

// ------------------------------------------- the migrated system shows

#[test]
fn version() {
    assert_golden(
        &render::version(&fx::version_state()),
        include_str!("../../tests/golden/version.txt"),
    );
    assert_golden(
        &as_json("version", &fx::version_state()),
        include_str!("../../tests/golden/version.json"),
    );
}

#[test]
fn switch() {
    assert_golden(
        &render::switch(&fx::switch_state()),
        include_str!("../../tests/golden/switch.txt"),
    );
    assert_golden(
        &as_json("switch", &fx::switch_state()),
        include_str!("../../tests/golden/switch.json"),
    );
}

#[test]
fn environment() {
    assert_golden(
        &render::environment(&fx::environment_state()),
        include_str!("../../tests/golden/environment.txt"),
    );
    assert_golden(
        &as_json("environment", &fx::environment_state()),
        include_str!("../../tests/golden/environment.json"),
    );
}

/// `show version` has to answer when syncd does not: the platform lines
/// are replaced by a note saying why, exactly as before the migration.
#[test]
fn version_without_syncd() {
    let down = super::model::VersionState {
        version: "1.4.0".into(),
        switch: None,
        syncd_error: "syncd not running".into(),
    };
    assert_eq!(
        render::version(&down),
        "Hemlock  1.4.0\n(syncd not running)\n"
    );
    let refused = super::model::VersionState {
        version: "1.4.0".into(),
        switch: None,
        syncd_error: "syncd unavailable: transport error".into(),
    };
    assert_eq!(
        render::version(&refused),
        "Hemlock  1.4.0\n(syncd unavailable: transport error)\n"
    );
}

/// A section with nothing in it is omitted, not printed empty — the
/// pre-migration behaviour.
#[test]
fn environment_omits_empty_sections() {
    let mut state = fx::environment_state();
    state.fans.clear();
    let text = render::environment(&state);
    assert!(!text.contains("Fans:"), "{text}");
    assert!(
        text.contains("Temperatures:") && text.contains("PSUs:"),
        "{text}"
    );
    assert_eq!(
        render::environment(&super::model::EnvironmentState::default()),
        ""
    );
}

/// The migration's own proof: these three shows printed inline before
/// the system suite, with no model and no golden. The expressions below
/// are the originals, verbatim from the pre-migration `show.rs`; if the
/// move changed a single byte of layout, this fails.
#[test]
fn the_migrated_shows_are_byte_identical() {
    use std::fmt::Write as _;

    // --- show version (the reachable-syncd branch) --------------------
    let state = fx::version_state();
    let switch = state.switch.clone().unwrap();
    let mut legacy = String::new();
    let _ = writeln!(legacy, "Hemlock  {}", state.version);
    let _ = writeln!(legacy, "Platform:  {}", switch.platform_id);
    let _ = writeln!(legacy, "Backend:   {}", switch.backend);
    let _ = writeln!(legacy, "Ports:     {}", switch.port_count);
    assert_eq!(render::version(&state), legacy);

    // --- show switch ---------------------------------------------------
    let mut legacy = String::new();
    let _ = writeln!(legacy, "Platform:   {}", switch.platform_id);
    let _ = writeln!(legacy, "Backend:    {}", switch.backend);
    let _ = writeln!(legacy, "Switch OID: {:#x}", switch.switch_oid);
    let _ = writeln!(legacy, "Ports:      {}", switch.port_count);
    assert_eq!(render::switch(&switch), legacy);

    // --- show environment ----------------------------------------------
    let env = fx::environment_state();
    let mut legacy = String::new();
    if !env.temperatures.is_empty() {
        let _ = writeln!(legacy, "Temperatures:");
        for t in &env.temperatures {
            let flag = if t.celsius >= t.crit_celsius {
                "  CRIT"
            } else if t.celsius >= t.warn_celsius {
                "  WARN"
            } else {
                ""
            };
            let _ = writeln!(
                legacy,
                "  {:<28} {:>6.1} C  (warn {:.0}, crit {:.0}){flag}",
                t.name, t.celsius, t.warn_celsius, t.crit_celsius
            );
        }
    }
    if !env.fans.is_empty() {
        let _ = writeln!(legacy, "Fans:");
        for f in &env.fans {
            if !f.present {
                let _ = writeln!(legacy, "  {:<28} not present", f.name);
                continue;
            }
            let _ = writeln!(
                legacy,
                "  {:<28} {:>5} rpm  pwm {:>3}%  {}",
                f.name,
                f.rpm,
                f.pwm_percent,
                if f.ok { "ok" } else { "FAULT" }
            );
        }
    }
    if !env.psus.is_empty() {
        let _ = writeln!(legacy, "PSUs:");
        for p in &env.psus {
            let status = match (p.present, p.ok) {
                (false, _) => "absent",
                (true, true) => "ok",
                (true, false) => "FAULT",
            };
            let _ = writeln!(legacy, "  {:<28} {status}", p.name);
        }
    }
    assert_eq!(render::environment(&env), legacy);
}
